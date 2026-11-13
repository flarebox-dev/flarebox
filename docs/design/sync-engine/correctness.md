## 3. Correctness properties

### 3.1 Vocabulary

Pierce and Vouillon describe a synchronizer run as a tuple `(o, a, b, a', b')`: the archive `o` (the last synchronized state), the two replicas `a` and `b` before the run, and the replicas after it. A path has changed in `a` if the root sort of `a@p` differs from `o@p` ([2004](references.md#ref-unison), §5). Balasubramaniam and Pierce split a synchronizer into update detection, which computes a predicate `dirty` that may over-report but must never miss an update (`¬dirty(p) ⟹ o(p) = a(p)`), and reconciliation ([1998](references.md#ref-bp), Definition 3.1.2).

The mapping to Koidrive is per node rather than per path, and one replica is the server:

| Unison / B&P | Koidrive |
|---|---|
| archive `o` | S |
| replica `a` | L (and the disk it observes) |
| replica `b` | R (the server state this device knows; the server merges all devices) |
| `dirty_a(p)` | L(n) ≠ S(n). Update detection is the fs rescans; it must be safe in the B&P sense, so a lossy watcher triggers a rescan rather than a guess (§5.1) |
| run | one planner step: an effect, a commit, or a tree update |

This is an adaptation: B&P and Unison prove their properties for path-keyed replicas, and nothing here inherits those proofs. The properties below are claims about this design, checked by the tests of §7.

### 3.2 Safety

The engine satisfies Unison's two change-propagation conditions at every step, not only at the end of a run, because a crash can end a run anywhere ([Pierce and Vouillon 2004](references.md#ref-unison), §10):

- **Preserve user changes**: if L(n) ≠ S(n), no effect changes n on disk. The only disk operation on a locally changed node is the one the user's own change implies after the server answers: renaming a fork to its conflict name (§2.7) or giving a recovered file a free name (§5.2), both of which keep its content.
- **Propagate only user changes**: every effect makes L(n) equal to R(n), and every op makes R(n) equal to L(n). Nothing is made up, with two deliberate exceptions: conflict names, which the server chooses (spec §6.1), and parked locations (§2.5), which are not visible in the synced folder and never uploaded.

Unison's third condition, **stop at conflicts**, is replaced on purpose. Unison asks a person; its authors note that systems with many replicas, such as the Windows 2000 file service, overwrite instead, because there may be nobody to ask ([Pierce and Vouillon 2004](references.md#ref-unison), footnote 3). Koidrive runs unattended on N devices, so it resolves every conflict deterministically and keeps both sides (spec §1 principle 4). The no-data-loss property below is what replaces "stop at conflicts".

### 3.3 No data loss

**[changed]** The property is stated per write, not per content digest, so that two files with identical bytes count separately (review B6).

A **write** is one user action that leaves a file with some bytes: a create or an overwrite on some device. Two writes are different writes even when their bytes are identical. Each write has a **home**: the node it was written to, or the node the server put it in instead (a conflict copy for `CONFLICT_COPY`, the surviving directory's child for `MERGED_INTO`). A replica (a device's disk or the server's live tree) **holds** a write if the live file at its home has the write's bytes.

A write w is **superseded** only by a user action on w itself, and how depends on whether w was ever published, that is, reached the server in an applied commit:

- an **unpublished** write is superseded when the user of the device that wrote it overwrites or deletes the file holding it before it is published. An offline create followed by a delete, or a temporary file that lives between two rescans, leaves nothing to publish, and the engine correctly sends nothing (review R3-3);
- a **published** write is superseded when, on some device whose disk showed w at its home, the user overwrote or deleted that file and the server accepted the change (an overwrite applied rather than turned into a conflict copy, a delete applied rather than superseded under R7).
- **[changed]** a rewind of the space by its owner (GUN-268) retires, on the server, every write published only after the restore point. The obligation for those bytes moves to the devices that still hold them: a copy held by recovery (§6.4) must be conserved until it is answered, published again if the answer is Publish or Keep, and found in the local trash if it is Discard. A retired write is not superseded: the property below exempts it until it is published again, when it becomes an ordinary published write, and until then it is checked against this obligation instead.

**[changed]** An action on another node never supersedes w, including the other side of the same conflict: overwriting n does not supersede a write that lives on in n's conflict copy (review D5), and one device's action never supersedes another device's unpublished write. Sync effects never supersede.

**Property (no data loss).** For writers within the contract of §5.2, once the system is quiescent (§3.4), every write that was neither superseded nor retired by a rewind is held by the server and by every device on which its home is eligible. On a device where its home is excluded (§5.10), the obligation is conservation, not materialization: if that device held the write when the home became excluded, it still holds it there.

This holds through all the conflict rules: concurrent edits keep both contents (R3), an edit concurrent with a delete keeps the edit (R7, R8), and a fork keeps the local write in a new node. A write that was superseded is still recoverable from the server for the retention period, as a previous version or in the trash (spec §12.2); that is a separate property of the server.

### 3.4 Convergence and termination

**Quiescence**: no user actions, every device online and connected, the network delivers every message, no faults are injected, every question the engine asks (§6.1) is answered, the scheduler is fair (every enabled action runs eventually) and every effect the driver starts completes.

**[changed]** The property is conditional (review B6). Some states legitimately persist under quiescence: a node reported as a problem (unreadable, a name the local file system cannot hold, an ignored file occupying the name), a node held for a decision nobody gives, or a space in recovery (§6.4) or paused after a failed root check. A node is **eligible** when none of these applies to it or to an ancestor.

**Property (convergence).** From any reachable state, after quiescence starts, every device eventually reaches L = S = R for every eligible node, every eligible node on every disk matches the server's live tree (path, kind, content), no parked node remains, and nothing is in flight. Ineligible nodes stay unchanged on disk and are reported.

This is the analogue of Unison's maximality (a maximal run propagates every non-conflicting change, [Pierce and Vouillon 2004](references.md#ref-unison), §5): with conflicts resolved instead of left alone, nothing eligible remains unpropagated. The property promises eventual convergence, not a proven bound. CanopyCheck checks termination with a cutoff on planning iterations ([Goldberg 2020](references.md#ref-trinity)); the simulation does the same with a step budget after quiescence starts, which is a test heuristic: exceeding it is reported as a likely livelock, to be confirmed from the trace.

### 3.5 No resurrection

**Property (no resurrection).** A node trashed by a delete whose author had seen its latest state stays trashed unless a later op revives it, and every op a device sends derives from one specific user action on that device that has not been propagated yet (an edit, move, rename, create or delete it observed on disk) or from a user restore request. **[changed]** "Specific and not yet propagated" is new: it rules out an op that re-sends an old, already propagated edit of a node the user once touched (review B6).

The two rules that keep a node alive, R7 and R8, both need a change the deleter had not seen. The engine cannot fabricate one: it sends ops only for L ≠ S, L changes only through observations, and S only moves toward what was sent or what R says. Two engine paths could break this and are guarded: a crash that makes a known file look new (handled by adoption, §2.8), and a lost store that makes the whole folder look new (handled like a first link: local files are matched against R by path and content before anything is uploaded).

### 3.6 Moves

Kleppmann et al. show that concurrent moves either duplicate subtrees (Dropbox at the time) or must pick one winner, and that a move which creates a cycle in combination with another must be ignored ([2021](references.md#ref-move), §2). Their CRDT orders moves by timestamp and undoes and redoes to apply them in that order. Koidrive has the simpler setting they describe as state machine replication: one leader orders every operation, so a move "still needs to check for cycles, but it never needs to undo or redo" ([2021](references.md#ref-move), §5.2). The properties:

- **No duplication**: a node is in exactly one place in every tree at every step (Nucleus has the same invariant, "any moved folder is visible in exactly one location", [Goldberg 2020](references.md#ref-trinity)). A move is one op and one `rename` on disk.
- **No cycles**: R, S and L are trees at every step. The server rejects a cycle-creating move (R6), the client reverts it locally, and parking (§2.5) avoids transient cycles on disk.
- **Last commit wins** between concurrent moves of one node (R5), which is last writer wins with the commit seq as the timestamp.
