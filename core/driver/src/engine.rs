// SPDX-License-Identifier: Apache-2.0

//! The engine state a folder task keeps in memory: R, L and S with their bookkeeping, the commits, the effects and
//! the ID generator. It is loaded from the store once, and every step that changes it ends with
//! [`Engine::checkpoint`] inside the step's transaction.
//!
//! R follows the server's log through [`Engine::apply_records`], and a bootstrap replaces it through a [`Staging`]
//! tree. [`Engine::observe`] puts an observation batch into L: each change goes to the node with the entry's
//! identity, or to the node at the change's path, and a change that leaves a node as L has it changes nothing.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::BuildHasher;
use std::path::{Component, Path, PathBuf};

use flarebox_engine::classify::Trees;
use flarebox_engine::commit::Commits;
use flarebox_engine::effects::Effects;
use flarebox_engine::ids::IdGen;
use flarebox_engine::names::name_key;
use flarebox_engine::tree::{
    Content, FileIdentity, Kind, LocalNode, Names, NodeId, RemoteNode, Shape, Tree, same_content,
};
use flarebox_fs::{Entry, EntryKind, FileId, Known, Snapshot};
use flarebox_proto::v1::{
    ChangeRecord, CommitRecord, NodeKind, NodeState, TreePage, change_record,
};
use flarebox_store::{Cursor, EngineState, Error, SyncedNode, Tx};

use crate::disk::Stop;
use crate::observe::{Observed, Reset};

pub(crate) struct Engine {
    pub trees: Trees,
    pub effects: Effects,
    pub commits: Commits,
    pub ids: IdGen,
    /// The history R belongs to and the seq of the last commit R holds. `None` until the first bootstrap.
    pub cursor: Option<Cursor>,
}

/// What applying a batch of the log did.
#[derive(Debug, PartialEq)]
pub(crate) enum Applied {
    /// Every record was applied, or was already applied.
    Through,
    /// A record does not follow the last one applied. Nothing from it on was applied.
    Gap,
}

impl Engine {
    /// The engine state of the space whose root node is `root`, as the store holds it. A space that never stored an
    /// ID generator gets one with a random seed.
    pub fn load(tx: &Tx<'_>, root: &str) -> Result<Self, Error> {
        let mut trees = Trees::new(root);
        for state in tx.remote_nodes()? {
            if state.node_id == root {
                continue;
            }
            let (id, node) = RemoteNode::from_state(&state)
                .map_err(|error| invalid(format!("remote node {}: {error:?}", state.node_id)))?;
            trees.set_remote(id, node);
        }
        for node in tx.synced_nodes()? {
            trees.set_synced(node.node_id, Some(shape(node.shape)?));
        }
        for node in tx.local_nodes()? {
            let local = LocalNode {
                shape: shape(node.shape)?,
                identity: file_identity(node.identity),
                parked: node.parked,
            };
            trees.set_local(node.node_id, local);
        }
        let EngineState {
            trees: state,
            commits,
            effects,
        } = tx.engine_state()?;
        trees
            .restore(state)
            .map_err(|error| invalid(format!("{error:?}")))?;
        let commits =
            Commits::from_state(commits).map_err(|error| invalid(format!("{error:?}")))?;
        let effects =
            Effects::from_state(effects).map_err(|error| invalid(format!("{error:?}")))?;
        let ids = match tx.id_generator()? {
            Some(bytes) => {
                let state = <[u8; 8]>::try_from(bytes.as_slice()).map_err(|_| {
                    invalid(format!("an ID generator state of {} bytes", bytes.len()))
                })?;
                IdGen::new(u64::from_le_bytes(state))
            }
            None => IdGen::new(
                std::collections::hash_map::RandomState::new()
                    .hash_one(std::time::SystemTime::now()),
            ),
        };
        Ok(Engine {
            trees,
            effects,
            commits,
            ids,
            cursor: tx.cursor()?,
        })
    }

