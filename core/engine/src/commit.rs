// SPDX-License-Identifier: Apache-2.0

//! Building commits from local changes and acknowledging their results (docs/design/sync-engine.md
//! sections 2.4, 2.6, 2.7, 2.8 and 4.2, and RFC 0002): the rebase of concurrent changes onto R, the
//! one commit in flight with its history and device slot, and what each result and error does.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use koidrive_proto::v1::{self as proto, error, op, op_result};
use prost::Message;

use crate::classify::{Class, Origin, Trees, Waiting, classify};
use crate::first_link;
use crate::guard::{self, GuardState, RemoteRemoval};
use crate::ids::{IdGen, unix_ms};
use crate::names::{MAX_NAME_BYTES, name_key, nfc};
use crate::question::{Answer, ConflictFile, Question, QuestionKind, Revisions};
use crate::restore::{Outcome, RESOLVED_KEPT, Rejection, Resolved, RestoreId, RestoreIntent};
use crate::state::{Agreed, CommitsState, StateError};
use crate::tree::{Content, Kind, Names, NodeId, Shape, same, same_content};

/// The most ops one commit holds (spec v0 section 13).
pub const MAX_OPS: usize = 1000;
/// How many of this device's acknowledged commits [`Commits::held_through`] remembers. A server
/// restored from a backup older than all of them gives 0, so no lost delete is sent again.
pub const SLOTS_KEPT: usize = 1000;

/// The most bytes of change records a commit is planned to make, as estimated. Its records and
/// its result must each fit in one 1 MiB `ServerMessage` (spec v0 section 7.2), and the estimate is
/// rough, so it stays well below.
pub const MAX_COMMIT_BYTES: usize = 512 * 1024;

/// What one change record costs besides the node's name and content: its IDs, revisions, version
/// and framing.
const RECORD_BYTES: usize = 256;

/// The seq a commit builds on and the commit token there (RFC 0013): the `commit_id` and
/// `committed_at_ms` of the commit R applied at that seq. An empty token claims nothing, so a
/// server checks only the seq.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Base {
    pub seq: u64,
    pub commit_id: String,
    pub committed_at_ms: i64,
}

impl From<u64> for Base {
    fn from(seq: u64) -> Self {
        Self {
            seq,
            ..Self::default()
        }
    }
}

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
    /// A policy halt (RFC 0006 section 1): the server refused the commit with `code`, which only
    /// the user or a client update can resolve. The commit in flight is kept with its slot and
    /// never resent, nothing more is planned, and downloads continue. No `Welcome` lifts it, and
    /// it is part of the stored state (section 2.8), so it outlasts a restart.
    Policy { code: i32 },
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
    /// The server lost commits of this device that it had accepted: list the tree again, as for
    /// `AHEAD_OF_SERVER` (section 4.4), so that what it lost is sent again.
    AheadOfServer,
}

/// How a person lifts a policy halt (RFC 0006 section 1, point 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lift {
    /// Send the refused commit again, unchanged, once the person fixed the cause on the server, such
    /// as the access a `FORBIDDEN` refusal named.
    Resend,
    /// Forget the refused commit, so its changes are planned again into a commit with a new
    /// `commit_id`, as after `UNSUPPORTED_VERSION`. A refused commit stores nothing, so its slot
    /// stays free.
    Replan,
}

/// Why [`Commits::lift`] lifted nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiftError {
    /// No policy halt holds.
    NotHalted,
    /// The halt is a `CONFIRMATION_REQUIRED`, which only the owner's confirmation lifts (RFC 0006
    /// section 10), and no RFC defines that confirmation yet.
    NeedsConfirmation,
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
    /// alone in its commit is planned as deletes of its contents first, a restore that was alone
    /// ends as [`Outcome::DoesNotFit`], and any other op alone in its commit marks its node as a
    /// problem.
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
/// server rejected an op for, the restores a person asked for, and whether committing has stopped.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Commits {
    accepted: u64,
    in_flight: Option<InFlight>,
    problems: BTreeSet<NodeId>,
    halted: Option<Halt>,
    /// The most ops the next commit holds after a `LIMIT_EXCEEDED`, doubled back towards
    /// [`MAX_OPS`] by every acknowledged commit.
    cap: Option<usize>,
    /// Folders whose delete did not fit alone, deleted contents first.
    split: BTreeSet<NodeId>,
    /// Folders merged into one L holds elsewhere, by the folder they were merged into. Creates and
    /// moves into them go to that folder, and the nodes they bring are reverted, so on disk they
    /// follow into it. An alias ends with its folder's revert: a folder the user moves elsewhere
    /// is then created on its own, and what is created in it goes into it.
    aliases: BTreeMap<NodeId, NodeId>,
    /// Restores without an outcome, in request order (see [`Commits::restore`]).
    restores: BTreeMap<RestoreId, RestoreIntent>,
    /// The last [`RESOLVED_KEPT`] restores that ended, oldest first.
    resolved: Vec<Resolved>,
    /// The seq of each of this device's last [`SLOTS_KEPT`] acknowledged commits, by slot.
    slots: BTreeMap<u64, u64>,
    /// The open questions, in the order they were asked.
    questions: Vec<Question>,
    /// The local entries a [`Answer::Publish`] to a first link's question released (section 6.4),
    /// until the server applies their create. The person chose to keep them beside what R holds,
    /// so a create of one that takes a conflict name asks nothing.
    published: BTreeSet<NodeId>,
    guard: GuardState,
}

impl Commits {
    /// What the store keeps of the commit side.
    pub fn state(&self) -> CommitsState {
        CommitsState {
            accepted: self.accepted,
            in_flight: self.in_flight.clone(),
            problems: self.problems.clone(),
            halted: self.halted,
            cap: self.cap,
            split: self.split.clone(),
            aliases: self.aliases.clone(),
            restores: self.restores.clone(),
            resolved: self.resolved.clone(),
            slots: self.slots.clone(),
            questions: self.questions.clone(),
            published: self.published.clone(),
            guard: self.guard.clone(),
        }
    }

    /// The commit side the store kept. Refuses a commit in flight that does not take slot
    /// `accepted + 1`, whose result does not have one outcome per op, or that lists planned
    /// deletes for a node none of its `Delete` ops targets, or that holds a `Restore` op no queued
    /// restore asks for. Also refuses a cap of 0 or of at least [`MAX_OPS`], a folder merged into
    /// itself, and a restore that has an outcome and is still queued or has a second outcome.
    pub fn from_state(state: CommitsState) -> Result<Self, StateError> {
        if let Some(flight) = &state.in_flight {
            let device_seq = flight.commit.device_seq;
            if state.accepted.checked_add(1) != Some(device_seq) {
                return Err(StateError::DeviceSeq {
                    accepted: state.accepted,
                    device_seq,
                });
            }
            let ops = &flight.commit.ops;
            if let Some(result) = &flight.result
                && result.results.len() != ops.len()
            {
                return Err(StateError::ResultMismatch);
            }
            let deleted = |id: &str| {
                ops.iter().any(
                    |op| matches!(&op.op, Some(op::Op::Delete(delete)) if delete.node_id == id),
                )
            };
            if let Some(id) = flight.deletes.keys().find(|id| !deleted(id)) {
                return Err(StateError::DeleteWithoutOp(id.clone()));
            }
            carried(&state.restores, ops).map_err(StateError::RestoreNotQueued)?;
        }
        let mut ended = BTreeSet::new();
        for resolved in &state.resolved {
            if state.restores.contains_key(&resolved.id) || !ended.insert(&resolved.id) {
                return Err(StateError::RestoreResolved(resolved.id.clone()));
            }
        }
        if let Some(cap) = state.cap.filter(|cap| !(1..MAX_OPS).contains(cap)) {
            return Err(StateError::Cap(cap));
        }
        if let Some((id, _)) = state.aliases.iter().find(|(id, into)| id == into) {
            return Err(StateError::SelfAlias(id.clone()));
        }
        let CommitsState {
            accepted,
            in_flight,
            problems,
            halted,
            cap,
            split,
            aliases,
            restores,
            resolved,
            slots,
            questions,
            published,
            guard,
        } = state;
        Ok(Self {
            accepted,
            in_flight,
            problems,
            halted,
            cap,
            split,
            aliases,
            restores,
            resolved,
            slots,
            questions,
            published,
            guard,
        })
    }

    /// The seq of the newest commit of this device that the server counts, or 0 when none is
    /// known. A server that lost commits keeps every seq up to the backup it was restored from, and
    /// this commit is among them, so a node state of a seq at or below it is from before the loss
    /// (section 4.4).
    pub fn held_through(&self) -> u64 {
        self.slots.last_key_value().map_or(0, |(_, &seq)| seq)
    }

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

