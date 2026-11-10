# Sync driver

Status: part 1 of step C (plans/macos-0.1.0.md). The driver links folders, keeps a session with each space and applies the server's log to the store's remote tree, observes the disk, and serves the CLI. It does not run the sync engine yet: nothing is uploaded and nothing is written to a linked folder except its root marker. Part 2 wires the engine once its commit layer (#28) is merged; [What part 2 wires](#what-part-2-wires) lists exactly what that adds.

## Crates and boundaries

```mermaid
flowchart LR
  APP["core/ffi: macOS app (step D)"] -.-> D
  D --> NET["core/net: Session, Api"]
  D --> ST["core/store: one store per space"]
  D --> FS["core/fs: watchers, root check, marker"]
  D --> CH["core/chunk: Content"]
  D -.-> E["core/engine (part 2)"]
```

- **`core/driver`** (`flarebox-driver`) is everything a client runs on one device around the engine. It is a library with an async API, so the daemon calls it from its Tokio runtime and the macOS app can call it in-process through `core/ffi`, holding a runtime of its own. It never prints and never exits; it logs through `tracing`.
- The driver depends on `core/net`, `core/store`, `core/fs` and `core/chunk`, and not on `core/engine`. Part 2 adds that dependency.

## Profile layout

A profile is one device credential and the stores it writes. The CLI uses the XDG base directories; the app passes its own container (`Paths::under`).

| Path | Contents |
|---|---|
| `$XDG_CONFIG_HOME/flarebox/config.toml` | The device-local configuration. Edited by the person; no secrets, no state, never synced |
| `$XDG_STATE_HOME/flarebox/` (mode 0700) | Machine-bound state, never copied |
| `…/credential.json` (mode 0600) | The device credential: server, user and device ids, device name, refresh credential |
| `…/lock` | The profile lock (`flock`), held by the one process that writes the profile |
| `…/spaces/<space_id>/store.sqlite` | One `core/store` per linked space: R, S, L, the cursor, the link and the questions |
| `$XDG_RUNTIME_DIR/flarebox/daemon.sock` (mode 0600) | The daemon's socket; in the state directory when `XDG_RUNTIME_DIR` is unset |
| `<folder>/.flarebox/` | Inside each linked folder: the root marker `space.json` and, from part 2, staging, quarantine, parked entries and the local trash |

**One credential, one writer (H6 of reports/BOX-71).** Same-device changes never count as unseen on the server, and `device_seq` is strict, so two writers sharing a credential would overwrite each other silently or refuse each other's commits. The driver keeps the rule in four places:

1. `pair` refuses to replace a credential: a profile is paired once. The macOS app and the CLI on the same machine are separate profiles and pair separately.
2. Opening a driver takes the profile lock, so a second daemon, or a command while the daemon is not answering, is refused.
3. Each store records the device and server that linked it (`Link` in `core/store`). A store whose device is not the profile's halts with a problem instead of syncing.
4. `link` refuses a folder that holds, or sits inside, the state, runtime or configuration directory, so the credential and the stores are never synced.

A profile copied whole to another machine (credential and stores together) passes these checks. RFC 0002 section 6 detects it through `device_seq` once the engine compares the counter in part 2.

## Configuration and selective sync

```toml
[sync]
include = ["Projects/"]   # when not empty, only these paths are synced
exclude = ["*.iso"]       # .fbignore patterns
```

The rules apply to every folder of the profile (H13). `SyncRules::lines` turns them into the lines of one root `.fbignore` file: `/*`, then negations that re-include each listed path and the folders leading to it (a path cannot be re-included once a parent is excluded), then the exclude patterns. That file goes into the matcher input, `ignore::IgnoreInput`, as a `RuleSource::Device` file after the folder's own `.fbignore` files, so the device can always narrow what it syncs. Like `.fbignore`, the rules only save bandwidth and disk: the server still sends every name in the space, and the credential can read everything.

The matcher is GUN-270's. Until it lands, `ignore::matcher` returns `Unfiltered`, which excludes nothing, and the driver logs a warning when the configuration has rules. Replacing that function body is the whole integration; the observer already filters every change through `Matcher::is_excluded`.

## The driver API

`Driver::open(paths)` takes the lock and reads the configuration, the credential and the stores. Then:

| Call | What it does |
|---|---|
| `pair(server, code, name)` | `POST /v1/pair/complete` with a code from a paired device, then writes the credential. Headless: no browser |
| `link(folder, space)` | Resolves the space by id, then by unique name (`GET /v1/spaces`); checks the folder (exists, no overlap with the profile or another linked folder, no marker of another space, a no-replace rename, design §5.2); writes the root marker if missing; creates the store with its `Link`. Starts the folder if the driver runs |
| `unlink(folder)` | Stops the folder and deletes its store. The files and the folder's `.flarebox/` stay, because the local trash may hold displaced files |
| `status()` | The account and, per folder, state, cursor, head, remote node count, local entry count, unanswered questions and the current problem |
| `questions()`, `answer(id, answer)` | Lists unanswered questions from the stores and records an answer, refusing one the question does not offer |
| `start()`, `stop()` | Starts one task per folder on the current runtime; stops them and waits |

How `core/ffi`'s `Core` trait maps onto it in step D:

| `Core` | Driver |
|---|---|
| `state` | `status` and `questions`. Step D adds a change notification (a `watch` channel next to the status) so the app gets `SessionEvent`s instead of polling |
| `start`, `stop`, `pair`, `link_folder`, `unlink_folder`, `answer` | the calls of the same name (`unlink_folder` takes a space id; the app finds the path in the status) |
| `connect`, `claim`, `start_pairing`, `spaces`, `create_space` | not in the driver yet; each is one `core/net` `Api` call |
| `pause`, `resume`, `sign_out`, `dismiss_notice`, `devices` | not in the driver yet; notices and the pause need state the engine produces, and no server lists devices |

## A folder's task

1. **Root check** (design §6.2): the folder exists, is readable, is on the device recorded at link time and holds a marker for its space. A folder that fails is halted with the reason and checked again every 30 seconds; nothing else runs for it meanwhile.
2. **Observation** runs on a thread of its own (below) and reports how many entries the folder holds.
3. **Session**: `core/net`'s `Session` with one `Auth` built from the credential, which reconnects with backoff on its own. Events are handled in order:

| Event | Part 1 |
|---|---|
| `Connected(Welcome)` | With no cursor in the store: bootstrap (below), then subscribe from the first page's `head_seq`. With a cursor of the same history: subscribe from `last_seq`, once per session (the session resumes by itself after a reconnect). With a cursor of another history: halt; that is a recovery (§6.4) |
| `Changes` | Applied to R in one store transaction, in seq order, skipping records the store holds: each `NodeState` is a post-image, `NodePurged` removes the node, `SpaceUpdated` and `MemberUpdated` change nothing yet. A gap resubscribes from the cursor. A change of an unknown kind halts before its record (spec v0 §9.1: fail closed) |
| `CaughtUp` | State `up_to_date` |
| `ResyncRequired` | `LOG_TRUNCATED`: bootstrap again. `HISTORY_CHANGED`, `AHEAD_OF_SERVER` and unknown reasons: halt |
| `Disconnected` | State `offline` with the reason; the session retries |
| `Stopped` | Halt with the reason (credentials refused, device revoked, space not found or forbidden, unsupported version) |
| `Committed` | Logged; part 1 sends no commits |

**Bootstrap** (spec v0 §7.5) pages through `ListTree` 5,000 nodes at a time and replaces R in one transaction, with the cursor at the first page's `head_seq`. Replaying the log from there fixes up nodes that changed between pages.

**Refusals (H1).** `policy::after_refusal` decides what follows a refused call, and `ListTree` goes through it today:

| Code | Disposition |
|---|---|
| `RETRY_LATER` | retry after `retry_after_ms`, or after backoff when it is 0 (the session already retries it a few times) |
| `QUOTA_EXCEEDED` | retry after `retry_after_ms` only when it is above 0; halt otherwise |
| `INTERNAL` | retry after backoff |
| any other known code | halt and report |
| `UNSPECIFIED` or a code this build does not know | halt and report with the raw number; never resent |

`core/net` is getting the same rule (BOX-52). When it lands, the driver keeps `policy` only for what `core/net` does not decide.

## Observation

`FsObserver` implements the `Observer` trait over `core/fs`'s `NativeWatcher` (inotify on Linux, FSEvents on macOS), falling back to a full rescan every 30 seconds when the platform watcher cannot cover the tree. Each batch is an `Observation`, the input the engine's `Observed(changes, root_check)` needs (design §1, §9):

- `Observed::Created`, `Modified`, `Renamed` and `Removed`, with paths relative to the root and the file identity of every entry;
- the `Content` of every created or modified file, chunked with `core/chunk`'s V0 profile. The file is opened one folder at a time without following links, must still be the entry the scan saw, and must not change while it is read; otherwise the change carries no content and the path is listed in `busy` (design §5.8);
- the root check made after the batch's scans, and the folders the scans could not read.

Changes whose path the matcher excludes are dropped. In part 1 the driver starts each observer from an empty snapshot, so its first batch lists the whole folder, and it only counts the entries.

## What part 2 wires

Once #28 is merged, part 2 puts the engine between the inputs and outputs part 1 already produces and consumes, per folder:

1. **Engine state.** Build the engine from the store (R with the cursor, S, L with identities, the commit in flight, the ID generator, the accepted device counter and the effect counter) and run its `handle(now_ms, input)` loop in the folder task. After each input, write the engine's row changes in one store transaction, then execute its outputs (design §1).
2. **Server log.** Replace `folder::apply_changes` and `folder::replace_tree` with the engine inputs `Changes`, `TreePage`, `CaughtUp` and `ResyncRequired`, and the engine's `Resume` and `ListTree` outputs. `Connected(Welcome)` goes to the engine, which compares `history_id` and `device_seq` with the store (design §2.8, RFC 0002 §2); the halts on another history, `HISTORY_CHANGED` and `AHEAD_OF_SERVER` become recovery (§6.4).
3. **Observations.** Feed each `Observation` to the engine as `Observed`, with its root check. Start the observer from a snapshot rebuilt from L instead of an empty one, after recovering the effect in flight and checking the marker's effect counter against the store (§2.8, §5.3). Read `.fbignore` files into `IgnoreInput` with the device rules.
4. **Effects.** Execute `ApplyEffect` through `core/fs`'s `Space::apply` with the quarantine protocol, writing the marker counter first, and report `EffectDone` or `EffectFailed`. File contents come from `GetDownloadTargets` and `Transfer::download` into staging, through the chunk cache in the store.
5. **Commits.** Execute `SendCommit`: `PrepareUpload` and `Transfer::upload` for the chunks it references, then `Session::commit`. Report `CommitResult` or `CommitFailed` with the refusal disposition of `policy`.
6. **Questions and problems.** Store each `AskUser` as a question row whose body is a `Question`; give recorded answers to the engine as `UserDecision` and delete the rows it has consumed. Show `Problem` outputs in the folder's status.
7. **Pass condition:** two Linux profiles linked to one space sync files both ways, as an integration test against the Worker under `wrangler dev`.

## Spec notes

These belong to the spec owner (RFC 0006, #44); the driver follows them already:

- H1: a client treats an unknown `Error.Code` as a refusal that halts the operation, and retries `QUOTA_EXCEEDED` only when `retry_after_ms` is above 0.
- H6: two processes that write must never share a `device_id`; a second client on the same machine pairs on its own.
