# 8. Server guarantees the engine relies on

The engine's safety depends on these server properties (spec §8, §12):

- A `commit_id` is applied at most once, and a retry returns the stored result.
- Seqs increase by one per commit within a history.
- Every chunk of every retained version is in R2.
- No chunk row is deleted while an upload target for it is still valid (spec §12.5): the hold TTL is at least the target's lifetime plus the upload time. This holds even when a `PUT` completes arbitrarily late. The server either bounds how late a `PUT` can complete and enforces that bound, or tolerates any lateness structurally. A finite hold TTL alone does not guarantee it.
- Under fairness, an unreferenced, unheld chunk is eventually deleted, and a valid commit is eventually applied or definitively rejected. Each liveness property states its fairness assumption, and eventual GC is kept separate from the safety properties above.