    /// Ends a step that changed the engine, inside the step's transaction. It settles the trees, then writes the L
    /// row and the S row of every node that was dirty before settling, the engine state and the ID generator. A row
    /// of a node the tree no longer holds is removed.
    ///
    /// The dirty set names every changed row: each setter of [`Trees`] marks its node, and [`Trees::settle`] changes
    /// S only for nodes it took from the dirty set.
    pub fn checkpoint(&mut self, tx: &Tx<'_>) -> Result<(), Error> {
        let touched: Vec<NodeId> = self.trees.dirty().iter().cloned().collect();
        self.trees.settle();
        for id in &touched {
            match self.trees.local().get(id) {
                Some(node) => tx.put_local_node(&stored(id, node))?,
                None => {
                    tx.remove_local_node(id)?;
                }
            }
            match self.trees.synced().get(id) {
                Some(shape) => tx.put_synced_node(&SyncedNode {
                    node_id: id.clone(),
                    shape: stored_shape(shape),
                })?,
                None => {
                    tx.remove_synced_node(id)?;
                }
            }
        }
        tx.set_engine_state(&EngineState {
            trees: self.trees.state(),
            commits: self.commits.state(),
            effects: self.effects.state(),
        })?;
        tx.set_id_generator(&self.ids.state.to_le_bytes())
    }

    /// Applies commits of the log to R, in the store and in the trees, in seq order from the one after the cursor
    /// (design §4.4). A commit at or below the cursor is skipped, and a commit further ahead is a gap.
    pub fn apply_records(
        &mut self,
        tx: &Tx<'_>,
        records: &[CommitRecord],
    ) -> Result<Applied, Stop> {
        for record in records {
            let Some(cursor) = &self.cursor else {
                return Err(Stop::Protocol(
                    "changes arrived before the space's tree was read".into(),
                ));
            };
            if record.seq <= cursor.last_seq {
                continue;
            }
            if record.seq != cursor.last_seq + 1 {
                return Ok(Applied::Gap);
            }
            self.apply_commit(tx, record.seq, &record.changes)?;
        }
        Ok(Applied::Through)
    }

    /// Applies the changes of commit `seq`, the one after the cursor: each node takes its state after the commit,
    /// the cursor moves to `seq`, and a commit result R has now reached is acknowledged. A change of a kind this
    /// build does not know stops the task before anything of the commit is applied (spec v0 section 9.1).
    pub fn apply_commit(
        &mut self,
        tx: &Tx<'_>,
        seq: u64,
        changes: &[ChangeRecord],
    ) -> Result<(), Stop> {
        if changes.iter().any(|change| change.change.is_none()) {
            return Err(Stop::Protocol(format!(
                "commit {seq} holds a kind of change this build does not know; update Flarebox"
            )));
        }
        let Some(cursor) = &mut self.cursor else {
            return Err(Stop::Protocol(
                "a commit arrived before the space's tree was read".into(),
            ));
        };
        for change in changes {
            match &change.change {
                Some(change_record::Change::Node(state)) => {
                    tx.put_remote_node(state)?;
                    if state.node_id != self.trees.remote().root {
                        let (id, node) = remote_node(state)?;
                        self.trees.set_remote(id, node);
                    }
                }
                Some(change_record::Change::Purged(purged)) => {
                    tx.remove_remote_node(&purged.node_id)?;
                    self.trees.remove_remote(&purged.node_id);
                }
                Some(change_record::Change::Space(_) | change_record::Change::Member(_)) | None => {
                }
            }
        }
        cursor.last_seq = seq;
        tx.set_cursor(cursor)?;
        self.commits.acknowledge(&mut self.trees, seq);
        Ok(())
    }

