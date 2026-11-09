// SPDX-License-Identifier: Apache-2.0

//! Classifying a node (docs/design/sync-engine.md section 2.3) and the dirty set the planner reads.

use std::collections::BTreeSet;

use crate::names::name_key;
use crate::tree::{Kind, LocalNode, Names, NodeId, RemoteNode, Shape, Tree, TreeError, same};

/// Where a node stands against its merge base S.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// L = S = R.
    InSync,
    /// L = S ≠ R: apply R to disk.
    Remote,
    /// R = S ≠ L: upload as ops.
    Local,
    /// L ≠ S, R ≠ S, L = R: both sides made the same change, so S := R.
    Same,
    /// L ≠ S, R ≠ S, L ≠ R: rebase on the client.
    Concurrent,
}

/// The table of section 2.3, with `None` as ⊥ in each tree. A node that is not dirty on one side
/// takes the other side's value, and a node dirty on both sides is never overwritten.
pub fn classify(l: Option<&Shape>, s: Option<&Shape>, r: Option<&Shape>) -> Class {
    match (same(l, s), same(r, s)) {
        (true, true) => Class::InSync,
        (true, false) => Class::Remote,
        (false, true) => Class::Local,
        (false, false) if same(l, r) => Class::Same,
        (false, false) => Class::Concurrent,
    }
}

/// R, L and S of one space, and the nodes whose entries changed since they were last found in sync.
/// Every mutation goes through these methods, so a change in any tree marks its node dirty.
#[derive(Clone, Debug, PartialEq)]
pub struct Trees {
    r: Tree<RemoteNode>,
    l: Tree<LocalNode>,
    s: Tree<Shape>,
    dirty: BTreeSet<NodeId>,
}

impl Trees {
    pub fn new(root: impl Into<NodeId>) -> Self {
        let root = root.into();
        Self {
            r: Tree::new(root.clone()),
            l: Tree::new(root.clone()),
            s: Tree::new(root),
            dirty: BTreeSet::new(),
        }
    }

    pub fn remote(&self) -> &Tree<RemoteNode> {
        &self.r
    }

    pub fn local(&self) -> &Tree<LocalNode> {
        &self.l
    }

    pub fn synced(&self) -> &Tree<Shape> {
        &self.s
    }

    pub fn dirty(&self) -> &BTreeSet<NodeId> {
        &self.dirty
    }

    pub fn set_remote(&mut self, id: NodeId, node: RemoteNode) {
        self.dirty.insert(id.clone());
        self.r.insert(id, node);
    }

    pub fn remove_remote(&mut self, id: &str) {
        if self.r.remove(id).is_some() {
            self.dirty.insert(id.to_owned());
        }
    }

    pub fn set_local(&mut self, id: NodeId, node: LocalNode) {
        self.dirty.insert(id.clone());
        self.l.insert(id, node);
    }

    pub fn remove_local(&mut self, id: &str) {
        if self.l.remove(id).is_some() {
            self.dirty.insert(id.to_owned());
        }
    }

    /// Sets S, or clears it with `None` (S := ⊥).
    pub fn set_synced(&mut self, id: NodeId, shape: Option<Shape>) {
        self.set_synced_unmarked(&id, shape);
        self.dirty.insert(id);
    }

    pub fn class(&self, id: &str) -> Class {
        classify(self.l.live(id), self.s.live(id), self.r.live(id))
    }

    /// Settles what needs no decision: a node changed the same way on both sides takes S := R, and
    /// a node in sync leaves the dirty set. S stays a valid tree: an update that would orphan a
    /// child, put a node under a missing or non-directory parent, close a cycle or take a sibling's
    /// `name_key` waits until the updates it depends on have been made. Updates that depend on each
    /// other, such as two names swapped or rotated the same way on both sides, are made together
    /// when S is valid after all of them. A node whose update never fits stays dirty as
    /// [`Class::Same`]. Returns every dirty node that is left, with its class, in `node_id` order.
    pub fn settle(&mut self) -> Vec<(NodeId, Class)> {
        let mut pending = Vec::new();
        let mut work = Vec::new();
        for id in std::mem::take(&mut self.dirty) {
            match self.class(&id) {
                Class::InSync => {}
                Class::Same => pending.push(id),
                class => work.push((id, class)),
            }
        }
        loop {
            let before = pending.len();
            pending.retain(|id| !self.take_remote(id));
            if pending.len() < before {
                continue;
            }
            let taken = self.take_remote_together(&pending);
            if taken.is_empty() {
                break;
            }
            pending.retain(|id| !taken.contains(id));
        }
        work.extend(pending.into_iter().map(|id| (id, Class::Same)));
        work.sort_by(|a, b| a.0.cmp(&b.0));
        self.dirty = work.iter().map(|(id, _)| id.clone()).collect();
        work
    }

