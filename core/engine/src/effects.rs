// SPDX-License-Identifier: Apache-2.0

//! Applying remote changes to disk (docs/design/sync-engine.md sections 2.5, 5.2 and 5.3): the one
//! effect in flight for a node whose only change is remote, parking for remote states that cannot
//! be reached one move at a time, and the effect counter of section 2.8.

use std::collections::{BTreeMap, BTreeSet};

use crate::classify::{Class, Trees, classify};
use crate::ids::IdGen;
use crate::ignore::is_rules_file;

use crate::names::name_key;
use crate::state::{EffectsState, StateError};
use crate::tree::{Content, FileIdentity, Kind, LocalNode, NodeId, Shape, same, same_content};

/// A directory on the way to an entry, with the identity L holds for it. The driver resolves a
/// location one directory at a time and fails the effect when an identity differs (section 5.2).
#[derive(Clone, Debug, PartialEq)]
pub struct Dir {
    pub name: String,
    pub identity: FileIdentity,
}

/// Where the directories of a location start.
#[derive(Clone, Debug, PartialEq)]
pub enum Base {
    Root,
    /// The parked directory `.koidrive/parked/<node>`, which must have `identity`.
    Parked {
        node: NodeId,
        identity: FileIdentity,
    },
}

/// An entry's place on disk.
#[derive(Clone, Debug, PartialEq)]
pub enum Location {
    /// `.koidrive/parked/<node_id>` itself.
    Parked(NodeId),
    /// `name` inside the last of `dirs`, which start at `base`.
    At {
        base: Base,
        dirs: Vec<Dir>,
        name: String,
    },
}

/// A file an effect displaces and what it must still be: the driver quarantines it, compares it
/// with this, and moves it to the local trash only on a match.
#[derive(Clone, Debug, PartialEq)]
pub struct Displaced {
    pub at: Location,
    pub identity: FileIdentity,
    pub content: Content,
}

/// One disk change of section 2.5. Every name change is a `mkdir` or rename that never replaces an
/// existing entry.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    CreateDir {
        at: Location,
    },
    /// Downloads `content` into staging and puts it at `at`. With `replaces`, that file goes
    /// to quarantine first; it sits elsewhere when the node also moves.
    WriteFile {
        at: Location,
        content: Content,
        replaces: Option<Displaced>,
    },
    /// Renames the entry with `identity`, also into and out of the parked directory.
    Move {
        from: Location,
        identity: FileIdentity,
        to: Location,
    },
    RemoveFile {
        displaced: Displaced,
    },
    /// Removes a directory, which must be empty once quarantined.
    RemoveDir {
        at: Location,
        identity: FileIdentity,
    },
}

/// An effect as it is persisted before the driver receives it.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectRecord {
    pub effect_id: String,
    pub node: NodeId,
    /// The value the driver writes to the root marker before it touches the disk: one more than
    /// the effects cleared so far (section 2.8).
    pub counter: u64,
    pub effect: Effect,
    /// L's shape for the node once the effect is done, or ⊥ for a removal.
    pub after: Option<Shape>,
}

impl EffectRecord {
    pub fn staging(&self) -> String {
        format!(".koidrive/staging/{}", self.effect_id)
    }

    pub fn quarantine(&self) -> String {
        format!(".koidrive/quarantine/{}", self.effect_id)
    }

    /// Whether L takes the outcome of this effect as L is now, by the checks [`Effects::plan`] makes
    /// before it plans one. The engine plans an effect for L as it is then, and a later observation
    /// may have given the effect's name to another node, taken a folder on its way out of L, moved
    /// such a folder under the node, put an entry into a folder it removes, or put the node it
    /// creates into L. The planner plans none of these either, so an effect that fails for one of
    /// them and is unblocked is planned again only once L has changed.
    pub fn applies(&self, trees: &Trees) -> bool {
        View::new(trees).applies(self)
    }

