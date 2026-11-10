// SPDX-License-Identifier: Apache-2.0

//! Building commits from local changes and acknowledging their results (docs/design/sync-engine.md
//! sections 2.4, 2.6, 2.7, 2.8 and 4.2, and RFC 0002): the rebase of concurrent changes onto R, the
//! one commit in flight with its history and device slot, and what each result and error does.

use std::collections::{BTreeMap, BTreeSet};

use flarebox_proto::v1::{self as proto, error, op, op_result};
use prost::Message;

use crate::classify::{Class, Trees};
use crate::ids::IdGen;
use crate::names::{MAX_NAME_BYTES, name_key, nfc};
use crate::tree::{Content, Kind, Names, NodeId, Shape, same, same_content};

/// The most ops one commit holds (spec v0 section 13).
pub const MAX_OPS: usize = 1000;

/// The most bytes of change records a commit is planned to make, as estimated. Its records and
/// its result must each fit in one 1 MiB `ServerMessage` (spec v0 section 7.2), and the estimate is
/// rough, so it stays well below.
pub const MAX_COMMIT_BYTES: usize = 512 * 1024;

/// What one change record costs besides the node's name and content: its IDs, revisions, version
/// and framing.
const RECORD_BYTES: usize = 256;

/// The commit in flight as it is persisted before it is sent (section 2.2).
#[derive(Clone, Debug, PartialEq)]
pub struct InFlight {
    pub commit: proto::Commit,
    /// For each `Delete` op, by its target, every node the client planned to delete with it.
    pub deletes: BTreeMap<NodeId, Vec<NodeId>>,
    /// The result once it has arrived, kept until R holds its records.
    pub result: Option<proto::CommitResult>,
}

/// Why no commit is built or resent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Halt {
    /// The server takes no writes from this device. Downloads continue, and a `Welcome` that
    /// allows writes lifts it.
    ReadOnly,
    /// The store cannot account for the server's state (sections 2.8 and 6.4). The commit in flight
    /// is kept as evidence and nothing is planned from this store.
    Recovery,
    /// The server refused the commit with an error the engine cannot act on: a code it does not
    /// know, \`UNSPECIFIED\` among them, or \`QUOTA_EXCEEDED\` without a retry time. The commit in
    /// flight is kept and never resent, nothing more is planned, and downloads continue.
    Refused,
}

/// What to do with the commit in flight after connecting (section 2.8, RFC 0002 section 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reconnect {
    /// Nothing to send now.
    Idle,
    /// Send the commit in flight again, unchanged.
    Resend,
    /// Enter recovery.
    Recovery,
}

/// What a call-level error on the commit in flight leads to (section 4.2, RFC 0002 section 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Send the same commit again: after uploading its chunks again, after the back-off the error
    /// names, or after signing in again.
    Resend,
    /// Nothing was applied and the commit is forgotten. Its slot stays free, and S never changed
    /// for it, so its changes are planned again.
    Abandoned,
    /// Abandoned, and the server is behind what this device saw: bootstrap before planning again.
    Resync,
    /// The commit did not fit (`LIMIT_EXCEEDED`). Nothing was applied and the commit is
    /// forgotten; its changes are planned again in commits half its size, a delete that was
    /// alone in its commit is planned as deletes of its contents first, and any other op alone in
    /// its commit marks its node as a problem.
    Split,
    Halted(Halt),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitError {
    NotInFlight,
    /// The result does not have one outcome per op.
    ResultMismatch,
}

/// The commit side of a space: the accepted device counter, the commit in flight, the nodes the
/// server rejected an op for, and whether committing has stopped.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Commits {
    accepted: u64,
    in_flight: Option<InFlight>,
    problems: BTreeSet<NodeId>,
    halted: Option<Halt>,
    waiting: BTreeMap<NodeId, Option<Shape>>,
    /// The most ops the next commit holds after a `LIMIT_EXCEEDED`, doubled back towards
    /// [`MAX_OPS`] by every acknowledged commit.
    cap: Option<usize>,
    /// Folders whose delete did not fit alone, deleted contents first.
    split: BTreeSet<NodeId>,
    /// Folders merged into one L holds elsewhere, by the folder they were merged into. Creates and
    /// moves into them go to that folder, and the nodes they bring are reverted, so on disk they
    /// follow into it.
    aliases: BTreeMap<NodeId, NodeId>,
}

impl Commits {
    /// The accepted counter *a* of section 2.8: the `device_seq` of the last commit whose result
    /// was applied.
    pub fn accepted(&self) -> u64 {
        self.accepted
    }

    pub fn in_flight(&self) -> Option<&InFlight> {
        self.in_flight.as_ref()
    }

    /// Nodes an op was rejected for. They are left out of planning until [`Commits::clear_problem`],
    /// after the next rescan or a user action.
    pub fn problems(&self) -> &BTreeSet<NodeId> {
        &self.problems
    }

    pub fn clear_problem(&mut self, node: &str) -> bool {
        self.problems.remove(node)
    }

    pub fn halted(&self) -> Option<Halt> {
        self.halted
    }

    /// S updates of acknowledged ops that would not leave S a valid tree yet, by node. Each is
    /// made as soon as it fits, or dropped once S agrees with L for the node anyway; until then
    /// the node is left out of planning.
    pub fn waiting(&self) -> &BTreeMap<NodeId, Option<Shape>> {
        &self.waiting
    }

