# RFC 0007: Name limits and space list pages

Status: proposed. Applies to spec v0 (format_version 1).

Spec v0 §15 puts no bound on the names that requests carry, and `GET /v1/spaces` returns every space in one answer. So a client cannot bound the size of an answer it reads. This RFC bounds both. A bare section number such as section 2 refers to this RFC; sections of spec v0 are written as spec v0 §N.

The schema change adds one field:

| Message | Field | Section |
|---|---|---|
| `SpaceList` | `next_cursor = 2` | 2 |

## 1. Name limits (spec v0 §15)

The server refuses a request with `INVALID_ARGUMENT` (HTTP 400) when one of these fields is longer than 255 bytes in UTF-8:

| Request | Fields |
|---|---|
| `ClaimRequest` (`POST /v1/claim`) | `user_name`, `device_name`, `platform` |
| `PairCompleteRequest` (`POST /v1/pair/complete`) | `device_name`, `platform` |
| `CreateSpaceRequest` (`POST /v1/spaces`) | `name` |

The server measures each field as the request carries it, before it trims the field or replaces an empty one. It checks the fields before anything else, so a refused request changes nothing. A claim code or a pairing code stays valid, and no failure is counted against the client.

Every space name, device name, user name and platform the server stores is therefore at most 255 bytes. That includes the defaults for empty fields, `Flarebox` for a space and `Unnamed device` for a device. Node names keep their own rule from spec v0 §6.1.

Known limitation: a deployment that stored a longer name before this rule keeps it. The server does not shorten stored names, so the bounds of section 3 do not hold for its answers that carry such a name.

## 2. Space list pages (spec v0 §15)

`GET /v1/spaces?cursor=` returns the spaces of the caller's user in creation order, by `space_id`. A page holds at most 100. `next_cursor` is empty on the last page, and the client passes it back as `cursor` to get the next. A client that ignores `next_cursor` sees only the first 100 spaces.

## 3. Answer sizes

With sections 1 and 2, the answers of these endpoints have fixed upper bounds, as long as every stored name follows section 1:

| Answer | At most |
|---|---|
| `DeviceCredentials`, `AccessToken`, `SpaceInfo`, `PairStartResponse`, `ServerInfo`, `RewindResult` | 4 KiB |
| `SpaceList` page | 32 KiB |
| `DeviceList` page (RFC 0006 section 7) | 64 KiB |

The largest fixed-shape answer is `DeviceCredentials`. Its access token carries the device name JSON-escaped, where a control character takes six bytes, inside claims encoded as base64url, which takes four bytes for every three. A 255-byte device name therefore adds at most 2,040 bytes to the token. In a list, a `SpaceInfo` takes at most 316 bytes and a `DeviceInfo` at most 643, because both carry the names of section 1.

A client that caps its reads at these bounds cannot read an answer that carries an older, longer name. It reports such an answer as too large and does not misread it. Until deployments from before this rule are gone, a client that must still list their spaces keeps a larger cap.