    /// Lifts a policy halt the way a person chose. With [`Lift::Resend`] the commit in flight
    /// stays, and the next `Welcome` that accounts for its slot sends it again (section 2.8); a
    /// server loss under the halt has already dropped it, and its changes are planned again. With
    /// [`Lift::Replan`] it is forgotten, and its restores stay queued for the next commit, as when
    /// a refusal forgets a commit. Nothing else changes.
    pub fn lift(&mut self, lift: Lift) -> Result<(), LiftError> {
        let Some(Halt::Policy { code }) = self.halted else {
            return Err(LiftError::NotHalted);
        };
        if code == error::Code::ConfirmationRequired as i32 {
            return Err(LiftError::NeedsConfirmation);
        }
        self.halted = None;
        if lift == Lift::Replan {
            self.in_flight = None;
        }
        Ok(())
    }

    /// Queues a restore a person asked for. The next commit that is built carries its `Restore`
    /// op, and acknowledging that commit moves the restore to [`Commits::resolved`] with its
    /// outcome. On a read-only device it ends at once as rejected. False, and nothing changes,
    /// when a restore with this ID is queued or has an outcome, so a request can be repeated.
    pub fn restore(&mut self, id: RestoreId, intent: RestoreIntent) -> bool {
        if self.restores.contains_key(&id) || self.resolved.iter().any(|ended| ended.id == id) {
            return false;
        }
        self.restores.insert(id, intent);
        self.reject_unsent_when_read_only();
        true
    }

    /// The restores without an outcome, sent or not, in request order.
    pub fn restores(&self) -> &BTreeMap<RestoreId, RestoreIntent> {
        &self.restores
    }

    /// The queued restores that wait because their node is a problem
    /// ([`RestoreIntent::held_by`]), so a surface can say why they are pending.
    pub fn held_restores(&self) -> impl Iterator<Item = &RestoreId> {
        let held = |(_, intent): &(&RestoreId, &RestoreIntent)| intent.held_by(&self.problems);
        self.restores.iter().filter(held).map(|(id, _)| id)
    }

    /// The last [`RESOLVED_KEPT`] restores that ended, oldest first. An older outcome is dropped.
    pub fn resolved(&self) -> &[Resolved] {
        &self.resolved
    }

    /// The open questions, in the order they were asked. No commit carries an op for a node one of
    /// them holds ([`Question::holds`]) until it is answered.
    pub fn questions(&self) -> &[Question] {
        &self.questions
    }

    /// Matches L against R on the first link of the folder, once R holds the server's tree and L
    /// the first scan's (section 6.4): every local entry R holds at the same path with the same
    /// kind and content is adopted, and the others are held under one question about what was
    /// created on this device. A space with nothing live has nothing to match, and L syncs as it is.
    pub fn first_link(&mut self, trees: &mut Trees, ids: &mut IdGen, now_ms: u64) {
        if trees.remote().iter().all(|(_, node)| node.trashed) {
            return;
        }
        let tops = first_link::adopt(trees);
        if let Some(question) = first_link::ask(trees, tops, None, ids, now_ms) {
            self.questions.push(question);
        }
        self.hold(trees);
    }

    /// Takes the person's answer to the open question `id`, routed by its kind. [`Answer::Dismiss`]
    /// closes a notice about other devices' deletions at once (section 6.1), and the guard never
    /// counts its deletions again. [`Answer::KeepBoth`] closes a conflict at once (section 2.7).
    /// [`Answer::KeepOriginal`] and [`Answer::KeepCopy`] stay on the conflict until a build carries
    /// them out ([`Commits::build`]). Any other answer stays on the question until the next build,
    /// which takes it only if the question still holds what it listed, and asks a new question
    /// otherwise.
    ///
    /// For the mass-delete guard's question (section 6.1), the next build counts what is pending
    /// first. [`Answer::Delete`] then makes the held deletions without counting them again.
    /// [`Answer::Restore`] clears S for every node of the question that is still deleted, so the
    /// effects download them again, and holds any other pending deletion under a new question.
    ///
    /// For a first link's question about entries created on this device (section 6.4),
    /// [`Answer::Publish`] lets the build send them as creates, and the server renames one whose
    /// name R holds. The person chose to keep both files then, so no conflict question is asked
    /// about it. [`Answer::Discard`] sets S to L for every entry, so that, R holding none of
    /// them, the effects move them to the local trash, and then download what R holds at their
    /// paths.
    ///
    /// An S update that would leave S invalid, such as for a folder whose child was moved out and
    /// is not acknowledged yet, waits ([`Trees::waiting`]). False, and nothing changes, when no
    /// open question has the ID, its kind does not take the answer, or it already took one.
    pub fn decide(&mut self, id: &str, answer: Answer) -> bool {
        let Some(index) = (self.questions.iter()).position(|question| {
            question.id == id
                && question.answer.is_none()
                && question.kind.answers().contains(&answer)
                && (matches!(
                    question.kind,
                    QuestionKind::MassDelete { .. }
                        | QuestionKind::RemoteMassDelete { .. }
                        | QuestionKind::Conflict { .. }
                ) || first_link::asks(question))
        }) else {
            return false;
        };
        match self.questions[index].kind {
            QuestionKind::RemoteMassDelete { .. } => {
                self.questions.remove(index);
                self.guard.dismiss_remote();
            }
            QuestionKind::Conflict { .. } if answer == Answer::KeepBoth => {
                self.questions.remove(index);
            }
            _ => self.questions[index].answer = Some(answer),
        }
        true
    }

    /// Counts a removal that applied another device's deletion, as [`Effects::done`] returns it
    /// (section 6.1). When the removals of the window cross a limit of the guard, a notice opens.
    /// While it is open, each removal updates its counts and adds the limits crossed, and none
    /// leaves the window, until the person dismisses it. The rest of a deletion the dismissed
    /// notice counted, such as the files of a folder still being removed, is not counted.
    /// `last_seq` is the seq up to which R holds every change. The trees settle first, so the space
    /// counts the files S holds after the removal.
    ///
    /// [`Effects::done`]: crate::effects::Effects::done
    pub fn count_remote(
        &mut self,
        removal: RemoteRemoval,
        trees: &mut Trees,
        last_seq: u64,
        ids: &mut IdGen,
        now_ms: u64,
    ) {
        trees.settle();
        let open = (self.questions.iter())
            .position(|question| matches!(question.kind, QuestionKind::RemoteMassDelete { .. }));
        let noticed = open.is_some();
        let Some(window) = (self.guard).count_remote(removal, last_seq, noticed, &*trees, now_ms)
        else {
            return;
        };
        let mut exceeded = window.exceeded();
        let kind = |exceeded| QuestionKind::RemoteMassDelete {
            deletions: window.deletions,
            affected_files: window.affected_files,
            space_files: window.space_files,
            exceeded,
        };
        match open {
            Some(index) => {
                let notice = &mut self.questions[index];
                if let QuestionKind::RemoteMassDelete {
                    exceeded: before, ..
                } = &notice.kind
                {
                    exceeded.extend(before);
                    exceeded.sort();
                    exceeded.dedup();
                }
                notice.kind = kind(exceeded);
            }
            None if exceeded.is_empty() => {}
            None => self.questions.push(Question {
                id: ids.uuid_v7(now_ms),
                asked_at_ms: now_ms,
                kind: kind(exceeded),
                held: BTreeMap::new(),
                answer: None,
                shown: BTreeMap::new(),
            }),
        }
    }

    /// Asks again about what a first link holds, as L holds it now (section 6.4), and takes the
    /// answer to the question if it still holds what it listed. A held entry the user deleted
    /// leaves the question, one the user edited or moved stays, and either change replaces the
    /// question with a new one, which drops the answer.
    fn ask_again(&mut self, trees: &mut Trees, ids: &mut IdGen, now_ms: u64) {
        let shows = |question: &Question| first_link::asks(question) && !question.shown.is_empty();
        let Some(index) = self.questions.iter().position(shows) else {
            return;
        };
        let asked = &self.questions[index];
        let held = asked.holds().cloned().collect::<Vec<_>>();
        match first_link::ask(trees, held, Some(asked), ids, now_ms) {
            Some(question) if question.answer.is_none() => self.questions[index] = question,
            Some(question) => {
                self.questions.remove(index);
                if question.answer == Some(Answer::Discard) {
                    trees.discard(question.shown.keys().cloned());
                    let updates = (question.shown.into_iter())
                        .map(|(id, shape)| (id, Some(shape)))
                        .collect();
                    for (id, shape) in update_synced(trees, updates) {
                        trees.wait(id, shape, Origin::Other);
                    }
                } else {
                    self.published.extend(question.shown.into_keys());
                }
            }
            None => {
                self.questions.remove(index);
            }
        }
        self.hold(trees);
    }

    /// Tells the trees which local entries the open questions hold ([`Question::shown`],
    /// [`Trees::set_asked`]). The planner does whenever its questions change; a caller that
    /// restores it from a state calls this.
    pub fn hold(&self, trees: &mut Trees) {
        let shown = self
            .questions
            .iter()
            .flat_map(|question| question.shown.keys());
        trees.set_asked(shown.cloned().collect());
    }

    fn resolve(&mut self, id: RestoreId, outcome: Outcome) {
        let intent = self.restores.remove(&id).expect("a queued restore");
        self.resolved.push(Resolved {
            id,
            intent,
            outcome,
        });
        let excess = self.resolved.len().saturating_sub(RESOLVED_KEPT);
        self.resolved.drain(..excess);
    }