    /// Builds the next commit from local changes, unless one is in flight or committing has
    /// stopped. The caller builds only when connected and caught up, with `base_seq` the
    /// `last_seq` of R and `history_id` the history R belongs to.
    ///
    /// Concurrent changes are rebased onto R first (section 2.4): a file whose content changed
    /// differently on both sides is forked, its local version becoming a new node that is created
    /// under the original name; a node changed remotely keeps the nodes L deleted on its way to the
    /// root, which are downloaded again instead of deleted. Then every local change becomes ops,
    /// ordered on a scratch copy of R (section 2.6): an op is emitted once it is valid there. When
    /// none is, a create or move that only waits for a name is sent anyway, for the server to
    /// rename, except that a folder created where a moving folder still is takes a pending name
    /// first, and a folder it may be merged into is not deleted in the same commit. A delete waits
    /// until nothing that survives is left under it, a create or move until its parent exists, and
    /// a move that would close a cycle against R is reverted: the node is marked in
    /// [`Trees::reverted`], and the effects move it back to its R position on disk, bringing back
    /// the folders L deleted on its way. A parked or reverted node waits until the effects have
    /// moved it, and so does a node with a problem or a waiting S update. A commit stops at
    /// [`MAX_OPS`], at [`MAX_COMMIT_BYTES`] of estimated change records, or at half the size of
    /// the last commit that did not fit. The rebase changes the trees even when no commit results,
    /// so the caller plans effects again after building.
    pub fn build(
        &mut self,
        trees: &mut Trees,
        ids: &mut IdGen,
        now_ms: u64,
        history_id: &str,
        base_seq: u64,
    ) -> Option<&proto::Commit> {
        if self.in_flight.is_some() || self.halted.is_some() {
            return None;
        }
        self.retry_waiting(trees, BTreeSet::new(), Vec::new());
        let parked = trees.local().iter().filter(|(_, node)| node.parked);
        let skip: BTreeSet<NodeId> = self
            .problems
            .iter()
            .chain(self.waiting.keys())
            .chain(parked.map(|(id, _)| id))
            .cloned()
            .collect();
        let (needed, agreed) = rebase(trees, ids, now_ms, &skip);
        let needed = self.retry_waiting(trees, needed, agreed);
        self.split.retain(|id| trees.synced().live(id).is_some());
        self.aliases.retain(|id, _| trees.local().get(id).is_some());
        let cap = self.cap.unwrap_or(MAX_OPS);
        let (ops, deletes, reverts) = plan(trees, &skip, &needed, cap, &self.split, &self.aliases);
        for id in reverts {
            trees.revert(id);
        }
        if ops.is_empty() {
            return None;
        }
        self.in_flight = Some(InFlight {
            commit: proto::Commit {
                commit_id: ids.uuid_v7(now_ms),
                base_seq,
                ops,
                history_id: history_id.to_owned(),
                device_seq: self.accepted + 1,
                ..Default::default()
            },
            deletes,
            result: None,
        });
        self.in_flight.as_ref().map(|flight| &flight.commit)
    }

    /// Keeps the result of the commit in flight until it can be acknowledged.
    pub fn received(&mut self, result: proto::CommitResult) -> Result<(), CommitError> {
        let flight = self.in_flight.as_mut().ok_or(CommitError::NotInFlight)?;
        if result.results.len() != flight.commit.ops.len() {
            return Err(CommitError::ResultMismatch);
        }
        flight.result = Some(result);
        Ok(())
    }