    fn parks(&self) -> bool {
        matches!(
            self.effect,
            Effect::Move {
                to: Location::Parked(_),
                ..
            }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectError {
    /// The ID is not the effect in flight.
    NotInFlight(String),
    /// A created or written entry was reported done without its file identity.
    MissingIdentity,
    /// L does not take the outcome of the effect in flight ([`EffectRecord::applies`]).
    DoesNotApply(String),
}

/// The disk side of a space: the effect in flight, the effects cleared so far, and the effect that
/// failed last for each blocked node. An effect that failed is never emitted again unchanged, so it
/// is not retried blindly; a change to anything it was computed from, such as the identity of a
/// directory on its path, gives the node a new effect.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Effects {
    in_flight: Option<EffectRecord>,
    cleared: u64,
    failed: BTreeMap<NodeId, Effect>,
}

impl Effects {
    /// What the store keeps of the disk side.
    pub fn state(&self) -> EffectsState {
        EffectsState {
            in_flight: self.in_flight.clone(),
            cleared: self.cleared,
            failed: self.failed.clone(),
        }
    }

    /// The disk side the store kept. Refuses an effect in flight whose counter is not one more
    /// than the effects cleared.
    pub fn from_state(state: EffectsState) -> Result<Self, StateError> {
        if let Some(record) = &state.in_flight
            && state.cleared.checked_add(1) != Some(record.counter)
        {
            return Err(StateError::EffectCounter {
                cleared: state.cleared,
                counter: record.counter,
            });
        }
        let EffectsState {
            in_flight,
            cleared,
            failed,
        } = state;
        Ok(Self {
            in_flight,
            cleared,
            failed,
        })
    }

    pub fn in_flight(&self) -> Option<&EffectRecord> {
        self.in_flight.as_ref()
    }

    /// The effect counter *e* of section 2.8.
    pub fn cleared(&self) -> u64 {
        self.cleared
    }

    /// The nodes whose last effect failed, with that effect. Each stays blocked until the effect
    /// planned for it differs, or until [`Effects::unblock`].
    pub fn blocked(&self) -> &BTreeMap<NodeId, Effect> {
        &self.failed
    }

    /// Lets the planner emit the failed effect of `node` again, for a caller that knows the
    /// cause is gone. Returns false when the node was not blocked.
    pub fn unblock(&mut self, node: &str) -> bool {
        self.failed.remove(node).is_some()
    }

    /// Emits the next effect, unless one is in flight. A dirty node whose only change is remote,
    /// or that is parked, gets the effect that makes L equal to R when it applies to L as it is:
    /// its parent is a directory there, no other entry there holds its name key, and a move does
    /// not go into the node's own subtree. A directory is removed only once it is empty. When no
    /// such effect applies, a node that blocks one is parked, if it is a candidate itself or its S
    /// update waits ([`Trees::waiting`]), since the commit layer does not plan it either: the
    /// entry holding a name, or a directory that would move into its own subtree. A node whose park
    /// failed gets no effect until L changes: the parked directory always has room, so the park
    /// failed on the entry itself, and the node's own effect would fail on it too, taking turns
    /// with the park for as long as L stays as it is. A rename that changes only case or
    /// normalization goes through the parked directory too.
    ///
    /// A parked node with a local change, such as an edit observed in the parked directory, is
    /// only moved out: to its R position, or to the position L keeps for it when R has none. Its
    /// content is never replaced or removed, so the change is planned like any other local one.
    ///
    /// An excluded node ([`Trees::excluded`]) gets no effect and is never parked, and neither is a
    /// node whose own place an excluded node holds. A parked node that the rules now leave out is
    /// only moved back to the position L keeps for it, and nothing is planned while the rules are
    /// unknown ([`Trees::rules_known`]), or for a node below a folder whose rules are unknown
    /// ([`Trees::held`]). A `.koiignore` file that R
    /// holds and the disk does not yet is written before any other effect below its folder
    /// (section 5.10). The driver reads the rules of a rules file it wrote and passes them to
    /// [`Trees::set_rules`] before it asks for the next effect, so the rules are in force before
    /// the nodes they cover arrive.
    pub fn plan(&mut self, trees: &Trees, ids: &mut IdGen, now_ms: u64) -> Option<&EffectRecord> {
        if self.in_flight.is_some() || !trees.rules_known() {
            return None;
        }
        self.failed
            .retain(|id, _| candidate(trees, id) || trees.waiting().contains_key(id));
        let view = View::new(trees);
        let writes_rules = |id: &str| {
            let remote = trees.remote().live(id)?;
            (remote.kind == Kind::File && is_rules_file(&remote.name))
                .then_some(remote.parent.as_str())
        };
        let gates: BTreeSet<&str> = (trees.remote_rules_files().iter())
            .filter(|id| trees.dirty().contains(*id))
            .filter_map(|id| writes_rules(id).filter(|_| candidate(trees, id)))
            .collect();
        let gated = |id: &str| {
            let parent = |id: &str| {
                let remote = trees.remote().live(id);
                let shape = remote.or(trees.local().get(id).map(|node| &node.shape))?;
                Some(shape.parent.as_str())
            };
            let mut above = parent(id);
            if writes_rules(id).is_some() {
                above = above.and_then(parent);
            }
            for _ in 0..=trees.remote().len() + trees.local().len() {
                match above {
                    Some(dir) if gates.contains(dir) => return true,
                    Some(dir) => above = parent(dir),
                    None => return false,
                }
            }
            false
        };
        let park_failed = |id: &str| {
            let failed = self.failed.get(id);
            matches!(
                failed,
                Some(Effect::Move {
                    to: Location::Parked(_),
                    ..
                })
            ) && view.park(id).map(|(park, _)| park).as_ref() == failed
        };
        let mut blockers = BTreeSet::new();
        let mut chosen = None;
        for id in trees.dirty() {
            if !candidate(trees, id) || trees.held(id) || (!gates.is_empty() && gated(id)) {
                continue;
            }
            match view.step(id) {
                Step::Apply(effect, _)
                    if self.failed.get(id) == Some(&effect) || park_failed(id) => {}
                Step::Apply(effect, after) => {
                    chosen = Some((id.clone(), effect, after));
                    break;
                }
                Step::Blocked(Some(blocker)) => {
                    blockers.insert(blocker);
                }
                Step::Blocked(None) | Step::Skip => {}
            }
        }
        let (node, effect, after) = chosen.or_else(|| {
            blockers.into_iter().find_map(|id| {
                let waiting = trees.waiting().contains_key(&id);
                if !(candidate(trees, &id) || waiting)
                    || trees.excluded(&id)
                    || trees.held(&id)
                    || trees.local().get(&id).is_none_or(|node| node.parked)
                    || view.stuck(&id, trees.local().len())
                {
                    return None;
                }
                let (effect, after) = view.park(&id)?;
                (self.failed.get(&id) != Some(&effect)).then_some((id, effect, after))
            })
        })?;
        self.failed.remove(&node);
        self.in_flight = Some(EffectRecord {
            effect_id: ids.uuid_v7(now_ms),
            node,
            counter: self.cleared + 1,
            effect,
            after,
        });
        self.in_flight.as_ref()
    }

    /// The driver did the effect. L takes the node's new shape, with `identity` for an entry the
    /// effect created. When L matched S, or the shape S's waiting update holds, the effect gave L
    /// the shape R had when it was planned, so S takes that shape too. When that would not leave S
    /// a valid tree yet, as when S still holds a child the user moved out of a removed folder, the
    /// update waits ([`Trees::waiting`]) instead of being lost.
    /// A move changes only the position, so for a move L only has to match that shape's position,
    /// and L and S each keep their content, which can be newer than the content the move was
    /// planned with. An edit the user made while the move was in flight stays the user's.
    /// A change R made while the effect was in flight then stays a remote change and is never
    /// taken for the user's: a file R restored during its removal is downloaded again, and a file
    /// R moved back during its move is moved back. S stays as it is when L no longer matched it:
    /// L then holds a change of the user's, such as one that recovery after a crash reports as
    /// the effect's outcome (a missing file as the removal, the planned bytes as the write), and
    /// that change stays the user's.
    /// An outcome L does not take ([`EffectRecord::applies`]) is refused, and the effect stays
    /// in flight.
    pub fn done(
        &mut self,
        trees: &mut Trees,
        effect_id: &str,
        identity: Option<FileIdentity>,
    ) -> Result<(), EffectError> {
        if (self.in_flight.as_ref())
            .is_some_and(|record| record.effect_id == effect_id && !record.applies(trees))
        {
            return Err(EffectError::DoesNotApply(effect_id.to_owned()));
        }
        let identity = match self.in_flight.as_ref().map(|record| &record.effect) {
            Some(Effect::CreateDir { .. } | Effect::WriteFile { .. }) => {
                identity.ok_or(EffectError::MissingIdentity)?
            }
            Some(
                Effect::Move { identity, .. }
                | Effect::RemoveDir { identity, .. }
                | Effect::RemoveFile {
                    displaced: Displaced { identity, .. },
                },
            ) => *identity,
            None => return Err(EffectError::NotInFlight(effect_id.to_owned())),
        };
        let record = self.take(effect_id)?;
        let parked = record.parks();
        let base = match trees.waiting().get(&record.node) {
            Some(waiting) => waiting.as_ref(),
            None => trees.synced().live(&record.node),
        };
        let local = trees.local().live(&record.node);
        let unchanged = match (&record.effect, local, base) {
            (Effect::Move { .. }, Some(local), Some(base)) => {
                local.kind == base.kind && local.same_position(base)
            }
            _ => same(local, base),
        };
        if unchanged {
            let synced = match (&record.effect, &record.after, base) {
                (Effect::Move { .. }, Some(after), Some(base)) => Some(Shape {
                    parent: after.parent.clone(),
                    name: after.name.clone(),
                    ..base.clone()
                }),
                _ => record.after.clone(),
            };
            if !trees.set_synced_if_valid(record.node.clone(), synced.clone()) {
                trees.wait(record.node.clone(), synced);
            }
        }
        let content = (trees.local().get(&record.node)).map(|node| node.shape.content.clone());
        match record.after {
            None => trees.remove_local(&record.node),
            Some(shape) => trees.set_local(
                record.node.clone(),
                LocalNode {
                    shape: match (&record.effect, content) {
                        (Effect::Move { .. }, Some(content)) => Shape { content, ..shape },
                        _ => shape,
                    },
                    identity,
                    parked,
                },
            ),
        }
        self.failed.remove(&record.node);
        self.cleared = record.counter;
        Ok(())
    }

    /// Whether L confirms what recovery reports after a crash: that the effect in flight is done,
    /// with `identity` for the entry it created (section 5.3). Recovery judges a target by the name
    /// and the content it finds there. An entry that L holds as another node is that node's, and a
    /// removal whose node L places elsewhere found its name empty because the node moved.
    pub fn confirms(&self, trees: &Trees, identity: Option<&FileIdentity>) -> bool {
        let Some(record) = &self.in_flight else {
            return false;
        };
        let local = trees.local();
        if (local.iter()).any(|(id, node)| *id != record.node && Some(&node.identity) == identity) {
            return false;
        }
        match &record.effect {
            Effect::RemoveDir { at, .. }
            | Effect::RemoveFile {
                displaced: Displaced { at, .. },
            } => {
                local.get(&record.node).is_none()
                    || View::new(trees).location(&record.node).as_ref() == Some(at)
            }
            _ => true,
        }
    }

    /// The driver did not do the effect. Its observations of the disk go to L separately; the node
    /// is blocked until the effect planned for it differs from this one.
    pub fn failed(&mut self, effect_id: &str) -> Result<(), EffectError> {
        let record = self.take(effect_id)?;
        self.failed.insert(record.node, record.effect);
        self.cleared = record.counter;
        Ok(())
    }

    fn take(&mut self, effect_id: &str) -> Result<EffectRecord, EffectError> {
        self.in_flight
            .take_if(|record| record.effect_id == effect_id)
            .ok_or_else(|| EffectError::NotInFlight(effect_id.to_owned()))
    }
}

/// A node the planner considers: one whose only change is remote, one that is parked, one whose
/// local position is reverted, or one whose S update waits and that R no longer has. A reverted or
/// waiting node that R no longer has is removed only when it holds no unuploaded edit; otherwise
/// the commit layer makes it a new node, so the edit is uploaded. A reverted node that L does not
/// hold is created only as a folder that a node L holds needs on its way to the root: otherwise the
/// user deleted it, and that delete is committed instead. A node whose waiting update forgets S
/// ([`Trees::forget_lost`]) is never one: once the update is made, L holds a node S does not,
/// which is created again. An excluded node is never one.
fn candidate(trees: &Trees, id: &str) -> bool {
    let forgets = trees.waiting().get(id).is_some_and(Option::is_none);
    let changed = remote_only(trees, id)
        || trees.local().get(id).is_some_and(|node| node.parked)
        || (trees.reverted().contains(id)
            && (trees.local().get(id).is_some() || holds_below(trees, id))
            && (trees.remote().live(id).is_some() || !trees.edited(id)))
        || (trees.waiting().contains_key(id)
            && trees.remote().live(id).is_none()
            && !trees.edited(id));
    changed && !forgets && !trees.excluded(id) && !trees.reclaimed(id)
}

/// Whether L holds a node whose way to the root passes `id`. Each node on that way goes to its R
/// position, or to its local one when R no longer has it, as a parked edit that is moved back, or
/// when it is parked and ignored, as [`View::step`] moves it back.
fn holds_below(trees: &Trees, id: &str) -> bool {
    let (local, remote) = (trees.local(), trees.remote());
    let parent = |node: &str| {
        let home = local
            .get(node)
            .filter(|held| held.parked && trees.ignored(node))
            .map(|held| &held.shape);
        (home
            .or_else(|| remote.live(node))
            .or_else(|| local.live(node)))
        .map(|shape| shape.parent.as_str())
    };
    local.iter().any(|(node, _)| {
        let mut up = parent(node);
        for _ in 0..=remote.len() + local.len() {
            match up {
                Some(parent) if parent == id => return true,
                Some(next) => up = parent(next),
                None => return false,
            }
        }
        false
    })
}

/// Whether the node's only change is remote. While its S update waits, the waiting shape stands
/// for S: L still holds the shape the server acknowledged, and only R differs from it. The update
/// may never fit S while L holds that shape, as when its name key is taken in S, so the effects
/// take R's shape and the update stops waiting once L leaves it.
fn remote_only(trees: &Trees, id: &str) -> bool {
    let (local, remote) = (trees.local().live(id), trees.remote().live(id));
    match trees.waiting().get(id) {
        Some(waiting) => classify(local, waiting.as_ref(), remote) == Class::Remote,
        None => trees.class(id) == Class::Remote,
    }
}

#[allow(clippy::large_enum_variant)]
enum Step {
    Apply(Effect, Option<Shape>),
    /// Waits, for the given node to move away when there is one.
    Blocked(Option<NodeId>),
    Skip,
}

/// L as the disk holds it: parked nodes sit in the parked directory and hold no name.
struct View<'a> {
    trees: &'a Trees,
}

impl<'a> View<'a> {
    fn new(trees: &'a Trees) -> Self {
        Self { trees }
    }

