// SPDX-License-Identifier: Apache-2.0

//! Classifying a node (docs/design/sync-engine.md section 2.3) and the dirty set the planner reads.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use flarebox_ignore::Status;

use crate::ignore::{LocalRules, Matcher, RuleFile, is_rules_file};
use crate::names::name_key;
use crate::state::{StateError, TreesState};
use crate::tree::{
    FileIdentity, Kind, LocalNode, Names, Node, NodeId, RemoteNode, Shape, Tree, TreeError, same,
    same_content,
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

/// Whether `tree` holds `id` live and placed below one of `folders`, through live, placed folders.
fn below<N: Node>(tree: &Tree<N>, id: &str, folders: &BTreeSet<NodeId>) -> bool {
    let held = |id: &str| {
        tree.get(id)
            .filter(|node| node.is_live() && node.is_placed())
    };
    let Some(mut current) = held(id) else {
        return false;
    };
    for _ in 0..tree.len() {
        let parent = current.shape().parent.as_str();
        if folders.contains(parent) {
            return true;
        }
        if parent == tree.root {
            return false;
        }
        match held(parent) {
            Some(node) => current = node,
            None => return false,
        }
    }
    false
}

/// The names from the root to `id` in `tree` with the node's kind, or `None` when the tree does not
/// hold the node live and placed under live, placed folders.
fn path<'a, N: Node>(tree: &'a Tree<N>, id: &str) -> Option<(Vec<&'a str>, Kind)> {
    let held = |id: &str| {
        tree.get(id)
            .filter(|node| node.is_live() && node.is_placed())
    };
    let shape = held(id)?.shape();
    let mut names = vec![shape.name.as_str()];
    let mut current = shape.parent.as_str();
    for _ in 0..=tree.len() {
        if current == tree.root {
            names.reverse();
            return Some((names, shape.kind));
        }
        let parent = held(current)?.shape();
        names.push(&parent.name);
        current = &parent.parent;
    }
    None
}

/// The nodes of a tree by parent and the `name_key` of their name.
#[derive(Clone, Debug, Default)]
struct ByName(BTreeMap<(NodeId, String), BTreeSet<NodeId>>);

impl ByName {
    fn insert(&mut self, id: &str, shape: &Shape) {
        let key = (shape.parent.clone(), name_key(&shape.name));
        self.0.entry(key).or_default().insert(id.to_owned());
    }

    fn remove(&mut self, id: &str, shape: &Shape) {
        let key = (shape.parent.clone(), name_key(&shape.name));
        if let Some(ids) = self.0.get_mut(&key) {
            ids.remove(id);
            if ids.is_empty() {
                self.0.remove(&key);
            }
        }
    }

    /// The nodes under `parent` whose name has the `name_key` of `name`.
    fn holders<'a>(&'a self, parent: &str, name: &str) -> impl Iterator<Item = &'a NodeId> + 'a {
        let key = (parent.to_owned(), name_key(name));
        self.0.get(&key).into_iter().flatten()
    }

    fn children<'a>(&'a self, parent: &'a str) -> impl Iterator<Item = &'a NodeId> + 'a {
        self.0
            .range((parent.to_owned(), String::new())..)
            .take_while(move |((of, _), _)| of == parent)
            .flat_map(|(_, ids)| ids)
    }
}

