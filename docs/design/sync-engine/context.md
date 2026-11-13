# 1. The engine is a pure state machine

The engine decides sync for one space on one device through one entry point, `handle(now_ms, input)`, which updates its state and queues outputs. The driver writes the state changes of each call in one store transaction and only then runs the queued outputs, so every output is backed by durable state. The engine does no I/O, reads no clock besides `now_ms` and draws no randomness of its own.

| Input | From | Meaning |
|---|---|---|
| `Observed(changes, root_check)` | fs driver | What the disk holds now at some paths, from `core/fs` rescans: created, modified (with chunked `Content`), renamed, removed. `root_check` is the result of the §6.2 checks for this batch; removals in a batch without a passing check are not trusted (§6.1) |
| `Rescanned(entries)` | fs driver | A full or subtree listing after startup, an event overflow or a crash |
| `EffectDone(id, identity)`, `EffectFailed(id, observed)` | fs driver | The outcome of a disk effect. For an effect that creates or writes an entry, `EffectDone` carries the new entry's file identity, so L tracks the entry the effect made. A failure carries fresh observations. After a restart the driver recovers the effect record first (§5.3) and reports its outcome the same way |
| `Connected(Welcome)`, `Disconnected` | net | Session state |
| `Changes(records)`, `CaughtUp(head_seq)`, `ResyncRequired(reason)`, `TreePage(page)` | net | The server log and bootstrap |
| `CommitResult(result)`, `CommitFailed(commit_id, error)` | net | The outcome of the commit in flight |
| `UserOps(ops)`, `UserDecision(decision)` | app | Restores a person asked for (§6.3), and answers to questions |

The outputs are `Resume`, `ListTree`, `SendCommit(commit)`, `ApplyEffect(effect)`, `Problem(problem)` for something only a person can fix, and `AskUser(question)`.

The engine never sees file bytes. A file is described by its `Content` (size, mtime, executable bit, chunk list, manifest digest; spec §5.1), which the driver computes with `core/chunk`. Two contents are equal for sync when size, manifest digest and executable bit are equal. A change of mtime alone is not a change.