    fn root(&self) -> &'a str {
        &self.trees.local().root
    }

    fn step(&self, id: &str) -> Step {
        let local = self.trees.local().get(id);
        let remote = self.trees.remote().live(id);
        if let Some(local) = local.filter(|local| local.parked && self.trees.ignored(id)) {
            return self.unpark(id, local, &local.shape);
        }
        let (local, remote) = match (local, remote) {
            (None, None) => return Step::Skip,
            (Some(local), None)
                if local.parked
                    && !remote_only(self.trees, id)
                    && (!(self.trees.reverted().contains(id)
                        || self.trees.waiting().contains_key(id))
                        || self.trees.edited(id)) =>
            {
                return self.unpark(id, local, &local.shape);
            }
            (Some(local), None) => return self.remove(id, local),
            (None, Some(remote)) => {
                if let Some(blocked) = self.blocked(id, remote) {
                    return blocked;
                }
                let Some(at) = self.location_in(&remote.parent, &remote.name) else {
                    return Step::Skip;
                };
                let effect = match (&remote.kind, &remote.content) {
                    (Kind::Dir, _) => Effect::CreateDir { at },
                    (Kind::File, Some(content)) => Effect::WriteFile {
                        at,
                        content: content.clone(),
                        replaces: None,
                    },
                    (Kind::File, None) => return Step::Skip,
                };
                return Step::Apply(effect, Some(remote.clone()));
            }
            (Some(local), Some(remote)) => (local, remote),
        };
        if local.parked && !remote_only(self.trees, id) {
            return self.unpark(id, local, remote);
        }
        if local.shape.kind != remote.kind {
            return Step::Skip;
        }
        let rewrite = match (&local.shape.content, &remote.content) {
            (Some(old), Some(new)) => !same_content(old, new) && remote_only(self.trees, id),
            _ => false,
        };
        let moved = local.parked || !local.shape.same_position(remote);
        let Some(here) = self.location(id) else {
            return Step::Skip;
        };
        if !moved {
            if !rewrite {
                return Step::Skip;
            }
            let displaced = self.displaced(here.clone(), local);
            return match (displaced, &remote.content) {
                (Some(displaced), Some(content)) => Step::Apply(
                    Effect::WriteFile {
                        at: here,
                        content: content.clone(),
                        replaces: Some(displaced),
                    },
                    Some(remote.clone()),
                ),
                _ => Step::Skip,
            };
        }
        if !local.parked
            && local.shape.parent == remote.parent
            && name_key(&local.shape.name) == name_key(&remote.name)
        {
            return match self.park(id) {
                Some((effect, after)) => Step::Apply(effect, after),
                None => Step::Skip,
            };
        }
        if let Some(blocked) = self.blocked(id, remote) {
            return blocked;
        }
        let Some(to) = self.location_in(&remote.parent, &remote.name) else {
            return Step::Skip;
        };
        if rewrite {
            let displaced = self.displaced(here, local);
            return match (displaced, &remote.content) {
                (Some(displaced), Some(content)) => Step::Apply(
                    Effect::WriteFile {
                        at: to,
                        content: content.clone(),
                        replaces: Some(displaced),
                    },
                    Some(remote.clone()),
                ),
                _ => Step::Skip,
            };
        }
        let after = Shape {
            parent: remote.parent.clone(),
            name: remote.name.clone(),
            ..local.shape.clone()
        };
        Step::Apply(
            Effect::Move {
                from: here,
                identity: local.identity,
                to,
            },
            Some(after),
        )
    }