    /// Acknowledges the commit in flight once R holds its result's records, that is once
    /// `last_seq` has reached the result's seq (section 2.7): the caller applies a result whose
    /// seq is `last_seq + 1` to R first, and a result further ahead waits for the commits before
    /// it. S takes what was sent, not what the server returned, so any difference shows up as a
    /// remote change; a create or move the server renamed takes the server's name instead, and the
    /// node is reverted so that the disk takes it too. A conflict copy is re-keyed in L, and so is
    /// a directory merged into one L does not hold yet. When L already holds the directory it was
    /// merged into, the created directory and the nodes created in it are reverted: on disk they
    /// move into the merged directory and the emptied one is removed. The accepted counter takes
    /// the commit's slot. Returns false while there is nothing to acknowledge.
    ///
    /// S stays a valid tree. When the updates together would break it, because S still lags R
    /// elsewhere, each is made as soon as it fits and the rest wait (see [`Commits::waiting`]).
    pub fn acknowledge(&mut self, trees: &mut Trees, last_seq: u64) -> bool {
        let Some(flight) = self
            .in_flight
            .take_if(|flight| flight.result.as_ref().is_some_and(|r| r.seq <= last_seq))
        else {
            return false;
        };
        let result = flight.result.expect("taken with a result");
        let mut synced = Synced::default();
        let mut merged: BTreeMap<NodeId, NodeId> = BTreeMap::new();
        let aliases = self.aliases.clone();
        for (op, outcome) in flight.commit.ops.iter().zip(&result.results) {
            use op_result::Code;
            let code = Code::try_from(outcome.code).unwrap_or(Code::Unspecified);
            let applied = matches!(code, Code::Applied | Code::AppliedRenamed);
            let parent = |sent: &str| merged.get(sent).cloned().unwrap_or_else(|| sent.to_owned());
            let renamed = code == Code::AppliedRenamed && !outcome.name.is_empty();
            let name = |sent: &str| match renamed {
                true => outcome.name.clone(),
                false => sent.to_owned(),
            };
            let (node, done) = match &op.op {
                Some(op::Op::CreateDir(create)) => {
                    let shape = Shape {
                        parent: parent(&create.parent_id),
                        name: name(&create.name),
                        kind: Kind::Dir,
                        content: None,
                    };
                    let into = &outcome.node_id;
                    if code == Code::MergedInto && trees.local().get(into).is_some() {
                        trees.revert(create.node_id.clone());
                        merged.insert(create.node_id.clone(), into.clone());
                        self.aliases.insert(create.node_id.clone(), into.clone());
                    } else if code == Code::MergedInto {
                        rekey(trees, &create.node_id, into);
                        synced.set(into, Some(shape));
                        merged.insert(create.node_id.clone(), into.clone());
                    } else if applied {
                        synced.set(&create.node_id, Some(shape));
                    }
                    (&create.node_id, applied || code == Code::MergedInto)
                }
                Some(op::Op::CreateFile(create)) => {
                    if applied {
                        let shape = Shape {
                            parent: parent(&create.parent_id),
                            name: name(&create.name),
                            kind: Kind::File,
                            content: create.content.clone(),
                        };
                        synced.set(&create.node_id, Some(shape));
                    }
                    (&create.node_id, applied)
                }
                Some(op::Op::UpdateContent(update)) => {
                    let id = &update.node_id;
                    let edited = synced.get(trees, id).map(|shape| Shape {
                        content: update.content.clone(),
                        ..shape
                    });
                    if applied && edited.is_some() {
                        synced.set(id, edited);
                    } else if code == Code::ConflictCopy {
                        synced.set(&outcome.node_id, edited);
                        synced.set(id, None);
                        rekey(trees, id, &outcome.node_id);
                    }
                    (id, applied || code == Code::ConflictCopy)
                }
                Some(op::Op::Move(moved)) => {
                    let id = &moved.node_id;
                    let cycle = code == Code::Rejected && outcome.reject_reason == "CYCLE";
                    let target = parent(&moved.new_parent_id);
                    if let Some(shape) = synced.get(trees, id)
                        && applied
                        && target != *id
                    {
                        let shape = Shape {
                            parent: target,
                            name: name(&moved.new_name),
                            ..shape
                        };
                        synced.set(id, Some(shape));
                    }
                    if cycle {
                        trees.revert(id.clone());
                    }
                    (id, applied || cycle)
                }
                Some(op::Op::Delete(delete)) => {
                    let done = applied || code == Code::Superseded;
                    if done {
                        for id in flight.deletes.get(&delete.node_id).into_iter().flatten() {
                            synced.set(id, None);
                        }
                    }
                    (&delete.node_id, done)
                }
                _ => continue,
            };
            let follows = trees.local().get(node).is_some_and(|local| {
                let held = &local.shape.parent;
                aliases.contains_key(held) || merged.contains_key(held)
            });
            if renamed || (done && follows) {
                trees.revert(node.clone());
            }
            if !done {
                self.problems.insert(node.clone());
            }
        }
        for (id, shape) in synced.apply(trees) {
            self.waiting.insert(id, shape);
        }
        self.retry_waiting(trees, BTreeSet::new(), Vec::new());
        self.accepted = flight.commit.device_seq;
        self.cap = self.cap.map(|cap| cap * 2).filter(|cap| *cap < MAX_OPS);
        true
    }

    /// Makes the waiting S updates that fit now, together with S := ⊥ for the `needed` nodes the
    /// keep rule could not clear yet and the `agreed` updates of the rebase, which may depend on
    /// each other. Returns the needed nodes still not cleared; agreed updates that do not fit are
    /// dropped, since the next rebase finds them again.
    fn retry_waiting(
        &mut self,
        trees: &mut Trees,
        needed: BTreeSet<NodeId>,
        agreed: Vec<(NodeId, Option<Shape>)>,
    ) -> BTreeSet<NodeId> {
        self.waiting.retain(|id, _| {
            !same(
                trees.synced().live(id),
                trees.local().get(id).map(|node| &node.shape),
            )
        });
        let updates = std::mem::take(&mut self.waiting)
            .into_iter()
            .chain(needed.iter().map(|id| (id.clone(), None)))
            .chain(agreed.iter().cloned())
            .collect();
        let mut still = BTreeSet::new();
        for (id, shape) in update_synced(trees, updates) {
            if needed.contains(&id) {
                still.insert(id);
            } else if !agreed.iter().any(|(other, _)| *other == id) {
                self.waiting.insert(id, shape);
            }
        }
        still
    }

