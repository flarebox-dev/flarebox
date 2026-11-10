# Refresh credential rotation

Status: design for spec/rfc/0011-refresh-credential-rotation.md. Nothing here is built yet.

RFC 0011 defines the protocol: the device generates its next credential, stores it with a deadline, and sends it with a token request, and the server stores it in place of the old one if the request arrives before the deadline. This note says how the server, `core/net`, `core/driver`, `core/ffi` and the CLI implement it, and how to test it.

## Server

**No schema change.** `devices.refresh_hash` already holds the one credential the server accepts. A rotation overwrites it.

**`refreshAccess`.** It checks the shape of the request first: a malformed `next_refresh_credential` (not `^[A-Za-z0-9_-]{43}$`), or one without `rotation` and `rotate_before_ms`, is `INVALID_ARGUMENT`. It computes the SHA-256 of the presented credential and of the proposal before it reads the row, because hashing is asynchronous. Then, with no `await` between the read and the write, it takes the time once, loads the active principal with the existing guards (unknown, revoked or expired is `UNAUTHENTICATED`), and compares the hash. A rotation whose deadline has not passed updates `refresh_hash` in the same statement as the existing `last_seen_ms` update. The answer carries that same time as `server_time_ms` when the request set `rotation`. A rotation adds no row write, since every refresh already writes `last_seen_ms`.

**Proto.** `TokenRequest` gains `rotation = 3`, `next_refresh_credential = 4` and `rotate_before_ms = 5`, and `AccessToken` gains `server_time_ms = 3` and `rotated = 4`. All are additive, so `buf breaking` against main passes.

## Clients

**One credential per process.** Today each folder task builds its own `Auth` (core/driver/src/driver.rs, `spawn`), as do linking, the sign-out revocation and the FFI's server calls. After a rotation, every one of them except the one that rotated would present a credential the server refuses. So the driver builds one `Arc<Auth>` when it loads or receives the credential, and every caller in the process uses it: folder tasks, `link`, the sign-out revocation, and the FFI's `signed_in` while a driver is open.

**`core/net`.** `Auth` keeps its `Credentials`, the staged credential and its deadline behind the same lock as its cached token, so one refresh at a time reads or changes them, and `Auth::credentials` returns a copy. It takes an optional store with three operations, each of which returns once its write is durable: stage a credential with its deadline, make the staged credential current, and clear the staged slot. With a store, `access_token` follows RFC 0011 §2 steps 1 to 6. Without a store, it never sets `rotation`, and on `UNAUTHENTICATED` from `POST /v1/token` it calls a reload function once and presents what it returns (RFC 0011 §2, processes that only read).

**`core/driver`: two files.** The slots are two files in the state directory, so that a build from before rotation can still read the profile:

- `credential.json` keeps its format. `Credential` has `deny_unknown_fields`, so a new field would make the file unreadable to an older build. Its modification time is when the credential last changed, which gives the credential's age without a new field.
- `credential.next` holds the staged credential and its deadline, only while a rotation's outcome is unknown. An older build ignores it.

Both are written to a temporary file with mode 0600, excluded from Time Machine as the credential file is at pairing, synced, renamed into place, and followed by a sync of the directory. Promoting writes `credential.json` first and removes `credential.next` second. A crash in between leaves both files with the same credential. The next refresh presents it as current, is accepted with a time later than the deadline or not, and either way ends with one file. `save_new` stays as it is for pairing. Only a `Driver`, which holds the profile lock, gives its `Auth` a store. Sign-out deletes both files.

**Reading the two files as one state.** Only the owning process writes them, and it changes them in three ways: staging creates `credential.next`, promoting replaces `credential.json` and then removes `credential.next`, and clearing removes `credential.next`. A reader:

1. Notes the identity (device, inode and modification time) of `credential.next`, or that it is absent, and reads it.
2. Reads `credential.json` and notes its identity.
3. Checks both identities again. If either changed, it starts over.

Every change gives a file a new identity or removes it: the writer replaces files by renaming new ones into place. So if neither identity changed, `credential.json` held the same value from step 2 to step 3, and `credential.next` held the same value at step 1 and step 3. At the moment of step 3, the two files hold exactly what the reader read. A writer that changes them faster than a reader can read them would make the reader retry forever. The owning process changes them a few times a day, so a retry limit of 10 is enough, after which the reader fails with an I/O error instead of reporting a sign-out.

An older build that starts while `credential.next` exists uses `credential.json`. If the server already stored the staged credential, that build is refused and signed out. This only happens on a downgrade while a rotation's outcome is unknown.

**CLI.** The commands that call the server directly (`device list`, `device revoke`, `usage`) build an `Auth` without a store, whose reload function reads both files as above. The command that opens its own driver goes through the driver's `Auth`.

**macOS app.** The app opens its profile through the FFI driver, so it rotates like the daemon.

## Tests

- Server (`server/test/`): `rotation` adds `server_time_ms` and nothing else; a proposal before its deadline is stored and sets `rotated`, after which the old credential is refused and the device is not revoked; a proposal after its deadline is answered without `rotated` and changes nothing; malformed or incomplete proposals are `INVALID_ARGUMENT`; a revoked or expired principal's proposal is refused and changes nothing; of two proposals from the same credential, only the first one stored works.
- `core/net`, against a stub server that can hold a request and answer it later, for each step of RFC 0011 §2: the stage write fails; the rotation answer is lost; the rotation request is held while the client presents the current credential and then stored before the deadline, after which the client moves to the staged credential; the request is held past the deadline and then refused, after which the client clears the staged slot; a server without RFC 0011 answers without `server_time_ms`. In each case the client ends with a credential the stub accepts. A store-less `Auth` that is refused presents the reloaded current and then staged credential.
- `core/driver`: the age comes from the file's modification time; a crash between the two writes of a promotion ends with one file; the reader retries when a promotion or a staging happens between its steps, driven by a hook between steps 1 and 2; folder tasks and `link` share one `Auth`, so one rotation leaves every task working.
- Against `wrangler dev`, with the rotation period shortened for the test: a daemon rotates while a CLI command holds the old credential, and the command succeeds after reading the files again; two profiles with copies of one credential both run, and only one keeps working after the first stored rotation.