    /// S := R for the largest group of `pending` nodes found whose updates leave S valid when made
    /// together. Starting from all of them, each failed check drops one node the failure points
    /// to, so this ends after at most one check per node. Returns the nodes it updated.
    fn take_remote_together(&mut self, pending: &[NodeId]) -> BTreeSet<NodeId> {
        let mut group: BTreeSet<NodeId> = pending.iter().cloned().collect();
        while !group.is_empty() {
            let mut s = self.s.clone();
            for id in &group {
                match self.r.live(id) {
                    Some(shape) => {
                        s.insert(id.clone(), shape.clone());
                    }
                    None => {
                        s.remove(id);
                    }
                }
            }
            let culprit = match s.check(Names::ByKey) {
                Ok(()) => {
                    self.s = s;
                    return group;
                }
                Err(TreeError::MissingParent(id) | TreeError::ParentNotDir(id)) => {
                    if group.contains(&id) {
                        Some(id)
                    } else {
                        s.live(&id)
                            .map(|shape| shape.parent.clone())
                            .filter(|parent| group.contains(parent))
                    }
                }
                Err(TreeError::Cycle(id)) => {
                    let mut path = vec![id];
                    loop {
                        let last = path.last().expect("the path starts with a node");
                        let Some(parent) = s.live(last).map(|shape| shape.parent.clone()) else {
                            break None;
                        };
                        if let Some(start) = path.iter().position(|id| *id == parent) {
                            break path[start..]
                                .iter()
                                .filter(|id| group.contains(*id))
                                .max()
                                .cloned();
                        }
                        path.push(parent);
                    }
                }
                Err(TreeError::NameTaken { parent, name }) => {
                    let key = name_key(&name);
                    s.iter()
                        .filter(|(id, shape)| {
                            group.contains(*id)
                                && shape.parent == parent
                                && name_key(&shape.name) == key
                        })
                        .map(|(id, _)| id.clone())
                        .last()
                }
                Err(TreeError::RootStored) => None,
            };
            match culprit {
                Some(id) => {
                    group.remove(&id);
                }
                None => break,
            }
        }
        BTreeSet::new()
    }

    /// S := R for one node, made only when S stays a valid tree.
    fn take_remote(&mut self, id: &str) -> bool {
        let next = self.r.live(id).cloned();
        let has_children = self.s.iter().any(|(_, shape)| shape.parent == id);
        match &next {
            None if has_children => return false,
            None => {}
            Some(shape) => {
                if has_children && shape.kind != Kind::Dir {
                    return false;
                }
                if shape.parent != self.s.root {
                    match self.s.live(&shape.parent) {
                        Some(parent) if parent.kind == Kind::Dir => {}
                        _ => return false,
                    }
                }
                let mut ancestor = shape.parent.as_str();
                while ancestor != self.s.root {
                    if ancestor == id {
                        return false;
                    }
                    match self.s.live(ancestor) {
                        Some(parent) => ancestor = &parent.parent,
                        None => return false,
                    }
                }
                let key = name_key(&shape.name);
                if self.s.iter().any(|(other, sibling)| {
                    other != id && sibling.parent == shape.parent && name_key(&sibling.name) == key
                }) {
                    return false;
                }
            }
        }
        self.set_synced_unmarked(id, next);
        true
    }

    fn set_synced_unmarked(&mut self, id: &str, shape: Option<Shape>) {
        match shape {
            Some(shape) => {
                self.s.insert(id.to_owned(), shape);
            }
            None => {
                self.s.remove(id);
            }
        }
    }

    /// The invariants of section 2.2 for all three trees.
    pub fn check(&self) -> Result<(), (char, TreeError)> {
        self.r.check(Names::ByKey).map_err(|e| ('R', e))?;
        self.s.check(Names::ByKey).map_err(|e| ('S', e))?;
        self.l.check(Names::Exact).map_err(|e| ('L', e))
    }
}
