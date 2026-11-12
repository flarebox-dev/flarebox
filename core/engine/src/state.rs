// SPDX-License-Identifier: Apache-2.0

//! The engine state the store keeps between runs (docs/design/sync-engine.md section 2.8), as
//! plain values. The nodes of R, L and S are not here: the store keeps them per node and loads
//! them with [`Trees::set_remote`], [`Trees::set_local`] and [`Trees::set_synced`]. Everything
//! else the engine needs to continue where it stopped is in [`TreesState`], [`CommitsState`] and
//! [`EffectsState`]. Restoring checks the invariants the engine relies on and refuses a state
//! that breaks one, with a [`StateError`] that names it.
//!
//! [`Trees::set_remote`]: crate::classify::Trees::set_remote
//! [`Trees::set_local`]: crate::classify::Trees::set_local
//! [`Trees::set_synced`]: crate::classify::Trees::set_synced

use std::collections::{BTreeMap, BTreeSet};

use crate::classify::Waiting;
use crate::commit::{Halt, InFlight};
use crate::effects::{Effect, EffectRecord};
use crate::guard::GuardState;
use crate::question::Question;
use crate::restore::{Resolved, RestoreId, RestoreIntent};
use crate::tree::{FileIdentity, NodeId, Shape, TreeError};

/// A position L and R agreed on, with the revision of R's record that held it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agreed {
    pub rev: u64,
    pub parent: NodeId,
    pub name: String,
}

/// A position a create or a move of this device set: the seq of its commit, and whether the store
/// upgrade recorded it, without the positions R held before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovedHere {
    pub seq: u64,
    pub upgraded: bool,
}

/// What [`Trees`](crate::classify::Trees) holds besides the nodes of R, L and S.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreesState {
    /// Nodes whose entries changed since they were last found in sync.
    pub dirty: BTreeSet<NodeId>,
    /// Dirty nodes the last settle left without searching. Each is also in `dirty`.
    pub unsearched: BTreeSet<NodeId>,
    /// Nodes whose local position the server will not take.
    pub reverted: BTreeSet<NodeId>,
    /// For a parked node re-keyed while parked, the name of its entry in the parked directory.
    /// Each key is a node L holds as parked.
    pub parked_as: BTreeMap<NodeId, NodeId>,
    /// S updates of acknowledged ops that wait until S stays a valid tree with them, by node.
    /// `None` removes the node from S.
    pub waiting: BTreeMap<NodeId, Waiting>,
    /// The nodes whose content the commit in flight creates or updates, each with whether S's
    /// content changed since the commit was built.
    pub watched: BTreeMap<NodeId, bool>,
    /// The nodes the commit in flight creates or moves, each with the latest position L and R
    /// agreed on since the commit was built.
    pub watched_positions: BTreeMap<NodeId, Option<Agreed>>,
    /// The nodes a delete of this device trashed, each with the seq of its commit, while R holds
    /// them trashed by that commit.
    pub trashed_here: BTreeMap<NodeId, u64>,
    /// The nodes the person discarded in answer to a first-link question (section 6.4), each with
    /// the shape the question showed, while L, S or the S update waiting for them holds them.
    pub discarded: BTreeMap<NodeId, Shape>,
    /// The nodes whose local copy the user moved into an ignored folder that is no node, and the folders L keeps
    /// for them. L holds each at the position it had before.
    pub outside: BTreeSet<NodeId>,
    /// The folders of `outside` that the disk lost, which L keeps only for what is recorded below them.
    pub lost: BTreeSet<NodeId>,
    /// The nodes whose local copy is in an ignored folder and whose name in L another entry took, each with the
    /// identity L held for it. L holds none of them, and R holds each live.
    pub displaced: BTreeMap<NodeId, FileIdentity>,
    /// The files whose content an edit of this device set, each with the seq of its commit, while
    /// R holds that content.
    pub edited_here: BTreeMap<NodeId, u64>,
    /// The nodes whose position a create or a move of this device set, while R holds them at that
    /// position, also after a delete and a restore of the node.
    pub moved_here: BTreeMap<NodeId, MovedHere>,
    /// The earlier live records R held of each node, oldest first.
    pub past_positions: BTreeMap<NodeId, Vec<Agreed>>,
    /// The files whose content in L is an edit of this device that the server does not hold,
    /// although S and R may hold the same bytes (section 4.4).
    pub unsent_edits: BTreeSet<NodeId>,
    /// The nodes a delete of a conflict answer removes, each with the file the answer keeps, which the delete
    /// requires live (RFC 0015). Each is held while R holds the node trashed by that delete, or while its delete is
    /// sent again after the server lost it (section 4.4).
    pub kept_originals: BTreeMap<NodeId, NodeId>,
    /// For each file of R, the `version_id`s it held before its current one as the log moved it
    /// forward, oldest first, at most
    /// [`REPLACED_VERSIONS_KEPT`](crate::classify::REPLACED_VERSIONS_KEPT). A server never gives a
    /// version ID to two versions, also across a loss of commits (RFC 0016), so a version of this
    /// list that a restored server holds is one that the content S took from R replaced.
    pub replaced_versions: BTreeMap<NodeId, Vec<String>>,
    /// The nodes this device sends again after a server loss although no change of its user is in
    /// them (section 4.4): a file whose content S took back from R so that the content L holds is
    /// sent again, and a node S forgot that L holds as S did. A delete that R holds of the node, or
    /// of a folder L holds it under, beats the resend: S then takes L's shape, so the delete reaches
    /// L. The record also ends when S takes L's shape or L changes the node.
    pub resends: BTreeSet<NodeId>,
}

