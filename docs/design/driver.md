# Sync driver and Linux daemon

`core/driver` (`koidrive-driver`) is everything a client runs on one device around the sync engine: the profile, one task per linked folder, the session with each space, and the observer of each folder. The Linux daemon `koi _daemon` and the macOS app both run it. Paths, configuration keys, the API and the event handling are in [driver-reference.md](driver-reference.md). Engine sections cited as §N are in [the sync engine reference](sync-engine/README.md).

## Both clients share one library

The driver is a library with an async API. It never prints or exits, and it logs through `tracing`. The daemon calls it from its Tokio runtime. The macOS app calls it in-process through `core/ffi`, whose `DriverCore` wraps a `Driver` and sends the calls the driver does not make, such as listing devices or spaces, through `core/net`'s `Api` with the driver's credential (`Driver::signed_in`). A `koi` command sends its request to the daemon's socket, or opens a driver of its own when no daemon runs.

## One credential has one writer

Two processes that share a device credential would overwrite each other's commits or refuse them, because the server does not count a device's own changes as unseen and `device_seq` is strict. The driver keeps the rule in five places:

1. `pair` and `claim` refuse to replace a credential. The app and the CLI on one machine are separate profiles and pair separately.
2. Opening a driver takes the profile lock, so a second daemon is refused.
3. Each store records the device and server that linked it. A store of another device halts instead of syncing.
4. `link` refuses a folder that holds or sits inside the profile's directories or another linked folder, compared by path and by folder identity, so links and mounts cannot hide an overlap.
5. A linked folder's `.koidrive/device.json` names its device, and a running folder holds an `flock` on the folder itself. Another profile's `link` refuses the folder, and its task halts.

On macOS the credential file is excluded from Time Machine before anything is written to it. A Mac restored from a backup has its stores and folders but no credential, so those folders halt as signed out until the device pairs again.

## A folder task runs one loop

A task checks the root (§6.2), checks placement and the folder's claim, starts the observer on its own thread, and opens a `core/net` `Session` that reconnects with backoff. A failed root check halts the folder and checks it again later. Any other halt waits for a person.

`Folder` in `folder.rs` owns the engine, the store and the disk, and is their only writer. Each of its methods takes one input: a session event, an observation batch, or the result of work it asked for. The method changes the engine, ends with `Engine::checkpoint` in the same store transaction, and returns the work to start. No transaction contains an `await`, so a store loaded after any step gives the same engine. Around it, one `select!` hands each input to the folder and runs the returned work: a tree listing, staging one effect, uploading one commit's chunks, the commit call, or a wait.

After each input the folder plans in the order of the engine's reference loop (`core/sim/tests/engine_sync.rs`). Nothing is planned before the first scan or while the store is behind the log. Effects run one at a time, and only while no commit is in flight. A commit is built when no effect is planned and the store holds the server's head. A refused commit goes back to the engine, which sends it again, plans its changes again or stops committing.

## A first link adopts what matches and holds the rest

A folder with no cursor in its store waits for its first scan before it takes the server's tree. Then `koidrive_engine::first_link::adopt` matches the folder against the space from the root down (§6.4). An entry with the same name, kind and content as a live node takes that node. Everything else is held under one question, sending nothing, until the person publishes it or moves it to the local trash. Entries created after the first link are sent at once.

## Bootstrap reads the whole tree, then follows the log

A bootstrap pages through `ListTree`, and once the folder has synced, through `ListTrash` too, then replays the log onto that tree. The tree replaces the store's remote tree in one transaction once the replay reaches the server's head, so the store holds a valid tree at every commit. `LOG_TRUNCATED` and `AHEAD_OF_SERVER` bootstrap again. A space rewound on the server (`HISTORY_CHANGED`) halts the folder.

## The observer reports changes with their content

`FsObserver` runs `core/fs`'s watcher (inotify on Linux, FSEvents on macOS) and rescans every 30 seconds where the watcher cannot cover the tree. Each batch carries the changes, the content of every created or modified file chunked with `core/chunk`, and the root check. It starts from a snapshot rebuilt from the store, so the first scan after a restart reports only what changed meanwhile, and reads only the files that differ.

## The daemon serves the CLI over a socket

`koi _daemon` opens the driver, binds a socket only its user can reach, starts every folder, and serves one JSON line each way per connection until SIGTERM or SIGINT. It shows a desktop notification when a decision opens. Its logs never carry a refresh credential, an access token or a pairing code. Linux runs it as a systemd user service, and macOS as a launch agent.

## A rewound space halts its folders, and status reports no progress

A rewound space halts each folder that syncs it, because the driver does not run the engine's recovery (§6.4) for it. `status()` reports no per-folder progress, so the app's `SyncProgress` is always empty.
