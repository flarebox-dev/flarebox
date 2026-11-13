# RFC 0021: How a person signs in

Status: proposed. Applies to spec v0 (format_version 1). Amends RFC 0006 §2 and §6, RFC 0009 §1 and RFC 0020 §3.1. Registers the features `human_auth`, `device_authorization` and `hosted_auth` (RFC 0022). Builds on RFC 0020 for users, roles and the fence on removed users.

A person has no credential of their own today. A person is the set of their devices, and a new device joins only with a pairing code from a device the person already has (spec v0 §15.2), or as the owner's device through the claim or the recovery key (`docs/design/deploy-and-upgrade.md` §2). That works for a second laptop. It does not work for a browser, for a phone that should sign in to a deployment on any domain, or for an invited person whose first device is a browser.

This RFC adds three things. A person can hold passkeys and sign in with them in a browser, which creates a browser principal with a session. A new client can ask to be approved from anywhere the person is already signed in, following the OAuth 2.0 Device Authorization Grant. A deployment that Koidrive hosts can let an external identity provider confirm who the person is. In every case the deployment's Account DO issues the credential: a device, browser or agent credential is never issued by another service.

A bare section number such as §3 refers to this RFC.

## 1. Model

| Record | Where | Fields that matter |
|---|---|---|
| Browser principal | Account DO `devices`, `kind = 'browser'` | One signed-in browser of one user. Full scope. `expires_at_ms` is the session's absolute end. New `stepped_up_at_ms` (§4) |
| Passkey | Account DO `passkeys` (new) | `credential_id` (primary key), `user_id`, `rp_id`, `public_key`, `sign_count`, `transports`, `name`, `created_at_ms`, `created_by` (a principal id), `last_used_ms`, `removed_at_ms` |
| Challenge | Account DO `auth_challenges` (new) | `challenge_hash`, `purpose` (`register`, `sign_in` or `step_up`), `user_id` and `device_id` when known, `expires_at_ms` |
| Enrollment | Account DO `passkey_enrollments` (new) | A one-time link that lets a browser register a passkey for a user (§3.2) |
| Device authorization | Account DO `device_authorizations` (new) | `device_code_hash`, `user_code_hash`, client name and platform, state, approving principal, the new principal's id, `expires_at_ms`, `last_poll_ms` (§5) |
| External identity | Account DO `users` | New `idp_issuer`, `idp_subject` (unique together) and `idp_org_id` (§7) |

A browser principal is a principal like any other (RFC 0006 §2). It belongs to one user, appears in that user's device list with its kind, is revoked like a device (RFC 0009), and is revoked with every other principal when its user is removed (RFC 0020 §3.5). Its authority is its user's role. The fence of RFC 0020 §3.5 applies to every transaction in this RFC that stores a passkey, a principal or an approval.

## 2. Features

| Feature | Turns on |
|---|---|
| `human_auth` | Passkeys, browser principals and sessions (§3, §4) and the sign-in pages |
| `device_authorization` | The routes of §5 |
| `hosted_auth` | Sign-in through an external identity provider (§7). It replaces passkey sign-in on that deployment |

While a feature is off its routes answer 404 (RFC 0022 §3). Pairing codes, the claim and the owner's recovery key work whatever the features say.

## 3. Passkeys

### 3.1 The relying party

The relying party ID is the host name of the deployment's origin, such as `koidrive.alice.workers.dev` or a custom domain. The Account DO stores it in `settings` the first time a passkey is registered, and from then on it accepts WebAuthn ceremonies only from `https://<rp_id>`. Every passkey row records the RP ID it was made for. A request that reaches the Worker under another host name is redirected to the stored one before any ceremony starts.

Changing the deployment's host name changes the origin, and passkeys made for the old RP ID stop matching. An owner who moves the deployment sets the new RP ID with `PUT /v1/auth/relying-party` from a full-scope device. Every passkey of the old RP ID then stops working for sign-in, and each person registers a new passkey through an active device (§3.2) or a recovery code. Related Origin Requests could let the new origin keep using the old RP ID (passkeys.dev/docs/advanced/related-origins, read 2026-11-13). Browser support for it is not checked, so this RFC does not rely on it.

### 3.2 Registering a passkey

A browser registers a passkey only for a user it can prove it acts for, in one of these ways:

1. **A signed-in browser** of the user adds another passkey, after a step-up (§4).
2. **An enrollment link from another principal.** A full-scope device of the user calls `POST /v1/auth/enrollments` and gets a link `https://<rp_id>/auth/enroll#<token>`. The token is 128 random bits, stored as a hash, single-use and valid for 10 minutes. Opening the link starts a registration for that user.
3. **A user invite redeemed in a browser** (RFC 0020 §3.2). The browser registers the passkey in the redemption, and the user's first principal is the browser principal.
4. **A recovery code redeemed in a browser** (RFC 0020 §3.1).

The ceremony asks for a discoverable credential (`residentKey: required`) and user verification (`userVerification: required`). Attestation is not requested. The server verifies the response as WebAuthn Level 3 §7.1 requires: the challenge it issued for this purpose and user, the origin, the RP ID hash, the user-verified flag and the public key. The challenge is 256 random bits, single-use and valid for 5 minutes. A successful registration stores the passkey and, for ways 2 to 4, creates a browser principal and its session (§4) in the same transaction.

A user lists and removes their passkeys from any full-scope principal. Removing a passkey from a browser needs a step-up. Removing the passkey that a browser signed in with does not end that browser's session. Revoking the browser principal does.

### 3.3 Signing in

`POST /v1/auth/sign-in/options` returns a challenge with no allowed credentials, so the authenticator offers the user's discoverable passkeys. `POST /v1/auth/sign-in` takes the assertion. The server finds the passkey by credential ID, refuses it if it or its user is removed or its RP ID differs from the stored one, and verifies the assertion as WebAuthn Level 3 §7.2 requires, including user verification. A signature counter that is not zero and not above the stored one is refused, because it may come from a cloned authenticator. A counter of zero, which synced passkeys report, is accepted. A successful sign-in creates a browser principal and its session in one transaction with the fence.

Wrong attempts go through the per-client limiter that pairing uses. Unknown credential IDs and failed signatures get the same answer.

## 4. Browser sessions

A session is a browser principal with a cookie. The cookie value is the principal's refresh credential: 256 random bits that the server stores only as a SHA-256, as for every refresh credential (spec v0 §15.2).

- **Cookie.** Named `__Host-koi_session`, with `Secure`, `HttpOnly`, `SameSite=Lax` and `Path=/`, and no `Domain`, so it is sent only to the deployment's own host. `Lax` allows the top-level redirect back from an external identity provider (§7).
- **Lifetime.** A session ends 24 hours after sign-in (`expires_at_ms`) or after 1 hour without a request, whichever comes first. The server then refuses the principal as it refuses an expired one (RFC 0006 §7). Signing out revokes the principal.
- **Requests.** The Worker accepts the cookie in place of an access token on the routes the web client uses, WebSocket upgrades included. A request that carries the cookie and changes state must carry an `Origin` header equal to the deployment's origin and an `X-Koidrive-CSRF` header equal to the CSRF token that sign-in returned for this session. The server answers no CORS preflight, so another origin cannot send these headers. A request that fails either check is refused with `FORBIDDEN` and changes nothing.
- **Step-up.** A browser principal uses these capabilities only within 5 minutes after a passkey assertion for its own user (`purpose = step_up`), recorded in `stepped_up_at_ms`: `user.invite`, `user.manage`, `user.admin`, `space.recover`, `device.manage.any`, `member.manage`, approving a device authorization (§5.3), creating an enrollment, and adding or removing a passkey. Outside that window they answer `FORBIDDEN` with the reason "step-up required", and the web client asks for the passkey and tries again. A device principal needs no step-up, because it proves possession of the device's stored credential on every refresh.

## 5. Device authorization

A client that has no credential yet (the CLI, the macOS app, the iOS app) asks to be approved by the person, following RFC 8628 (rfc-editor.org/rfc/rfc8628). The request and reply bodies are protobuf, like every other route of this API (spec v0 §15.1). The flow, the codes and the error meanings are RFC 8628's. Pairing codes stay as they are.

### 5.1 Starting

