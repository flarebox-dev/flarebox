// SPDX-License-Identifier: Apache-2.0

//! Classifying a node (docs/design/sync-engine.md section 2.3) and the dirty set the planner reads.

use std::collections::BTreeSet;

use crate::tree::{LocalNode, Names, NodeId, RemoteNode, Shape, Tree, TreeError, same};

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
        self.dirty.insert(id.clone());
        match shape {
            Some(shape) => {
                self.s.insert(id, shape);
            }
            None => {
                self.s.remove(&id);
            }
        }
    }

    pub fn class(&self, id: &str) -> Class {
        classify(self.l.live(id), self.s.live(id), self.r.live(id))
    }

    /// Settles what needs no decision: a node changed the same way on both sides takes S := R, and
    /// a node in sync leaves the dirty set. Returns every dirty node that is left, with its class.
    pub fn settle(&mut self) -> Vec<(NodeId, Class)> {
        let mut work = Vec::new();
        for id in std::mem::take(&mut self.dirty) {
            match self.class(&id) {
                Class::InSync => {}
                Class::Same => {
                    let shape = self.r.live(&id).cloned();
                    match shape {
                        Some(shape) => {
                            self.s.insert(id, shape);
                        }
                        None => {
                            self.s.remove(&id);
                        }
                    }
                }
                class => work.push((id, class)),
            }
        }
        self.dirty = work.iter().map(|(id, _)| id.clone()).collect();
        work
    }

    /// The invariants of section 2.2 for all three trees.
    pub fn check(&self) -> Result<(), (char, TreeError)> {
        self.r.check(Names::ByKey).map_err(|e| ('R', e))?;
        self.s.check(Names::ByKey).map_err(|e| ('S', e))?;
        self.l.check(Names::Exact).map_err(|e| ('L', e))
    }
}