    /// Whether L takes `record`'s outcome: a removal once the node holds no entry, a park always,
    /// a create while L does not hold the node, and anything else while its folder is one
    /// [`View::step`] could place the node in and no other node holds its exact name. L tells
    /// names apart exactly, so a name another node holds in another case does not stop it.
    fn applies(&self, record: &EffectRecord) -> bool {
        let Some(after) = &record.after else {
            return self.trees.local_children(&record.node).next().is_none();
        };
        if record.parks() {
            return true;
        }
        let creates = matches!(
            record.effect,
            Effect::CreateDir { .. } | Effect::WriteFile { replaces: None, .. }
        );
        if creates && self.trees.local().get(&record.node).is_some() {
            return false;
        }
        let local = self.trees.local();
        self.blocked_in(&record.node, &after.parent).is_none()
            && self.location_in(&after.parent, &after.name).is_some()
            && !(self.trees.local_holders(&after.parent, &after.name)).any(|other| {
                *other != record.node
                    && local
                        .get(other)
                        .is_some_and(|node| node.shape.name == after.name)
            })
    }

    /// Moves a parked node to `target`'s position with its content as L has it.
    fn unpark(&self, id: &str, local: &LocalNode, target: &Shape) -> Step {
        if let Some(blocked) = self.blocked(id, target) {
            return blocked;
        }
        let Some(to) = self.location_in(&target.parent, &target.name) else {
            return Step::Skip;
        };
        let after = Shape {
            parent: target.parent.clone(),
            name: target.name.clone(),
            ..local.shape.clone()
        };
        Step::Apply(
            Effect::Move {
                from: Location::Parked(self.trees.parked_name(id).to_owned()),
                identity: local.identity,
                to,
            },
            Some(after),
        )
    }

