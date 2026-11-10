// SPDX-License-Identifier: Apache-2.0

//! Classifying a node (docs/design/sync-engine.md section 2.3) and the dirty set the planner reads.

use std::collections::{BTreeMap, BTreeSet};

use crate::names::name_key;
use crate::tree::{
    Kind, LocalNode, Names, NodeId, RemoteNode, Shape, Tree, TreeError, same, same_content,
};

/// The most mutually dependent [`Class::Same`] updates [`Trees::settle`] searches as one group.
/// Every subset of a group is tried, so the search costs up to 2^n checks of n nodes.
pub const SETTLE_GROUP_LIMIT: usize = 12;

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
    unsearched: BTreeSet<NodeId>,
    reverted: BTreeSet<NodeId>,
    parked_as: BTreeMap<NodeId, NodeId>,
    waiting: BTreeMap<NodeId, Option<Shape>>,
}

impl Trees {
    pub fn new(root: impl Into<NodeId>) -> Self {
        let root = root.into();
        Self {
            r: Tree::new(root.clone()),
            l: Tree::new(root.clone()),
            s: Tree::new(root),
            dirty: BTreeSet::new(),
            unsearched: BTreeSet::new(),
            reverted: BTreeSet::new(),
            parked_as: BTreeMap::new(),
            waiting: BTreeMap::new(),
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
        if !node.parked {
            self.parked_as.remove(&id);
        }
        self.dirty.insert(id.clone());
        self.l.insert(id, node);
    }

    pub fn remove_local(&mut self, id: &str) {
        self.parked_as.remove(id);
        if self.l.remove(id).is_some() {
            self.dirty.insert(id.to_owned());
        }
    }

    /// The name of a parked node's entry in the parked directory: its ID, or, for a node re-keyed
    /// while parked, the ID it was parked under. The entry keeps that name until it leaves the
    /// parked directory.
    pub fn parked_name<'a>(&'a self, id: &'a str) -> &'a str {
        self.parked_as.get(id).map_or(id, String::as_str)
    }

    /// Records that parked node `id` sits in the parked directory under `name`.
    pub fn park_as(&mut self, id: NodeId, name: NodeId) {
        if id != name {
            self.parked_as.insert(id, name);
        }
    }

    /// S updates of acknowledged ops that would not leave S a valid tree yet, by node. The commit
    /// layer makes each as soon as it fits, or drops it once S agrees with L for the node anyway;
    /// until then the node is left out of planning.
    pub fn waiting(&self) -> &BTreeMap<NodeId, Option<Shape>> {
        &self.waiting
    }

    pub(crate) fn take_waiting(&mut self) -> BTreeMap<NodeId, Option<Shape>> {
        std::mem::take(&mut self.waiting)
    }

    pub(crate) fn wait(&mut self, id: NodeId, shape: Option<Shape>) {
        self.dirty.insert(id.clone());
        self.waiting.insert(id, shape);
    }

    /// Sets S, or clears it with `None` (S := ⊥).
    pub fn set_synced(&mut self, id: NodeId, shape: Option<Shape>) {
        self.set_synced_unmarked(&id, shape);
        self.dirty.insert(id);
    }

    /// Sets S for one node, or clears it, only when S stays a valid tree. Returns whether it did.
    pub fn set_synced_if_valid(&mut self, id: NodeId, shape: Option<Shape>) -> bool {
        if !self.fits_synced(&id, shape.as_ref()) {
            return false;
        }
        self.set_synced(id, shape);
        true
    }

    pub fn class(&self, id: &str) -> Class {
        classify(self.l.live(id), self.s.live(id), self.r.live(id))
    }

    /// Whether L holds content that S does not, or, while the node's S update waits, content the
    /// waiting shape does not: an edit not uploaded yet.
    pub fn edited(&self, id: &str) -> bool {
        let content = self.l.get(id).and_then(|node| node.shape.content.as_ref());
        let synced = match self.waiting.get(id) {
            Some(shape) => shape.as_ref(),
            None => self.s.live(id),
        };
        match (content, synced.and_then(|shape| shape.content.as_ref())) {
            (Some(local), Some(synced)) => !same_content(local, synced),
            (local, _) => local.is_some(),
        }
    }

    /// Settles what needs no decision: a node changed the same way on both sides takes S := R, and
    /// a node in sync leaves the dirty set unless it is parked. S stays a valid tree, so an update that would orphan a
    /// child, put a node under a missing or non-directory parent, close a cycle or take a sibling's
    /// `name_key` is made only together with the updates it depends on. Settling stops only when no
    /// non-empty set of the remaining [`Class::Same`] updates can be made together with S still valid,
    /// searching each group of updates that depend on each other exhaustively. A group of more
    /// than [`SETTLE_GROUP_LIMIT`] updates is not searched: its nodes stay dirty as [`Class::Same`] and
    /// [`Trees::unsearched`] lists them. Returns every dirty node that is left, with its class, in
    /// `node_id` order.
    pub fn settle(&mut self) -> Vec<(NodeId, Class)> {
        self.reverted
            .retain(|id| match (self.l.get(id), self.r.live(id)) {
                (Some(local), Some(remote)) => !local.shape.same_position(remote),
                (local, remote) => local.is_some() || remote.is_some(),
            });
        let mut pending = Vec::new();
        let mut work = Vec::new();
        for id in std::mem::take(&mut self.dirty) {
            match self.class(&id) {
                Class::InSync if !self.l.get(&id).is_some_and(|node| node.parked) => {}
                Class::Same => pending.push(id),
                class => work.push((id, class)),
            }
        }
        let parked: Vec<NodeId> = pending
            .iter()
            .filter(|id| self.l.get(id).is_some_and(|node| node.parked))
            .cloned()
            .collect();
        self.unsearched.clear();
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
        for id in parked.into_iter().filter(|id| !pending.contains(id)) {
            let class = self.class(&id);
            work.push((id, class));
        }
        work.extend(pending.into_iter().map(|id| (id, Class::Same)));
        work.sort_by(|a, b| a.0.cmp(&b.0));
        self.dirty = work.iter().map(|(id, _)| id.clone()).collect();
        work
    }

    /// Nodes whose local position the server will not take: a move that closes a cycle against R,
    /// a create or move the server renamed, a folder merged into one L holds elsewhere with the
    /// nodes created in it, and folders L deleted on the way from such a node to the root. The
    /// commit planner sends no position change for them but still sends their content edits and deletes; the
    /// effects put each at its R position without replacing its content, and [`Trees::settle`]
    /// forgets it once L has it there.
    pub fn reverted(&self) -> &BTreeSet<NodeId> {
        &self.reverted
    }

    pub fn revert(&mut self, id: NodeId) {
        self.dirty.insert(id.clone());
        self.reverted.insert(id);
    }

    /// The [`Class::Same`] nodes the last [`Trees::settle`] left without searching, because the
    /// group of updates they belong to has more than [`SETTLE_GROUP_LIMIT`] members.
    pub fn unsearched(&self) -> &BTreeSet<NodeId> {
        &self.unsearched
    }

    /// S := R together for a non-empty set of `pending` nodes whose updates leave S valid, when one
    /// exists. Returns the nodes it updated, or an empty set.
    fn take_remote_together(&mut self, pending: &[NodeId]) -> BTreeSet<NodeId> {
        let (taken, unsearched) = self.search_together(pending);
        self.unsearched = unsearched;
        for id in &taken {
            let next = self.r.live(id).cloned();
            self.set_synced_unmarked(id, next);
        }
        debug_assert_eq!(self.s.check(Names::ByKey), Ok(()));
        taken
    }

    /// Finds a non-empty set of `pending` nodes whose updates leave S valid when made together, and
    /// the nodes it did not search.
    ///
    /// Nodes outside the candidates keep their S shape. A candidate whose update breaks S because
    /// of such a fixed node alone (a fixed child, a fixed parent that is not a live directory, a
    /// fixed sibling holding its name key, or a parent chain through fixed nodes that leads back to
    /// it) can never be updated, so it becomes fixed too, until no more do. An update then reaches
    /// other nodes only through the first candidate on its new or old parent chain and through the
    /// candidates holding its new name key in S. Those links split the candidates into groups
    /// where any valid set's part in one group is valid on its own, so searching every subset of
    /// each group, up to [`SETTLE_GROUP_LIMIT`] members, finds a valid set whenever one exists.
    fn search_together(&self, pending: &[NodeId]) -> (BTreeSet<NodeId>, BTreeSet<NodeId>) {
        let root = self.s.root.as_str();
        let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        let mut holders: BTreeMap<(&str, String), Vec<&str>> = BTreeMap::new();
        for (id, shape) in self.s.iter() {
            children.entry(&shape.parent).or_default().push(id);
            holders
                .entry((&shape.parent, name_key(&shape.name)))
                .or_default()
                .push(id);
        }
        let mut candidates: BTreeSet<&str> = pending.iter().map(String::as_str).collect();
        loop {
            let never: Vec<&str> = candidates
                .iter()
                .copied()
                .filter(|&id| {
                    let fixed = |other: &&str| !candidates.contains(other);
                    let fixed_child = children.get(id).is_some_and(|c| c.iter().any(fixed));
                    let Some(shape) = self.r.live(id) else {
                        return fixed_child;
                    };
                    (fixed_child && shape.kind != Kind::Dir)
                        || (shape.parent != root
                            && fixed(&shape.parent.as_str())
                            && !self
                                .s
                                .live(&shape.parent)
                                .is_some_and(|p| p.kind == Kind::Dir))
                        || holders
                            .get(&(shape.parent.as_str(), name_key(&shape.name)))
                            .is_some_and(|h| h.iter().any(|h| *h != id && fixed(h)))
                        || self
                            .above(&shape.parent, &candidates)
                            .is_none_or(|up| up == id)
                })
                .collect();
            if never.is_empty() {
                break;
            }
            for id in never {
                candidates.remove(id);
            }
        }
        let mut next: BTreeMap<(&str, bool), &str> = BTreeMap::new();
        let mut links: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for &id in &candidates {
            for (taken, shape) in [(true, self.r.live(id)), (false, self.s.live(id))] {
                let Some(up) = shape
                    .and_then(|shape| self.above(&shape.parent, &candidates))
                    .filter(|up| *up != root)
                else {
                    continue;
                };
                next.insert((id, taken), up);
                links.entry(id).or_default().push(up);
                links.entry(up).or_default().push(id);
            }
            if let Some(shape) = self.r.live(id) {
                let key = (shape.parent.as_str(), name_key(&shape.name));
                for &other in holders.get(&key).into_iter().flatten() {
                    if other != id && candidates.contains(other) {
                        links.entry(id).or_default().push(other);
                        links.entry(other).or_default().push(id);
                    }
                }
            }
        }
        let mut unsearched = BTreeSet::new();
        let mut seen = BTreeSet::new();
        for &start in &candidates {
            if !seen.insert(start) {
                continue;
            }
            let mut group = vec![start];
            let mut i = 0;
            while let Some(&id) = group.get(i) {
                i += 1;
                for &other in links.get(id).into_iter().flatten() {
                    if seen.insert(other) {
                        group.push(other);
                    }
                }
            }
            if group.len() > SETTLE_GROUP_LIMIT {
                unsearched.extend(group.iter().map(|id| id.to_string()));
                continue;
            }
            for mask in (1..1u32 << group.len()).rev() {
                let taken = |id: &str| {
                    group
                        .iter()
                        .position(|member| *member == id)
                        .is_some_and(|i| mask >> i & 1 == 1)
                };
                let entry = |id: &str| {
                    if taken(id) {
                        self.r.live(id)
                    } else {
                        self.s.live(id)
                    }
                };
                let fits = group.iter().all(|&id| {
                    let Some(shape) = entry(id) else {
                        return true;
                    };
                    if shape.parent != root
                        && !entry(&shape.parent).is_some_and(|p| p.kind == Kind::Dir)
                    {
                        return false;
                    }
                    if taken(id)
                        && holders
                            .get(&(shape.parent.as_str(), name_key(&shape.name)))
                            .is_some_and(|h| h.iter().any(|h| *h != id && !taken(h)))
                    {
                        return false;
                    }
                    let mut current = id;
                    for _ in 0..group.len() {
                        match next.get(&(current, taken(current))) {
                            Some(up) => current = up,
                            None => return true,
                        }
                    }
                    false
                });
                if fits {
                    let taken = group.iter().filter(|id| taken(id));
                    return (taken.map(|id| id.to_string()).collect(), unsearched);
                }
            }
        }
        (BTreeSet::new(), unsearched)
    }

    /// Follows S parents from `start` past nodes that are not candidates, and returns the first
    /// candidate or the root it reaches, or `None` when the chain breaks.
    fn above<'a>(&'a self, start: &'a str, candidates: &BTreeSet<&str>) -> Option<&'a str> {
        let mut current = start;
        for _ in 0..=self.s.len() {
            if current == self.s.root || candidates.contains(current) {
                return Some(current);
            }
            current = &self.s.live(current)?.parent;
        }
        None
    }

    /// S := R for one node, made only when S stays a valid tree.
    fn take_remote(&mut self, id: &str) -> bool {
        let next = self.r.live(id).cloned();
        if !self.fits_synced(id, next.as_ref()) {
            return false;
        }
        self.set_synced_unmarked(id, next);
        true
    }

    /// True when S stays a valid tree with `next` as the node's shape.
    fn fits_synced(&self, id: &str, next: Option<&Shape>) -> bool {
        let has_children = self.s.iter().any(|(_, shape)| shape.parent == id);
        let Some(shape) = next else {
            return !has_children;
        };
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
        for _ in 0..=self.s.len() {
            if ancestor == self.s.root {
                break;
            }
            if ancestor == id {
                return false;
            }
            match self.s.live(ancestor) {
                Some(parent) => ancestor = &parent.parent,
                None => return false,
            }
        }
        if ancestor != self.s.root {
            return false;
        }
        let key = name_key(&shape.name);
        !self.s.iter().any(|(other, sibling)| {
            other != id && sibling.parent == shape.parent && name_key(&sibling.name) == key
        })
    }

    fn set_synced_unmarked(&mut self, id: &str, shape: Option<Shape>) {
        self.waiting.remove(id);
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
