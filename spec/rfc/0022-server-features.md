# RFC 0022: Server features

Status: proposed. Applies to spec v0 (format_version 1). Amends spec v0 §15 (`GET /v1/server`) and defines `Welcome.features`. Adds a field to `ServerInfo`.

A client can't tell whether the server it talks to offers a capability that a newer RFC adds. The owner upgrades the server on their own schedule (`docs/design/deploy-and-upgrade.md` §3.4), so a new client often meets an old server, and the protocol version alone does not say which routes exist. Work that ships before it is finished also needs a switch: a release can carry routes that stay off until the work is complete.

This RFC lets the server announce the features it has turned on, under names that later RFCs register. A bare section number such as §2 refers to this RFC.

## 1. Messages

- `ServerInfo`: `features = 7`, a repeated string.
- `Welcome.features` (8) exists and is empty today. It now carries the same list as `ServerInfo.features` at the moment the server sends it.

No field is removed or renumbered.

## 2. Feature names

A feature name is 1 to 64 characters of `a` to `z`, `0` to `9` and `_`, starting with a letter. The RFC that adds a feature registers its name in the table below and states what turning it on enables. Names are never reused for a different meaning.

| Name | RFC | Enables |
|---|---|---|
| `people` | RFC 0020 | User and member routes, invites, user removal |
| `human_auth` | RFC 0021 | Passkeys and browser sessions |
| `device_authorization` | RFC 0021 | The device authorization routes |
| `hosted_auth` | RFC 0021 | Sign-in through an external identity provider |

The namespace is shared with the `Hello.features` that RFC 0006 §5 uses for change-record kinds. A record-kind feature appears in `ServerInfo.features` when the server can send records of that kind.

## 3. Server rules

1. The server lists every feature that is on, in any order, without duplicates.
2. A feature that is off is absent. Every route it enables answers 404, as an unknown path does today, and no message, record or reply of that feature is sent. Data that a feature wrote while it was on stays in storage.
3. Turning a feature off never weakens a check. Authorization that an RFC adds applies whether its feature is on or off, so a deployment that turns a feature off keeps refusing what the RFC refuses.
4. Each release fixes which features are on by default. A deployment can change that only through the Worker variable `KOIDRIVE_FEATURES`, a comma-separated list where `name` turns a feature on and `-name` turns it off. The variable exists for tests and test deployments. It is not part of the owner guide, and a feature name the server does not know is ignored.
5. The Account DO and the Worker can run different releases for up to 300 seconds after a deploy (`server/wrangler.jsonc`, `code_update_strategy`). A feature whose routes call new Account DO or Space DO methods is listed only when those objects answer that they run code that has them, as `ServerInfo.setup_attempts` does today.

## 4. Client rules

1. A client offers a feature to the user only when the server lists it and the client implements it.
2. A client that needs a feature the server does not list tells the user that the server must be updated, with the server's `server_version`. The client does not try the routes to find out.
3. A client reads `ServerInfo.features` when it sets up or adds a device, and again whenever `Welcome.server_version` or `Welcome.features` differs from what it stored.
4. A client ignores names it does not know.

A 0.1 server sends no `features` field, which a client reads as an empty list.

## 5. Tests

- The server lists exactly the features that are on, after the default and the variable are applied, and the same list in `Welcome`.
- A route of a feature that is off answers 404 for every principal, and the authorization checks of that feature's RFC still hold.
- A server whose Account DO lacks the methods a feature needs leaves the feature out until the Account DO runs the new code.
- A client that meets a server without a feature it needs reports that the server must be updated and calls none of the feature's routes.

## 6. Alternatives considered

1. **Infer features from `server_version`.** A client would need a table of releases and features, and a test build would report a release number that does not match what it enables.
2. **Probe the routes.** A 404 does not distinguish a missing feature from a wrong path, and a probe of a route that changes state is unsafe.
3. **A build-time switch only.** A deployment for tests could not turn a feature on without a separate build, so the deploy test would not exercise the release that ships.
