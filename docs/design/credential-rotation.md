# Refresh credential rotation

Status: design for spec/rfc/0011-refresh-credential-rotation.md. Nothing here is built yet. Where the client stores its credential (a file as today, or the Keychain on macOS) is decided separately; this note works with either.

RFC 0011 defines the protocol. This note says how the server, `core/net`, `core/driver`, `core/ffi` and the CLI implement it, and how to test it.

## Server

**Schema.** One step appended to `server/src/account/schema.ts` adds two columns to `devices`:

- `refresh_issued_at_ms INTEGER`: when the current credential was issued. `NULL` for rows from before the step, which counts as due (RFC 0011 §1). Claim, pairing and recovery set it.
- `next_refresh_hash BLOB`: the SHA-256 of the successor, or `NULL`.

**`refreshAccess`.** It takes `rotate` from the request and compares the presented hash with both columns:

1. It matches `next_refresh_hash`: set `refresh_hash` to it, `refresh_issued_at_ms` to now and `next_refresh_hash` to `NULL`, then answer with a token and no successor.
2. It matches `refresh_hash`, `rotate` is set and the credential is due: generate a successor, store its hash in `next_refresh_hash` (replacing any), and answer with a token and the successor.
3. It matches `refresh_hash` otherwise: answer with a token, as today.
4. Anything else: `UNAUTHENTICATED`, as today.

Cases 1 and 2 write in the same statement as the existing `last_seen_ms` update, so a rotation adds no row write beyond the one every refresh already makes. `revokeUserDevice` also sets `next_refresh_hash` to `NULL`. The rotation period is a constant next to `ACCESS_TOKEN_MS`. The Worker passes `TokenRequest.rotate` through and puts the successor in `AccessToken.next_refresh_credential`.

**Proto.** `TokenRequest.rotate = 3` and `AccessToken.next_refresh_credential = 3`, both additive, so `buf breaking` against main passes.

## Clients

**One credential per process.** Today each folder task builds its own `Auth` (core/driver/src/driver.rs, `spawn`), as do linking, the sign-out revocation and the FFI's server calls. Each would ask for its own successor, and each new successor replaces the last (RFC 0011 §1). So the driver builds one `Arc<Auth>` when it loads or receives the credential, and every caller in the process uses it: folder tasks, `link`, `revoke`, and the FFI's `signed_in` while a driver is open.

**`core/net`.** `Auth` keeps its `Credentials` behind the same lock as its cached token, and takes an optional store:

```rust
pub trait CredentialStore: Send + Sync {
    /// Keeps `next` next to the current credential; returns once it is written.
    fn stage(&self, next: &str) -> io::Result<()>;
    /// Makes the staged credential the only one.
    fn confirm(&self, next: &str) -> io::Result<()>;
    /// Forgets the staged credential.
    fn discard(&self) -> io::Result<()>;
}
```

With a store, `access_token` sets `rotate` and follows RFC 0011 §3. After `stage` succeeds, it presents the successor before it returns, so the successor is confirmed within one call. If `confirm` fails after the server accepted the successor, the record still holds the successor as staged, and the next start presents it first. Without a store, `Auth` never sets `rotate`. `Auth::credentials` returns a copy, because the credential can now change.

**`core/driver`.** `Credential` gets `next_refresh_credential: Option<String>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`, so files written before this change still load. `Credential` has `deny_unknown_fields`, so an older build can't load a file that holds a staged credential. That window lasts from `stage` to `confirm`, one request, unless the confirmation failed. The file store's `stage`, `confirm` and `discard` write a temporary file with mode 0600, sync it, rename it over `credential.json` and sync the directory. `save_new` stays as it is for pairing. Only a `Driver`, which holds the profile lock, passes a store to `Auth`. Sign-out deletes the whole record, the staged credential included.

**CLI.** The commands that call the server directly (`device list`, `device revoke`, `usage`) build an `Auth` without a store. On `UNAUTHENTICATED` from `POST /v1/token`, they load the credential again once and retry if its refresh credential differs (RFC 0011 §3). The command that opens its own driver goes through the driver's `Auth`.

**macOS app.** The app opens its profile through the FFI driver, so it rotates like the daemon. Whether the record lives in `credential.json` or in a Keychain item, it holds the current and the staged credential together and is replaced in one write.

## Tests

- Server (`server/test/`): no successor before the period or without `rotate`; a successor after it; a second request replaces the successor; presenting the successor makes it current and refuses the old credential; an old credential after a confirmation gets `UNAUTHENTICATED` and does not revoke the device; revocation forgets the successor; a row from before the schema step is due.
- `core/net`, against a stub server: the successor answer is lost, and the next refresh still works with the current credential; `stage` fails, and the client keeps the current credential; the successor is refused, and the client discards it and goes on with the current one.
- `core/driver`: a record with a staged credential presents it first after a restart; folder tasks and `link` share one `Auth`, so one rotation leaves every task working.
- Against `wrangler dev`, with the rotation period shortened for the test: a daemon rotates while a CLI command holds the old credential, and the command succeeds after reading the record again; two profiles with copies of one record both refresh, and only one keeps working.
