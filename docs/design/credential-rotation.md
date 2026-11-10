# Refresh credential rotation

Status: design for spec/rfc/0011-refresh-credential-rotation.md. Nothing here is built yet.

RFC 0011 defines the protocol: the device generates its next credential, stores it, and sends it with a token request, and the server stores it in place of the old one. This note says how the server, `core/net`, `core/driver`, `core/ffi` and the CLI implement it, and how to test it.

## Server

**No schema change.** `devices.refresh_hash` already holds the one credential the server accepts. A rotation overwrites it.

**`refreshAccess`.** It computes the SHA-256 of the presented credential and, if the request carries one, of `next_refresh_credential` before it reads the row, because hashing is asynchronous. Then, with no `await` between the read and the write:

1. It loads the active principal and applies the existing guards: unknown, revoked or expired is `UNAUTHENTICATED`.
2. It compares the presented hash with `refresh_hash`. A mismatch is `UNAUTHENTICATED`.
3. With a proposal, it updates `refresh_hash` together with the existing `last_seen_ms` update, in one statement, and sets `rotated` in the answer.

A proposal that does not match `^[A-Za-z0-9_-]{43}$` is `INVALID_ARGUMENT` before anything is read. A rotation adds no row write, since every refresh already writes `last_seen_ms`.

**Proto.** `TokenRequest.next_refresh_credential = 3` and `AccessToken.rotated = 3`, both additive, so `buf breaking` against main passes.

## Clients

**One credential per process.** Today each folder task builds its own `Auth` (core/driver/src/driver.rs, `spawn`), as do linking, the sign-out revocation and the FFI's server calls. After a rotation, every one of them except the one that rotated would present a credential the server refuses. So the driver builds one `Arc<Auth>` when it loads or receives the credential, and every caller in the process uses it: folder tasks, `link`, the sign-out revocation, and the FFI's `signed_in` while a driver is open.

**`core/net`.** `Auth` keeps its `Credentials` behind the same lock as its cached token, so one refresh at a time reads or changes them, and `Auth::credentials` returns a copy. It takes an optional store with three operations, each of which returns once its write is durable: stage a new credential, make the staged credential current, and clear the staged slot. With a store, `access_token` follows RFC 0011 §2 steps 1 to 6, and it learns the credential's age and any staged credential when it is built. Without a store, it never proposes a credential, and on `UNAUTHENTICATED` from `POST /v1/token` it calls a reload function once and presents what it returns (RFC 0011 §2, processes that only read).

**`core/driver`.** The two slots are two files in the state directory:

- `credential.json` keeps its format, so a build from before rotation still reads it. Its modification time is when the credential last changed, which gives the credential's age without a new field. `Credential` has `deny_unknown_fields`, so a new field would make the file unreadable to an older build.
- `credential.next` holds the staged credential, only while a rotation is unfinished. An older build ignores it.

Both are written to a temporary file with mode 0600, excluded from Time Machine as the credential file is at pairing, synced, renamed into place, and followed by a sync of the directory. Making the staged credential current writes `credential.json` first and removes `credential.next` second. A crash in between leaves both files with the same credential, and the next refresh presents the staged one, which the server accepts, and finishes. `save_new` stays as it is for pairing. Only a `Driver`, which holds the profile lock, gives its `Auth` a store. Sign-out deletes both files.

An older build that starts while `credential.next` exists uses `credential.json`. If the server already stored the staged credential, that build is refused and signed out. This only happens on a downgrade in the middle of a rotation, which is one request long unless the answer was lost.

**CLI.** The commands that call the server directly (`device list`, `device revoke`, `usage`) build an `Auth` without a store, whose reload function reads both files again. The command that opens its own driver goes through the driver's `Auth`.

**macOS app.** The app opens its profile through the FFI driver, so it rotates like the daemon.

## Tests

- Server (`server/test/`): a request with a proposal stores it and sets `rotated`; the old credential is then refused, and the device is not revoked; a request without one changes nothing; a malformed proposal is `INVALID_ARGUMENT` and changes nothing; a revoked or expired principal's proposal is refused and changes nothing; two requests with different proposals from the same credential leave exactly one of them working.
- `core/net`, against a stub server, for each cut of RFC 0011 §2: the stage write fails; the request is lost; the answer is lost and the next refresh presents the staged credential; the staged credential is refused and the current one is presented; a server without RFC 0011 answers without `rotated`. In each case the client ends with a credential the stub accepts. A store-less `Auth` that is refused presents the reloaded staged credential first and then the current one.
- `core/driver`: the age comes from the file's modification time; a crash between the two writes of a promotion finishes on the next refresh; folder tasks and `link` share one `Auth`, so one rotation leaves every task working.
- Against `wrangler dev`, with the rotation period shortened for the test: a daemon rotates while a CLI command holds the old credential, and the command succeeds after reading the files again; two profiles with copies of one credential both run, and only one keeps working after the first rotation.