    /// On a read-only device nothing is committed, so a queued restore would wait without an
    /// end. Each ends as rejected, except those the commit in flight carries: that commit may
    /// have been applied, and its result decides them.
    fn reject_unsent_when_read_only(&mut self) {
        if self.halted != Some(Halt::ReadOnly) {
            return;
        }
        let ops = self.in_flight.as_ref().map(|flight| &flight.commit.ops[..]);
        let sent: BTreeSet<RestoreId> = carried(&self.restores, ops.unwrap_or_default())
            .expect("a Restore op in flight is queued")
            .into_values()
            .collect();
        let unsent: Vec<RestoreId> = (self.restores.keys())
            .filter(|id| !sent.contains(*id))
            .cloned()
            .collect();
        for id in unsent {
            self.resolve(id, Outcome::Rejected(Rejection::ReadOnly));
        }
    }

    /// Builds the next commit from local changes, unless one is in flight or committing has
    /// stopped. While committing has stopped it still makes the waiting S updates that fit (see
    /// [`Trees::waiting`]), so the effects plan their nodes again as downloads go on. The caller
    /// builds only when connected and caught up, with `base` the `last_seq` of R and its commit
    /// token, and `history_id` the history R belongs to. A resend keeps the token, so a server that
    /// lost the commit at `base_seq` refuses it (RFC 0013).
    ///
    /// Concurrent changes are rebased onto R first (section 2.4): a file whose content changed
    /// differently on both sides is forked, its local version becoming a new node that is created
    /// under the original name; a node changed remotely keeps the nodes L deleted on its way to the
    /// root, which are downloaded again instead of deleted. Then every local change becomes ops,
    /// ordered on a scratch copy of R (section 2.6): an op is emitted once it is valid there. When
    /// none is, moves that each wait for a name another one frees, as in a swap, go through a
    /// pending name: one node of the cycle moves to a free name first and to its own name last, in
    /// the same commit when the commit holds the whole cycle. Otherwise a create or move that only
    /// waits for a name is sent anyway, for the server to rename, except that a folder created
    /// where a moving folder still is takes a pending name first, and a folder it may be merged
    /// into is not deleted in the same commit. A delete waits
    /// until nothing that survives is left under it, a create or move until its parent exists, and
    /// a move that would close a cycle against R is reverted: the node is marked in
    /// [`Trees::reverted`], and the effects move it back to its R position on disk, bringing back
    /// the folders L deleted on its way. A parked or reverted node waits until the effects have
    /// moved it, and so does a node with a problem or a waiting S update. A commit stops at
    /// [`MAX_OPS`], at [`MAX_COMMIT_BYTES`] of estimated change records, or at half the size of
    /// the last commit that did not fit. The rebase changes the trees even when no commit results,
    /// so the caller plans effects again after building.
    ///
    /// The queued restores come first, in request order, so local changes of other nodes do not
    /// hold them back. A restore follows every op this commit has for its own node: an unsent
    /// edit of a file is sent before the restore of an earlier version and so becomes a version
    /// itself. While such an op is left for a later commit, the restore waits with it. It also
    /// waits while its node is not settled: a node left out of planning (a problem node, a node
    /// whose S update waits and that L did not move away from the waiting position, a parked one),
    /// and a node L and R agree on while S lags ([`Class::Same`]). A folder is not deleted in the
    /// commit that restores a node into it.
    ///
    /// An excluded node ([`Trees::excluded`]) gets no op and is not forked, and nothing is built
    /// while the rules are unknown ([`Trees::rules_known`]). A folder L deleted that still holds
    /// an excluded node in R is not deleted, since that would trash the excluded node on the
    /// server: each topmost deleted node below it with no excluded node in its subtree gets its
    /// own delete, and the folder stays (section 5.10).
    ///
    /// A node an open question holds gets no op. Every other deletion that could be planned now goes
    /// through the mass-delete guard first (section 6.1), and the commit carries the changes it
    /// does not hold.
    pub fn build(
        &mut self,
        trees: &mut Trees,
        ids: &mut IdGen,
        now_ms: u64,
        history_id: &str,
        base: impl Into<Base>,
    ) -> Option<&proto::Commit> {
        let base = base.into();
        if self.in_flight.is_some() {
            return None;
        }
        self.retry_waiting(trees, BTreeSet::new(), Vec::new());
        if self.halted.is_some() || !trees.rules_known() {
            return None;
        }
        self.ask_again(trees, ids, now_ms);
        trees.reclaim();
        let parked = trees.local().iter().filter(|(_, node)| node.parked);
        let waiting = (trees.waiting().keys()).filter(|id| !moved_locally(trees, id));
        let mut skip: BTreeSet<NodeId> = self
            .problems
            .iter()
            .chain(waiting)
            .chain(parked.map(|(id, _)| id))
            .cloned()
            .collect();
        let (needed, agreed) = rebase(trees, ids, now_ms, &skip);
        let excluded: Vec<NodeId> = (trees.dirty().iter())
            .filter(|id| trees.excluded(id) || trees.held(id))
            .cloned()
            .collect();
        skip.extend(excluded);
        let needed = self.retry_waiting(trees, needed, agreed);
        let cap = self.cap.unwrap_or(MAX_OPS);
        let (asking, answered): (Vec<String>, Vec<proto::Op>) = self
            .conflicts(trees, &skip, cap, ids, now_ms)
            .into_iter()
            .unzip();
        skip.extend(
            (answered.iter().filter_map(|op| op.op.as_ref())).map(|op| op_node(op).to_owned()),
        );
        let guarded =
            |question: &Question| matches!(question.kind, QuestionKind::MassDelete { .. });
        let index = self.questions.iter().position(guarded);
        let mut asked = index.map(|index| self.questions.remove(index));
        let open = asked.as_ref().map(|question| question.id.clone());
        skip.extend(self.questions.iter().flat_map(Question::holds).cloned());
        let deleted = (trees.dirty().iter())
            .filter(|id| !skip.contains(*id) && deletes(trees, id, &needed))
            .cloned()
            .collect();
        let pending = guard::deletions(trees, &deleted);
        let checked = (self.guard).check(&mut asked, pending, trees, ids, now_ms);
        skip.extend(checked.held);
        let restored: Vec<NodeId> = (checked.restored.into_values())
            .flat_map(|held| held.members.into_keys())
            .collect();
        skip.extend(restored.iter().cloned());
        let updates = restored.into_iter().map(|id| (id, None)).collect();
        for (id, shape) in update_synced(trees, updates) {
            trees.wait(id, shape, Origin::Other);
        }
        if let Some(question) = asked {
            let kept = index.filter(|_| open.as_ref() == Some(&question.id));
            self.questions
                .insert(kept.unwrap_or(self.questions.len()), question);
        }
        self.hold(trees);
        self.split.retain(|id| trees.synced().live(id).is_some());
        self.published.retain(|id| trees.local().get(id).is_some());
        self.aliases
            .retain(|id, _| trees.local().get(id).is_some() && trees.reverted().contains(id));
        let (planned, deletes, reverts) = plan(
            trees,
            &skip,
            &needed,
            cap,
            &answered,
            &self.split,
            &self.aliases,
            &self.restores,
        );
        for id in reverts {
            trees.revert(id);
        }
        let ops: Vec<proto::Op> = planned.into_iter().chain(answered).collect();
        if ops.is_empty() {
            return None;
        }
        trees.watch_content(ops.iter().filter_map(|op| match &op.op {
            Some(op::Op::CreateFile(create)) => Some(create.node_id.clone()),
            Some(op::Op::UpdateContent(update)) => Some(update.node_id.clone()),
            _ => None,
        }));
        trees.watch_positions(ops.iter().filter_map(|op| match &op.op {
            Some(op::Op::CreateDir(create)) => Some(create.node_id.clone()),
            Some(op::Op::CreateFile(create)) => Some(create.node_id.clone()),
            Some(op::Op::Move(moved)) => Some(moved.node_id.clone()),
            _ => None,
        }));

        let commit_id = ids.uuid_v7(now_ms);
        for question in &mut self.questions {
            if let QuestionKind::Conflict { sent_in, .. } = &mut question.kind
                && asking.contains(&question.id)
            {
                *sent_in = Some(commit_id.clone());
            }
        }
        self.in_flight = Some(InFlight {
            commit: proto::Commit {
                commit_id,
                base_seq: base.seq,
                base_commit_id: base.commit_id,
                base_committed_at_ms: base.committed_at_ms,
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

    /// Goes through the conflict questions (section 2.7) and returns the ops of the answers that
    /// can be carried out now, at most `cap` of them. A conflict ends once R no longer holds either
    /// file live with the meta revision it was asked with, or L moved or deleted a file S holds:
    /// someone renamed, moved or deleted one by hand. It is asked again, under a new ID and
    /// without its answer, once R holds another content revision of either file. An answer waits
    /// until both files are in sync and planned (`skip` lists the nodes that are not), and a
    /// build carries out one op per conflict and none for a node another answer already took.
    /// [`Answer::KeepOriginal`] deletes the copy. [`Answer::KeepCopy`] first gives the original
    /// the copy's content, so the original's own content stays in its version history, and once
    /// that has reached L it deletes the copy. S is left as it is, so the result reaches the disk
    /// as a remote change would, and only a file L still holds as S does is changed or removed.
    fn conflicts(
        &mut self,
        trees: &Trees,
        skip: &BTreeSet<NodeId>,
        cap: usize,
        ids: &mut IdGen,
        now_ms: u64,
    ) -> Vec<(String, proto::Op)> {
        let mut ops = Vec::new();
        let mut kept = Vec::new();
        let mut taken = BTreeSet::new();
        for mut question in std::mem::take(&mut self.questions) {
            let QuestionKind::Conflict { original, copy, .. } = &mut question.kind else {
                kept.push(question);
                continue;
            };
            match drift(trees, original, copy) {
                Drift::Ended => continue,
                Drift::Content => {
                    for file in [&mut *original, &mut *copy] {
                        let remote = trees.remote().get(&file.node_id).expect("live in R");
                        file.revisions.content_rev = remote.content_rev;
                    }
                    question.id = ids.uuid_v7(now_ms);
                    question.asked_at_ms = now_ms;
                    question.answer = None;
                    kept.push(question);
                    continue;
                }
                Drift::None => {}
            }
            let ready = in_sync(trees, original, copy)
                && !skip.contains(&original.node_id)
                && !skip.contains(&copy.node_id);
            let free = !taken.contains(&original.node_id) && !taken.contains(&copy.node_id);
            let content = |file: &ConflictFile| {
                let remote = trees.remote().get(&file.node_id).expect("live in R");
                remote.shape.content.clone()
            };
            let delete_copy = op::Op::Delete(proto::Delete {
                node_id: copy.node_id.clone(),
            });
            let op = match question.answer {
                Some(Answer::KeepOriginal) => Some(delete_copy),
                Some(Answer::KeepCopy) => match (content(original), content(copy)) {
                    (Some(current), Some(copied)) if same_content(&current, &copied) => {
                        Some(delete_copy)
                    }
                    (Some(_), Some(copied))
                        if copied.chunks.len() == copied.chunk_count as usize =>
                    {
                        Some(op::Op::UpdateContent(proto::UpdateContent {
                            node_id: original.node_id.clone(),
                            content: Some(copied),
                        }))
                    }
                    _ => {
                        question.answer = None;
                        None
                    }
                },
                _ => None,
            };
            if let Some(op) = op
                && ready
                && free
                && ops.len() < cap
            {
                taken.insert(original.node_id.clone());
                taken.insert(copy.node_id.clone());
                ops.push((question.id.clone(), proto::Op { op: Some(op) }));
            }
            kept.push(question);
        }
        self.questions = kept;
        ops
    }

    /// Whether a build has something to do for a conflict question: a file of it was renamed,
    /// moved, deleted or changed since it was asked, or its answer can be carried out
    /// ([`Commits::conflicts`]). The caller builds then even when nothing else changed.
    pub fn conflicts_due(&self, trees: &Trees) -> bool {
        self.questions.iter().any(|question| match &question.kind {
            QuestionKind::Conflict { original, copy, .. } => {
                drift(trees, original, copy) != Drift::None
                    || (question.answer.is_some() && in_sync(trees, original, copy))
            }
            _ => false,
        })
    }

    /// The ID of the conflict question whose answer `op` of commit `commit_id` carries out
    /// ([`Commits::conflicts`]): the commit was built with it, and the op is its `Delete` of the copy
    /// or `UpdateContent` of the original. Such an op leaves S as it is when it is acknowledged,
    /// and its node is not marked as a problem. Another op for either file, such as the upload of
    /// an edit, is acknowledged as any op is.
    fn answer_for(&self, commit_id: &str, op: &op::Op) -> Option<String> {
        let question = self.questions.iter().find(|question| {
            let QuestionKind::Conflict {
                original,
                copy,
                sent_in,
            } = &question.kind
            else {
                return false;
            };
            sent_in.as_deref() == Some(commit_id)
                && match op {
                    op::Op::Delete(delete) => delete.node_id == copy.node_id,
                    op::Op::UpdateContent(update) => update.node_id == original.node_id,
                    _ => false,
                }
        });
        question.map(|question| question.id.clone())
    }

    /// Asks about the conflict between the files `original` and `copy` that op `index` of commit
    /// `commit_id` made, unless one of them is not a live file in R or it is already asked. The
    /// question's ID comes from the op, so acknowledging the commit again asks the same question,
    /// and it is asked when the commit was built.
    fn ask_conflict(
        &mut self,
        trees: &Trees,
        commit_id: &str,
        index: usize,
        original: NodeId,
        copy: NodeId,
    ) {
        let file = |id: NodeId| {
            let node = trees.remote().get(&id)?;
            (!node.trashed && node.shape.kind == Kind::File).then_some(ConflictFile {
                node_id: id,
                revisions: Revisions {
                    content_rev: node.content_rev,
                    meta_rev: node.meta_rev,
                },
            })
        };
        let (Some(original), Some(copy)) = (file(original), file(copy)) else {
            return;
        };
        let asked = self.questions.iter().any(|question| {
            matches!(&question.kind, QuestionKind::Conflict { original: o, copy: c, .. }
                if o.node_id == original.node_id && c.node_id == copy.node_id)
        });
        if asked {
            return;
        }
        self.questions.push(Question {
            id: format!("{commit_id}-{index}"),
            asked_at_ms: unix_ms(commit_id).unwrap_or_default(),
            kind: QuestionKind::Conflict {
                original,
                copy,
                sent_in: None,
            },
            held: BTreeMap::new(),
            answer: None,
            shown: BTreeMap::new(),
        });
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
    /// node is reverted so that the disk takes it too. Content S took from R after the commit was
    /// built is newer than what was sent: the server applies a content update only when no other
    /// device changed the content after the commit's `base_seq`, and a created file has no content
    /// before its create, so that change came after the commit. When S's content changed since then ([`Trees::took_content`]), even back to what it
    /// was, S keeps what it holds. A position is newer than the one a create or move sent only when L
    /// and R agreed on it, after the commit was built, at a revision of R after the result's seq,
    /// whether or not S took it: the server applies a move whatever another device did after
    /// `base_seq`, so the move replaced a position from an earlier record, and a position only R
    /// came back to is a remote change. When L and R agreed on a newer position
    /// ([`Trees::position_after`]), S takes that position, and a rename by the server does not
    /// revert the node. S first takes what every op sent, as without an agreement, and then each
    /// agreed position S can hold, with the rest of the shape the ops gave the node. A node whose
    /// agreed position S cannot hold, because another node holds its name or its parent is gone,
    /// keeps what was sent, and is reverted when the server renamed it. A conflict copy is re-keyed in L, and so is
    /// a directory merged into one L does not hold yet. When L already holds the conflict copy,
    /// because a log replay brought it before the result and the effects downloaded it, the local
    /// file keeps its ID and takes what was sent as its S, so the server's content replaces it. When L already holds the directory it was
    /// merged into, the created directory and the nodes created in it are reverted: on disk they
    /// move into the merged directory and the emptied one is removed. The accepted counter takes
    /// the commit's slot. Returns false while there is nothing to acknowledge.
    ///
    /// A create the server refuses with ID_EXISTS while R holds the node live is no problem. Only
    /// another device can have created a node under this device's ID first, as when both send a
    /// file the server lost again (section 4.4). S stays without the node, so the next plan
    /// compares L with what R holds instead of sending the create again: equal content leaves
    /// nothing to send, and other content is kept beside R's as a new node (section 2.4).
    ///
    /// A `Restore` op changes neither S nor L. What it sent is an instruction, and only the server
    /// knows the nodes and the content it brought back, so all of it is a remote change that R
    /// already holds and the effects apply. The restore leaves the queue with its outcome,
    /// whatever the result, and its node is never marked as a problem.
    ///
    /// S stays a valid tree. When the updates together would break it, because S still lags R
    /// elsewhere, each is made as soon as it fits and the rest wait (see [`Trees::waiting`]).
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
        let mut agreed: BTreeMap<NodeId, Agreed> = BTreeMap::new();
        let mut unreverted: BTreeSet<NodeId> = BTreeSet::new();
        let aliases = self.aliases.clone();
        let mut conflicts: Vec<(usize, NodeId, NodeId)> = Vec::new();
        let mut answers = Vec::new();
        for (index, (op, outcome)) in flight.commit.ops.iter().zip(&result.results).enumerate() {
            use op_result::Code;
            let code = Code::try_from(outcome.code).unwrap_or(Code::Unspecified);
            if let Some(sent) = &op.op
                && let Some(id) = self.answer_for(&flight.commit.commit_id, sent)
            {
                answers.push((id, index, sent, code, &outcome.node_id));
                continue;
            }
            let applied = matches!(code, Code::Applied | Code::AppliedRenamed);
            let taken = code == Code::Rejected && outcome.reject_reason == "ID_EXISTS";
            let parent = |sent: &str| merged.get(sent).cloned().unwrap_or_else(|| sent.to_owned());
            let renamed = code == Code::AppliedRenamed && !outcome.name.is_empty();
            let name = |sent: &str| match renamed {
                true => outcome.name.clone(),
                false => sent.to_owned(),
            };
            let sent = match &op.op {
                Some(op::Op::CreateDir(create)) => Some((&create.parent_id, &create.name)),
                Some(op::Op::CreateFile(create)) => Some((&create.parent_id, &create.name)),
                Some(op::Op::Move(moved)) => Some((&moved.new_parent_id, &moved.new_name)),
                _ => None,
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
                        agreed.extend(agree(trees, &create.node_id, result.seq));
                        synced.set(&create.node_id, Some(shape));
                    }
                    let exists = taken && trees.remote().live(&create.node_id).is_some();
                    (
                        &create.node_id,
                        applied || exists || code == Code::MergedInto,
                    )
                }
                Some(op::Op::CreateFile(create)) => {
                    if applied {
                        let shape = Shape {
                            parent: parent(&create.parent_id),
                            name: name(&create.name),
                            kind: Kind::File,
                            content: create.content.clone(),
                        };
                        let shape = created(trees, &synced, &create.node_id, shape);
                        agreed.extend(agree(trees, &create.node_id, result.seq));
                        synced.set(&create.node_id, Some(shape));
                    }
                    let exists = taken && trees.remote().live(&create.node_id).is_some();
                    (&create.node_id, applied || exists)
                }
                Some(op::Op::UpdateContent(update)) => {
                    let id = &update.node_id;
                    let edited = synced.get(trees, id).map(|shape| Shape {
                        content: update.content.clone(),
                        ..shape
                    });
                    let current = trees.synced().live(id).and_then(|s| s.content.as_ref());
                    let downloaded =
                        code == Code::ConflictCopy && trees.local().get(&outcome.node_id).is_some();
                    if applied && trees.took_content(id) {
                        let kept = synced.get(trees, id).map(|shape| Shape {
                            content: current.cloned(),
                            ..shape
                        });
                        synced.set(id, kept);
                    } else if (applied && edited.is_some()) || downloaded {
                        synced.set(id, edited);
                    } else if code == Code::ConflictCopy {
                        synced.set(&outcome.node_id, edited);
                        synced.set(id, None);
                        rekey(trees, id, &outcome.node_id);
                    }
                    if applied {
                        trees.record_edited(id, result.seq);
                    }
                    if code == Code::ConflictCopy {
                        conflicts.push((index, id.clone(), outcome.node_id.clone()));
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
                        agreed.extend(agree(trees, id, result.seq));
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
                            trees.record_trashed(id, result.seq);
                        }
                    }
                    (&delete.node_id, done)
                }
                _ => continue,
            };
            let published = matches!(&op.op, Some(op::Op::CreateDir(_) | op::Op::CreateFile(_)))
                && done
                && self.published.remove(node);
            if renamed
                && !published
                && let Some((sent_parent, sent_name)) = sent
                && let Some(original) = holder(
                    trees,
                    merged.get(sent_parent).unwrap_or(sent_parent),
                    sent_name,
                    node,
                )
            {
                conflicts.push((index, original, node.clone()));
            }
            let follows = trees.local().get(node).is_some_and(|local| {
                let held = &local.shape.parent;
                aliases.contains_key(held) || merged.contains_key(held)
            });
            let left = sent.is_some_and(|(parent, name)| {
                (trees.local().get(node)).is_some_and(|local| {
                    local.shape.parent != *parent || nfc(&local.shape.name) != nfc(name)
                })
            });
            let kept = agreed.contains_key(node);
            if (renamed && !left && !kept) || (done && follows) {
                trees.revert(node.clone());
            } else if renamed && !left {
                unreverted.insert(node.clone());
            }
            if !done {
                self.problems.insert(node.clone());
            }
        }
        let mut rest = synced.apply(trees);
        let mut positions = Synced::default();
        for (id, at) in &agreed {
            let held = match rest.iter().find(|(other, _)| other == id) {
                Some((_, shape)) => shape.clone(),
                None => trees.synced().get(id).cloned(),
            };
            if let Some(shape) = held {
                let shape = Shape {
                    parent: at.parent.clone(),
                    name: at.name.clone(),
                    ..shape
                };
                positions.set(id, Some(shape));
            }
        }
        let tried: BTreeSet<NodeId> = positions.ids().collect();
        let refused: BTreeSet<NodeId> = (positions.apply(trees).into_iter())
            .map(|(id, _)| id)
            .collect();
        let taken: BTreeSet<&NodeId> = tried.difference(&refused).collect();
        rest.retain(|(id, _)| !taken.contains(id));
        for id in unreverted.iter().filter(|id| !taken.contains(id)) {
            trees.revert(id.clone());
        }
        for (id, shape) in rest {
            trees.wait(id, shape, Origin::Commit(flight.commit.device_seq));
        }
        let mut closed = BTreeSet::new();
        for (id, index, sent, code, made) in answers {
            use op_result::Code;
            let question = self.questions.iter_mut().find(|question| question.id == id);
            let Some(question) = question else {
                continue;
            };
            let QuestionKind::Conflict {
                original, sent_in, ..
            } = &mut question.kind
            else {
                continue;
            };
            *sent_in = None;
            match (sent, code) {
                (op::Op::Delete(_), Code::Applied) => {
                    closed.insert(id);
                }
                (op::Op::UpdateContent(update), Code::Applied) => {
                    let remote = trees.remote().get(&original.node_id);
                    let took = remote.filter(|node| {
                        (node.shape.content.as_ref().zip(update.content.as_ref()))
                            .is_some_and(|(now, sent)| same_content(now, sent))
                    });
                    if let Some(node) = took {
                        original.revisions.content_rev = node.content_rev;
                    }
                }
                (op::Op::UpdateContent(_), Code::ConflictCopy) => {
                    question.answer = None;
                    conflicts.push((index, original.node_id.clone(), made.clone()));
                }
                _ => question.answer = None,
            }
        }
        self.questions
            .retain(|question| !closed.contains(&question.id));
        for (index, original, copy) in conflicts {
            self.ask_conflict(trees, &flight.commit.commit_id, index, original, copy);
        }
        let restored = carried(&self.restores, &flight.commit.ops);
        for (index, id) in restored.expect("a Restore op in flight is queued") {
            self.resolve(id, Outcome::from_result(&result.results[index]));
        }
        trees.watch_content([]);
        trees.watch_positions([]);
        self.retry_waiting(trees, BTreeSet::new(), Vec::new());
        self.accepted = flight.commit.device_seq;
        self.slots.insert(flight.commit.device_seq, result.seq);
        while self.slots.len() > SLOTS_KEPT {
            self.slots.pop_first();
        }
        self.cap = self.cap.map(|cap| cap * 2).filter(|cap| *cap < MAX_OPS);
        true
    }

    /// Makes the waiting S updates that fit now, together with S := ⊥ for the `needed` nodes the
    /// keep rule could not clear yet and the `agreed` updates of the rebase, which may depend on
    /// each other. Returns the needed nodes still not cleared; agreed updates that do not fit are
    /// dropped, since the next rebase finds them again, except that one for a node whose update
    /// waited waits in its place, since the rebase took it from the waiting shape. A needed node whose own update waited
    /// keeps that update waiting, so it is made once it fits, also after the keep rule no longer
    /// needs the node. Each update that waits again keeps its origin.
    fn retry_waiting(
        &mut self,
        trees: &mut Trees,
        needed: BTreeSet<NodeId>,
        agreed: Vec<(NodeId, Option<Shape>)>,
    ) -> BTreeSet<NodeId> {
        let mut waited: BTreeMap<NodeId, Waiting> = (trees.take_waiting().into_iter())
            .filter_map(|(id, Waiting { shape, origin })| {
                let shape = outstanding(trees, &id, shape)?;
                Some((id, Waiting { shape, origin }))
            })
            .collect();
        let updates = (waited.iter())
            .map(|(id, waiting)| (id.clone(), waiting.shape.clone()))
            .chain(needed.iter().map(|id| (id.clone(), None)))
            .chain(agreed.iter().cloned())
            .collect();
        let mut still = BTreeSet::new();
        for (id, shape) in update_synced(trees, updates) {
            if needed.contains(&id) {
                if let Some(waiting) = waited.remove(&id) {
                    trees.wait(id.clone(), waiting.shape, waiting.origin);
                }
                still.insert(id);
            } else if let Some(waiting) = waited.get(&id) {
                trees.wait(id, shape, waiting.origin);
            } else if !agreed.iter().any(|(other, _)| *other == id) {
                trees.wait(id, shape, Origin::Other);
            }
        }
        still
    }

    /// What a call-level error on the commit in flight means. An error that proves nothing was
    /// applied forgets the commit; one that leaves the outcome unknown keeps it to resend; a
    /// history mismatch or a stale device state keeps it as evidence and stops committing; and
    /// every other code, known or not, is a policy halt ([`Halt::Policy`], RFC 0006 section 1).
    /// `QUOTA_EXCEEDED` is resent only when it names a retry time, after that wait.
    ///
    /// The restores of a forgotten commit stay queued and go into the next commit, except on a
    /// device that is now read-only, where they end as rejected. A halt keeps them queued.
    pub fn failed(&mut self, error: &proto::Error) -> Result<Failure, CommitError> {
        use error::Code;
        if self.in_flight.is_none() {
            return Err(CommitError::NotInFlight);
        }
        let failure = match Code::try_from(error.code) {
            Ok(Code::HistoryMismatch | Code::StaleDeviceState) => Failure::Halted(Halt::Recovery),
            Ok(Code::ReadOnly) => Failure::Halted(Halt::ReadOnly),
            Ok(Code::AheadOfServer) => Failure::Resync,
            Ok(Code::LimitExceeded) => Failure::Split,
            Ok(Code::HeadMoved | Code::UnsupportedVersion) => Failure::Abandoned,
            Ok(Code::QuotaExceeded) if error.retry_after_ms > 0 => Failure::Resend,
            Ok(Code::MissingChunks | Code::RetryLater | Code::Unauthenticated | Code::Internal) => {
                Failure::Resend
            }
            _ => Failure::Halted(Halt::Policy { code: error.code }),
        };
        match failure {
            Failure::Halted(halt @ (Halt::Recovery | Halt::Policy { .. })) => {
                self.halted = Some(halt)
            }
            Failure::Resend => {}
            Failure::Halted(halt) => {
                self.halted = Some(halt);
                self.in_flight = None;
            }
            Failure::Abandoned | Failure::Resync => self.in_flight = None,
            Failure::Split => {
                let flight = self.in_flight.take().expect("checked");
                let ops = flight.commit.ops;
                let restored = carried(&self.restores, &ops);
                let restore = restored
                    .expect("a Restore op in flight is queued")
                    .remove(&0);
                if ops.len() > 1 {
                    self.cap = Some(ops.len() / 2);
                } else if let Some(id) = restore {
                    self.resolve(id, Outcome::DoesNotFit);
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
        self.reject_unsent_when_read_only();
        Ok(failure)
    }

    /// Forgets the commit in flight when the driver knows it was not applied: after
    /// `MISSING_CHUNKS` when the file changed since it was chunked (section 4.2). False when no
    /// commit is in flight, its result has arrived, or a policy halt keeps it.
    pub fn abandon(&mut self) -> bool {
        if matches!(self.halted, Some(Halt::Policy { .. })) {
            return false;
        }
        self.in_flight
            .take_if(|flight| flight.result.is_none())
            .is_some()
    }

    /// Compares the `Welcome` with the commit in flight and the accepted counter *a* (section
    /// 2.8). With *w* the server's counter, *w* = *a* means the commit in flight was not accepted
    /// and *w* = *a* + 1 that it was and its response was lost; both resend it unchanged, and
    /// the server answers an accepted one with its stored result. *w* < *a* means the server lost
    /// commits it had accepted, since its counter only grows and the store learns a slot only from
    /// its result. So does *w* = *a* while the result of the commit in flight is held, since the
    /// server sent that result. Either way *a* takes *w*, and the commit in flight is dropped, its
    /// restores staying queued, so that its changes are planned again once R is listed again. Any other *w*, or a commit
    /// built in another history, starts recovery. A read-only device resends nothing. A device
    /// under a policy halt resends nothing whatever the `Welcome` says, but *w* < *a* is a loss
    /// there too: *a* takes *w*, the refused commit is dropped, since its slot is no longer the one
    /// after the server's count, and R is listed again, while the halt stays. The slots above *w*
    /// are forgotten with every loss, so that [`Commits::held_through`] does not count commits the
    /// server lost, and a later `Welcome` finds *w* = *a*, so a loss is handled once. Whether R
    /// itself belongs to the `Welcome`'s history is checked where R is kept.
    ///
    /// Recovery keeps the queued restores with the commit in flight: nothing is built under that
    /// halt, so none is sent again or planned again, and each stays as the request it was
    /// (section 6.4). On a read-only device the restores no commit in flight carries end as
    /// rejected.
    pub fn connected(&mut self, welcome: &proto::Welcome) -> Reconnect {
        let (a, w) = (self.accepted, welcome.device_seq);
        let policy = matches!(self.halted, Some(Halt::Policy { .. }));
        let reconnect = match &self.in_flight {
            _ if policy && w >= a => return Reconnect::Idle,
            _ if policy => Reconnect::AheadOfServer,
            _ if self.halted == Some(Halt::Recovery) => Reconnect::Recovery,
            Some(flight) if flight.commit.history_id != welcome.history_id => Reconnect::Recovery,
            None if w == a => Reconnect::Idle,
            Some(flight) if flight.result.is_none() && (w == a || w == a + 1) => Reconnect::Resend,
            Some(_) if w == a + 1 => Reconnect::Idle,
            Some(_) if w == a => Reconnect::AheadOfServer,
            _ if w < a => Reconnect::AheadOfServer,
            _ => Reconnect::Recovery,
        };
        if reconnect == Reconnect::Recovery {
            self.halted = Some(Halt::Recovery);
            return reconnect;
        }
        if reconnect == Reconnect::AheadOfServer {
            self.accepted = w;
            self.in_flight = None;
            self.slots.split_off(&(w + 1));
        }
        if policy {
            return reconnect;
        }
        if welcome.read_only {
            self.halted = Some(Halt::ReadOnly);
            self.reject_unsent_when_read_only();
            return match reconnect {
                Reconnect::Resend => Reconnect::Idle,
                other => other,
            };
        }
        if self.halted == Some(Halt::ReadOnly) {
            self.halted = None;
        }
        reconnect
    }
}

/// The queued restore each `Restore` op of a commit carries, by the op's index: the first in
/// request order whose op equals it and that no earlier op took. Queued restores with equal ops
/// are interchangeable, so it makes no difference which of them an op is counted for. Err with
/// the index of a `Restore` op that no queued restore is left for.
fn carried(
    restores: &BTreeMap<RestoreId, RestoreIntent>,
    ops: &[proto::Op],
) -> Result<BTreeMap<usize, RestoreId>, usize> {
    let mut queued: HashMap<&proto::Restore, VecDeque<&RestoreId>> = HashMap::new();
    for (id, intent) in restores {
        queued.entry(intent.op()).or_default().push_back(id);
    }
    let mut found = BTreeMap::new();
    for (index, op) in ops.iter().enumerate() {
        if let Some(op::Op::Restore(restore)) = &op.op {
            let id = queued.get_mut(restore).and_then(VecDeque::pop_front);
            found.insert(index, id.ok_or(index)?.clone());
        }
    }
    Ok(found)
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

    /// S for `id` with the updates so far. A waiting update stands in for S.
    fn get(&self, trees: &Trees, id: &str) -> Option<Shape> {
        match self.updates.iter().rev().find(|(other, _)| other == id) {
            Some((_, shape)) => shape.clone(),
            None => acknowledged(trees, id).cloned(),
        }
    }

    /// Makes the updates that leave S valid and returns the rest (see [`update_synced`]).
    fn ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.updates.iter().map(|(id, _)| id.clone())
    }

    fn apply(self, trees: &mut Trees) -> Vec<(NodeId, Option<Shape>)> {
        update_synced(trees, self.updates)
    }
}

/// Whether planning deletes the node: L deleted it, S holds it, and the keep rule does not need it.
fn deletes(trees: &Trees, id: &str, needed: &BTreeSet<NodeId>) -> bool {
    trees.local().get(id).is_none()
        && trees.synced().live(id).is_some()
        && trees.class(id) == Class::Local
        && !needed.contains(id)
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

/// What changed about the files of a conflict since it was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Drift {
    None,
    /// R holds another content revision of a file.
    Content,
    /// A file was renamed, moved or deleted: R no longer holds it live with the meta revision it
    /// was asked with, or L moved or deleted it while S holds it.
    Ended,
}

fn drift(trees: &Trees, original: &ConflictFile, copy: &ConflictFile) -> Drift {
    let files = [original, copy];
    let moved_here = |id: &str| {
        (trees.synced().live(id)).is_some_and(|synced| {
            (trees.local().get(id)).is_none_or(|local| !local.shape.same_position(synced))
        })
    };
    let ended = files.iter().any(|file| {
        let remote = trees
            .remote()
            .get(&file.node_id)
            .filter(|node| !node.trashed);
        moved_here(&file.node_id)
            || remote.is_none_or(|node| node.meta_rev != file.revisions.meta_rev)
    });
    let changed = files.iter().any(|file| {
        let remote = trees.remote().get(&file.node_id);
        remote.is_some_and(|node| node.content_rev != file.revisions.content_rev)
    });
    match (ended, changed) {
        (true, _) => Drift::Ended,
        (false, true) => Drift::Content,
        (false, false) => Drift::None,
    }
}

/// Whether L, S and R agree on both files of a conflict.
fn in_sync(trees: &Trees, original: &ConflictFile, copy: &ConflictFile) -> bool {
    [original, copy]
        .iter()
        .all(|file| trees.class(&file.node_id) == Class::InSync)
}

/// The node other than `other` that R holds live under `parent` with the name key of `name`.
fn holder(trees: &Trees, parent: &str, name: &str, other: &str) -> Option<NodeId> {
    let key = name_key(name);
    (trees.remote().iter())
        .find(|(id, node)| {
            !node.trashed
                && *id != other
                && node.shape.parent == parent
                && name_key(&node.shape.name) == key
        })
        .map(|(id, _)| id.clone())
}

/// Gives the local node `from` the ID `to`; its children follow, and so does a folder's unknown
/// rules ([`Trees::set_rules_unknown`]).
pub(crate) fn rekey(trees: &mut Trees, from: &str, to: &str) {
    let Some(node) = trees.local().get(from).cloned() else {
        return;
    };
    let rules_unknown = trees.unknown_rules().contains(from);
    let children: Vec<NodeId> = trees
        .local()
        .iter()
        .filter(|(_, child)| child.shape.parent == from)
        .map(|(id, _)| id.clone())
        .collect();
    let parked_name = trees.parked_name(from).to_owned();
    let parked = node.parked;
    trees.remove_local(from);
    trees.set_local(to.to_owned(), node);
    if parked {
        trees.park_as(to.to_owned(), parked_name);
    }
    if rules_unknown {
        trees.set_rules_unknown(to);
    }
    for id in children {
        let mut child = trees.local().get(&id).cloned().expect("listed above");
        child.shape.parent = to.to_owned();
        trees.set_local(id, child);
    }
}

/// The rebase of section 2.4, before ops are planned. A reverted file that R no longer has but that
/// holds an edit not uploaded yet becomes a new node, so the edit is uploaded as a create instead
/// of being removed with the node. Nodes in `skip` are not rebased otherwise, though what they changed
/// remotely still keeps their ancestors. An excluded node is not rebased and keeps nothing; S
/// still takes what both sides agree on for it. Returns the nodes the keep rule could
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
        let held = trees.reverted().contains(id) || trees.waiting().contains_key(id);
        if held
            && trees.remote().live(id).is_none()
            && trees.edited(id)
            && !trees.excluded(id)
            && !trees.held(id)
        {
            rekey(trees, id, &ids.uuid_v7(now_ms));
        }
    }
    for id in dirty.iter().filter(|id| !skip.contains(*id)) {
        if acknowledged_class(trees, id) != Class::Concurrent
            || !edited_on_both_sides(trees, id)
            || trees.excluded(id)
        {
            continue;
        }
        let local = trees.local().get(id).cloned().expect("a local edit");
        trees.remove_local(id);
        trees.set_local(ids.uuid_v7(now_ms), local);
    }
    let mut needed = BTreeSet::new();
    let mut agreed = Vec::new();
    for id in &dirty {
        if !trees.excluded(id) {
            keep(trees, id, &mut needed);
        }
        if skip.contains(id) {
            continue;
        }
        match acknowledged_class(trees, id) {
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
        acknowledged(trees, id),
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
    let s = content(acknowledged(trees, id));
    let changed = |c: &Content| s.as_ref().is_none_or(|s| !same_content(c, s));
    changed(&l) && changed(&r) && !same_content(&l, &r)
}

/// The keep rule (R7): a node that changed remotely, or that L still has but must move to its R
/// position because it is reverted or parked, keeps every node L deleted on the way it will hang
/// from the root, by setting S := ⊥ for them, so they are downloaded again. A kept node that S
/// cannot drop yet, because S still has children under it, is reverted instead, so the effects
/// bring it back at its R position, and it goes into `needed` so it is not deleted. A reverted node
/// L deleted is an ordinary local delete. While a node's S update waits, its change is compared
/// with the waiting shape, and S := ⊥ drops the waiting update, which would otherwise hold a kept
/// node out of planning for good.
fn keep(trees: &mut Trees, id: &str, needed: &mut BTreeSet<NodeId>) {
    let reverted = trees
        .local()
        .get(id)
        .is_some_and(|node| node.parked || trees.reverted().contains(id));
    let acknowledged = acknowledged(trees, id);
    if trees.remote().live(id).is_none()
        || (!reverted && same(trees.remote().live(id), acknowledged))
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
            None if trees.synced().live(&current).is_some()
                || trees.waiting().contains_key(&current) =>
            {
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

/// The shape a create sent, with the content S holds when it changed since the create was built
/// ([`Trees::took_content`]).
fn created(trees: &Trees, synced: &Synced, id: &str, sent: Shape) -> Shape {
    match synced.get(trees, id) {
        Some(held) if trees.took_content(id) => Shape {
            content: held.content,
            ..sent
        },
        _ => sent,
    }
}

/// The position of `id` that L and R agreed on after `seq` ([`Trees::position_after`]), if any.
fn agree(trees: &Trees, id: &str, seq: u64) -> Option<(NodeId, Agreed)> {
    (trees.position_after(id, seq)).map(|at| (id.to_owned(), at.clone()))
}

/// What S holds for `id`, or its waiting update while one waits.
fn acknowledged<'a>(trees: &'a Trees, id: &str) -> Option<&'a Shape> {
    match trees.waiting().get(id) {
        Some(waiting) => waiting.shape.as_ref(),
        None => trees.synced().live(id),
    }
}

/// The node's class with its waiting update, while one waits, standing in for S.
fn acknowledged_class(trees: &Trees, id: &str) -> Class {
    let (local, remote) = (trees.local().live(id), trees.remote().live(id));
    classify(local, acknowledged(trees, id), remote)
}

/// Whether L moved a node whose S update waits away from the waiting position, where R still
/// holds it. Such a node is planned and rebased against the waiting shape, since the update may
/// never fit while L holds the node elsewhere, as when a folder R trashed takes its name key in S
/// and L cannot remove that folder while the node is inside it.
fn moved_locally(trees: &Trees, id: &str) -> bool {
    let (
        Some(local),
        Some(Waiting {
            shape: Some(waiting),
            ..
        }),
    ) = (trees.local().live(id), trees.waiting().get(id))
    else {
        return false;
    };
    let held = |shape: &Shape| shape.same_position(waiting);
    !held(local) && trees.remote().live(id).is_some_and(held)
}

/// What is left of a waiting S update. Its position is stale once R no longer has the node there
/// and L has left it too: it then falls back to the position S holds, or to R's when S does not
/// hold the node, so L's and R's positions are compared with the S they came from, as for any
/// change both sides made. Its content stays, so the acknowledged content remains the base L's
/// content is compared with. None when nothing is left: the update comes to what S holds, or
/// neither L nor R holds the node. A removal of a node S still holds stays, even then and while R
/// holds the node, only as long as what S holds under it leaves S with L's next commits
/// ([`leaves_s`]), so that R taking the node back meanwhile does not read as L removing it. Once
/// it cannot leave, the removal could never be made, and the keep rule brings the node back.
fn outstanding(trees: &Trees, id: &str, waiting: Option<Shape>) -> Option<Option<Shape>> {
    let local = trees.local().get(id).map(|node| &node.shape);
    let synced = trees.synced().live(id);
    let Some(remote) = trees.remote().live(id) else {
        let removal = waiting.is_none() && synced.is_some() && leaves_s(trees, id);
        return (local.is_some() || removal).then_some(waiting);
    };
    let Some(mut left) = waiting else {
        return leaves_s(trees, id).then_some(None);
    };
    if !left.same_position(remote) && !local.is_some_and(|local| local.same_position(&left)) {
        let base = synced.unwrap_or(remote);
        left.parent = base.parent.clone();
        left.name = base.name.clone();
    }
    (synced != Some(&left)).then_some(Some(left))
}

/// Whether everything S holds under `id` leaves S with L's commits: each node L holds there is
/// elsewhere in L, and each one L does not hold leaves with what S holds under it in turn. A node
/// L still holds where S has it, such as one parked under a folder the disk removed, keeps `id`
/// in S, and so does a reverted node R holds there, since no commit sends its local position and
/// the effects move it back.
fn leaves_s(trees: &Trees, id: &str) -> bool {
    let stays = |child: &str, parent: &str| {
        trees.reverted().contains(child)
            && (trees.remote().live(child)).is_some_and(|remote| remote.parent == parent)
    };
    let mut below = vec![id];
    while let Some(parent) = below.pop() {
        for child in trees.synced_children(parent) {
            match trees.local().get(child) {
                Some(node) if node.shape.parent == *parent || stays(child, parent) => return false,
                Some(_) => {}
                None => below.push(child),
            }
        }
    }
    true
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
        Want::Restore { cost, .. } => *cost,
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
    /// A restore a person asked for, with the bytes of change records it makes, roughly: for a
    /// node R holds as trashed, a record for every node trashed with it, and otherwise one.
    Restore {
        op: proto::Restore,
        cost: usize,
    },
}

impl Want {
    /// The nodes a want other than a restore changes. A restore of one of them waits for it.
    fn nodes(&self) -> impl Iterator<Item = &NodeId> {
        let (one, many) = match self {
            Want::Create { id, .. } | Want::Move { id, .. } | Want::Content { id, .. } => {
                (Some(id), None)
            }
            Want::Delete { members, .. } => (None, Some(members)),
            Want::Restore { .. } => (None, None),
        };
        one.into_iter().chain(many.into_iter().flatten())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fit {
    Valid,
    NameTaken,
    Cycle,
    Wait,
}

type Planned = (Vec<proto::Op>, BTreeMap<NodeId, Vec<NodeId>>, Vec<NodeId>);

/// The ops for every queued restore and every local change, in an order valid on a scratch copy
/// of R, the nodes each delete covers, and the nodes to revert: moves that close a cycle, and
/// nodes in a merged folder that S or R already has where the folder it was merged into holds
/// them, so they follow R. The ops `answered` already holds for the commit count toward its limits.
#[allow(clippy::too_many_arguments)]
fn plan(
    trees: &Trees,
    problems: &BTreeSet<NodeId>,
    needed: &BTreeSet<NodeId>,
    cap: usize,
    answered: &[proto::Op],
    split: &BTreeSet<NodeId>,
    aliases: &BTreeMap<NodeId, NodeId>,
    restores: &BTreeMap<RestoreId, RestoreIntent>,
) -> Planned {
    let mut wants = Vec::new();
    if !restores.is_empty() {
        let mut groups: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, node) in trees.remote().iter().filter(|(_, node)| node.trashed) {
            *groups.entry(&node.trash_group).or_default() += record_cost(Some(&node.shape));
        }
        for intent in restores.values() {
            let cost = match trees.remote().get(intent.node()) {
                Some(node) if node.trashed && intent.version_id().is_none() => {
                    groups[node.trash_group.as_str()]
                }
                node => record_cost(node.map(|node| &node.shape)),
            };
            wants.push(Want::Restore {
                op: intent.op().clone(),
                cost,
            });
        }
    }
    let mut deleted = BTreeSet::new();
    let mut reverts = Vec::new();
    for id in trees.dirty().iter().filter(|id| !problems.contains(*id)) {
        let class = acknowledged_class(trees, id);
        if !matches!(class, Class::Local | Class::Concurrent) {
            continue;
        }
        let synced = acknowledged(trees, id);
        let Some(local) = trees.local().get(id).map(|node| &node.shape) else {
            if deletes(trees, id, needed) {
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
    let mut scratch = Scratch::new(trees);
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
        let kept = (scratch.subtree(id).iter()).any(|node| trees.excluded(node));
        match children.get(id.as_str()) {
            Some(kids) if kept || bytes > MAX_COMMIT_BYTES || split.contains(id) => {
                tops.extend(kids)
            }
            _ if kept => {}
            _ => wants.push(Want::Delete {
                id: id.clone(),
                members,
            }),
        }
    }

    let restored: BTreeSet<&str> = restores.values().map(RestoreIntent::node).collect();
    for node in wants.iter().flat_map(Want::nodes) {
        if restored.contains(node.as_str()) {
            *scratch.unsent.entry(node.clone()).or_default() += 1;
        }
    }
    let unsettled = |node: &&str| problems.contains(*node) || trees.class(node) == Class::Same;
    for node in restored.into_iter().filter(unsettled) {
        *scratch.unsent.entry(node.to_owned()).or_default() += 1;
    }
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
        for node in want.nodes() {
            if let Some(left) = scratch.unsent.get_mut(node) {
                *left -= 1;
                if *left == 0 {
                    scratch.unsent.remove(node);
                }
            }
        }
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
            Want::Restore { op, .. } => {
                let trashed = (trees.remote().get(&op.node_id))
                    .filter(|node| node.trashed && op.version_id.is_empty());
                match trashed {
                    Some(node) => {
                        let position = &node.shape;
                        let parent = Some(&op.target_parent_id).filter(|id| !id.is_empty());
                        let parent = parent.unwrap_or(&position.parent);
                        let name = Some(&op.name).filter(|name| !name.is_empty());
                        let name = name.unwrap_or(&position.name);
                        let fit = scratch.fit_at(&op.node_id, parent, name);
                        if matches!(fit, Fit::Valid | Fit::NameTaken) {
                            scratch.place(&op.node_id, parent, name, node.shape.kind);
                        }
                    }
                    None => scratch.revive(&op.node_id),
                }
                op::Op::Restore(op)
            }
        };
        ops.push(proto::Op { op: Some(op) });
    };
    let mut emitted = answered.len();
    let mut bytes = (answered.iter().filter_map(|op| op.op.as_ref()))
        .map(|op| match op {
            op::Op::UpdateContent(update) => {
                RECORD_BYTES + update.content.as_ref().map_or(0, |c| c.encoded_len())
            }
            op => record_cost(trees.synced().live(op_node(op))),
        })
        .sum();
    let fits = |emitted: usize, bytes: usize, ops: usize, cost: usize| {
        emitted + ops <= cap && (emitted == 0 || bytes + cost <= MAX_COMMIT_BYTES)
    };
    let room = |emitted: &mut usize, bytes: &mut usize, want: &Want| {
        let cost = cost(trees, want);
        if !fits(*emitted, *bytes, 1, cost) {
            return false;
        }
        *emitted += 1;
        *bytes += cost;
        true
    };
    let capped = loop {
        let (mut progress, mut full) = (false, false);
        for slot in wants.iter_mut() {
            if slot
                .as_ref()
                .is_some_and(|want| scratch.fit(want, &moving) == Fit::Valid)
            {
                if !room(&mut emitted, &mut bytes, slot.as_ref().expect("checked")) {
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
        if let Some(cycle) = name_cycle(&wants, &scratch, &moving) {
            let Some(Want::Move { id, parent, name }) = &wants[cycle[0]] else {
                unreachable!("a name cycle holds moves");
            };
            let aside = Want::Move {
                id: id.clone(),
                parent: parent.clone(),
                name: scratch.free_name(parent, name),
            };
            let all: usize = (cycle.iter())
                .filter_map(|at| wants[*at].as_ref())
                .map(|want| cost(trees, want))
                .sum();
            let whole = fits(emitted, bytes, cycle.len() + 1, all + cost(trees, &aside));
            if (emitted > 0 && !whole) || !room(&mut emitted, &mut bytes, &aside) {
                break true;
            }
            let id = id.clone();
            let unsent = scratch.unsent.get(&id).copied();
            emit(aside, &mut scratch, &mut moving);
            moving.insert(id.clone());
            if let Some(unsent) = unsent {
                scratch.unsent.insert(id, unsent);
            }
            continue;
        }
        let forced = wants.iter_mut().find(|slot| {
            slot.as_ref()
                .is_some_and(|want| scratch.fit(want, &moving) == Fit::NameTaken)
        });
        let Some(slot) = forced else {
            break false;
        };
        if !room(&mut emitted, &mut bytes, slot.as_ref().expect("checked")) {
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

/// A cycle of moves that each wait only for a name held by the node of the next one, as in a swap
/// or a rotation through a third name, by their indices in `wants`.
fn name_cycle(
    wants: &[Option<Want>],
    scratch: &Scratch,
    moving: &BTreeSet<NodeId>,
) -> Option<Vec<usize>> {
    let blocked: BTreeMap<&str, usize> = (wants.iter().enumerate())
        .filter_map(|(at, slot)| match slot {
            Some(want @ Want::Move { id, .. }) if scratch.fit(want, moving) == Fit::NameTaken => {
                Some((id.as_str(), at))
            }
            _ => None,
        })
        .collect();
    for &start in blocked.keys() {
        let mut cycle = Vec::new();
        let mut node = start;
        while let Some(&at) = blocked.get(node) {
            if cycle.contains(&at) {
                break;
            }
            cycle.push(at);
            let Some(Want::Move { id, parent, name }) = &wants[at] else {
                unreachable!("indexed from the moves");
            };
            let holders: Vec<&NodeId> = (scratch.children.get(parent))
                .and_then(|keys| keys.get(&name_key(name)))
                .into_iter()
                .flatten()
                .filter(|holder| *holder != id)
                .collect();
            let [holder] = holders.as_slice() else {
                break;
            };
            if holder.as_str() == start {
                return Some(cycle);
            }
            node = holder;
        }
    }
    None
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
    /// For each node a queued restore names, how many wants that change it are not emitted yet,
    /// and one more while the node is not settled: this build leaves it out, so its changes are
    /// not among the wants, or S has not taken what L and R agree on. The restore waits for all
    /// of them, so that it is applied to what this device sent and S shows what the device
    /// holds.
    unsent: BTreeMap<NodeId, usize>,
}

impl Scratch {
    fn new(trees: &Trees) -> Self {
        let mut scratch = Self {
            root: trees.remote().root.clone(),
            nodes: BTreeMap::new(),
            children: BTreeMap::new(),
            merged: BTreeSet::new(),
            unsent: BTreeMap::new(),
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
            Want::Restore { op, .. } if self.unsent.contains_key(&op.node_id) => Fit::Wait,
            Want::Restore { .. } => Fit::Valid,
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