    fn remove(&self, id: &str, local: &LocalNode) -> Step {
        if self.trees.local_children(id).next().is_some() {
            return Step::Blocked(None);
        }
        let Some(at) = self.location(id) else {
            return Step::Skip;
        };
        let effect = match local.shape.kind {
            Kind::Dir => Effect::RemoveDir {
                at,
                identity: local.identity,
            },
            Kind::File => match self.displaced(at, local) {
                Some(displaced) => Effect::RemoveFile { displaced },
                None => return Step::Skip,
            },
        };
        Step::Apply(effect, None)
    }

    /// Why `id` cannot be put at `remote`'s position in L yet, if it cannot.
    fn blocked(&self, id: &str, remote: &Shape) -> Option<Step> {
        if let Some(step) = self.blocked_in(id, &remote.parent) {
            return Some(step);
        }
        self.trees
            .local_holders(&remote.parent, &remote.name)
            .find(|other| *other != id)
            .map(|holder| Step::Blocked(Some(holder.clone())))
    }

    /// Why `id` cannot be put into `parent` in L yet, if it cannot: `parent` is no folder of L,
    /// or it is `id` or inside it.
    fn blocked_in(&self, id: &str, parent: &str) -> Option<Step> {
        if parent == self.root() {
            return None;
        }
        match self.trees.local().get(parent) {
            Some(node) if node.shape.kind == Kind::Dir => {}
            _ => return Some(Step::Blocked(None)),
        }
        if self.inside(parent, id) {
            let stays = self.held_by_excluded(parent, id);
            return Some(Step::Blocked((!stays).then(|| id.to_owned())));
        }
        None
    }

