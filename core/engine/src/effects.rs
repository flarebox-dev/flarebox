// SPDX-License-Identifier: Apache-2.0

//! Applying remote changes to disk (docs/design/sync-engine.md sections 2.5, 5.2 and 5.3): the one
//! effect in flight for a node whose only change is remote, parking for remote states that cannot
//! be reached one move at a time, and the effect counter of section 2.8.

use std::collections::{BTreeMap, BTreeSet};

use crate::classify::{Class, Trees};
use crate::ids::IdGen;
use crate::names::name_key;
use crate::tree::{Content, FileIdentity, Kind, LocalNode, NodeId, Shape, same_content};

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
    /// The parked directory `.flarebox/parked/<node>`, which must have `identity`.
    Parked {
        node: NodeId,
        identity: FileIdentity,
    },
}

/// An entry's place on disk.
#[derive(Clone, Debug, PartialEq)]
pub enum Location {
    /// `.flarebox/parked/<node_id>` itself.
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
        format!(".flarebox/staging/{}", self.effect_id)
    }

    pub fn quarantine(&self) -> String {
        format!(".flarebox/quarantine/{}", self.effect_id)
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
    /// such effect applies, a node that blocks one is parked, if it has a remote change of its
    /// own: the entry holding a name, or a directory that would move into its own subtree. A
    /// rename that changes only case or normalization goes through the parked directory too.
    ///
    /// A parked node with a local change, such as an edit observed in the parked directory, is
    /// only moved out: to its R position, or to the position L keeps for it when R has none. Its
    /// content is never replaced or removed, so the change is planned like any other local one.
    pub fn plan(&mut self, trees: &Trees, ids: &mut IdGen, now_ms: u64) -> Option<&EffectRecord> {
        if self.in_flight.is_some() {
            return None;
        }
        self.failed.retain(|id, _| candidate(trees, id));
        let view = View::new(trees);
        let mut blockers = BTreeSet::new();
        let mut chosen = None;
        for id in trees.dirty() {
            if !candidate(trees, id) {
                continue;
            }
            match view.step(id) {
                Step::Apply(effect, _) if self.failed.get(id) == Some(&effect) => {}
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
                if !candidate(trees, &id) || trees.local().get(&id).is_none_or(|node| node.parked) {
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
    /// effect created.
    pub fn done(
        &mut self,
        trees: &mut Trees,
        effect_id: &str,
        identity: Option<FileIdentity>,
    ) -> Result<(), EffectError> {
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
        match record.after {
            None => trees.remove_local(&record.node),
            Some(shape) => trees.set_local(
                record.node.clone(),
                LocalNode {
                    shape,
                    identity,
                    parked,
                },
            ),
        }
        self.failed.remove(&record.node);
        self.cleared = record.counter;
        Ok(())
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

/// A node the planner considers: one whose only change is remote, one that is parked, or one whose
/// local position is reverted. A reverted node that R no longer has is removed only when it holds
/// no unuploaded edit; otherwise the commit layer makes it a new node, so the edit is uploaded.
fn candidate(trees: &Trees, id: &str) -> bool {
    trees.class(id) == Class::Remote
        || trees.local().get(id).is_some_and(|node| node.parked)
        || (trees.reverted().contains(id)
            && (trees.remote().live(id).is_some() || !trees.edited(id)))
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
    children: BTreeMap<&'a str, Vec<&'a str>>,
}

impl<'a> View<'a> {
    fn new(trees: &'a Trees) -> Self {
        let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (id, node) in trees.local().iter().filter(|(_, node)| !node.parked) {
            children.entry(&node.shape.parent).or_default().push(id);
        }
        Self { trees, children }
    }

    fn root(&self) -> &'a str {
        &self.trees.local().root
    }

    fn step(&self, id: &str) -> Step {
        let local = self.trees.local().get(id);
        let remote = self.trees.remote().live(id);
        let (local, remote) = match (local, remote) {
            (None, None) => return Step::Skip,
            (Some(local), None)
                if local.parked
                    && self.trees.class(id) != Class::Remote
                    && (!self.trees.reverted().contains(id) || self.trees.edited(id)) =>
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
        if local.parked && self.trees.class(id) != Class::Remote {
            return self.unpark(id, local, remote);
        }
        if local.shape.kind != remote.kind {
            return Step::Skip;
        }
        let rewrite = match (&local.shape.content, &remote.content) {
            (Some(old), Some(new)) => {
                !same_content(old, new) && self.trees.class(id) == Class::Remote
            }
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
                from: Location::Parked(id.to_owned()),
                identity: local.identity,
                to,
            },
            Some(after),
        )
    }

    fn remove(&self, id: &str, local: &LocalNode) -> Step {
        if self.children.get(id).is_some_and(|c| !c.is_empty()) {
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
        if remote.parent != self.root() {
            match self.trees.local().get(&remote.parent) {
                Some(parent) if parent.shape.kind == Kind::Dir => {}
                _ => return Some(Step::Blocked(None)),
            }
            if self.inside(&remote.parent, id) {
                return Some(Step::Blocked(Some(id.to_owned())));
            }
        }
        let key = name_key(&remote.name);
        self.children
            .get(remote.parent.as_str())
            .into_iter()
            .flatten()
            .find(|other| {
                **other != id
                    && self
                        .trees
                        .local()
                        .get(other)
                        .is_some_and(|node| name_key(&node.shape.name) == key)
            })
            .map(|holder| Step::Blocked(Some((*holder).to_owned())))
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
            return Some(Location::Parked(id.to_owned()));
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
                        node: current.to_owned(),
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