    /// Replaces R, in the store and in the trees, with a bootstrap's tree that the log has caught up, and sets the
    /// cursor. S and L stay, so the planner then moves what differs (design §4.4). A tree that is not valid stops
    /// the task before anything is replaced.
    pub fn replace_remote(&mut self, tx: &Tx<'_>, staging: Staging) -> Result<(), Stop> {
        let root = self.trees.remote().root.clone();
        let mut tree = Tree::new(root.clone());
        for state in staging.nodes.values().filter(|state| state.node_id != root) {
            let (id, node) = remote_node(state)?;
            tree.insert(id, node);
        }
        tree.check(Names::ByKey).map_err(|error| {
            Stop::Protocol(format!(
                "the tree the server listed is not a valid tree: {error:?}"
            ))
        })?;
        let gone: Vec<NodeId> = (self.trees.remote().iter())
            .map(|(id, _)| id)
            .filter(|id| tree.get(id).is_none())
            .cloned()
            .collect();
        for id in gone {
            self.trees.remove_remote(&id);
        }
        for (id, node) in tree.iter() {
            if self.trees.remote().get(id) != Some(node) {
                self.trees.set_remote(id.clone(), node.clone());
            }
        }
        tx.clear_remote_nodes()?;
        for state in staging.nodes.values() {
            tx.put_remote_node(state)?;
        }
        let cursor = Cursor {
            history_id: staging.history_id,
            last_seq: staging.last_seq,
        };
        tx.set_cursor(&cursor)?;
        self.commits.acknowledge(&mut self.trees, cursor.last_seq);
        self.cursor = Some(cursor);
        Ok(())
    }

    /// L as the snapshot a scan of the folder would give if nothing changed since L was written, for the observer to
    /// start from: every placed node at its path, a folder with the zero size and time `core/fs` gives every
    /// folder, a file with the size, time and executable bit of its content. The first scan then reports what
    /// changed while nothing observed the folder, removals included, and reads only the files that differ.
    pub fn snapshot(&self) -> Snapshot {
        let index = Index::new(&self.trees);
        let local = self.trees.local();
        let mut entries = BTreeMap::new();
        let mut pending = vec![(PathBuf::new(), local.root.clone())];
        while let Some((path, id)) = pending.pop() {
            for child in index.children_of(&id) {
                let Some(node) = local.get(&child) else {
                    continue;
                };
                let (kind, size, mtime_ns, executable) =
                    match (&node.shape.kind, &node.shape.content) {
                        (Kind::Dir, _) => (EntryKind::Dir, 0, 0, false),
                        (Kind::File, Some(content)) => (
                            EntryKind::File,
                            content.size,
                            i128::from(content.mtime_ns),
                            content.executable,
                        ),
                        (Kind::File, None) => continue,
                    };
                let at = path.join(&node.shape.name);
                entries.insert(
                    at.clone(),
                    Entry {
                        kind,
                        id: file_id(node.identity),
                        size,
                        mtime_ns,
                        executable,
                    },
                );
                pending.push((at, child));
            }
        }
        Snapshot { entries }
    }

    /// The path of a node L holds, relative to the root. `None` for a node L does not hold under the root, such as
    /// a parked one.
    pub fn path_of(&self, id: &str) -> Option<PathBuf> {
        let local = self.trees.local();
        let mut names = Vec::new();
        let mut current = id;
        while current != local.root {
            let node = local.get(current).filter(|node| !node.parked)?;
            names.push(node.shape.name.as_str());
            current = &node.shape.parent;
            if names.len() > local.len() {
                return None;
            }
        }
        Some(names.into_iter().rev().collect())
    }

