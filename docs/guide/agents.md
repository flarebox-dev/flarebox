# Agents and scripts

Every `koi` command can run without a person at the terminal. Add `--json` and read one JSON object from stdout, then branch on the exit status. Start with `koi status --json`.

## Ask for JSON

`--json` works on every command and in any position, as in `koidrive --json status` or `koi folder link ~/Work --space Work --json`. It selects schema 1, the only schema so far. `--json=1` names it explicitly, and any other version is a usage error. A later schema will need its number; plain `--json` keeps meaning schema 1.

With `--json`, every command except `koi _daemon` prints exactly one JSON object on stdout, also when it fails, when it shows help and when it shows the version. The daemon prints one object per line, as [Watch the daemon](#watch-the-daemon) describes. Logs and progress go to stderr. A command with `--json` never asks a question, never opens a browser and prints no pending-decision line. It reads stdin only when an explicit input flag, such as `koi setup --pairing-code-stdin`, requests it.

Without `--json`, a command whose result is one value prints only that value on stdout, and what it means on stderr, so that shell capture works: `koi space create` prints the new space's ID, `koi history restore` the restore's ID, and `koi device invite` the pairing code, as in `id=$(koi space create Notes)`.

## The envelope

```json
{
  "schema_version": 1,
  "command": "history restore",
  "outcome": "accepted",
  "data": { "request_id": "019b...", "folder": "/home/alice/Koidrive", "path": "notes/old.txt" },
  "error": null,
  "decisions": [],
  "next_commands": [{ "argv": ["koi", "history", "requests", "/home/alice/Koidrive"] }]
}
```

| Field | Meaning |
|---|---|
| `schema_version` | Always 1 |
| `command` | The subcommand, such as `folder link`. Empty for `koi` alone |
| `outcome` | `ok`, `accepted`, `unchanged` or `error` |
| `data` | The command's result. Its shape depends on the command. `null` when there is none |
| `error` | `null` on success. Otherwise `code`, `message` and `retryable` |
| `decisions` | The sync decisions waiting on this device, described below |
| `next_commands` | Commands to run next, each as an `argv` array. Present only when there is a real next step |

`accepted` means the command recorded work that happens later. Answering a decision and asking for a restore return it. Check `koi status` or `koi history requests` to see the work finish.

`unchanged` means the state the command asks for was already there, so the command changed nothing. It exits 0, and `data` describes the state as it is. [Run a change again](#run-a-change-again) lists the commands that return it.

A failed command can still carry `data`. For example, `koi space usage` returns the spaces that answered when the server refuses another one.

Each command's envelope has a JSON Schema (draft 2020-12) in [docs/json-schema/v1](../json-schema/v1), named after the command: `koi-folder-link.json` for `koi folder link`, and `koi.json` for `koi` alone. `koi-help.json` covers `koi help` with any command after it. Schema 1 can gain optional fields, so a validator should allow fields it doesn't know.

`koi help --json`, or `--help --json` after any command, lists that command and every command under it in `data.commands`. Each entry has the command's `about`, its `usage` line, `aliases`, `arguments`, `options`, `groups` and `subcommands`. Each argument and option lists what it `requires` and `conflicts` with, and an option says whether its value is optional and must follow an equals sign, as `--json=1` does. A group names options of which one must be given, or at most one may be. The entry also has `auth` (`none`, `device` when it needs this device's credential from `koi setup`, or `cloudflare` when it needs a Cloudflare login that only `--interactive` starts), `output` (`envelope`, or `lines` for `koi _daemon`) and `schema`, the file of its schema in docs/json-schema/v1.

## Decisions

`koi status` and `koi resolve` list every pending decision in `decisions`. Every other command lists the decisions the running daemon holds. Each decision has every item it holds and the full command for each answer:

```json
{
  "id": "019b...",
  "space_id": "019a...",
  "kind": "mass_delete",
  "asked_at_ms": 1762900000000,
  "requires_choice": true,
  "counts": { "deletions": 24, "affected_files": 1240, "space_files": 2000, "exceeded": ["deletions"] },
  "items": [{ "node_id": "019c...", "path": "archive", "files": 1240 }],
  "answers": [
    { "token": "delete", "consequence": "Delete these files on the server and other devices", "argv": ["koi", "resolve", "019b...", "delete"] },
    { "token": "restore", "consequence": "Download these files to this device again", "argv": ["koi", "resolve", "019b...", "restore"] }
  ]
}
```

`kind` is `mass_delete`, `remote_mass_delete`, `ambiguous_disappearance`, `recovery` or `conflict`. A `recovery` decision also has a `group`. A `conflict` decision's `items` are its two files, and its answers are `keep-node:<id>` for each file's node ID and `keep-both` (see [Decisions](status.md#decisions)). A `remote_mass_delete` notice has `requires_choice: false`. It only takes `dismiss` and holds nothing back. Don't pick an answer that deletes files on your own. Ask the person.

## Run a change again

A command can time out before you see its result. Run it again: the commands below never make a change twice.

These commands set a state, so running one again when the state already holds returns `unchanged` with exit status 0:

- `koi folder link <folder> --space <space>` when the folder is already linked to that space. Linking it to another space fails and changes nothing. With a space name rather than an ID, the command reads the server's space list, so it fails while the server can't be reached.
- `koi folder unlink <folder>` when the folder isn't linked. `data` then has only `path`.
- `koi device revoke <id>` when the device is already revoked.
- `koi space create <name>` when a space has that name. It prints that space's ID, as for a new space.
- `koi resolve <id> <answer>` with the answer the decision already took. Another answer, or a decision that was replaced, fails with `stale_decision` (exit 7).

These commands make a new change each time they run: `koi history restore`, `koi device invite`, `koi folder restart` and `koi folder retry`. Give them `--request-id <id>` on the first attempt. The ID is 1 to 128 ASCII letters, digits, `-` and `_`, and you pick it. Running the command again with the same ID and arguments returns the first outcome, with the same `data` and the same stdout, and changes nothing. This holds after the daemon or the machine restarts.

- The same ID with other arguments fails with `stale_decision` (exit 7) and no next command. Repeat the first call as it was, or pick a new ID. An ID for `koi device invite` also belongs to the device and server that ran it.
- A pairing code expires after about 10 minutes. After that, `koi device invite` with its ID fails with `stale_decision`. Run it with a new ID.
- A command whose change is known to have failed records nothing, so running it again with its ID tries again.
- If the first run stopped before it recorded its outcome, still runs, or couldn't tell whether its change happened (for example, the server didn't answer), the command fails with `interrupted` (exit 130). Read the state with `koi status --json` or `koi history requests`. To make the change, use a new ID.

The records are in `requests/` in the profile's state directory (`~/.local/state/koidrive` by default), readable only by you. A record of `koi device invite` holds the pairing code. Pairing codes never appear in the logs.

## Exit status

| Status | Meaning | What to do |
|---|---|---|
| 0 | The command succeeded, found nothing to change, or recorded work for later | Read `outcome`. `accepted` isn't done yet |
| 1 | Another failure | Read `error` and `next_commands` |
| 2 | Invalid arguments, or input the command needs is missing | Correct the call. Running it again unchanged fails again |
| 3 | `status --check` found a decision that needs a choice | Read the answers in `decisions` |
| 4 | A credential is missing or refused, or a person must log in | Run the command in `next_commands`, or ask a person to |
| 5 | A folder is halted or stopped | Fix the cause the folder's `problem` names |
| 6 | The server is offline, unavailable or limits the rate | Try again later. `retryable` is true |
| 7 | The decision answered no longer exists, or a request ID was used with other arguments or its pairing code expired | Read the current state, then decide again |
| 130 | Ctrl-C stopped the command, or an earlier run with the same request ID has no recorded outcome | Read the state before you run a change again |

The `error.code` values match these: `failed`, `usage`, `decision_required`, `auth_required`, `halted`, `unavailable`, `stale_decision` and `interrupted`.

## Check health with status --check

`koi status` exits 0 whenever it can read the status. `koi status --check` exits with the status of the most urgent condition:

1. 4 when this device isn't set up.
2. 5 when a folder is halted, or stopped because the daemon isn't running. `service` in the same output says whether the background service runs and why not.
3. 3 when a decision needs a choice. A notice that only takes `dismiss` doesn't count.
4. 6 when a folder is offline, or puts a call off until later. The folder's `wait` says why: `quota` when the server refused for a used-up limit or full storage, `retry` when the call kept failing for another reason. `until_ms` is when it's sent again.

Connecting and catching up aren't failures, so they exit 0. Neither is a pause: while sync is paused, `data.paused` is true (it's left out otherwise), `data.conditions` holds `paused`, which exits 0, the folders the pause stopped add no `stopped` condition, and `next_commands` names `koi resume`. `data.conditions` lists every condition found, with or without `--check`.

`data.update` says whether a newer release is out: `state` is `available`, `up_to_date`, `unknown` (no check has had an answer yet) or `off`, and `version` is the version this device runs. With `available` it adds `latest`, `url` and `command`, which is `koi update`. `checked_at` is when GitHub last answered. `--check` ignores it.

## Watch the daemon

`koi _daemon --json` prints one JSON object per line on stdout, each with `schema_version` and `event`: `started` with the version, the socket and the number of folders, `folder_state` with the folder, its space and its new state whenever a folder's state changes, `stopping` with the signal, and `stopped`. Its logs on stderr are JSON lines too. If it can't start, it prints one error envelope on a single line.

## Questions need a terminal

No command asks a question unless both stdin and stderr are terminals. Without them, commands that would ask stop instead and say which flag answers the question.

- `koi setup` runs the guided setup only on a terminal, without `--json` and without the `CI` environment variable. Otherwise it lists the setup commands and exits 0. With `--json` they are in `data.choices` and `next_commands`. `--folder` and `--space`, with `--pairing-code-stdin` on a new device, run a [scripted setup](setup.md#scripted-setup) that asks nothing.
- `--interactive` asks for the guided setup explicitly. It fails with exit status 2 when no terminal is attached, and can't be used with `--json`.
- `koi server deploy`, `add-device`, `upgrade` and `delete` log in with Cloudflare in a browser, so they run only with `--interactive` and a terminal on stdin and stderr. Otherwise, with or without `--json`, they stop with `auth_required` (exit 4) and name the same command with `--interactive` for a person to run.
- `--no-browser` reads the login address from the terminal, so it needs `--interactive` too.

`koi join` and `koi server claim` never need a person, so an agent can set up a device with a pairing code or a claim code.