`POST /v1/device-authorization` with `DeviceAuthorizationRequest` (the client's name and platform), unauthenticated. The reply, `DeviceAuthorization`, carries:

- `device_code`: 256 random bits, kept by the client, stored as a hash.
- `user_code`: 8 characters from the 20 consonants `BCDFGHJKLMNPQRSTVWXZ`, shown as `XXXX-XXXX`, about 34.6 bits, as RFC 8628 §6.1 suggests. Matching ignores case, spaces and hyphens. The server draws again if the code is in use.
- `verification_uri` (`https://<rp_id>/approve`) and `verification_uri_complete` (the same with the code).
- `expires_in_ms` of 10 minutes and `interval_ms` of 5 seconds.

The per-client limiter caps how many requests one address may start.

### 5.2 Polling

`POST /v1/device-authorization/token` with the `device_code`. The reply, `DeviceAuthorizationPoll`, has a `state`:

| State | RFC 8628 §3.5 | Meaning |
|---|---|---|
| `PENDING` | `authorization_pending` | Not decided yet |
| `SLOW_DOWN` | `slow_down` | The client polled sooner than `interval_ms` after its last poll. The interval grows by 5 seconds for this request |
| `DENIED` | `access_denied` | The person refused |
| `EXPIRED` | `expired_token` | The request expired, or its credentials were already collected and replay ended |
| `APPROVED` | (token response) | `credentials` holds the new principal's `DeviceCredentials` |

The first refresh credential is an HMAC, under the deployment's server key, of the principal ID and the device code, as RFC 0020 §3.2 does for invites. So a poll whose answer was lost gets the same credentials again, until the principal first refreshes, is revoked, or 10 minutes pass after approval.

### 5.3 Approving

A signed-in principal of the person opens the verification page or runs `koi device approve <code>`.

1. `GET /v1/device-authorization?user_code=` returns the client's name, platform and when it started, so the person can check that it is the device in front of them. A wrong code goes through the per-principal limiter.
2. `POST /v1/device-authorization/decision` with the user code and `approve` or `deny`. Approving needs `device.pair` and, from a browser, a step-up (§4).

Approval creates the principal in one Account DO transaction with the fence: a device of the approver's user, with the approver's scope and an expiry no later than the approver's (RFC 0020 §2.2), and `issued_by` set to the approver. A request decides once. The second decision is refused.

A client on a phone can open `verification_uri_complete` in the system browser view (`ASWebAuthenticationSession` on iOS), where the person signs in with a passkey and approves on the same phone. WebAuthn works there as in Safari (passkeys.dev/docs/reference/ios, read 2026-11-13). The app's own passkey API is not used, because it needs an associated domain for each relying party (developer.apple.com, "Supporting passkeys", read 2026-11-13), and every self-hosted deployment has its own domain.

## 6. Recovery

A person who lost a way to sign in recovers in this order:

1. Another passkey of theirs, in a browser.
2. An active principal of theirs: it approves a new device (§5.3) or creates an enrollment link for a new passkey (§3.2).
3. A recovery code from an admin (RFC 0020 §3.1), redeemed in a browser or by a client.
4. The deployment's owner only: the recovery key set as a Worker secret (`docs/design/deploy-and-upgrade.md` §2), unchanged.

There is no personal recovery code. A member who is not the owner never gets owner recovery. RFC 0009 §1's last-device warning counts browser principals and passkeys: revoking the last principal of a user who still has a passkey does not warn when `human_auth` is on.

## 7. An external identity provider (hosted deployments)

A deployment that Koidrive hosts for a customer can let WorkOS AuthKit confirm who a person is. Self-hosted deployments do not use it. The deployment keeps issuing every credential itself, so the clients, the routes of §5 and every check stay the same. Only the step where the server learns which user is in front of it changes.

- **Configuration.** Worker variables name the provider (`workos`), its client ID and the customer's organization ID. The API key is a Worker secret. With `hosted_auth` on, passkey sign-in and registration (§3) are off on that deployment, because AuthKit offers passkeys itself (workos.com/docs/authkit/passkeys, read 2026-11-13).
- **Sign-in.** The browser goes to AuthKit and comes back to the deployment with an authorization code, which the deployment exchanges with PKCE. The deployment verifies the access token, a JWT, against the provider's JWKS (workos.com/docs/authkit/sessions, read 2026-11-13): signature, issuer, the client as audience, expiry, and an organization claim equal to the configured one. It then finds the user by `(idp_issuer, idp_subject)` and creates a browser principal as §3.3 does. The provider's tokens are not kept after this step, and no other route accepts them.
- **Linking.** A user gets an external identity only by redeeming a user invite (RFC 0020 §3.2) while signed in at the provider. The first sign-in binds the provider's subject to the new user. A subject with no user is refused. A removed user's subject stays bound, so it cannot be used again.
- **Step-up.** A step-up (§4) asks the provider for a fresh sign-in and accepts a token issued less than 5 minutes ago.
- **Removal.** Removing a user in Koidrive revokes their principals (RFC 0020 §3.5) whatever the provider says. Removing a person only at the provider stops new sign-ins, and their existing principals keep working until an admin removes the user.

AuthKit also offers a device flow (workos.com/docs/authkit/cli-auth, read 2026-11-13). Clients do not use it. They use §5 against the deployment, so a client works the same against a self-hosted and a hosted deployment.

## 8. Wire changes

Protobuf, all additive.

| Message | Fields |
|---|---|
| `DeviceAuthorizationRequest` | `client_name = 1`, `platform = 2` |
| `DeviceAuthorization` | `device_code = 1`, `user_code = 2`, `verification_uri = 3`, `verification_uri_complete = 4`, `expires_in_ms = 5`, `interval_ms = 6` |
| `DeviceAuthorizationPollRequest` | `device_code = 1` |
| `DeviceAuthorizationPoll` | `state = 1`, `credentials = 2` (`DeviceCredentials`), `interval_ms = 3` |
| `PendingDeviceAuthorization` | `client_name = 1`, `platform = 2`, `created_at_ms = 3` |
| `DeviceAuthorizationDecision` | `user_code = 1`, `approve = 2` |
| `PasskeyEnrollment` | `url = 1`, `expires_at_ms = 2` |
| `PasskeyInfo` | `credential_id = 1`, `name = 2`, `created_at_ms = 3`, `last_used_ms = 4` |
| `PasskeyList` | `passkeys = 1` |
| `DeviceInfo.Kind` (existing enum) | new value `KIND_BROWSER = 3` |

The WebAuthn ceremony routes under `/v1/auth/` carry the JSON forms that WebAuthn Level 3 defines (`PublicKeyCredentialCreationOptionsJSON`, `RegistrationResponseJSON` and their assertion counterparts), because only a browser calls them.

| Method and path | Who | Feature |
|---|---|---|
| `POST /v1/auth/enrollments` | Full-scope principal of the user, browsers after a step-up | `human_auth` |
| `POST /v1/auth/register/options`, `POST /v1/auth/register` | A browser with an enrollment, invite or recovery code, or a signed-in browser after a step-up | `human_auth` |
| `POST /v1/auth/sign-in/options`, `POST /v1/auth/sign-in` | Anyone | `human_auth` |
| `POST /v1/auth/step-up/options`, `POST /v1/auth/step-up` | A browser principal | `human_auth` |
| `POST /v1/auth/sign-out` | A browser principal | `human_auth` |
| `GET /v1/auth/passkeys`, `DELETE /v1/auth/passkeys/{credential_id}` | Full-scope principal of the user | `human_auth` |
| `POST /v1/device-authorization`, `POST /v1/device-authorization/token` | Anyone | `device_authorization` |
| `GET /v1/device-authorization`, `POST /v1/device-authorization/decision` | Principal with `device.pair` | `device_authorization` |
| `PUT /v1/auth/relying-party` | The deployment's owner, from a full-scope device | `human_auth` |
| `GET /v1/auth/hosted/start`, `GET /v1/auth/hosted/callback` | Anyone | `hosted_auth` |

Account DO, one forward-only migration: `devices` gains `stepped_up_at_ms INTEGER`, `users` gains `idp_issuer TEXT`, `idp_subject TEXT` and `idp_org_id TEXT` with a unique index on the pair, and new tables `passkeys`, `auth_challenges`, `passkey_enrollments` and `device_authorizations`. A server that finds `kind = 'browser'` and does not know it denies the principal everything (RFC 0006 §2), so a rollback across this migration fails closed.

RFC 0006 §2 gains the kind `browser` and RFC 0006 §6 gains device authorization as a way one principal creates another. RFC 0009 §1's last-device rule changes as §6 says. RFC 0020 §3.1's recovery codes can be redeemed in a browser.

## 9. Security

| Threat | Answer |
|---|---|
| Someone tricks a person into approving a code shown on the attacker's device | The approval page shows the client's name, platform and start time, and says to approve only a code on a device in front of the person. A browser approval needs a step-up. The code expires in 10 minutes and decides once |
| Guessing a user code | Approving needs a signed-in principal, and wrong codes go through a per-principal limiter. About 34.6 bits per code |
| A stolen device code | It yields credentials only after the person approves, and only to the first collector within the replay window |
| Cross-site requests with the cookie | `Origin` must match and the CSRF header must carry the session's token. No CORS answers. `SameSite=Lax` keeps the cookie off cross-site subrequests |
| Script injection in the web client | The cookie is `HttpOnly`. Dangerous capabilities need a step-up within 5 minutes. Sessions end after 1 hour idle or 24 hours. The pages send a strict Content-Security-Policy |
| A cloned authenticator | A non-zero counter that does not grow is refused |
| A removed user signing in | Every storing transaction runs the fence of RFC 0020 §3.5, and sign-in, approval and registration store a principal |
| A compromised identity provider (hosted) | It can sign in as any linked person of that organization, as any identity provider can. It cannot mint device, browser or agent credentials, and it is not trusted for an organization other than the configured one |

## 10. Tests

- **Passkeys.** Registration and sign-in against a software authenticator in workerd: a wrong origin, a wrong RP ID, a reused challenge, an expired challenge, a missing user-verified flag, a counter that does not grow, and a removed passkey or user are each refused. A sign-in that races the user's removal creates no principal.
- **Sessions.** A cookie request without `Origin` or without the CSRF header is refused and changes nothing. A session ends at 1 hour idle and at 24 hours. A browser without a step-up is refused every capability of §4, and gets it within 5 minutes after one. Signing out revokes the principal, and its WebSockets close with 4003.
- **Device authorization.** Polling faster than the interval gets `SLOW_DOWN`. A code decides once. A lost poll answer gets the same credentials. An approver whose user is removed between viewing and approving creates nothing. A scoped approver creates a principal no wider than itself. A request expires after 10 minutes.
- **Recovery.** Each way in §6 ends with a working principal for the same user, and the device list shows who issued it.
- **Hosted.** A token for another organization, another client, an unknown subject, or an expired token is refused. An invite redeemed through the provider binds the subject once. A provider token is refused on every file route.
- **Features.** With each feature off its routes answer 404 and pairing still works.

## 11. Alternatives considered

1. **Cloudflare's workers-oauth-provider as the authorization server.** It does not offer the device grant. A pull request that added it was closed without merging (github.com/cloudflare/workers-oauth-provider/pull/194, closed 2026-07-29, read 2026-11-13). Its token store would also be a second place that issues credentials beside the Account DO. The routes of §5 are three endpoints on tables like the pairing codes'.
2. **WorkOS for self-hosted deployments.** Every owner would need an account at a third party before anyone can sign in, which a self-hosted product should not require. The same interface can add it later as an option.
3. **Native passkeys in the iOS app.** They need an associated domain per relying party, which a self-hosted deployment on its own domain cannot provide (§5.3).
4. **Passwords.** A deployment would store password hashes and need resets, which passkeys and recovery codes make unnecessary.
5. **Authorization code with PKCE and a redirect into native apps** (RFC 8252). The device grant covers the CLI, which has no browser to redirect into, and a phone can open the same page locally (§5.3). One flow is less to build and to test. WorkOS states the same for its device flow: it "works for any public client that cannot reliably receive a browser redirect, including native mobile" (workos.com/docs/authkit/cli-auth, read 2026-11-13).
6. **SimpleWebAuthn or our own WebAuthn verification.** The server uses `@simplewebauthn/server`, MIT, which lists Cloudflare Workers among its runtimes (jsr.io/@simplewebauthn/server 14.1.0, read 2026-11-13), rather than writing CBOR and COSE parsing itself. This is an implementation choice and does not change the protocol.

## 12. Decisions

The project owner decided on 2026-11-13: self-hosted deployments sign in with passkeys and device authorization and need no external service, and WorkOS AuthKit is used only for deployments Koidrive hosts.

Open questions:

1. Creating users from the identity provider's directory on first sign-in, without an invite, for hosted customers who ask for it.
2. Following removals at the provider automatically.
3. Keeping passkeys across a host name change once Related Origin Requests is checked in the browsers people use.