    /// What a call-level error on the commit in flight means. An error that proves nothing was
    /// applied forgets the commit; one that leaves the outcome unknown keeps it to resend; a
    /// history mismatch or a stale device state keeps it as evidence and stops committing; and an
    /// error the engine cannot act on refuses it (see [\`Halt::Refused\`]). \`QUOTA_EXCEEDED\` is
    /// resent only when it names a retry time, after that wait.
    pub fn failed(&mut self, error: &proto::Error) -> Result<Failure, CommitError> {
        use error::Code;
        if self.in_flight.is_none() {
            return Err(CommitError::NotInFlight);
        }
        let failure = match Code::try_from(error.code) {
            Ok(Code::HistoryMismatch | Code::StaleDeviceState) => Failure::Halted(Halt::Recovery),
            Ok(Code::ReadOnly | Code::Forbidden) => Failure::Halted(Halt::ReadOnly),
            Ok(Code::AheadOfServer) => Failure::Resync,
            Ok(Code::LimitExceeded) => Failure::Split,
            Ok(Code::InvalidArgument | Code::HeadMoved | Code::UnsupportedVersion) => {
                Failure::Abandoned
            }
            Ok(Code::QuotaExceeded) if error.retry_after_ms > 0 => Failure::Resend,
            Ok(Code::MissingChunks | Code::RetryLater | Code::Unauthenticated | Code::Internal) => {
                Failure::Resend
            }
            _ => Failure::Halted(Halt::Refused),
        };
        match failure {
            Failure::Halted(halt @ (Halt::Recovery | Halt::Refused)) => self.halted = Some(halt),
            Failure::Resend => {}
            Failure::Halted(halt) => {
                self.halted = Some(halt);
                self.in_flight = None;
            }
            Failure::Abandoned | Failure::Resync => self.in_flight = None,
            Failure::Split => {
                let flight = self.in_flight.take().expect("checked");
                let ops = flight.commit.ops;
                if ops.len() > 1 {
                    self.cap = Some(ops.len() / 2);
                } else if let Some(op) = ops.into_iter().next().and_then(|op| op.op) {
                    match op {
                        op::Op::Delete(delete) if !self.split.contains(&delete.node_id) => {
                            self.split.insert(delete.node_id);
                        }
                        op => {
                            self.problems.insert(op_node(&op).to_owned());
                        }
                    }
                }
            }
        }
        Ok(failure)
    }

    /// Forgets the commit in flight when the driver knows it was not applied: after
    /// `MISSING_CHUNKS` when the file changed since it was chunked (section 4.2). False when no
    /// commit is in flight or its result has arrived.
    pub fn abandon(&mut self) -> bool {
        self.in_flight
            .take_if(|flight| flight.result.is_none())
            .is_some()
    }

    /// Compares the `Welcome` with the commit in flight and the accepted counter *a* (section
    /// 2.8). With *w* the server's counter, *w* = *a* means the commit in flight was not accepted
    /// and *w* = *a* + 1 that it was and its response was lost; both resend it unchanged, and
    /// the server answers an accepted one with its stored result. Any other *w*, or a commit
    /// built in another history, starts recovery. A read-only or refused device resends nothing. Whether R
    /// itself belongs to the `Welcome`'s history is checked where R is kept.
    pub fn connected(&mut self, welcome: &proto::Welcome) -> Reconnect {
        let (a, w) = (self.accepted, welcome.device_seq);
        let reconnect = match &self.in_flight {
            _ if self.halted == Some(Halt::Recovery) => Reconnect::Recovery,
            Some(flight) if flight.commit.history_id != welcome.history_id => Reconnect::Recovery,
            None if w == a => Reconnect::Idle,
            Some(flight) if flight.result.is_none() && (w == a || w == a + 1) => Reconnect::Resend,
            Some(_) if w == a + 1 => Reconnect::Idle,
            _ => Reconnect::Recovery,
        };
        if reconnect == Reconnect::Recovery {
            self.halted = Some(Halt::Recovery);
            return reconnect;
        }
        if self.halted == Some(Halt::Refused) {
            return Reconnect::Idle;
        }
        if welcome.read_only {
            self.halted = Some(Halt::ReadOnly);
            return Reconnect::Idle;
        }
        if self.halted == Some(Halt::ReadOnly) {
            self.halted = None;
        }
        reconnect
    }
}

fn op_node(op: &op::Op) -> &str {
    match op {
        op::Op::CreateDir(op) => &op.node_id,
        op::Op::CreateFile(op) => &op.node_id,
        op::Op::UpdateContent(op) => &op.node_id,
        op::Op::Move(op) => &op.node_id,
        op::Op::Delete(op) => &op.node_id,
        op::Op::Restore(op) => &op.node_id,
    }
}

/// The S updates an acknowledgement makes, in op order, made together at the end.
#[derive(Default)]
struct Synced {
    updates: Vec<(NodeId, Option<Shape>)>,
}

impl Synced {
    fn set(&mut self, id: &str, shape: Option<Shape>) {
        self.updates.push((id.to_owned(), shape));
    }

    /// S for `id` with the updates so far.
    fn get(&self, trees: &Trees, id: &str) -> Option<Shape> {
        match self.updates.iter().rev().find(|(other, _)| other == id) {
            Some((_, shape)) => shape.clone(),
            None => trees.synced().live(id).cloned(),
        }
    }

    /// Makes the updates that leave S valid and returns the rest (see [`update_synced`]).
    fn apply(self, trees: &mut Trees) -> Vec<(NodeId, Option<Shape>)> {
        update_synced(trees, self.updates)
    }
}