    /// Puts `changes` into L and returns the nodes whose L entry changed. Paths a change names in the previous state
    /// (`Removed::path`, `Renamed::from`) are resolved before any change applies. A created entry with the identity of
    /// a node L holds is that node, wherever L had it; any other created entry is a new node. A node that held the
    /// name a change puts an entry at is gone from there, so it leaves L with everything under it. A file whose
    /// content could not be read is left out until the observer reports it with its content.
    pub fn observe(&mut self, changes: &[Observed], now_ms: u64) -> BTreeSet<NodeId> {
        let mut index = Index::new(&self.trees);
        let before: Vec<Option<NodeId>> = changes
            .iter()
            .map(|change| match change {
                Observed::Removed { path, .. } => index.at(path),
                Observed::Renamed { from, id, .. } => index
                    .by_identity(&file_identity(*id))
                    .or_else(|| index.at(from)),
                Observed::Created { .. } | Observed::Modified { .. } => None,
            })
            .collect();
        let mut touched = BTreeSet::new();
        for (change, before) in changes.iter().zip(before) {
            match change {
                Observed::Removed { .. } => {
                    if let Some(id) = before {
                        self.remove(&mut index, &id, &mut touched);
                    }
                }
                Observed::Renamed { to, kind, id, .. } => {
                    let identity = file_identity(*id);
                    let content = before
                        .as_ref()
                        .and_then(|id| self.trees.local().get(id))
                        .and_then(|node| node.shape.content.clone());
                    self.place(
                        &mut index,
                        before,
                        to,
                        *kind,
                        identity,
                        content,
                        now_ms,
                        &mut touched,
                    );
                }
                Observed::Created {
                    path,
                    kind,
                    id,
                    content,
                } => {
                    if *kind == EntryKind::File && content.is_none() {
                        continue;
                    }
                    let identity = file_identity(*id);
                    let node = index.by_identity(&identity);
                    self.place(
                        &mut index,
                        node,
                        path,
                        *kind,
                        identity,
                        content.clone(),
                        now_ms,
                        &mut touched,
                    );
                }
                Observed::Modified { path, id, content } => {
                    let Some(content) = content else { continue };
                    let identity = file_identity(*id);
                    let node = index.by_identity(&identity).or_else(|| index.at(path));
                    self.place(
                        &mut index,
                        node,
                        path,
                        EntryKind::File,
                        identity,
                        Some(content.clone()),
                        now_ms,
                        &mut touched,
                    );
                }
            }
        }
        touched
    }

    /// L's view of each of `scopes`, relative paths, for the observer: the kind and identity of the placed node at
    /// the scope and of every placed node under it. The empty path covers all of L, and a path L does not hold gets
    /// no entries. L's content for a file is left out, because it does not prove what the disk holds.
    pub fn resets(&self, scopes: impl IntoIterator<Item = PathBuf>) -> Vec<Reset> {
        let index = Index::new(&self.trees);
        let local = self.trees.local();
        scopes
            .into_iter()
            .map(|scope| {
                let mut known = BTreeMap::new();
                let mut pending: Vec<(PathBuf, NodeId)> = index
                    .at(&scope)
                    .map(|id| (scope.clone(), id))
                    .into_iter()
                    .collect();
                while let Some((path, id)) = pending.pop() {
                    if let Some(node) = local.get(&id)
                        && !path.as_os_str().is_empty()
                    {
                        let kind = match node.shape.kind {
                            Kind::Dir => EntryKind::Dir,
                            Kind::File => EntryKind::File,
                        };
                        let id = file_id(node.identity);
                        known.insert(path.clone(), Known { kind, id });
                    }
                    for child in index.children_of(&id) {
                        if let Some(node) = local.get(&child) {
                            pending.push((path.join(&node.shape.name), child));
                        }
                    }
                }
                Reset { scope, known }
            })
            .collect()
    }