/// A node this device sent under a pending name (docs/design/sync-engine.md section 2.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// The pending name the node was sent under.
    pub sent: String,
    /// The name L gave the node, which it takes once it leaves the pending name.
    pub name: String,
    /// Whether R has held the node under `sent`. Until then the commit that sends it there may
    /// still be in flight, and R need not hold it there.
    pub landed: bool,
}

/// What [`Commits`](crate::commit::Commits) holds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommitsState {
    /// The accepted counter *a*, the `device_seq` of the last commit whose result was applied.
    pub accepted: u64,
    /// The commit in flight, with its result once that has arrived. Its `device_seq` is
    /// `accepted + 1`.
    pub in_flight: Option<InFlight>,
    /// Nodes an op was rejected for, left out of planning until cleared.
    pub problems: BTreeSet<NodeId>,
    /// Why no commit is built or resent, if committing has stopped.
    pub halted: Option<Halt>,
    /// The most ops the next commit holds after a `LIMIT_EXCEEDED`. It is at least 1 and below
    /// [`MAX_OPS`](crate::commit::MAX_OPS).
    pub cap: Option<usize>,
    /// Folders whose delete did not fit alone, deleted contents first.
    pub split: BTreeSet<NodeId>,
    /// Folders merged into one L holds elsewhere, by the folder they were merged into.
    pub aliases: BTreeMap<NodeId, NodeId>,
    /// The restores a person asked for that have no outcome yet, in request order. The commit in
    /// flight carries those its `Restore` ops name.
    pub restores: BTreeMap<RestoreId, RestoreIntent>,
    /// The restores that ended, oldest first, at most
    /// [`RESOLVED_KEPT`](crate::restore::RESOLVED_KEPT).
    pub resolved: Vec<Resolved>,
    /// The seq of each of this device's last [`SLOTS_KEPT`](crate::commit::SLOTS_KEPT)
    /// acknowledged commits, by slot.
    pub slots: BTreeMap<u64, u64>,
    /// The open questions, in the order they were asked.
    pub questions: Vec<Question>,
    /// The local entries a Publish answer to a first link's question released, until the server
    /// applies their create.
    pub published: BTreeSet<NodeId>,
    /// The nodes this device sent under a pending name, while R holds each at that name, live or
    /// in the trash, and until a move of this device that takes it off that name is acknowledged.
    pub pending: BTreeMap<NodeId, Pending>,
    /// The mass-delete guard's window and confirmed deletions.
    pub guard: GuardState,
}

/// What [`Effects`](crate::effects::Effects) holds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EffectsState {
    /// The effect in flight. Its `counter` is `cleared + 1`.
    pub in_flight: Option<EffectRecord>,
    /// The effect counter *e*, the number of effects cleared so far.
    pub cleared: u64,
    /// The effect that failed last for each blocked node.
    pub failed: BTreeMap<NodeId, Effect>,
}

/// An invariant a restored state breaks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateError {
    /// R, L or S, named by its letter, is not a valid tree.
    Tree(char, TreeError),
    /// A node is parked or not in sync but not dirty, so settling and planning would skip it.
    NotDirty(NodeId),
    /// A node is unsearched but not dirty.
    UnsearchedNotDirty(NodeId),
    /// A node has a parked name but L does not hold it as parked.
    NotParked(NodeId),
    /// A node is recorded as trashed by a delete of this device, but R does not hold it trashed by
    /// that commit.
    NotTrashedHere(NodeId),
    /// A node is recorded as moved into an ignored folder, or as a folder the disk lost, but L does not hold it or the
    /// lost folder is not recorded.
    NotIgnoredLocal(NodeId),
    /// A node is recorded as displaced, but L holds it or R does not hold it live.
    NotDisplaced(NodeId),
    /// A file is recorded as edited by this device, but R does not hold the content of that
    /// commit.
    NotEditedHere(NodeId),
    /// A node is recorded as moved by this device, but R does not hold it at a revision at or after
    /// that commit.
    NotMovedHere(NodeId),
    /// Earlier records are kept for a node R does not hold.
    NotPastPosition(NodeId),
    /// A file is recorded with an edit the server does not hold, but L does not hold it.
    NotUnsentEdit(NodeId),
    NotRemote(NodeId),
    /// A node is recorded as sent again after a server loss, but L does not hold it.
    NotResend(NodeId),
    /// The commit in flight does not take the slot after the accepted counter.
    DeviceSeq {
        accepted: u64,
        device_seq: u64,
    },
    /// The result of the commit in flight does not have one outcome per op.
    ResultMismatch,
    /// Planned deletes are listed for a node that no `Delete` op of the commit in flight targets.
    DeleteWithoutOp(NodeId),
    /// The op cap is 0, or not below `MAX_OPS`.
    Cap(usize),
    /// A folder is merged into itself.
    SelfAlias(NodeId),
    /// The commit in flight holds a `Restore` op, at this index, that no queued restore asks for.
    RestoreNotQueued(usize),
    /// A restore is queued and also has an outcome, or has two outcomes.
    RestoreResolved(RestoreId),
    /// The effect in flight does not carry the counter after the cleared one.
    EffectCounter {
        cleared: u64,
        counter: u64,
    },
}