/// Makes the S updates, the last for each node, that leave S a valid tree: all of them together
/// when they fit together, otherwise each that fits on its own and then the rest together again,
/// while any is made. Returns the rest.
fn update_synced(
    trees: &mut Trees,
    updates: Vec<(NodeId, Option<Shape>)>,
) -> Vec<(NodeId, Option<Shape>)> {
    let mut left: Vec<(NodeId, Option<Shape>)> = Vec::new();
    for (id, shape) in updates {
        left.retain(|(other, _)| *other != id);
        left.push((id, shape));
    }
    while !left.is_empty() {
        let before: Vec<(NodeId, Option<Shape>)> = left
            .iter()
            .map(|(id, _)| (id.clone(), trees.synced().live(id).cloned()))
            .collect();
        for (id, shape) in &left {
            trees.set_synced(id.clone(), shape.clone());
        }
        if trees.synced().check(Names::ByKey).is_ok() {
            return Vec::new();
        }
        for (id, shape) in before {
            trees.set_synced(id, shape);
        }
        let count = left.len();
        left.retain(|(id, shape)| !trees.set_synced_if_valid(id.clone(), shape.clone()));
        if left.len() == count {
            break;
        }
    }
    left
}

/// Gives the local node `from` the ID `to`; its children follow.
fn rekey(trees: &mut Trees, from: &str, to: &str) {
    let Some(node) = trees.local().get(from).cloned() else {
        return;
    };
    let children: Vec<NodeId> = trees
        .local()
        .iter()
        .filter(|(_, child)| child.shape.parent == from)
        .map(|(id, _)| id.clone())
        .collect();
    trees.remove_local(from);
    trees.set_local(to.to_owned(), node);
    for id in children {
        let mut child = trees.local().get(&id).cloned().expect("listed above");
        child.shape.parent = to.to_owned();
        trees.set_local(id, child);
    }
}

/// The rebase of section 2.4, before ops are planned. A reverted file that R no longer has but that
/// holds an edit not uploaded yet becomes a new node, so the edit is uploaded as a create instead
/// of being removed with the node. Nodes in `skip` are not rebased otherwise, though what they changed
/// remotely still keeps their ancestors. Returns the nodes the keep rule could
/// not clear from S yet, and the S updates both sides agree on, which are made together with the
/// waiting ones.
fn rebase(
    trees: &mut Trees,
    ids: &mut IdGen,
    now_ms: u64,
    skip: &BTreeSet<NodeId>,
) -> (BTreeSet<NodeId>, Vec<(NodeId, Option<Shape>)>) {
    let dirty: Vec<NodeId> = trees.dirty().iter().cloned().collect();
    for id in &dirty {
        if trees.reverted().contains(id) && trees.remote().live(id).is_none() && trees.edited(id) {
            rekey(trees, id, &ids.uuid_v7(now_ms));
        }
    }
    for id in dirty.iter().filter(|id| !skip.contains(*id)) {
        if trees.class(id) != Class::Concurrent || !edited_on_both_sides(trees, id) {
            continue;
        }
        let local = trees.local().get(id).cloned().expect("a local edit");
        trees.remove_local(id);
        trees.set_local(ids.uuid_v7(now_ms), local);
    }
    let mut needed = BTreeSet::new();
    let mut agreed = Vec::new();
    for id in &dirty {
        keep(trees, id, &mut needed);
        if skip.contains(id) {
            continue;
        }
        match trees.class(id) {
            Class::Concurrent => {
                agreed.extend(take_agreed(trees, id).map(|s| (id.clone(), Some(s))))
            }
            Class::Same => agreed.push((id.clone(), trees.remote().live(id).cloned())),
            _ => {}
        }
    }
    (needed, agreed)
}

/// S takes the part of a concurrent change that both sides made the same way, position or
/// content, so only the parts that differ are left to rebase. None when they agree on nothing new.
fn take_agreed(trees: &Trees, id: &str) -> Option<Shape> {
    let (Some(local), Some(synced), Some(remote)) = (
        trees.local().get(id).map(|node| &node.shape),
        trees.synced().live(id),
        trees.remote().live(id),
    ) else {
        return None;
    };
    let mut next = synced.clone();
    if local.same_position(remote) {
        next.parent = remote.parent.clone();
        next.name = remote.name.clone();
    }
    if let (Some(l), Some(r)) = (&local.content, &remote.content)
        && same_content(l, r)
    {
        next.content = Some(r.clone());
    }
    (!next.same(synced)).then_some(next)
}

/// A file whose content changed on both sides, each differently (R3).
fn edited_on_both_sides(trees: &Trees, id: &str) -> bool {
    let content = |shape: Option<&Shape>| shape.and_then(|shape| shape.content.clone());
    let (Some(l), Some(r)) = (
        content(trees.local().live(id)),
        content(trees.remote().live(id)),
    ) else {
        return false;
    };
    let s = content(trees.synced().live(id));
    let changed = |c: &Content| s.as_ref().is_none_or(|s| !same_content(c, s));
    changed(&l) && changed(&r) && !same_content(&l, &r)
}