    /// True when an excluded node is on the way up from `inner` to `id`, which holds it on disk.
    /// No effect moves an excluded node, so parking `id` would never let it move into `inner`.
    fn held_by_excluded(&self, inner: &str, id: &str) -> bool {
        let mut current = inner;
        for _ in 0..=self.trees.local().len() {
            if current == id {
                return false;
            }
            if self.trees.excluded(current) {
                return true;
            }
            match self.trees.local().get(current) {
                Some(node) => current = &node.shape.parent,
                None => return false,
            }
        }
        false
    }

    /// True when `id` could not leave the parked directory again: the name it moves to is held by
    /// an excluded node, whose local copy no effect moves, or by a node that is stuck the same way.
    /// Parking it would hide it for as long as the rules stay as they are.
    fn stuck(&self, id: &str, depth: usize) -> bool {
        match self.step(id) {
            Step::Blocked(Some(holder)) if holder != id => {
                self.trees.excluded(&holder) || (depth > 0 && self.stuck(&holder, depth - 1))
            }
            _ => false,
        }
    }

    /// True when `ancestor` is `id` or holds it on disk.
    fn inside(&self, id: &str, ancestor: &str) -> bool {
        let mut current = id;
        for _ in 0..=self.trees.local().len() {
            if current == ancestor {
                return true;
            }
            match self.trees.local().get(current) {
                Some(node) if !node.parked && node.shape.parent != self.root() => {
                    current = &node.shape.parent;
                }
                _ => return false,
            }
        }
        false
    }