    /// Puts `node`, or a new node when it is `None`, at `path` with `identity`. A file keeps the content L had for it
    /// when `content` is `None`. Changes nothing when the parent folder is not in L, or the path is not a name L can
    /// hold.
    #[allow(clippy::too_many_arguments)]
    fn place(
        &mut self,
        index: &mut Index,
        node: Option<NodeId>,
        path: &Path,
        kind: EntryKind,
        identity: FileIdentity,
        content: Option<Content>,
        now_ms: u64,
        touched: &mut BTreeSet<NodeId>,
    ) {
        let (Some(parent), Some(name)) = (
            path.parent().and_then(|parent| index.at(parent)),
            path.file_name().and_then(|name| name.to_str()),
        ) else {
            return;
        };
        let kind = match kind {
            EntryKind::File => Kind::File,
            EntryKind::Dir => Kind::Dir,
        };
        let shape = Shape {
            parent: parent.clone(),
            name: name.to_owned(),
            kind,
            content: match kind {
                Kind::File => content,
                Kind::Dir => None,
            },
        };
        let existing = node.as_ref().and_then(|id| self.trees.local().get(id));
        let unchanged = existing.is_some_and(|old| {
            !old.parked
                && old.identity == identity
                && old.shape.parent == shape.parent
                && old.shape.name == shape.name
                && old.shape.kind == shape.kind
                && match (&old.shape.content, &shape.content) {
                    (Some(old), Some(new)) => same_content(old, new),
                    (old, new) => old.is_none() && new.is_none(),
                }
        });
        if unchanged {
            return;
        }
        let shape = Shape {
            content: shape.content.or_else(|| {
                existing
                    .filter(|_| kind == Kind::File)
                    .and_then(|old| old.shape.content.clone())
            }),
            ..shape
        };
        if kind == Kind::File && shape.content.is_none() {
            return;
        }
        let id = node.unwrap_or_else(|| self.ids.uuid_v7(now_ms));
        if let Some(holder) = index.child(&parent, name).filter(|holder| **holder != id) {
            let holder = holder.clone();
            self.remove(index, &holder, touched);
        }
        index.forget(&self.trees, &id);
        index.insert(&id, &shape, identity);
        self.trees.set_local(
            id.clone(),
            LocalNode {
                shape,
                identity,
                parked: false,
            },
        );
        touched.insert(id);
    }

    /// Removes `id` and every node placed under it from L.
    fn remove(&mut self, index: &mut Index, id: &NodeId, touched: &mut BTreeSet<NodeId>) {
        let mut doomed = vec![id.clone()];
        let mut next = 0;
        while let Some(current) = doomed.get(next).cloned() {
            doomed.extend(index.children_of(&current));
            next += 1;
        }
        for id in doomed {
            index.forget(&self.trees, &id);
            self.trees.remove_local(&id);
            touched.insert(id);
        }
    }
}

/// L's placed nodes by parent and name, and all of its nodes by identity.
struct Index {
    root: NodeId,
    children: BTreeMap<(NodeId, String), NodeId>,
    identities: HashMap<FileIdentity, NodeId>,
}

impl Index {
    fn new(trees: &Trees) -> Self {
        let local = trees.local();
        let mut index = Index {
            root: local.root.clone(),
            children: BTreeMap::new(),
            identities: HashMap::new(),
        };
        for (id, node) in local.iter() {
            index.identities.insert(node.identity, id.clone());
            if !node.parked {
                index.children.insert(
                    (node.shape.parent.clone(), node.shape.name.clone()),
                    id.clone(),
                );
            }
        }
        index
    }

    /// The node at `path`, or the root for the empty path.
    fn at(&self, path: &Path) -> Option<NodeId> {
        let mut current = self.root.clone();
        for component in path.components() {
            let Component::Normal(name) = component else {
                return None;
            };
            current = self.child(&current, name.to_str()?)?.clone();
        }
        Some(current)
    }

    fn child(&self, parent: &str, name: &str) -> Option<&NodeId> {
        self.children.get(&(parent.to_owned(), name.to_owned()))
    }

    fn children_of(&self, parent: &str) -> Vec<NodeId> {
        self.children
            .range((parent.to_owned(), String::new())..)
            .take_while(|((holder, _), _)| holder == parent)
            .map(|(_, id)| id.clone())
            .collect()
    }

    fn by_identity(&self, identity: &FileIdentity) -> Option<NodeId> {
        self.identities.get(identity).cloned()
    }

    fn forget(&mut self, trees: &Trees, id: &str) {
        if let Some(node) = trees.local().get(id) {
            let key = (node.shape.parent.clone(), node.shape.name.clone());
            if self.children.get(&key).is_some_and(|held| held == id) {
                self.children.remove(&key);
            }
            if self
                .identities
                .get(&node.identity)
                .is_some_and(|held| held == id)
            {
                self.identities.remove(&node.identity);
            }
        }
    }