/// The keep rule (R7): a node that changed remotely, or that L still has but must move to its R
/// position because it is reverted or parked, keeps every node L deleted on the way it will hang
/// from the root, by setting S := ⊥ for them, so they are downloaded again. A kept node that S
/// cannot drop yet, because S still has children under it, is reverted instead, so the effects
/// bring it back at its R position, and it goes into `needed` so it is not deleted. A reverted node
/// L deleted is an ordinary local delete.
fn keep(trees: &mut Trees, id: &str, needed: &mut BTreeSet<NodeId>) {
    let reverted = trees
        .local()
        .get(id)
        .is_some_and(|node| node.parked || trees.reverted().contains(id));
    if trees.remote().live(id).is_none()
        || (!reverted && same(trees.remote().live(id), trees.synced().live(id)))
    {
        return;
    }
    let mut current = id.to_owned();
    for _ in 0..=trees.remote().len() {
        let Some(remote) = trees.remote().live(&current) else {
            return;
        };
        let parent = remote.parent.clone();
        match trees.local().get(&current) {
            None if trees.synced().live(&current).is_some() => {
                if !trees.set_synced_if_valid(current.clone(), None) {
                    trees.revert(current.clone());
                    needed.insert(current.clone());
                }
            }
            None => {}
            Some(local) => {
                let moved = trees
                    .synced()
                    .live(&current)
                    .is_none_or(|synced| !local.shape.same_position(synced));
                if moved && !trees.reverted().contains(&current) {
                    return;
                }
            }
        }
        if parent == trees.remote().root {
            return;
        }
        current = parent;
    }
}

/// The bytes a change record of a node with this shape takes, roughly.
fn record_cost(shape: Option<&Shape>) -> usize {
    RECORD_BYTES
        + shape.map_or(0, |shape| {
            shape.name.len() + shape.content.as_ref().map_or(0, |c| c.encoded_len())
        })
}

/// The bytes of change records `want` makes, roughly: one record for each node it changes, and a
/// record carries the node's current content.
fn cost(trees: &Trees, want: &Want) -> usize {
    match want {
        Want::Create { shape, .. } => record_cost(Some(shape)),
        Want::Move { id, name, .. } => {
            record_cost(trees.remote().live(id).or(trees.synced().live(id))) + name.len()
        }
        Want::Content { content, .. } => RECORD_BYTES + content.encoded_len(),
        Want::Delete { members, .. } => members
            .iter()
            .map(|node| record_cost(trees.synced().live(node)))
            .sum(),
    }
}

