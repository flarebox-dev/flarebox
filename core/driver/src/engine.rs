// SPDX-License-Identifier: Apache-2.0

//! The engine state a folder task keeps in memory: R, L and S with their bookkeeping, the effects and the ID
//! generator. It is loaded from the store once and written back inside the transaction of each step. The commits'
//! state is kept as the store holds it, so a save does not drop it.
//!
//! [`Engine::observe`] puts an observation batch into L: each change goes to the node with the entry's identity,
//! or to the node at the change's path, and a change that leaves a node as L has it changes nothing.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::BuildHasher;
use std::path::{Component, Path, PathBuf};

use flarebox_engine::classify::Trees;
use flarebox_engine::effects::Effects;
use flarebox_engine::ids::IdGen;
use flarebox_engine::state::CommitsState;
use flarebox_engine::tree::{
    Content, FileIdentity, Kind, LocalNode, NodeId, RemoteNode, Shape, same_content,
};
use flarebox_fs::{Entry, EntryKind, FileId};
use flarebox_proto::v1::NodeKind;
use flarebox_store::{EngineState, Error, Tx};

use crate::observe::{Observed, Reset};

pub(crate) struct Engine {
    pub trees: Trees,
    pub effects: Effects,
    pub ids: IdGen,
    commits: CommitsState,
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
            ids,
            commits,
        })
    }

    /// Writes the engine state and the ID generator, and the L rows of `local`: each node L holds is written, each
    /// one it no longer holds is removed. The caller names every node whose L entry changed since the last save.
    pub fn save<'a>(
        &self,
        tx: &Tx<'_>,
        local: impl IntoIterator<Item = &'a NodeId>,
    ) -> Result<(), Error> {
        for id in local {
            match self.trees.local().get(id) {
                Some(node) => tx.put_local_node(&stored(id, node))?,
                None => {
                    tx.remove_local_node(id)?;
                }
            }
        }
        tx.set_engine_state(&EngineState {
            trees: self.trees.state(),
            commits: self.commits.clone(),
            effects: self.effects.state(),
        })?;
        tx.set_id_generator(&self.ids.state.to_le_bytes())
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

    /// L's view of each of `scopes`, relative paths, as `core/fs` entries: the placed node at the scope and every
    /// placed node under it. The empty path covers all of L, and a path L does not hold gets no entries. A folder has
    /// the zero size and time `core/fs` gives every folder; a file has the size, time and executable bit of its
    /// content. A disk that matches L therefore rescans to no changes.
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
                        known.insert(path.clone(), entry(node));
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
    let kind = match node.shape.kind {
        Kind::Dir => NodeKind::Dir,
        Kind::File => NodeKind::File,
    };
    flarebox_store::LocalNode {
        node_id: id.clone(),
        shape: flarebox_store::Shape {
            parent_id: node.shape.parent.clone(),
            name: node.shape.name.clone(),
            kind: kind as i32,
            content: node.shape.content.clone(),
        },
        identity: file_id(node.identity),
        parked: node.parked,
    }
}

/// A node as the `core/fs` entry a scan of it makes.
fn entry(node: &LocalNode) -> Entry {
    let (kind, size, mtime_ns, executable) = match (&node.shape.kind, &node.shape.content) {
        (Kind::Dir, _) => (EntryKind::Dir, 0, 0, false),
        (Kind::File, None) => (EntryKind::File, 0, 0, false),
        (Kind::File, Some(content)) => (
            EntryKind::File,
            content.size,
            i128::from(content.mtime_ns),
            content.executable,
        ),
    };
    Entry {
        kind,
        id: file_id(node.identity),
        size,
        mtime_ns,
        executable,
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