    fn insert(&mut self, id: &NodeId, shape: &Shape, identity: FileIdentity) {
        self.children
            .insert((shape.parent.clone(), shape.name.clone()), id.clone());
        self.identities.insert(identity, id.clone());
    }
}

fn invalid(what: String) -> Error {
    Error::InvalidEngineState(what)
}

fn shape(shape: flarebox_store::Shape) -> Result<Shape, Error> {
    let kind = match NodeKind::try_from(shape.kind) {
        Ok(NodeKind::Dir) => Kind::Dir,
        Ok(NodeKind::File) => Kind::File,
        _ => return Err(invalid(format!("node kind {}", shape.kind))),
    };
    Ok(Shape {
        parent: shape.parent_id,
        name: shape.name,
        kind,
        content: shape.content,
    })
}

fn stored(id: &NodeId, node: &LocalNode) -> flarebox_store::LocalNode {
    flarebox_store::LocalNode {
        node_id: id.clone(),
        shape: stored_shape(&node.shape),
        identity: file_id(node.identity),
        parked: node.parked,
    }
}

fn stored_shape(shape: &Shape) -> flarebox_store::Shape {
    let kind = match shape.kind {
        Kind::Dir => NodeKind::Dir,
        Kind::File => NodeKind::File,
    };
    flarebox_store::Shape {
        parent_id: shape.parent.clone(),
        name: shape.name.clone(),
        kind: kind as i32,
        content: shape.content.clone(),
    }
}

fn remote_node(state: &NodeState) -> Result<(NodeId, RemoteNode), Stop> {
    RemoteNode::from_state(state).map_err(|error| {
        Stop::Protocol(format!(
            "remote node {} cannot be synced by this build: {error:?}",
            state.node_id
        ))
    })
}

/// A bootstrap's tree while the log catches it up (design §4.4). It is kept in memory, so the store's R stays the
/// valid tree it was until this one may replace it.
#[derive(Debug)]
pub(crate) struct Staging {
    history_id: String,
    /// The first page's `head_seq`, then the seq of the last commit replayed.
    last_seq: u64,
    /// The highest `head_seq` of any page.
    highest_page_seq: u64,
    /// The head of the last `CaughtUp`.
    caught_up: Option<u64>,
    nodes: BTreeMap<String, NodeState>,
}

impl Staging {
    /// The listing of a bootstrap. The log is replayed from the first page's `head_seq` (spec v0 section 7.5).
    pub fn new(history_id: String, pages: Vec<TreePage>) -> Self {
        Staging {
            history_id,
            last_seq: pages.first().map_or(0, |page| page.head_seq),
            highest_page_seq: pages.iter().map(|page| page.head_seq).max().unwrap_or(0),
            caught_up: None,
            nodes: pages
                .into_iter()
                .flat_map(|page| page.nodes)
                .map(|node| (node.node_id.clone(), node))
                .collect(),
        }
    }