enum Want {
    Create {
        id: NodeId,
        shape: Shape,
    },
    Move {
        id: NodeId,
        parent: NodeId,
        name: String,
    },
    Content {
        id: NodeId,
        content: Content,
    },
    Delete {
        id: NodeId,
        members: BTreeSet<NodeId>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fit {
    Valid,
    NameTaken,
    Cycle,
    Wait,
}

type Planned = (Vec<proto::Op>, BTreeMap<NodeId, Vec<NodeId>>, Vec<NodeId>);

/// The ops for every local change, in an order valid on a scratch copy of R, the nodes each
/// delete covers, and the nodes to revert: moves that close a cycle, and nodes in a merged folder
/// that S or R already has where the folder it was merged into holds them, so they follow R.
fn plan(
    trees: &Trees,
    problems: &BTreeSet<NodeId>,
    needed: &BTreeSet<NodeId>,
    cap: usize,
    split: &BTreeSet<NodeId>,
    aliases: &BTreeMap<NodeId, NodeId>,
) -> Planned {
    let mut wants = Vec::new();
    let mut deleted = BTreeSet::new();
    let mut reverts = Vec::new();
    for id in trees.dirty().iter().filter(|id| !problems.contains(*id)) {
        if !matches!(trees.class(id), Class::Local | Class::Concurrent) {
            continue;
        }
        let synced = trees.synced().live(id);
        let Some(local) = trees.local().get(id).map(|node| &node.shape) else {
            if synced.is_some() && trees.class(id) == Class::Local && !needed.contains(id) {
                deleted.insert(id.clone());
            }
            continue;
        };
        let aliased;
        let in_alias = aliases.contains_key(&local.parent);
        let local = match aliases.get(&local.parent) {
            Some(into) => {
                aliased = Shape {
                    parent: into.clone(),
                    ..local.clone()
                };
                &aliased
            }
            None => local,
        };
        let Some(remote) = trees.remote().get(id) else {
            if !trees.reverted().contains(id) {
                wants.push(Want::Create {
                    id: id.clone(),
                    shape: local.clone(),
                });
            }
            continue;
        };
        if remote.shape.kind != local.kind || synced.is_some_and(|s| s.kind != local.kind) {
            continue;
        }
        let remote = trees.remote().live(id);
        let placed = |shape: Option<&Shape>| shape.is_some_and(|s| s.same_position(local));
        if in_alias && (placed(synced) || placed(remote)) && !trees.reverted().contains(id) {
            reverts.push(id.clone());
        }
        if synced.is_none_or(|s| !s.same_position(local))
            && remote.is_none_or(|r| !r.same_position(local))
            && !trees.reverted().contains(id)
        {
            wants.push(Want::Move {
                id: id.clone(),
                parent: local.parent.clone(),
                name: local.name.clone(),
            });
        }
        let differs = |shape: Option<&Shape>, content: &Content| {
            shape
                .and_then(|shape| shape.content.as_ref())
                .is_none_or(|other| !same_content(other, content))
        };
        if let Some(content) = &local.content
            && differs(synced, content)
            && differs(remote, content)
        {
            wants.push(Want::Content {
                id: id.clone(),
                content: content.clone(),
            });
        }
    }
    let mut tops = Vec::new();
    let mut children: BTreeMap<&str, Vec<&NodeId>> = BTreeMap::new();
    for id in &deleted {
        match trees.synced().live(id).map(|s| &s.parent) {
            Some(parent) if deleted.contains(parent) => {
                children.entry(parent).or_default().push(id)
            }
            _ => tops.push(id),
        }
    }
    while let Some(id) = tops.pop() {
        let mut members = BTreeSet::new();
        let mut below = vec![id];
        while let Some(node) = below.pop() {
            members.insert(node.clone());
            below.extend(children.get(node.as_str()).into_iter().flatten());
        }
        let bytes: usize = members
            .iter()
            .map(|node| record_cost(trees.synced().live(node)))
            .sum();
        match children.get(id.as_str()) {
            Some(kids) if bytes > MAX_COMMIT_BYTES || split.contains(id) => tops.extend(kids),
            _ => wants.push(Want::Delete {
                id: id.clone(),
                members,
            }),
        }
    }

    let mut scratch = Scratch::new(trees);
    let mut moving: BTreeSet<NodeId> = wants
        .iter()
        .filter_map(|want| match want {
            Want::Move { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    let mut wants: Vec<Option<Want>> = wants.into_iter().map(Some).collect();
    let mut ops = Vec::new();
    let mut deletes = BTreeMap::new();
    let mut emit = |want: Want, scratch: &mut Scratch, moving: &mut BTreeSet<NodeId>| {
        let op = match want {
            Want::Create { id, shape } => {
                scratch.place(&id, &shape.parent, &shape.name, shape.kind);
                match shape.kind {
                    Kind::Dir => op::Op::CreateDir(proto::CreateDir {
                        node_id: id,
                        parent_id: shape.parent,
                        name: shape.name,
                    }),
                    Kind::File => op::Op::CreateFile(proto::CreateFile {
                        node_id: id,
                        parent_id: shape.parent,
                        name: shape.name,
                        content: shape.content,
                    }),
                }
            }
            Want::Move { id, parent, name } => {
                moving.remove(&id);
                scratch.place(&id, &parent, &name, scratch.nodes[&id].kind);
                op::Op::Move(proto::Move {
                    node_id: id,
                    new_parent_id: parent,
                    new_name: name,
                })
            }
            Want::Content { id, content } => {
                scratch.revive(&id);
                op::Op::UpdateContent(proto::UpdateContent {
                    node_id: id,
                    content: Some(content),
                })
            }
            Want::Delete { id, members } => {
                scratch.trash(&id);
                deletes.insert(id.clone(), members.into_iter().collect());
                op::Op::Delete(proto::Delete { node_id: id })
            }
        };
        ops.push(proto::Op { op: Some(op) });
    };
    let (mut emitted, mut bytes) = (0, 0);
    let mut room = |want: &Want| {
        let cost = cost(trees, want);
        if emitted == cap || (emitted > 0 && bytes + cost > MAX_COMMIT_BYTES) {
            return false;
        }
        emitted += 1;
        bytes += cost;
        true
    };
    let capped = loop {
        let (mut progress, mut full) = (false, false);
        for slot in wants.iter_mut() {
            if slot
                .as_ref()
                .is_some_and(|want| scratch.fit(want, &moving) == Fit::Valid)
            {
                if !room(slot.as_ref().expect("checked")) {
                    full = true;
                    break;
                }
                emit(slot.take().expect("checked"), &mut scratch, &mut moving);
                progress = true;
            }
        }
        if full {
            break true;
        }
        if progress {
            continue;
        }
        let forced = wants.iter_mut().find(|slot| {
            slot.as_ref()
                .is_some_and(|want| scratch.fit(want, &moving) == Fit::NameTaken)
        });
        let Some(slot) = forced else {
            break false;
        };
        if !room(slot.as_ref().expect("checked")) {
            break true;
        }
        if let Some(Want::Create { shape, .. }) = slot.as_mut()
            && shape.kind == Kind::Dir
        {
            let held = scratch.held(&shape.parent, &shape.name);
            if held.iter().any(|other| moving.contains(other)) {
                shape.name = scratch.free_name(&shape.parent, &shape.name);
            } else {
                scratch.merged.extend(held);
            }
        }
        emit(slot.take().expect("checked"), &mut scratch, &mut moving);
    };
    if !capped {
        for want in wants.iter().flatten() {
            if let Want::Move { id, .. } = want
                && scratch.fit(want, &moving) == Fit::Cycle
            {
                reverts.push(id.clone());
            }
        }
    }
    (ops, deletes, reverts)
}

struct Entry {
    parent: NodeId,
    key: String,
    kind: Kind,
    live: bool,
}

/// R as the ops emitted so far leave it on the server, as far as their order depends on it.
struct Scratch {
    root: NodeId,
    nodes: BTreeMap<NodeId, Entry>,
    /// Live children by parent, then by name key.
    children: BTreeMap<NodeId, BTreeMap<String, BTreeSet<NodeId>>>,
    /// Folders a forced create will be merged into (MERGED_INTO). Deleting one in the same
    /// commit would trash what the create brings, so their deletes wait for the next commit.
    merged: BTreeSet<NodeId>,
}

impl Scratch {
    fn new(trees: &Trees) -> Self {
        let mut scratch = Self {
            root: trees.remote().root.clone(),
            nodes: BTreeMap::new(),
            children: BTreeMap::new(),
            merged: BTreeSet::new(),
        };
        for (id, node) in trees.remote().iter() {
            let entry = Entry {
                parent: node.shape.parent.clone(),
                key: name_key(&node.shape.name),
                kind: node.shape.kind,
                live: false,
            };
            scratch.nodes.insert(id.clone(), entry);
            if !node.trashed {
                scratch.link(id);
            }
        }
        scratch
    }

    fn link(&mut self, id: &str) {
        let entry = self.nodes.get_mut(id).expect("linked nodes exist");
        entry.live = true;
        self.children
            .entry(entry.parent.clone())
            .or_default()
            .entry(entry.key.clone())
            .or_default()
            .insert(id.to_owned());
    }

    fn unlink(&mut self, id: &str) {
        let Some(entry) = self.nodes.get_mut(id).filter(|entry| entry.live) else {
            return;
        };
        entry.live = false;
        if let Some(named) = self
            .children
            .get_mut(&entry.parent)
            .and_then(|keys| keys.get_mut(&entry.key))
        {
            named.remove(id);
        }
    }

    fn fit(&self, want: &Want, moving: &BTreeSet<NodeId>) -> Fit {
        match want {
            Want::Create { id, shape } => self.fit_at(id, &shape.parent, &shape.name),
            Want::Move { id, parent, name } => self.fit_at(id, parent, name),
            Want::Content { id, .. } if moving.contains(id) => Fit::Wait,
            Want::Content { .. } => Fit::Valid,
            Want::Delete { id, members } => {
                let subtree = self.subtree(id);
                if !self.merged.contains(id)
                    && subtree
                        .iter()
                        .all(|node| members.contains(node) && !self.merged.contains(node))
                {
                    Fit::Valid
                } else {
                    Fit::Wait
                }
            }
        }
    }

    /// Whether `id` can take `name` under `parent`. A trashed parent counts: the server revives
    /// it (R8).
    fn fit_at(&self, id: &str, parent: &str, name: &str) -> Fit {
        let is_dir = parent == self.root
            || self
                .nodes
                .get(parent)
                .is_some_and(|entry| entry.kind == Kind::Dir);
        if !is_dir {
            return Fit::Wait;
        }
        if self.inside(parent, id) {
            return Fit::Cycle;
        }
        let taken = self
            .children
            .get(parent)
            .and_then(|keys| keys.get(&name_key(name)))
            .is_some_and(|named| named.iter().any(|other| other != id));
        if taken { Fit::NameTaken } else { Fit::Valid }
    }

    /// True when `ancestor` is `id` or holds it.
    fn inside(&self, id: &str, ancestor: &str) -> bool {
        let mut current = id;
        for _ in 0..=self.nodes.len() {
            if current == ancestor {
                return true;
            }
            match self.nodes.get(current) {
                Some(entry) => current = &entry.parent,
                None => return false,
            }
        }
        false
    }

    /// The live nodes under `id`.
    fn subtree(&self, id: &str) -> Vec<NodeId> {
        let mut found = Vec::new();
        let mut stack = vec![id.to_owned()];
        while let Some(current) = stack.pop() {
            for child in self
                .children
                .get(&current)
                .into_iter()
                .flat_map(|keys| keys.values().flatten())
            {
                found.push(child.clone());
                stack.push(child.clone());
            }
        }
        found
    }

    fn place(&mut self, id: &str, parent: &str, name: &str, kind: Kind) {
        self.unlink(id);
        let entry = Entry {
            parent: parent.to_owned(),
            key: name_key(name),
            kind,
            live: false,
        };
        self.nodes.insert(id.to_owned(), entry);
        self.link(id);
        self.revive(parent);
    }

    /// Makes `id` live with the trashed ancestors it needs, as R8 does.
    fn revive(&mut self, id: &str) {
        let mut current = id.to_owned();
        for _ in 0..=self.nodes.len() {
            let Some(entry) = self.nodes.get(&current).filter(|entry| !entry.live) else {
                return;
            };
            let parent = entry.parent.clone();
            self.link(&current);
            current = parent;
        }
    }

    /// The live folders named `name` under `parent`, which a created folder of that name merges
    /// into.
    fn held(&self, parent: &str, name: &str) -> Vec<NodeId> {
        self.children
            .get(parent)
            .and_then(|keys| keys.get(&name_key(name)))
            .into_iter()
            .flatten()
            .filter(|id| self.nodes[*id].kind == Kind::Dir)
            .cloned()
            .collect()
    }

    /// A name under `parent` that nothing holds, for a folder created while the folder holding
    /// its name moves away: it is renamed to its name in a later commit, instead of being merged
    /// into the folder that leaves.
    fn free_name(&self, parent: &str, name: &str) -> String {
        (1..)
            .map(|n| {
                let suffix = match n {
                    1 => " (pending)".to_owned(),
                    n => format!(" (pending {n})"),
                };
                let mut stem: Vec<char> = nfc(name).chars().collect();
                let named = |stem: &[char]| format!("{}{suffix}", stem.iter().collect::<String>());
                while nfc(&named(&stem)).len() > MAX_NAME_BYTES {
                    stem.pop();
                }
                named(&stem)
            })
            .find(|candidate| {
                self.children
                    .get(parent)
                    .and_then(|keys| keys.get(&name_key(candidate)))
                    .is_none_or(|named| named.is_empty())
            })
            .expect("a free name")
    }

    fn trash(&mut self, id: &str) {
        for node in self.subtree(id).into_iter().chain([id.to_owned()]) {
            self.unlink(&node);
        }
    }
}
