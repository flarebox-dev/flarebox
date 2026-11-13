# 10. Protocol rules this design depends on

The protocol changes this design needs are in these RFCs:

- [RFC 0001](../../../spec/rfc/0001-v0-clarifications.md): the `name_key` formula and Unicode 18.0.0 pin, the reserved `.koidrive` name at the root, the reject reason `NOT_FILE`, `CreateDir` onto a live file, long conflict names, and a no-op `Move` that changes nothing.
- [RFC 0002](../../../spec/rfc/0002-commit-outcomes-and-recovery.md): the history fence (`HISTORY_MISMATCH`), the device counter (`device_seq`, `STALE_DEVICE_STATE`), the check order, the `require_head` precondition, ordered application and acknowledgement, recovery, reusing a `node_id`, and the restore point in `ResyncRequired`.
- [RFC 0012](../../../spec/rfc/0012-trash-groups-across-a-loss.md): trash groups named by `commit_id` and op index.
- [RFC 0013](../../../spec/rfc/0013-commit-tokens.md): commit tokens in `Resume` and `Commit`.
- [RFC 0015](../../../spec/rfc/0015-delete-preconditions.md): `Delete` preconditions.
- [RFC 0016](../../../spec/rfc/0016-identical-content.md): an edit that meets equal unseen content, and version IDs that are never reused.
- [RFC 0018](../../../spec/rfc/0018-resends-and-deletes.md): `resend` on `UpdateContent` and the edit revision (`NodeState.edit_rev`) that R7 reads, so a delete beats a resend it did not see and the author's equal send still records an edit.

`.koiignore` needs no protocol change: it is an ordinary file, and ignoring is per device.