    pub fn history_id(&self) -> &str {
        &self.history_id
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Replays commits of the log in seq order. A node state replaces the listed node only when it is newer, since a
    /// page read later can already hold the result of a commit replayed now.
    pub fn replay(&mut self, records: &[CommitRecord]) -> Result<Applied, Stop> {
        for record in records {
            if record.seq <= self.last_seq {
                continue;
            }
            if record.seq != self.last_seq + 1 {
                return Ok(Applied::Gap);
            }
            self.replay_commit(record.seq, &record.changes)?;
        }
        Ok(Applied::Through)
    }

    /// Replays commit `seq`, the one after the last replayed.
    pub fn replay_commit(&mut self, seq: u64, changes: &[ChangeRecord]) -> Result<(), Stop> {
        if changes.iter().any(|change| change.change.is_none()) {
            return Err(Stop::Protocol(format!(
                "commit {seq} holds a kind of change this build does not know; update Flarebox"
            )));
        }
        for change in changes {
            match &change.change {
                Some(change_record::Change::Node(state)) => {
                    let newer = self.nodes.get(&state.node_id).is_none_or(|held| {
                        state.content_rev > held.content_rev || state.meta_rev > held.meta_rev
                    });
                    if newer {
                        self.nodes.insert(state.node_id.clone(), state.clone());
                    }
                }
                Some(change_record::Change::Purged(purged)) => {
                    self.nodes.remove(&purged.node_id);
                }
                Some(change_record::Change::Space(_) | change_record::Change::Member(_)) | None => {
                }
            }
        }
        self.last_seq = seq;
        Ok(())
    }

    /// Records a `CaughtUp` with the server's head.
    pub fn caught_up(&mut self, head_seq: u64) {
        self.caught_up = Some(head_seq);
    }

    /// True once the tree may replace R: the replay has reached the highest `head_seq` of any page and the head
    /// of the last `CaughtUp`.
    pub fn ready(&self) -> bool {
        self.last_seq >= self.highest_page_seq
            && self.caught_up.is_some_and(|head| self.last_seq >= head)
    }

    /// True when the tree holds a live node directly under `root` whose name is `name`, as the server compares
    /// names.
    pub fn holds(&self, root: &str, name: &str) -> bool {
        let key = name_key(name);
        self.nodes.values().any(|node| {
            node.parent_id == root
                && node.status == flarebox_proto::v1::NodeStatus::Live as i32
                && name_key(&node.name) == key
        })
    }
}

/// The engine's name for a `core/fs` identity, birth time included when the file system reports one.
pub(crate) fn file_identity(id: FileId) -> FileIdentity {
    FileIdentity {
        dev: id.dev,
        ino: id.ino,
        birth_ns: id.birth_ns,
    }
}

/// The `core/fs` name for an engine identity.
pub(crate) fn file_id(identity: FileIdentity) -> FileId {
    FileId {
        dev: identity.dev,
        ino: identity.ino,
        birth_ns: identity.birth_ns,
    }
}

#[cfg(test)]
mod tests {
    use flarebox_proto::v1::{NodePurged, NodeStatus, SpaceUpdated};
    use flarebox_store::Store;

    use super::*;

    const ROOT: &str = "root";

    fn node(id: &str, name: &str) -> NodeState {
        NodeState {
            node_id: id.into(),
            parent_id: ROOT.into(),
            name: name.into(),
            kind: NodeKind::Dir as i32,
            status: NodeStatus::Live as i32,
            ..Default::default()
        }
    }