    fn park(&self, id: &str) -> Option<(Effect, Option<Shape>)> {
        let local = self.trees.local().get(id)?;
        let effect = Effect::Move {
            from: self.location(id)?,
            identity: local.identity,
            to: Location::Parked(id.to_owned()),
        };
        Some((effect, Some(local.shape.clone())))
    }

    fn displaced(&self, at: Location, local: &LocalNode) -> Option<Displaced> {
        Some(Displaced {
            at,
            identity: local.identity,
            content: local.shape.content.clone()?,
        })
    }

    fn location(&self, id: &str) -> Option<Location> {
        let node = self.trees.local().get(id)?;
        if node.parked {
            return Some(Location::Parked(self.trees.parked_name(id).to_owned()));
        }
        self.location_in(&node.shape.parent, &node.shape.name)
    }

    fn location_in(&self, parent: &str, name: &str) -> Option<Location> {
        let mut dirs = Vec::new();
        let mut current = parent;
        for _ in 0..=self.trees.local().len() {
            if current == self.root() {
                dirs.reverse();
                return Some(Location::At {
                    base: Base::Root,
                    dirs,
                    name: name.to_owned(),
                });
            }
            let node = self.trees.local().get(current)?;
            if node.parked {
                dirs.reverse();
                return Some(Location::At {
                    base: Base::Parked {
                        node: self.trees.parked_name(current).to_owned(),
                        identity: node.identity,
                    },
                    dirs,
                    name: name.to_owned(),
                });
            }
            dirs.push(Dir {
                name: node.shape.name.clone(),
                identity: node.identity,
            });
            current = &node.shape.parent;
        }
        None
    }
}
