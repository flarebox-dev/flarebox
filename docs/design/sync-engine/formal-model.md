## 8. Is a TLA+ model worth it?

Yes, for the commit and GC protocol, and not for the planner.

The planner's state is large but local: the simulation and the planner property tests explore it well, and a model of it would duplicate the code. The commit and GC protocol is the opposite: small, timing-dependent, spread over clients, the Space DO, R2 and alarms, and invisible to the client simulation. Its failure modes are rare interleavings that lose data, such as a chunk deleted while a commit that references it is being validated, or an upload whose result was unknown landing after its row was deleted (spec §12.5). This is the kind of design TLA+ is used for at AWS, where model checking found subtle bugs in designs that had passed review ([Newcombe et al. 2015](references.md#ref-aws)).

A model of about 200–300 lines, checked with TLC for 2 clients, 2–3 chunks, 3 commits and discrete time, would cover `PrepareUpload` holds, pending and live chunks, refcounts, retention, the two-phase delete and the alarm, with these invariants:

- **No dangling reference**: every chunk of every retained version is in R2.
- **Upload safety**: no chunk row is deleted while an upload target for it is still valid (`hold TTL ≥ target lifetime + upload time`, spec §12.5), even when PUTs complete at arbitrary later times.
- **Commit idempotency**: a `commit_id` is applied at most once; a retry returns the stored result.
- **Gapless log**: seqs increase by one per commit within a history, and no client's `last_seq` exceeds `head_seq`.
- **Liveness** (under fairness): an unreferenced, unheld chunk is eventually deleted; a valid commit is eventually applied or definitively rejected.

**[changed]** The model should also cover the uncertain commit result and its store transaction on the client, and the history fence of §10, since those are where B3's failure lives; it is worth writing before the protocol is relied on, not only after GC lands. The upload part must either bound how late a `PUT` can complete, and enforce that bound, or tolerate arbitrarily late completion structurally; a finite hold TTL alone cannot guarantee safety against unbounded lateness. Each liveness property needs its fairness assumption stated, and eventual GC is kept separate from the safety invariants. The size above is an estimate, not a completeness criterion.

It belongs next to the spec (`spec/tla/`) and to the server track, which owns GC. Stateright, a model checker embedded in Rust ([Stateright](references.md#ref-stateright)), could reuse the reference server, but the reference server is not the TypeScript implementation, and a language-neutral model fits the spec better.