    fn record(seq: u64, changes: Vec<change_record::Change>) -> CommitRecord {
        CommitRecord {
            seq,
            changes: changes
                .into_iter()
                .map(|change| ChangeRecord {
                    change: Some(change),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn cursor(seq: u64) -> Cursor {
        Cursor {
            history_id: "h".into(),
            last_seq: seq,
        }
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store.sqlite")).unwrap();
        (dir, store)
    }

    /// A bootstrap's tree of `nodes` that the log has caught up at `seq`.
    fn listed(seq: u64, nodes: Vec<NodeState>) -> Staging {
        let page = TreePage {
            nodes,
            head_seq: seq,
            ..Default::default()
        };
        Staging::new("h".into(), vec![page])
    }

    /// The names R holds, in the store and in the trees, which must agree.
    fn names(tx: &Tx<'_>, engine: &Engine) -> Vec<String> {
        let mut stored: Vec<String> = (tx.remote_nodes().unwrap().into_iter())
            .map(|node| node.name)
            .collect();
        stored.sort();
        let mut held: Vec<String> = (engine.trees.remote().iter())
            .map(|(_, node)| node.shape.name.clone())
            .collect();
        held.sort();
        assert_eq!(stored, held);
        stored
    }

    #[test]
    fn records_are_applied_in_seq_order_and_replays_are_skipped() {
        let (_dir, mut store) = store();
        store
            .transaction(|tx| {
                let mut engine = Engine::load(tx, ROOT)?;
                engine.replace_remote(tx, listed(4, vec![node("a", "a"), node("b", "b")]))?;
                let records = [
                    record(4, vec![change_record::Change::Node(node("a", "stale"))]),
                    record(5, vec![change_record::Change::Node(node("a", "a2"))]),
                    record(
                        6,
                        vec![
                            change_record::Change::Purged(NodePurged {
                                node_id: "b".into(),
                            }),
                            change_record::Change::Node(node("c", "c")),
                            change_record::Change::Space(SpaceUpdated {}),
                        ],
                    ),
                ];
                assert_eq!(engine.apply_records(tx, &records)?, Applied::Through);
                assert_eq!(names(tx, &engine), ["a2", "c"]);
                assert_eq!(tx.cursor()?, Some(cursor(6)));
                assert_eq!(engine.cursor, Some(cursor(6)));
                Ok::<_, Stop>(())
            })
            .unwrap();
    }

    #[test]
    fn a_gap_stops_at_the_last_contiguous_record() {
        let (_dir, mut store) = store();
        store
            .transaction(|tx| {
                let mut engine = Engine::load(tx, ROOT)?;
                engine.replace_remote(tx, listed(1, Vec::new()))?;
                let records = [
                    record(2, vec![change_record::Change::Node(node("a", "a"))]),
                    record(4, vec![change_record::Change::Node(node("b", "b"))]),
                ];
                assert_eq!(engine.apply_records(tx, &records)?, Applied::Gap);
                assert_eq!(names(tx, &engine), ["a"]);
                assert_eq!(tx.cursor()?, Some(cursor(2)));
                Ok::<_, Stop>(())
            })
            .unwrap();
    }

    #[test]
    fn an_unknown_kind_of_change_stops_before_its_record() {
        let (_dir, mut store) = store();
        store
            .transaction(|tx| {
                let mut engine = Engine::load(tx, ROOT)?;
                engine.replace_remote(tx, listed(0, Vec::new()))?;
                let mut unknown = record(2, vec![change_record::Change::Node(node("b", "b"))]);
                unknown.changes.push(ChangeRecord { change: None });
                let records = [
                    record(1, vec![change_record::Change::Node(node("a", "a"))]),
                    unknown,
                ];
                let stop = engine.apply_records(tx, &records).unwrap_err();
                assert!(
                    matches!(&stop, Stop::Protocol(why) if why.contains("commit 2")),
                    "{stop}"
                );
                assert_eq!(names(tx, &engine), ["a"]);
                assert_eq!(tx.cursor()?, Some(cursor(1)));
                Ok::<_, Stop>(())
            })
            .unwrap();
    }

    #[test]
    fn a_bootstrap_replaces_the_whole_tree() {
        let (_dir, mut store) = store();
        store
            .transaction(|tx| {
                let mut engine = Engine::load(tx, ROOT)?;
                engine.replace_remote(tx, listed(3, vec![node("a", "a"), node("b", "b")]))?;
                engine.replace_remote(tx, listed(9, vec![node("c", "c")]))?;
                assert_eq!(names(tx, &engine), ["c"]);
                assert_eq!(tx.cursor()?, Some(cursor(9)));
                Ok::<_, Stop>(())
            })
            .unwrap();
    }

    #[test]
    fn a_listed_tree_that_is_not_a_tree_replaces_nothing() {
        let (_dir, mut store) = store();
        store
            .transaction(|tx| {
                let mut engine = Engine::load(tx, ROOT)?;
                engine.replace_remote(tx, listed(3, vec![node("a", "a")]))?;
                let orphan = NodeState {
                    parent_id: "missing".into(),
                    ..node("b", "b")
                };
                let stop = engine
                    .replace_remote(tx, listed(9, vec![orphan]))
                    .unwrap_err();
                assert!(matches!(stop, Stop::Protocol(_)), "{stop}");
                assert_eq!(names(tx, &engine), ["a"]);
                assert_eq!(tx.cursor()?, Some(cursor(3)));
                Ok::<_, Stop>(())
            })
            .unwrap();
    }
}