/// R, L and S of one space, and the nodes whose entries changed since they were last found in sync.
/// Every mutation goes through these methods, so a change in any tree marks its node dirty, and
/// [`Trees::settle`] and the planner look only at what changed.
///
/// The trees also own the ignore rules of this device (section 5.10) and decide with them which
/// nodes are excluded here ([`Trees::excluded`]). The rules are not part of the stored state: the
/// driver passes them again after a restart, and nothing is planned until it has
/// ([`Trees::rules_known`]).
#[derive(Clone, Debug)]
pub struct Trees {
    r: Tree<RemoteNode>,
    l: Tree<LocalNode>,
    s: Tree<Shape>,
    dirty: BTreeSet<NodeId>,
    unsearched: BTreeSet<NodeId>,
    reverted: BTreeSet<NodeId>,
    parked_as: BTreeMap<NodeId, NodeId>,
    waiting: BTreeMap<NodeId, Option<Shape>>,
    /// The nodes whose content the commit in flight updates, each with whether S's content
    /// changed since the commit was built.
    watched: BTreeMap<NodeId, bool>,
    ignore: Matcher,
    /// Nodes whose local copy the user moved into an ignored folder that is no node. L keeps the
    /// entry each had before the move.
    outside: BTreeSet<NodeId>,
    /// False from a restore until the rules of a full scan are in force.
    rules_known: bool,
    /// The folders whose rules the last scan of each could not read, by node: a folder renamed is the same folder,
    /// and two folders whose names differ only in case are two folders.
    unknown_rules: BTreeSet<NodeId>,
    /// Dirty nodes marked since the last settle, the only ones whose class can have changed.
    unsettled: BTreeSet<NodeId>,
    /// Dirty nodes the last settle left as [`Class::Same`], which the next one tries again.
    same: BTreeSet<NodeId>,
    /// Nodes whose entry in R, L or S changed since [`Trees::take_changed`].
    changed: BTreeSet<NodeId>,
    /// The placed nodes of L.
    l_names: ByName,
    /// Every node of L, placed or parked, by its identity.
    l_identities: HashMap<FileIdentity, BTreeSet<NodeId>>,
    /// The nodes of S.
    s_names: ByName,
    /// Nodes whose S position neither placed L nor R holds, which [`Trees::settle`] tries to
    /// release every time, since whether it may depends on the rules and on other nodes.
    orphaned: BTreeSet<NodeId>,
    /// The live files of R that are rules files.
    remote_rules_files: BTreeSet<NodeId>,
}

/// Equal when R, L, S and the state the store keeps are equal. What the trees track only until the
/// next settle or checkpoint is left out.
impl PartialEq for Trees {
    fn eq(&self, other: &Self) -> bool {
        self.r == other.r
            && self.l == other.l
            && self.s == other.s
            && self.state() == other.state()
            && self.ignore == other.ignore
            && self.outside == other.outside
            && self.rules_known == other.rules_known
            && self.unknown_rules == other.unknown_rules
    }
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
            watched: BTreeMap::new(),
            ignore: Matcher::new(LocalRules::default()),
            outside: BTreeSet::new(),
            rules_known: true,
            unknown_rules: BTreeSet::new(),
            unsettled: BTreeSet::new(),
            same: BTreeSet::new(),
            changed: BTreeSet::new(),
            l_names: ByName::default(),
            l_identities: HashMap::new(),
            s_names: ByName::default(),
            orphaned: BTreeSet::new(),
            remote_rules_files: BTreeSet::new(),
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

    /// The nodes whose entry in R, L or S changed since the last call, which empties the set. Each
    /// mutation adds its node, and so does [`Trees::settle`] for each node whose S it updates.
    pub fn take_changed(&mut self) -> BTreeSet<NodeId> {
        std::mem::take(&mut self.changed)
    }

