# RFC 0008: The user's name in device credentials

Status: proposed. Applies to spec v0 (format_version 1).

A device knows its user's name only if it claimed the server, because it sent that name in the claim. A device added by pairing or by an owner recovery gets no answer that carries the name. This RFC adds the name to `DeviceCredentials`. A bare section number such as section 1 refers to this RFC; sections of spec v0 are written as spec v0 §N.

The schema change adds one field:

| Message | Field | Section |
|---|---|---|
| `DeviceCredentials` | `user_name = 6` | 1 |

## 1. user_name (spec v0 §15)

`POST /v1/claim` and `POST /v1/pair/complete` set `DeviceCredentials.user_name` to the name the server stores for the user that the new device belongs to:

- After the first claim, it is the claim's `user_name`, or `Owner` if that field was empty.
- After an owner recovery or a pairing, it is the name the first claim stored. The `user_name` of a recovery request is not used.

0.1.0 has no route that changes the name, so every device of a user gets the same one. The name is at most 255 bytes (RFC 0007 section 1), and `DeviceCredentials` stays within the 4 KiB of RFC 0007 section 3.

A server without this field sends no `user_name`, so a client treats an empty one as unknown.
