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
    pub waiting: BTreeMap<NodeId, Option<Shape>>,
    /// The nodes whose content the commit in flight creates or updates, each with whether S's
    /// content changed since the commit was built.
    pub watched: BTreeMap<NodeId, bool>,
    /// The nodes the commit in flight creates or moves, each with the latest position L and R
    /// agreed on since the commit was built.
    pub watched_positions: BTreeMap<NodeId, Option<Agreed>>,
    /// The nodes a delete of this device trashed, each with the seq of its commit, while R holds
    /// them trashed by that commit.
    pub trashed_here: BTreeMap<NodeId, u64>,
    /// The nodes S took from a Discard answer to a first-link question (section 6.4), while S, or
    /// the S update waiting for them, still holds them.
    pub discarded: BTreeSet<NodeId>,
    /// The nodes whose local copy the user moved into an ignored folder that is no node, and the folders L keeps
    /// for them. L holds each at the position it had before.
    pub outside: BTreeSet<NodeId>,
    /// The nodes whose local copy is in an ignored folder and whose name in L another entry took, each with the
    /// identity L held for it. L holds none of them, and R holds each live.
    pub displaced: BTreeMap<NodeId, FileIdentity>,
    /// The files whose content an edit of this device set, each with the seq of its commit, while
    /// R holds that content.
    pub edited_here: BTreeMap<NodeId, u64>,
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
    /// A node is recorded as moved into an ignored folder, but L does not hold it.
    NotIgnoredLocal(NodeId),
    /// A node is recorded as displaced, but L holds it or R does not hold it live.
    NotDisplaced(NodeId),
    /// A file is recorded as edited by this device, but R does not hold the content of that
    /// commit.
    NotEditedHere(NodeId),
    /// The commit in flight does not take the slot after the accepted counter.
    DeviceSeq { accepted: u64, device_seq: u64 },
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
    EffectCounter { cleared: u64, counter: u64 },
}