    /// The placed nodes of L under `parent`, by the `name_key` of their names.
    pub fn local_children<'a>(&'a self, parent: &'a str) -> impl Iterator<Item = &'a NodeId> + 'a {
        self.l_names.children(parent)
    }

    /// The placed node of L under `parent` named exactly `name`.
    pub fn local_child(&self, parent: &str, name: &str) -> Option<&NodeId> {
        (self.l_names.holders(parent, name))
            .filter(|id| self.l.get(id).is_some_and(|node| node.shape.name == name))
            .last()
    }

    /// A node of L, placed or parked, with `identity`: the greatest ID when several have it.
    pub fn local_with_identity(&self, identity: &FileIdentity) -> Option<&NodeId> {
        self.l_identities.get(identity).and_then(|ids| ids.last())
    }

    /// The placed nodes of L under `parent` whose name has the `name_key` of `name`.
    pub fn local_holders<'a>(
        &'a self,
        parent: &str,
        name: &str,
    ) -> impl Iterator<Item = &'a NodeId> + 'a {
        self.l_names.holders(parent, name)
    }

    fn mark(&mut self, id: &str) {
        self.dirty.insert(id.to_owned());
        self.unsettled.insert(id.to_owned());
        self.changed.insert(id.to_owned());
    }

    pub fn set_remote(&mut self, id: NodeId, node: RemoteNode) {
        self.mark(&id);
        if node.is_live() && node.shape.kind == Kind::File && is_rules_file(&node.shape.name) {
            self.remote_rules_files.insert(id.clone());
        } else {
            self.remote_rules_files.remove(&id);
        }
        self.r.insert(id, node);
    }

    pub fn remove_remote(&mut self, id: &str) {
        self.remote_rules_files.remove(id);
        if self.r.remove(id).is_some() {
            self.mark(id);
        }
    }

    /// The live files of R that are rules files ([`is_rules_file`]).
    pub fn remote_rules_files(&self) -> &BTreeSet<NodeId> {
        &self.remote_rules_files
    }

    /// Sets L. A reverted node that L now holds at a position other than its old one and R's was
    /// moved by the user, and is no longer reverted: that move is committed instead.
    pub fn set_local(&mut self, id: NodeId, node: LocalNode) {
        if let Some(old) = self.l.get(&id) {
            let moved = !old.parked && !node.parked && !old.shape.same_position(&node.shape);
            let to_remote =
                (self.r.live(&id)).is_some_and(|remote| node.shape.same_position(remote));
            if moved && !to_remote {
                self.reverted.remove(&id);
            }
            if !old.parked {
                self.l_names.remove(&id, &old.shape);
            }
            let identity = old.identity;
            self.forget_identity(&id, &identity);
        }
        if !node.parked {
            self.parked_as.remove(&id);
            self.l_names.insert(&id, &node.shape);
        }
        if self.outside.remove(&id) {
            let children: Vec<NodeId> = self.l_names.children(&id).cloned().collect();
            self.outside.extend(children);
        }
        (self.l_identities.entry(node.identity).or_default()).insert(id.clone());
        self.mark(&id);
        self.l.insert(id, node);
    }

    pub fn remove_local(&mut self, id: &str) {
        self.parked_as.remove(id);
        self.unknown_rules.remove(id);
        self.outside.remove(id);
        if let Some(old) = self.l.remove(id) {
            if !old.parked {
                self.l_names.remove(id, &old.shape);
            }
            self.forget_identity(id, &old.identity);
            self.mark(id);
        }
    }

    fn forget_identity(&mut self, id: &str, identity: &FileIdentity) {
        if let Some(ids) = self.l_identities.get_mut(identity) {
            ids.remove(id);
            if ids.is_empty() {
                self.l_identities.remove(identity);
            }
        }
    }

    /// The rules in force on this device. New trees hold the built-in exclusions only.
    pub fn ignore(&self) -> &Matcher {
        &self.ignore
    }

    /// Replaces this device's include and exclude lists, which are never synced.
    pub fn set_local_rules(&mut self, local: LocalRules) {
        self.ignore.set_local(local, std::iter::empty());
    }

    /// Sets the rules of the `.fbignore` file in the folder at `dir`, the names from the root to
    /// it, or removes them with `None`. The driver parses each rules file it observes on this
    /// device's disk and passes the rules here with the observation.
    pub fn set_rules<S: AsRef<str>>(&mut self, dir: &[S], rules: Option<RuleFile>) {
        self.ignore.set_rules(dir, rules, std::iter::empty());
    }

    /// Forgets the rules of every `.fbignore` file, for a caller about to pass all of them again.
    pub fn clear_rules(&mut self) {
        self.ignore.clear_rules();
    }

    /// Records that a scan could not read the folder `id` of L, or the root, so the rules below it are unknown until
    /// a scan reads it again ([`Trees::set_rules_known`]) or L no longer holds it. Meanwhile nothing is planned for a
    /// node below it ([`Trees::held`]).
    pub fn set_rules_unknown(&mut self, id: &str) {
        if id == self.l.root || self.l.get(id).is_some() {
            self.unknown_rules.insert(id.to_owned());
        }
    }

    /// The folders whose rules are unknown ([`Trees::set_rules_unknown`]).
    pub fn unknown_rules(&self) -> &BTreeSet<NodeId> {
        &self.unknown_rules
    }

    /// Records that a scan read the folder `id` and passed its rules, or found it has none.
    pub fn set_rules_known(&mut self, id: &str) {
        self.unknown_rules.remove(id);
    }

    /// Whether L, S or R places the node below a folder whose rules are unknown
    /// ([`Trees::set_rules_unknown`]). Neither an effect nor a commit is planned for it.
    pub fn held(&self, id: &str) -> bool {
        !self.unknown_rules.is_empty()
            && (below(&self.l, id, &self.unknown_rules)
                || below(&self.s, id, &self.unknown_rules)
                || below(&self.r, id, &self.unknown_rules))
    }

    /// The caller has passed the rules files of a full scan made since the last restore, and the
    /// device-local list. Planning starts again.
    pub fn rules_scanned(&mut self) {
        self.rules_known = true;
    }

    /// False from [`Trees::restore`] until [`Trees::rules_scanned`]. The stored state does not hold
    /// the rules, so until the first full scan has reported them an excluded node would look
    /// like any other; meanwhile no effect and no commit is planned.
    pub fn rules_known(&self) -> bool {
        self.rules_known
    }

    /// Records that the user moved the local copy of `id` into an ignored folder that is no node,
    /// so L cannot hold its new position. L keeps the entry as it was, and the node is excluded
    /// until an observation places it again or removes it. When the node is placed again, its
    /// children are recorded in its place: the user may have moved them elsewhere while it was
    /// excluded, so each stays excluded until an observation places it too. This is not part of
    /// the stored state: after a restart the entry is missing from its path again, and the scan
    /// that finds it under the ignored folder reports the move once more.
    pub fn ignore_local(&mut self, id: &str) {
        if self.l.get(id).is_some() {
            self.outside.insert(id.to_owned());
        }
    }

    /// Whether S holds the node at a position that neither placed L nor R holds, the nodes
    /// [`Trees::released`] may give that position up for.
    fn orphaned_in_s(&self, id: &str) -> bool {
        let Some(synced) = self.s.live(id) else {
            return false;
        };
        let held = |shape: Option<&Shape>| shape.is_some_and(|shape| shape.same_position(synced));
        let placed = (self.l.get(id))
            .filter(|node| !node.parked)
            .map(|node| &node.shape);
        !held(placed) && !held(self.r.live(id))
    }

    /// What S may become for an excluded node whose S position neither L nor R holds any more,
    /// in order of preference, or `None` when the node is not one. A parked node holds no
    /// position in L, so one that the rules now leave out gives up its S position too, also
    /// while its old name is taken and it cannot be moved back. Nothing is planned for the node,
    /// so no op would ever move its S entry, and the name it holds in S would keep every other
    /// node from being acknowledged there. With the node in L and live in R, S takes R's
    /// position, or else the position the local copy has, and keeps its content, so the content
    /// is still compared with the last synced one when the node is included again. Otherwise, or
    /// when S can hold the node at neither position, S drops the node, which is then resolved
    /// like a node never synced: what R holds is downloaded, and what only L holds is uploaded.
    fn released(&self, id: &str) -> Option<Vec<Option<Shape>>> {
        let synced = self.s.live(id)?;
        let local = self.l.get(id);
        let remote = self.r.live(id);
        let held = |shape: Option<&Shape>| shape.is_some_and(|shape| shape.same_position(synced));
        let placed = local.filter(|node| !node.parked).map(|node| &node.shape);
        if held(placed) || held(remote) || !self.ignored(id) {
            return None;
        }
        let at = |shape: &Shape| {
            Some(Shape {
                parent: shape.parent.clone(),
                name: shape.name.clone(),
                ..synced.clone()
            })
        };
        let mut next = Vec::new();
        if local.is_some()
            && let Some(remote) = remote
        {
            next.push(at(remote));
            next.extend(placed.map(at));
        }
        next.push(None);
        Some(next)
    }

    /// The nodes recorded by [`Trees::ignore_local`] that no observation has placed or removed
    /// since.
    pub fn ignored_local(&self) -> &BTreeSet<NodeId> {
        &self.outside
    }

    /// Whether the rules exclude the entry of `kind` at `path`, the names from the root to it.
    pub fn ignores<S: AsRef<str>>(&self, path: &[S], kind: Kind) -> bool {
        let kind = match kind {
            Kind::Dir => flarebox_ignore::Kind::Dir,
            Kind::File => flarebox_ignore::Kind::File,
        };
        self.ignore.status(path, kind) != Status::Included
    }

    /// Whether the node is excluded on this device (section 5.10): its position in R, S or L
    /// matches the rules, or its local copy, or a folder L holds it under, was moved into an
    /// ignored folder. Nothing is planned for an excluded node, in either direction. Its S entry
    /// stays the merge base it was: S takes only what L and R both hold, and gives up a position
    /// neither holds any more, so that the entry never keeps another node out of S. A node that L holds at an included position
    /// different from S's is not excluded: the user moved it out, and the move is planned like
    /// any local move. A parked node is not excluded either until the effects have placed it
    /// again, since it must not stay in the parked directory ([`Trees::ignored`]).
    pub fn excluded(&self, id: &str) -> bool {
        !self.l.get(id).is_some_and(|node| node.parked) && self.ignored(id)
    }

    /// Whether L no longer holds the node while R holds an excluded node below it. The commit planner sends no
    /// `Delete` for such a folder, which stays on the server with what it holds that this device leaves out
    /// (section 5.10), and the effects do not create it again, so nothing is left to sync for it.
    pub fn kept_for_excluded(&self, id: &str) -> bool {
        if self.l.get(id).is_some() || self.r.live(id).is_none() {
            return false;
        }
        let below = |node: &str| {
            let mut current = node;
            for _ in 0..self.r.len() {
                match self.r.live(current) {
                    Some(shape) if shape.parent == id => return true,
                    Some(shape) => current = &shape.parent,
                    None => return false,
                }
            }
            false
        };
        (self.r.iter()).any(|(node, _)| below(node) && self.excluded(node))
    }

    /// Whether the node is outside the sync tree on this device, as [`Trees::excluded`] decides
    /// it, also when the node is parked. The effects move a parked node that is ignored back to
    /// the position L keeps for it, and it is excluded from then on.
    pub fn ignored(&self, id: &str) -> bool {
        let mut above = id;
        for _ in 0..=self.l.len() {
            if self.outside.contains(above) {
                return true;
            }
            match self.l.get(above) {
                Some(node) => above = &node.shape.parent,
                None => break,
            }
        }
        let ignored =
            |path: Option<(Vec<&str>, Kind)>| path.map(|(names, kind)| self.ignores(&names, kind));
        let local = ignored(path(&self.l, id));
        let moved = match (self.l.get(id), self.s.live(id)) {
            (Some(local), Some(synced)) => !local.shape.same_position(synced),
            _ => false,
        };
        if local == Some(false) && moved {
            return false;
        }
        local == Some(true)
            || ignored(path(&self.s, id)) == Some(true)
            || ignored(path(&self.r, id)) == Some(true)
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
    /// layer makes each as soon as it fits, falls back to S's position once R and L have both left
    /// the waiting one, and drops one once neither L nor R holds the node; until then the node is
    /// left out of planning.
    pub fn waiting(&self) -> &BTreeMap<NodeId, Option<Shape>> {
        &self.waiting
    }

    /// Watches S's content of `ids`, the nodes whose content the commit just built updates, and
    /// stops watching any other node.
    pub fn watch_content(&mut self, ids: impl IntoIterator<Item = NodeId>) {
        self.watched = ids.into_iter().map(|id| (id, false)).collect();
    }

    /// Whether S's content of `id` changed since [`Trees::watch_content`] started watching it,
    /// even if it changed back.
    pub fn took_content(&self, id: &str) -> bool {
        self.watched.get(id) == Some(&true)
    }

    pub(crate) fn take_waiting(&mut self) -> BTreeMap<NodeId, Option<Shape>> {
        std::mem::take(&mut self.waiting)
    }

    pub(crate) fn wait(&mut self, id: NodeId, shape: Option<Shape>) {
        self.mark(&id);
        self.waiting.insert(id, shape);
    }

    /// Sets S, or clears it with `None` (S := ⊥).
    pub fn set_synced(&mut self, id: NodeId, shape: Option<Shape>) {
        self.set_synced_unmarked(&id, shape);
        self.mark(&id);
    }

    /// Sets S for one node, or clears it, only when S stays a valid tree. Returns whether it did.
    pub fn set_synced_if_valid(&mut self, id: NodeId, shape: Option<Shape>) -> bool {
        if !self.fits_synced(&id, shape.as_ref()) {
            return false;
        }
        self.set_synced(id, shape);
        true
    }

    /// Forgets what the server may have lost, after a bootstrap from a server that is behind what
    /// this device applied (design §4.4). S forgets every node that R does not hold at all or holds
    /// live with another shape, and every node S holds below one. A node L still holds is then
    /// created again instead of being taken for a remote delete, and one whose content differs from
    /// R's is kept beside R's as a conflict copy instead of being replaced by R's older content. A
    /// node R holds in the trash keeps S, so it stays a remote delete, also below a node S forgets.
    /// S then forgets each such ancestor only once the delete has taken what it held: the update
    /// waits ([`Trees::waiting`]) and is made as soon as it fits.
    pub fn forget_lost(&mut self) {
        let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (id, shape) in self.s.iter() {
            children.entry(&shape.parent).or_default().push(id);
        }
        let mut lost: BTreeSet<&str> = (self.s.iter())
            .filter(|(id, synced)| {
                self.r.get(id).is_none()
                    || (self.r.live(id)).is_some_and(|remote| !same(Some(remote), Some(*synced)))
            })
            .map(|(id, _)| id.as_str())
            .collect();
        let mut below: Vec<&str> = lost.iter().copied().collect();
        while let Some(id) = below.pop() {
            for &child in children.get(id).into_iter().flatten() {
                let trashed = self.r.get(child).is_some_and(|remote| remote.trashed);
                if !trashed && lost.insert(child) {
                    below.push(child);
                }
            }
        }
        let mut waits = BTreeSet::new();
        for (_, shape) in self.s.iter().filter(|(id, _)| !lost.contains(id.as_str())) {
            let mut parent = shape.parent.as_str();
            while lost.contains(parent) && waits.insert(parent) {
                parent = &self.s.live(parent).expect("a lost node is in S").parent;
            }
        }
        let lost: Vec<(NodeId, bool)> = (lost.into_iter())
            .map(|id| (id.to_owned(), waits.contains(id)))
            .collect();
        for (id, waits) in lost {
            if waits {
                self.wait(id, None);
            } else {
                self.set_synced(id, None);
            }
        }
        debug_assert_eq!(self.s.check(Names::ByKey), Ok(()));
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
    /// a node in sync leaves the dirty set unless it is parked. An excluded node that is not in
    /// sync stays dirty, and its S entry takes no change that only one side made, except that it
    /// gives up a position neither side holds any more (see [`Trees::excluded`]). Only the nodes
    /// marked since the last settle are classified again, since a node's class changes only with
    /// its own entries. The [`Class::Same`] nodes the last settle left, and the nodes whose S
    /// position neither side holds, are tried again. S stays a valid tree, so an update that would orphan a
    /// child, put a node under a missing or non-directory parent, close a cycle or take a sibling's
    /// `name_key` is made only together with the updates it depends on. Settling stops only when no
    /// non-empty set of the remaining [`Class::Same`] updates can be made together with S still valid,
    /// searching each group of updates that depend on each other exhaustively. A group of more
    /// than [`SETTLE_GROUP_LIMIT`] updates is not searched: its nodes stay dirty as [`Class::Same`] and
    /// [`Trees::unsearched`] lists them. A [`Class::Same`] node whose update is left still takes R's
    /// content into S, unless an acknowledged update of the node waits, since content never breaks S, so a later change on either side is compared
    /// with the content both sides already hold. [`Trees::work`] gives the dirty nodes that are left.
    pub fn settle(&mut self) {
        self.reverted
            .retain(|id| match (self.l.get(id), self.r.live(id)) {
                (Some(local), Some(remote)) => !local.shape.same_position(remote),
                (local, remote) => local.is_some() || remote.is_some(),
            });
        let mut pending = std::mem::take(&mut self.same);
        let mut unsettled = std::mem::take(&mut self.unsettled);
        unsettled.extend(
            (self.orphaned.iter())
                .filter(|id| self.dirty.contains(*id))
                .cloned(),
        );
        let mut queue: Vec<NodeId> = unsettled.into_iter().collect();
        let mut next = 0;
        while let Some(id) = queue.get(next).cloned() {
            next += 1;
            let released: Vec<Option<Shape>> = self.released(&id).into_iter().flatten().collect();
            let fitting = (released.iter()).find(|next| self.fits_synced(&id, next.as_ref()));
            match fitting {
                Some(next) => self.set_synced_unmarked(&id, next.clone()),
                None if !released.is_empty() => {
                    for node in self.release_subtree(&id) {
                        if !queue[next..].contains(&node) {
                            queue.push(node);
                        }
                    }
                }
                None => {}
            }
            if self.orphaned_in_s(&id) {
                self.orphaned.insert(id.clone());
            } else {
                self.orphaned.remove(&id);
            }
            pending.remove(&id);
            match self.class(&id) {
                Class::InSync if !self.parked(&id) => {
                    self.dirty.remove(&id);
                }
                Class::Same => {
                    self.dirty.insert(id.clone());
                    pending.insert(id);
                }
                _ => {
                    self.dirty.insert(id);
                }
            }
        }
        self.unsearched.clear();
        let mut taken = Vec::new();
        loop {
            let before = pending.len();
            pending.retain(|id| {
                let fits = self.take_remote(id);
                if fits {
                    taken.push(id.clone());
                }
                !fits
            });
            if pending.is_empty() {
                break;
            }
            if pending.len() < before {
                continue;
            }
            let list: Vec<NodeId> = pending.iter().cloned().collect();
            let together = self.take_remote_together(&list);
            if together.is_empty() {
                break;
            }
            pending.retain(|id| !together.contains(id));
            taken.extend(together);
        }
        for id in taken {
            if !self.parked(&id) {
                self.dirty.remove(&id);
            }
        }
        for id in &pending {
            let (Some(synced), Some(remote)) = (self.s.get(id), self.r.live(id)) else {
                continue;
            };
            if self.waiting.contains_key(id) {
                continue;
            }
            if synced.content != remote.content {
                let next = Shape {
                    content: remote.content.clone(),
                    ..synced.clone()
                };
                self.set_synced_unmarked(id, Some(next));
            }
        }
        self.same = pending;
    }

    /// Every dirty node with its class, in `node_id` order. This reads the whole dirty set.
    pub fn work(&self) -> Vec<(NodeId, Class)> {
        self.dirty
            .iter()
            .map(|id| (id.clone(), self.class(id)))
            .collect()
    }

    fn parked(&self, id: &str) -> bool {
        self.l.get(id).is_some_and(|node| node.parked)
    }

    /// Nodes whose local position the server will not take: a move that closes a cycle against R,
    /// a create or move the server renamed, a folder merged into one L holds elsewhere with the
    /// nodes created in it, and folders L deleted on the way from such a node to the root. The
    /// commit planner sends no position change for them but still sends their content edits and deletes; the
    /// effects put each at its R position without replacing its content, and [`Trees::settle`]
    /// forgets it once L has it there. A node the user moves anywhere else stops being reverted
    /// ([`Trees::set_local`]), and so does a create or move the server renamed when L has left the
    /// position the commit sent before the result arrives.
    pub fn reverted(&self) -> &BTreeSet<NodeId> {
        &self.reverted
    }

    pub fn revert(&mut self, id: NodeId) {
        self.mark(&id);
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
        let mut candidates: BTreeSet<&str> = pending.iter().map(String::as_str).collect();
        loop {
            let never: Vec<&str> = candidates
                .iter()
                .copied()
                .filter(|&id| {
                    let fixed = |other: &&str| !candidates.contains(other);
                    let fixed_child = self.s_names.children(id).any(|c| fixed(&c.as_str()));
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
                        || self
                            .s_names
                            .holders(&shape.parent, &shape.name)
                            .any(|h| h != id && fixed(&h.as_str()))
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
                for other in self.s_names.holders(&shape.parent, &shape.name) {
                    let other = other.as_str();
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
                        && self
                            .s_names
                            .holders(&shape.parent, &shape.name)
                            .any(|h| h != id && !taken(h))
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
        let has_children = self.s_names.children(id).next().is_some();
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
        !self
            .s_names
            .holders(&shape.parent, &shape.name)
            .any(|other| other != id)
    }

    /// Drops the folder `id` from S with every node S holds below it, when S can hold the folder nowhere else and
    /// every one of those nodes is ignored too ([`Trees::released`]). Their S entries keep the folder's from
    /// being dropped, and none of them is ever planned, so without this the folder would keep its name in S for good.
    /// A node below that is not ignored keeps the whole subtree in S. Returns the nodes below that it dropped.
    fn release_subtree(&mut self, id: &str) -> Vec<NodeId> {
        let mut below = vec![id.to_owned()];
        let mut next = 0;
        while let Some(folder) = below.get(next).cloned() {
            next += 1;
            below.extend(self.s_names.children(&folder).cloned());
        }
        if !below.iter().all(|node| self.ignored(node)) {
            return Vec::new();
        }
        for node in below.iter().rev() {
            self.set_synced_unmarked(node, None);
        }
        below.split_off(1)
    }

    fn set_synced_unmarked(&mut self, id: &str, shape: Option<Shape>) {
        self.waiting.remove(id);
        self.changed.insert(id.to_owned());
        let old = match shape {
            Some(shape) => {
                self.s_names.insert(id, &shape);
                self.s.insert(id.to_owned(), shape)
            }
            None => self.s.remove(id),
        };
        if let Some(changed) = self.watched.get_mut(id) {
            let before = old.as_ref().and_then(|shape| shape.content.as_ref());
            let after = self.s.get(id).and_then(|shape| shape.content.as_ref());
            *changed |= match (before, after) {
                (Some(before), Some(after)) => !same_content(before, after),
                (before, after) => before.is_some() != after.is_some(),
            };
        }
        if let Some(old) = old {
            let kept = self.s.get(id).is_some_and(|new| {
                new.parent == old.parent && name_key(&new.name) == name_key(&old.name)
            });
            if !kept {
                self.s_names.remove(id, &old);
            }
        }
    }

    /// The invariants of section 2.2 for all three trees.
    pub fn check(&self) -> Result<(), (char, TreeError)> {
        self.r.check(Names::ByKey).map_err(|e| ('R', e))?;
        self.s.check(Names::ByKey).map_err(|e| ('S', e))?;
        self.l.check(Names::Exact).map_err(|e| ('L', e))
    }

    /// What the store keeps besides the nodes of R, L and S.
    pub fn state(&self) -> TreesState {
        TreesState {
            dirty: self.dirty.clone(),
            unsearched: self.unsearched.clone(),
            reverted: self.reverted.clone(),
            parked_as: self.parked_as.clone(),
            waiting: self.waiting.clone(),
            watched: self.watched.clone(),
        }
    }

    /// Replaces the dirty, unsearched, reverted, parked, waiting and watched nodes with `state`,
    /// once the store has loaded every node. The rules are unknown from here until
    /// [`Trees::rules_scanned`].
    /// Refuses trees that break section 2.2, a node that is not dirty
    /// although it is parked or not in sync, an unsearched node that is not dirty, and a parked
    /// name for a node L does not hold as parked.
    pub fn restore(&mut self, state: TreesState) -> Result<(), StateError> {
        self.check()
            .map_err(|(tree, error)| StateError::Tree(tree, error))?;
        let ids = (self.r.iter().map(|(id, _)| id))
            .chain(self.l.iter().map(|(id, _)| id))
            .chain(self.s.iter().map(|(id, _)| id));
        let settled = |id: &NodeId| {
            self.class(id) == Class::InSync && !self.l.get(id).is_some_and(|node| node.parked)
        };
        if let Some(id) = ids
            .filter(|id| !state.dirty.contains(*id))
            .find(|id| !settled(id))
        {
            return Err(StateError::NotDirty(id.clone()));
        }
        if let Some(id) = state.unsearched.difference(&state.dirty).next() {
            return Err(StateError::UnsearchedNotDirty(id.clone()));
        }
        let placed = |id: &&NodeId| !self.l.get(id).is_some_and(|node| node.parked);
        if let Some(id) = state.parked_as.keys().find(placed) {
            return Err(StateError::NotParked(id.clone()));
        }
        let TreesState {
            dirty,
            unsearched,
            reverted,
            parked_as,
            waiting,
            watched,
        } = state;
        self.unsettled = dirty.clone();
        self.same.clear();
        self.orphaned.clear();
        self.dirty = dirty;
        self.unsearched = unsearched;
        self.reverted = reverted;
        self.parked_as = parked_as;
        self.waiting = waiting;
        self.watched = watched;
        self.rules_known = false;
        self.unknown_rules.clear();
        Ok(())
    }
}
