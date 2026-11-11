// SPDX-License-Identifier: Apache-2.0

//! The engine state a folder task keeps in memory: R, L and S with their bookkeeping, the commits, the effects and
//! the ID generator. It is loaded from the store once, and every step that changes it ends with
//! [`Engine::checkpoint`] inside the step's transaction.
//!
//! R follows the server's log through [`Engine::apply_records`], and a bootstrap replaces it through a [`Staging`]
//! tree. [`Engine::observe`] puts an observation batch into L: each change goes to the node with the entry's
//! identity, or to the node at the change's path, and a change that leaves a node as L has it changes nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::BuildHasher;
use std::path::{Component, Path, PathBuf};

use flarebox_engine::classify::Trees;
use flarebox_engine::commit::Commits;
use flarebox_engine::effects::Effects;
use flarebox_engine::ids::IdGen;
use flarebox_engine::names::name_key;
use flarebox_engine::question as engine;
use flarebox_engine::tree::{
    Content, FileIdentity, Kind, LocalNode, Names, NodeId, RemoteNode, Shape, Tree, same_content,
};
use flarebox_fs::{Entry, EntryKind, FileId, Known, Snapshot};
use flarebox_proto::v1::{
    ChangeRecord, CommitRecord, NodeKind, NodeState, TrashPage, TreePage, change_record,
};
use flarebox_store::{Cursor, EngineState, Error, StoredQuestion, SyncedNode, Tx};

use crate::disk::Stop;
use crate::observe::{Observed, Reset, RulesChange};
use crate::question::{HeldItem, Question};

pub(crate) struct Engine {
    pub trees: Trees,
    pub effects: Effects,
    pub commits: Commits,
    pub ids: IdGen,
    /// The history R belongs to and the seq of the last commit R holds. `None` until the first bootstrap.
    pub cursor: Option<Cursor>,
    /// The engine state as the store holds it, which the next checkpoint changes row by row.
    pub stored: EngineState,
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
        let stored = tx.engine_state()?;
        trees
            .restore(stored.trees.clone())
            .map_err(|error| invalid(format!("{error:?}")))?;
        trees.take_changed();
        let commits = Commits::from_state(stored.commits.clone())
            .map_err(|error| invalid(format!("{error:?}")))?;
        let effects = Effects::from_state(stored.effects.clone())
            .map_err(|error| invalid(format!("{error:?}")))?;
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
            stored,
        })
    }

    /// Ends a step that changed the engine, inside the step's transaction. It settles the trees, then writes the L
    /// row and the S row of every node whose entry changed since the last checkpoint ([`Trees::take_changed`]), what
    /// changed in the engine state, and the ID generator. A row of a node the tree no longer holds is removed. A
    /// transaction that fails ends the task, which loads the engine again from the store.
    pub fn checkpoint(&mut self, tx: &Tx<'_>) -> Result<(), Error> {
        self.trees.settle();
        for id in &self.trees.take_changed() {
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
        let state = EngineState {
            trees: self.trees.state(),
            commits: self.commits.state(),
            effects: self.effects.state(),
        };
        tx.update_engine_state(&self.stored, &state)?;
        self.stored = state;
        tx.set_id_generator(&self.ids.state.to_le_bytes())
    }

    /// Takes the answers the person gave to the engine's questions (`Driver::answer`) and removes the rows of the
    /// ones it took, in the transaction the caller's checkpoint ends. The engine routes each answer by the question's
    /// kind. An answer to a question the engine no longer asks changes nothing: what it held changed since, and the
    /// engine asks about it again.
    pub fn accept_answers(&mut self, tx: &Tx<'_>) -> Result<(), Error> {
        for stored in tx.questions()? {
            let Some(Ok(answer)) = (stored.answer.as_ref()).map(|given| given.answer.parse())
            else {
                continue;
            };
            if (self.commits).decide(&stored.question_id, answer) {
                tracing::info!(question_id = stored.question_id, %answer, "took an answer to a question");
                tx.remove_question(&stored.question_id)?;
            }
        }
        Ok(())
    }

    /// Stores each question the engine asked since the last checkpoint and removes the rows of the ones it no longer
    /// asks, for `Driver::questions` to show.
    pub fn ask(&self, tx: &Tx<'_>, space_id: &str) -> Result<(), Error> {
        let asked = &self.stored.commits.questions;
        let asking = self.commits.questions();
        let open = |questions: &[engine::Question], id: &str| questions.iter().any(|q| q.id == id);
        for question in asked.iter().filter(|question| !open(asking, &question.id)) {
            tx.remove_question(&question.id)?;
        }
        for question in asking.iter().filter(|question| !open(asked, &question.id)) {
            tracing::warn!(
                space_id,
                question_id = question.id,
                kind = ?question.kind,
                items = question.held.len(),
                "holds changes until it is answered with flarebox questions"
            );
            tx.put_question(&StoredQuestion {
                question_id: question.id.clone(),
                asked_at_ms: question.asked_at_ms as i64,
                body: serde_json::to_vec(&self.shown(space_id, question))
                    .expect("a question serializes"),
                answer: None,
            })?;
        }
        Ok(())
    }

    /// The question as the person sees it: each item the engine holds with its path, in path order.
    fn shown(&self, space_id: &str, asked: &engine::Question) -> Question {
        let mut items: Vec<HeldItem> = (asked.held.iter())
            .map(|(id, held)| HeldItem {
                node_id: id.clone(),
                path: self.synced_path(id),
                files: held.files,
            })
            .collect();
        items.sort_by(|a, b| a.path.cmp(&b.path));
        Question {
            id: asked.id.clone(),
            space_id: space_id.to_owned(),
            asked_at_ms: asked.asked_at_ms as i64,
            kind: asked.kind.clone(),
            items,
        }
    }

    /// The path of a node S holds, relative to the linked folder.
    fn synced_path(&self, id: &str) -> String {
        let synced = self.trees.synced();
        let mut names = Vec::new();
        let mut current = id;
        while let Some(shape) = synced.live(current).filter(|_| names.len() <= synced.len()) {
            names.push(shape.name.as_str());
            current = &shape.parent;
        }
        names.reverse();
        names.join("/")
    }

    /// Queues the restores asked for since the last take (`Driver::restore`) and empties the store's inbox. The
    /// caller's checkpoint saves the queue in the same transaction, so a request is in the inbox or in the engine
    /// and never in both or in neither. A request the engine already holds is not queued again.
    pub fn accept_restores(&mut self, tx: &Tx<'_>) -> Result<(), Error> {
        for (id, intent) in tx.take_restore_requests()? {
            self.commits.restore(id, intent);
        }
        Ok(())
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

    /// Records that the server's log ends below the cursor (`AHEAD_OF_SERVER`, design §4.4). The store keeps this
    /// with the cursor until a bootstrap replaces R, also when the server's head catches up first or the task
    /// restarts.
    pub fn server_behind(&mut self, tx: &Tx<'_>) -> Result<(), Stop> {
        let Some(cursor) = &mut self.cursor else {
            return Ok(());
        };
        if !cursor.ahead_of_server {
            cursor.ahead_of_server = true;
            tx.set_cursor(cursor)?;
        }
        Ok(())
    }

    /// Replaces R, in the store and in the trees, with a bootstrap's tree that the log has caught up, and sets the
    /// cursor. S and L stay, so the planner then moves what differs (design §4.4). When the server reported that it
    /// is behind the cursor ([`Engine::server_behind`]), it lost commits this device applied, and S forgets the
    /// nodes it lost ([`Trees::forget_lost`]), so that they are uploaded again, and takes back the deletes of this
    /// device it lost, so that they are sent again. A tree that is not valid stops the task before anything is
    /// replaced.
    pub fn replace_remote(&mut self, tx: &Tx<'_>, staging: Staging) -> Result<(), Stop> {
        let behind = (self.cursor.as_ref()).is_some_and(|cursor| cursor.ahead_of_server);
        let trashed = self.trees.trashed_here();
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
        if behind {
            self.trees.forget_lost(trashed, self.commits.held_through());
        }
        tx.clear_remote_nodes()?;
        for state in staging.nodes.values() {
            tx.put_remote_node(state)?;
        }
        let cursor = Cursor {
            history_id: staging.history_id,
            last_seq: staging.last_seq,
            ahead_of_server: false,
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
        let local = self.trees.local();
        let mut entries = BTreeMap::new();
        let mut pending = vec![(PathBuf::new(), local.root.clone())];
        while let Some((path, id)) = pending.pop() {
            for child in self.trees.local_children(&id) {
                let Some(node) = local.get(child) else {
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
                pending.push((at, child.clone()));
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

    /// Gives the trees the rules files a batch lists, before the changes of that batch. An observer's first batch
    /// follows its scan of the whole tree and lists every rules file, so from the first call on the trees know the
    /// rules, and the engine plans again (design §5.10), except below a folder that could not be read
    /// ([`Engine::read`]). The rules of a folder whose path L cannot hold are left out, since no node is below it.
    /// When `all` is set the batch lists every rules file its scans read, and the trees forget the rules they had.
    pub fn rules(&mut self, rules: Vec<RulesChange>, all: bool) {
        if all {
            self.trees.clear_rules();
        }
        for RulesChange { dir, rules } in rules {
            if let Some(dir) = names(&dir) {
                self.trees.set_rules(&dir, rules);
            }
        }
        self.trees.rules_scanned();
    }

    /// Gives the trees, after the changes of a batch, the folders whose rules the batch's scans established (`read`)
    /// and the folders they could not read, whose rules are unknown until a later scan establishes them. Each path is
    /// where the scan read the folder, so it names the node L holds there by its exact name: a renamed folder is the
    /// same node, and two folders whose names differ only in case are two.
    pub fn read(&mut self, read: &[PathBuf], unreadable: &[PathBuf]) {
        let read = if self.trees.unknown_rules().is_empty() {
            &[]
        } else {
            read
        };
        if read.is_empty() && unreadable.is_empty() {
            return;
        }
        let read: Vec<NodeId> = (read.iter())
            .filter_map(|dir| local_at(&self.trees, dir))
            .collect();
        let unreadable: Vec<NodeId> = (unreadable.iter())
            .filter_map(|dir| local_at(&self.trees, dir))
            .collect();
        for id in read {
            self.trees.set_rules_known(&id);
        }
        for id in unreadable {
            self.trees.set_rules_unknown(&id);
        }
    }

    /// Puts `changes` into L and returns the nodes whose L entry changed. Paths a change names in the previous state
    /// (`Removed::path`, `Renamed::from`) are resolved before any change applies. The changes apply in batch order,
    /// except that a change waits for the change of the batch that creates or moves its folder into place, and for
    /// the rename that moves the entry at its path away. A scan lists renames before creations, so otherwise a file
    /// moved into a new folder would find no folder. A cycle of such waits, as in a swap, applies in batch order, and
    /// a node whose rename is still to come is set aside while another entry takes its name, with what is under it,
    /// until its own rename places it. A renamed or
    /// modified entry is the node L holds at its path with its identity, else the node with its identity, else the
    /// node at its path. A
    /// created entry with the identity of a node L holds is that node, wherever L had it. So is a created entry with
    /// the path and identity a node had before an earlier change of the batch took it out of L, unless the batch
    /// placed that node again. That keeps a child's node when its folder is replaced around it. Any other created
    /// entry is a new node. An entry that another path also links is a hard link, a file of its own (design §5.6),
    /// so its identity never takes a node from another path.
    ///
    /// A node that held the name a change puts an entry at is gone from there, so it leaves L with everything under
    /// it. A file that arrives over a file keeps the node of the file it replaces when L's own node for the arriving
    /// file never synced, as when an editor saves by renaming a new file over the old one. The arriving node then
    /// leaves L. A new folder that arrives where L keeps a recorded folder ([`Trees::ignore_local`]) that R holds
    /// live is that folder placed again: it keeps the folder's node, and what is recorded under it stays recorded, so
    /// the server keeps it. A recorded folder R no longer holds live leaves L instead, and the new folder is a new
    /// node, so it does not bring back a folder another device deleted. A file whose content could not be read is
    /// left out until the observer reports it with its content.
    /// A rename to a path the rules exclude is a move out of what this device syncs: the node takes that position
    /// when its folder is in L or arrives in the batch, and keeps its entry otherwise ([`Trees::ignore_local`]); an
    /// entry L never held stays out.
    pub fn observe(&mut self, changes: &[Observed], now_ms: u64) -> BTreeSet<NodeId> {
        let mut before: Vec<Option<NodeId>> = changes
            .iter()
            .map(|change| match change {
                Observed::Removed { path, .. } => local_at(&self.trees, path),
                Observed::Renamed {
                    from, id, linked, ..
                } => resolve(&self.trees, from, &file_identity(*id), *linked),
                Observed::Created { .. } | Observed::Modified { .. } => None,
            })
            .collect();
        let moving: BTreeSet<NodeId> = (changes.iter().zip(&before))
            .filter(|(change, _)| matches!(change, Observed::Renamed { .. }))
            .filter_map(|(_, before)| before.clone())
            .collect();
        let arrival: BTreeMap<&Path, usize> = (changes.iter().enumerate())
            .filter_map(|(at, change)| match change {
                Observed::Created { path, .. } | Observed::Renamed { to: path, .. } => {
                    Some((path.as_path(), at))
                }
                Observed::Removed { .. } | Observed::Modified { .. } => None,
            })
            .collect();
        let leaving: BTreeMap<&Path, usize> = (changes.iter().enumerate())
            .filter_map(|(at, change)| match change {
                Observed::Renamed { from, .. } => Some((from.as_path(), at)),
                Observed::Removed { .. } | Observed::Created { .. } | Observed::Modified { .. } => {
                    None
                }
            })
            .collect();
        let mut waits = vec![0; changes.len()];
        let mut unblocks: Vec<Vec<usize>> = vec![Vec::new(); changes.len()];
        for (at, change) in changes.iter().enumerate() {
            let firsts = match change {
                Observed::Created { path, .. } | Observed::Renamed { to: path, .. } => [
                    path.parent().and_then(|dir| arrival.get(dir)),
                    leaving.get(path.as_path()),
                ],
                Observed::Modified { path, .. } => [
                    path.parent().and_then(|dir| arrival.get(dir)),
                    arrival.get(path.as_path()),
                ],
                Observed::Removed { .. } => [None, None],
            };
            for &first in firsts.into_iter().flatten().filter(|&&first| first != at) {
                waits[at] += 1;
                unblocks[first].push(at);
            }
        }
        let mut ready: BTreeSet<usize> = (0..changes.len()).filter(|&at| waits[at] == 0).collect();
        let mut order = Vec::with_capacity(changes.len());
        let mut done = vec![false; changes.len()];
        while order.len() < changes.len() {
            let at = (ready.pop_first())
                .or_else(|| (0..changes.len()).find(|&at| !done[at]))
                .expect("a change is left");
            done[at] = true;
            order.push(at);
            for &next in &unblocks[at] {
                waits[next] -= 1;
                if waits[next] == 0 && !done[next] {
                    ready.insert(next);
                }
            }
        }
        for (change, before) in changes.iter().zip(&before) {
            let Observed::Renamed { to, kind, .. } = change else {
                continue;
            };
            let kind = match kind {
                EntryKind::File => Kind::File,
                EntryKind::Dir => Kind::Dir,
            };
            if let Some(node) = before
                && names(to).is_some_and(|to| self.trees.ignores(&to, kind))
                && (to.parent()).is_some_and(|dir| {
                    !arrival.contains_key(dir) && local_at(&self.trees, dir).is_none()
                })
            {
                self.trees.ignore_local(node);
            }
        }
        let mut touched = BTreeSet::new();
        let mut gone = Gone::new();
        let mut parked = Vec::new();
        for at in order {
            let (change, before) = (&changes[at], before[at].take());
            if let Observed::Created { path, .. } | Observed::Renamed { to: path, .. } = change
                && let Some(holder) = local_at(&self.trees, path)
                && moving.contains(&holder)
                && before.as_ref() != Some(&holder)
                && let Some(node) = self.trees.local().get(&holder).cloned()
            {
                self.trees.set_local(
                    holder.clone(),
                    LocalNode {
                        parked: true,
                        ..node
                    },
                );
                parked.push(holder);
            }
            match change {
                Observed::Removed { .. } => {
                    if let Some(id) = before {
                        self.remove(&id, &moving, true, &mut touched, &mut gone);
                    }
                }
                Observed::Renamed { to, kind, id, .. } => {
                    let node_kind = match kind {
                        EntryKind::File => Kind::File,
                        EntryKind::Dir => Kind::Dir,
                    };
                    if names(to).is_some_and(|to| self.trees.ignores(&to, node_kind)) {
                        let Some(node) = &before else { continue };
                        if to
                            .parent()
                            .and_then(|dir| local_at(&self.trees, dir))
                            .is_none()
                        {
                            self.trees.ignore_local(node);
                            continue;
                        }
                    }
                    let identity = file_identity(*id);
                    let content = before
                        .as_ref()
                        .and_then(|id| self.trees.local().get(id))
                        .and_then(|node| node.shape.content.clone());
                    self.place(
                        before,
                        to,
                        *kind,
                        identity,
                        content,
                        now_ms,
                        &moving,
                        &mut touched,
                        &mut gone,
                    );
                }
                Observed::Created {
                    path,
                    kind,
                    id,
                    linked,
                    content,
                } => {
                    if *kind == EntryKind::File && content.is_none() {
                        continue;
                    }
                    let identity = file_identity(*id);
                    let node = (self.trees.local_with_identity(&identity))
                        .filter(|_| !linked)
                        .cloned()
                        .or_else(|| {
                            (gone.get(path))
                                .filter(|(node, held)| {
                                    *held == identity && self.trees.local().get(node).is_none()
                                })
                                .map(|(node, _)| node.clone())
                        });
                    self.place(
                        node,
                        path,
                        *kind,
                        identity,
                        content.clone(),
                        now_ms,
                        &moving,
                        &mut touched,
                        &mut gone,
                    );
                }
                Observed::Modified {
                    path,
                    id,
                    linked,
                    content,
                } => {
                    let Some(content) = content else { continue };
                    let identity = file_identity(*id);
                    let node = resolve(&self.trees, path, &identity, *linked);
                    self.place(
                        node,
                        path,
                        EntryKind::File,
                        identity,
                        Some(content.clone()),
                        now_ms,
                        &moving,
                        &mut touched,
                        &mut gone,
                    );
                }
            }
        }
        for id in parked {
            if self.trees.local().get(&id).is_some_and(|node| node.parked) {
                self.remove(&id, &moving, false, &mut touched, &mut gone);
            }
        }
        for id in &moving {
            let local = self.trees.local();
            let orphaned = local.get(id).is_some_and(|node| {
                node.shape.parent != local.root && local.get(&node.shape.parent).is_none()
            });
            if orphaned {
                self.remove(id, &BTreeSet::new(), false, &mut touched, &mut gone);
            }
        }
        touched
    }

    /// L's view of each of `scopes`, relative paths, for the observer: the kind and identity of the placed node at
    /// the scope and of every placed node under it. The empty path covers all of L, and a path L does not hold gets
    /// no entries. L's content for a file is left out, because it does not prove what the disk holds.
    pub fn resets(&self, scopes: impl IntoIterator<Item = PathBuf>) -> Vec<Reset> {
        let local = self.trees.local();
        scopes
            .into_iter()
            .map(|scope| {
                let mut known = BTreeMap::new();
                let mut pending: Vec<(PathBuf, NodeId)> = local_at(&self.trees, &scope)
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
                    for child in self.trees.local_children(&id).cloned().collect::<Vec<_>>() {
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
        node: Option<NodeId>,
        path: &Path,
        kind: EntryKind,
        identity: FileIdentity,
        content: Option<Content>,
        now_ms: u64,
        moving: &BTreeSet<NodeId>,
        touched: &mut BTreeSet<NodeId>,
        gone: &mut Gone,
    ) {
        let (Some(parent), Some(name)) = (
            path.parent()
                .and_then(|parent| local_at(&self.trees, parent)),
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
                && node
                    .as_ref()
                    .is_some_and(|id| !self.trees.ignored_local().contains(id))
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
        let mut id = node.unwrap_or_else(|| self.ids.uuid_v7(now_ms));
        if let Some(holder) =
            (self.trees.local_child(&parent, name)).filter(|holder| **holder != id)
        {
            let holder = holder.clone();
            let replaced = self.trees.local().get(&holder).map(|node| node.shape.kind);
            let synced =
                self.trees.synced().get(&id).is_some() || self.trees.remote().get(&id).is_some();
            let kept = self.trees.ignored_local().contains(&holder)
                && self.trees.remote().live(&holder).is_some();
            if kind == Kind::File && replaced == Some(Kind::File) && !synced {
                if self.trees.local().get(&id).is_some() {
                    self.remove(&id, moving, false, touched, gone);
                }
                id = holder;
            } else if kind == Kind::Dir
                && replaced == Some(Kind::Dir)
                && kept
                && !synced
                && self.trees.local().get(&id).is_none()
            {
                id = holder;
            } else {
                self.remove(&holder, moving, false, touched, gone);
            }
        }
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

    /// Removes `id` and every node placed under it from L, except the nodes in `moving`, which the batch moves
    /// elsewhere, with what is placed under them, and adds each node it removes to `gone`. When the disk lost the
    /// entry (`outside`), a node that [`Trees::ignore_local`] recorded keeps its entry with what is under it, and so
    /// do the folders on its way to `id`, which are recorded too: the node's local copy is in an ignored folder, so
    /// L must keep it and its folders for the server to keep them (design §5.10).
    fn remove(
        &mut self,
        id: &NodeId,
        moving: &BTreeSet<NodeId>,
        outside: bool,
        touched: &mut BTreeSet<NodeId>,
        gone: &mut Gone,
    ) {
        let mut doomed = vec![id.clone()];
        let mut held = Vec::new();
        let mut next = 0;
        while let Some(current) = doomed.get(next).cloned() {
            for child in self.trees.local_children(&current) {
                if outside && self.trees.ignored_local().contains(child) {
                    held.push(child.clone());
                } else if !moving.contains(child) {
                    doomed.push(child.clone());
                }
            }
            next += 1;
        }
        let mut kept = BTreeSet::new();
        for node in &held {
            let mut above = self.trees.local().get(node).map(|n| n.shape.parent.clone());
            while let Some(folder) = above.filter(|folder| doomed.contains(folder)) {
                if !kept.insert(folder.clone()) {
                    break;
                }
                above = self
                    .trees
                    .local()
                    .get(&folder)
                    .map(|n| n.shape.parent.clone());
            }
        }
        doomed.retain(|node| !kept.contains(node));
        for folder in &kept {
            self.trees.ignore_local(folder);
        }
        for id in &doomed {
            if let (Some(path), Some(node)) = (self.path_of(id), self.trees.local().get(id)) {
                gone.insert(path, (id.clone(), node.identity));
            }
        }
        for id in doomed {
            self.trees.remove_local(&id);
            touched.insert(id);
        }
    }
}

/// The nodes a batch took out of L, by the path each had, with the identity it had.
type Gone = BTreeMap<PathBuf, (NodeId, FileIdentity)>;

/// The placed node of L at `path`, or the root for the empty path.
fn local_at(trees: &Trees, path: &Path) -> Option<NodeId> {
    let mut current = trees.local().root.clone();
    for component in path.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        current = trees.local_child(&current, name.to_str()?)?.clone();
    }
    Some(current)
}

/// The node at `path` when it has `identity`, else the node with `identity`, else the node at `path`. The identity of
/// a file with several links (`linked`) names no single path, so it never takes a node from another path, where the
/// other link may still be.
fn resolve(trees: &Trees, path: &Path, identity: &FileIdentity, linked: bool) -> Option<NodeId> {
    let at = local_at(trees, path);
    at.clone()
        .filter(|id| (trees.local().get(id)).is_some_and(|node| node.identity == *identity))
        .or_else(|| {
            trees
                .local_with_identity(identity)
                .filter(|_| !linked)
                .cloned()
        })
        .or(at)
}

/// The names from the root to `path`, or `None` when one of them is not a name L can hold.
fn names(path: &Path) -> Option<Vec<&str>> {
    path.components()
        .map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect()
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
    /// The listing of a bootstrap. The log is replayed from the first page's `head_seq` (spec v0 section 7.5). The
    /// pages of the trash, read after the tree when the server lost commits, carry no `head_seq`: a trashed node
    /// replaces a listed one only when it is newer, as in the replay, which covers what changed while they were read
    /// (RFC 0002 §6).
    pub fn new(history_id: String, pages: Vec<TreePage>, trash: Vec<TrashPage>) -> Self {
        let mut staging = Staging {
            history_id,
            last_seq: pages.first().map_or(0, |page| page.head_seq),
            highest_page_seq: pages.iter().map(|page| page.head_seq).max().unwrap_or(0),
            caught_up: None,
            nodes: pages
                .into_iter()
                .flat_map(|page| page.nodes)
                .map(|node| (node.node_id.clone(), node))
                .collect(),
        };
        for state in trash.into_iter().flat_map(|page| page.nodes) {
            staging.take(state);
        }
        staging
    }

    /// Holds `state` unless the node it holds is as new.
    fn take(&mut self, state: NodeState) {
        let newer = self.nodes.get(&state.node_id).is_none_or(|held| {
            state.content_rev > held.content_rev || state.meta_rev > held.meta_rev
        });
        if newer {
            self.nodes.insert(state.node_id.clone(), state);
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
                Some(change_record::Change::Node(state)) => self.take(state.clone()),
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
    use flarebox_engine::effects::{Base, Effect, EffectRecord, Location};
    use flarebox_engine::state::EffectsState;
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
            ahead_of_server: false,
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
        Staging::new("h".into(), vec![page], Vec::new())
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

    fn file(parent: &str, name: &str, digest: u8) -> Shape {
        Shape {
            parent: parent.into(),
            name: name.into(),
            kind: Kind::File,
            content: Some(Content {
                size: 3,
                manifest_digest: vec![digest; 32],
                ..Default::default()
            }),
        }
    }

    fn dir(parent: &str, name: &str) -> Shape {
        Shape {
            parent: parent.into(),
            name: name.into(),
            kind: Kind::Dir,
            content: None,
        }
    }

    fn held(shape: Shape, ino: u64) -> LocalNode {
        LocalNode {
            shape,
            identity: FileIdentity {
                dev: 1,
                ino,
                birth_ns: Some(0),
            },
            parked: false,
        }
    }

    /// An effect planned for an L that a batch then changed.
    struct Case {
        name: &'static str,
        kind: &'static str,
        engine: Engine,
        record: EffectRecord,
        /// Whether L as the batch left it still takes the effect.
        takes: bool,
    }

    /// L and S hold each node's first shape and R its second. The first `done` effects are applied, the next one
    /// is planned, and `batch` then changes L.
    fn case(
        name: &'static str,
        kind: &'static str,
        nodes: &[(&str, Option<Shape>, Option<Shape>)],
        done: usize,
        batch: impl FnOnce(&mut Trees),
        takes: bool,
    ) -> Case {
        let mut trees = Trees::new(ROOT);
        for (at, (id, synced, remote)) in nodes.iter().enumerate() {
            trees.set_synced((*id).into(), synced.clone());
            if let Some(shape) = synced {
                trees.set_local((*id).into(), held(shape.clone(), at as u64 + 1));
            }
            if let Some(shape) = remote {
                let node = RemoteNode {
                    shape: shape.clone(),
                    trashed: false,
                    content_rev: 1,
                    meta_rev: 1,
                    trash_group: String::new(),
                };
                trees.set_remote((*id).into(), node);
            }
        }
        let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
        let mut plan = |trees: &mut Trees, effects: &mut Effects| {
            trees.settle();
            (effects.plan(trees, &mut ids, 0).cloned())
                .unwrap_or_else(|| panic!("{name}: no effect"))
        };
        for _ in 0..done {
            let record = plan(&mut trees, &mut effects);
            let identity = held(dir(ROOT, ""), 50).identity;
            (effects.done(&mut trees, &record.effect_id, Some(identity))).unwrap();
        }
        let record = plan(&mut trees, &mut effects);
        batch(&mut trees);
        assert_eq!(trees.check(), Ok(()), "{name}: the batch leaves L a tree");
        let engine = Engine {
            trees,
            effects,
            commits: Commits::default(),
            ids,
            cursor: None,
            stored: EngineState::default(),
        };
        Case {
            name,
            kind,
            engine,
            record,
            takes,
        }
    }

    fn kind(effect: &Effect) -> &'static str {
        let parked = |at: &Location| {
            let base = matches!(at, Location::At { base, .. } if *base != Base::Root);
            base || matches!(at, Location::Parked(_))
        };
        match effect {
            Effect::CreateDir { .. } => "create a folder",
            Effect::WriteFile { replaces: None, .. } => "write a file",
            Effect::WriteFile { .. } => "replace a file",
            Effect::Move {
                to: Location::Parked(_),
                ..
            } => "park",
            Effect::Move { from, .. } if parked(from) => "move out of the parked folder",
            Effect::Move { .. } => "move",
            Effect::RemoveFile { .. } => "remove a file",
            Effect::RemoveDir { .. } => "remove a folder",
        }
    }

    /// Every kind of effect, each with batches after which L takes it and, where one exists, batches after which
    /// L does not.
    fn cases() -> Vec<Case> {
        let other =
            |shape: Shape| move |trees: &mut Trees| trees.set_local("u".into(), held(shape, 9));
        let unchanged = |_: &mut Trees| {};
        let folder = [("n", None, Some(dir(ROOT, "a")))];
        let d = ("d", Some(dir(ROOT, "d")), Some(dir(ROOT, "d")));
        let inside = [d.clone(), ("n", None, Some(file("d", "a", 1)))];
        let edit = [("n", Some(file(ROOT, "a", 1)), Some(file(ROOT, "a", 2)))];
        let rename = [("n", Some(file(ROOT, "x", 1)), Some(file(ROOT, "a", 1)))];
        let into = [d, ("n", Some(dir(ROOT, "n")), Some(dir("d", "n")))];
        let swap = [
            ("a", Some(file(ROOT, "x", 1)), Some(file(ROOT, "y", 1))),
            ("b", Some(file(ROOT, "y", 2)), Some(file(ROOT, "x", 2))),
        ];
        let gone_file = [("n", Some(file(ROOT, "a", 1)), None)];
        let gone_folder = [("n", Some(dir(ROOT, "a")), None)];
        vec![
            case(
                "nothing changed",
                "create a folder",
                &folder,
                0,
                unchanged,
                true,
            ),
            case(
                "its name went to another node",
                "create a folder",
                &folder,
                0,
                other(dir(ROOT, "a")),
                false,
            ),
            case(
                "a name of another case was taken",
                "create a folder",
                &folder,
                0,
                other(dir(ROOT, "A")),
                true,
            ),
            case(
                "nothing changed",
                "write a file",
                &inside,
                0,
                unchanged,
                true,
            ),
            case(
                "its name went to another node",
                "write a file",
                &inside,
                0,
                other(dir("d", "a")),
                false,
            ),
            case(
                "another name was taken",
                "write a file",
                &inside,
                0,
                other(file("d", "b", 2)),
                true,
            ),
            case(
                "its folder left L",
                "write a file",
                &inside,
                0,
                |trees| trees.remove_local("d"),
                false,
            ),
            case(
                "its folder became a file",
                "write a file",
                &inside,
                0,
                |trees| trees.set_local("d".into(), held(file(ROOT, "d", 3), 8)),
                false,
            ),
            case(
                "its folder was renamed",
                "write a file",
                &inside,
                0,
                |trees| trees.set_local("d".into(), held(dir(ROOT, "renamed"), 1)),
                true,
            ),
            case(
                "nothing changed",
                "replace a file",
                &edit,
                0,
                unchanged,
                true,
            ),
            case(
                "the file was renamed and its name went to another node",
                "replace a file",
                &edit,
                0,
                |trees| {
                    trees.set_local("n".into(), held(file(ROOT, "renamed", 1), 1));
                    trees.set_local("u".into(), held(file(ROOT, "a", 3), 9));
                },
                false,
            ),
            case(
                "the file left L",
                "replace a file",
                &edit,
                0,
                |trees| trees.remove_local("n"),
                true,
            ),
            case("nothing changed", "move", &rename, 0, unchanged, true),
            case(
                "its name went to another node",
                "move",
                &rename,
                0,
                other(file(ROOT, "a", 2)),
                false,
            ),
            case("nothing changed", "move", &into, 0, unchanged, true),
            case(
                "its new folder moved into it",
                "move",
                &into,
                0,
                |trees| trees.set_local("d".into(), held(dir("n", "d"), 1)),
                false,
            ),
            case(
                "its new folder left L",
                "move",
                &into,
                0,
                |trees| trees.remove_local("d"),
                false,
            ),
            case(
                "its new folder is parked, from a folder that left L",
                "move",
                &into,
                0,
                |trees| {
                    let parked = LocalNode {
                        parked: true,
                        ..held(dir("gone", "d"), 1)
                    };
                    trees.set_local("d".into(), parked);
                },
                true,
            ),
            case("nothing changed", "park", &swap, 0, unchanged, true),
            case(
                "another node took a name beside it",
                "park",
                &swap,
                0,
                other(file(ROOT, "z", 3)),
                true,
            ),
            case(
                "the file was renamed and its name went to another node",
                "park",
                &swap,
                0,
                |trees| {
                    trees.set_local("a".into(), held(file(ROOT, "renamed", 1), 1));
                    trees.set_local("u".into(), held(file(ROOT, "x", 3), 9));
                },
                true,
            ),
            case(
                "nothing changed",
                "move out of the parked folder",
                &swap,
                2,
                unchanged,
                true,
            ),
            case(
                "its name went to another node",
                "move out of the parked folder",
                &swap,
                2,
                other(file(ROOT, "y", 3)),
                false,
            ),
            case(
                "nothing changed",
                "remove a file",
                &gone_file,
                0,
                unchanged,
                true,
            ),
            case(
                "the file left L",
                "remove a file",
                &gone_file,
                0,
                |trees| trees.remove_local("n"),
                true,
            ),
            case(
                "nothing changed",
                "remove a folder",
                &gone_folder,
                0,
                unchanged,
                true,
            ),
            case(
                "an entry was put into it",
                "remove a folder",
                &gone_folder,
                0,
                other(file("n", "new", 2)),
                false,
            ),
            case(
                "the folder left L",
                "remove a folder",
                &gone_folder,
                0,
                |trees| trees.remove_local("n"),
                true,
            ),
        ]
    }

    #[test]
    fn l_takes_an_effect_exactly_when_it_stays_a_tree_with_the_outcome_recorded() {
        let mut refused = BTreeSet::new();
        for case in cases() {
            let context = format!("{}, {}", case.kind, case.name);
            assert_eq!(kind(&case.record.effect), case.kind, "{context}");
            assert_eq!(
                case.record.applies(&case.engine.trees),
                case.takes,
                "{context}"
            );
            let (mut trees, mut effects) = (case.engine.trees.clone(), case.engine.effects.clone());
            let identity = held(dir(ROOT, ""), 60).identity;
            let done = effects.done(&mut trees, &case.record.effect_id, Some(identity));
            assert_eq!(done.is_ok(), case.takes, "{context}: {done:?}");
            assert_eq!(trees.check(), Ok(()), "{context}");
            if !case.takes {
                refused.insert(case.kind);
            }
        }
        let every = [
            "create a folder",
            "move",
            "move out of the parked folder",
            "remove a folder",
            "replace a file",
            "write a file",
        ];
        assert_eq!(refused.into_iter().collect::<Vec<_>>(), every);
    }

    /// What keeps a refused effect from being planned and refused in a loop: unblocked, with nothing in flight, the
    /// planner gives its node no effect for that L. The one exception is a folder whose new folder moved into it,
    /// which the planner parks, and L takes a parked node.
    #[test]
    fn an_effect_that_l_does_not_take_is_not_planned_again_for_that_l() {
        let mut parked = Vec::new();
        for mut case in cases().into_iter().filter(|case| !case.takes) {
            let state = EffectsState {
                in_flight: None,
                ..case.engine.effects.state()
            };
            case.engine.effects = Effects::from_state(state).unwrap();
            case.engine.trees.settle();
            let Engine {
                trees,
                effects,
                ids,
                ..
            } = &mut case.engine;
            let planned = (effects.plan(trees, ids, 0).cloned())
                .filter(|planned| planned.node == case.record.node);
            let Some(planned) = planned else {
                continue;
            };
            let context = format!("{}, {}: {planned:?}", case.kind, case.name);
            assert_ne!(planned.effect, case.record.effect, "{context}");
            assert!(planned.applies(&case.engine.trees), "{context}");
            parked.push((case.name, kind(&planned.effect)));
        }
        assert_eq!(parked, [("its new folder moved into it", "park")]);
    }

    /// An engine with an empty space whose R is caught up, for tests of the mapper.
    fn mapper() -> (tempfile::TempDir, Engine) {
        let (dir, mut store) = store();
        let engine = store.transaction(|tx| Engine::load(tx, ROOT)).unwrap();
        (dir, engine)
    }

    fn ino(ino: u64) -> FileId {
        FileId {
            dev: 1,
            ino,
            birth_ns: None,
        }
    }

    fn bytes(size: u64) -> Content {
        Content {
            size,
            manifest_digest: vec![size as u8; 32],
            ..Default::default()
        }
    }

    fn created(path: &str, id: FileId, linked: bool, size: u64) -> Observed {
        Observed::Created {
            path: path.into(),
            kind: EntryKind::File,
            id,
            linked,
            content: Some(bytes(size)),
        }
    }

    fn renamed(from: &str, to: &str, id: FileId, linked: bool) -> Observed {
        Observed::Renamed {
            from: from.into(),
            to: to.into(),
            kind: EntryKind::File,
            id,
            linked,
        }
    }

    /// The node L holds at `path`, with its identity and content size.
    fn holding(engine: &Engine, path: &str) -> Option<(NodeId, FileIdentity, u64)> {
        let id = local_at(&engine.trees, Path::new(path))?;
        let node = engine.trees.local().get(&id)?;
        let size = node
            .shape
            .content
            .as_ref()
            .map_or(0, |content| content.size);
        Some((id, node.identity, size))
    }

    /// The scan reports a folder removed and entries renamed out of it in one batch, as it can for `rm -r b && mv a
    /// b` (GUN-353). The entries keep their nodes, with what L holds below them, and only the folder leaves L.
    #[test]
    fn entries_renamed_out_of_a_folder_removed_in_the_same_batch_keep_their_nodes() {
        let (_dir, mut engine) = mapper();
        let folder = |path: &str, id: u64| Observed::Created {
            path: path.into(),
            kind: EntryKind::Dir,
            id: ino(id),
            linked: false,
            content: None,
        };
        engine.observe(
            &[
                folder("drafts", 1),
                folder("final", 2),
                folder("drafts/inner", 3),
                created("drafts/inner/two.txt", ino(4), false, 2),
                created("drafts/one.txt", ino(5), false, 1),
            ],
            0,
        );
        let held = |engine: &Engine, paths: [&str; 3]| {
            paths.map(|path| holding(engine, path).map(|(id, _, size)| (id, size)))
        };
        let before = held(
            &engine,
            ["drafts/inner", "drafts/inner/two.txt", "drafts/one.txt"],
        );
        assert!(before.iter().all(Option::is_some));
        engine.observe(
            &[
                Observed::Removed {
                    path: "drafts".into(),
                    kind: EntryKind::Dir,
                },
                Observed::Renamed {
                    from: "drafts/inner".into(),
                    to: "final/inner".into(),
                    kind: EntryKind::Dir,
                    id: ino(3),
                    linked: false,
                },
                renamed("drafts/one.txt", "final/one.txt", ino(5), false),
            ],
            0,
        );
        let after = held(
            &engine,
            ["final/inner", "final/inner/two.txt", "final/one.txt"],
        );
        assert_eq!(after, before);
        assert_eq!(holding(&engine, "drafts"), None);
        assert_eq!(engine.trees.local().iter().count(), 4);
        assert_eq!(engine.trees.check(), Ok(()));
    }

    /// An entry renamed out of a removed folder to a folder L does not hold leaves L with that folder, so L stays a
    /// tree.
    #[test]
    fn an_entry_renamed_out_of_a_removed_folder_into_no_folder_leaves_l() {
        let (_dir, mut engine) = mapper();
        let drafts = Observed::Created {
            path: "drafts".into(),
            kind: EntryKind::Dir,
            id: ino(1),
            linked: false,
            content: None,
        };
        engine.observe(&[drafts, created("drafts/one.txt", ino(2), false, 1)], 0);
        engine.observe(
            &[
                Observed::Removed {
                    path: "drafts".into(),
                    kind: EntryKind::Dir,
                },
                renamed("drafts/one.txt", "elsewhere/one.txt", ino(2), false),
            ],
            0,
        );
        assert_eq!(engine.trees.local().iter().count(), 0);
        assert_eq!(engine.trees.check(), Ok(()));
    }

    /// A batch that lists every rules file takes the place of the rules the trees had: a folder it does not list has
    /// none. Any other batch lists only the rules files that changed.
    #[test]
    fn a_batch_with_every_rules_file_drops_the_rules_it_does_not_list() {
        let (_dir, mut engine) = mapper();
        let log = || RulesChange {
            dir: "proj".into(),
            rules: Some(flarebox_engine::ignore::RuleFile::parse(b"*.log\n")),
        };
        engine.rules(vec![log()], true);
        let proj = Observed::Created {
            path: "proj".into(),
            kind: EntryKind::Dir,
            id: ino(1),
            linked: false,
            content: None,
        };
        engine.observe(&[proj, created("proj/build.log", ino(2), false, 1)], 0);
        let (log_id, _, _) = holding(&engine, "proj/build.log").unwrap();
        assert!(engine.trees.excluded(&log_id));
        engine.rules(Vec::new(), false);
        assert!(engine.trees.excluded(&log_id), "no rules file changed");
        engine.rules(Vec::new(), true);
        assert!(!engine.trees.excluded(&log_id), "no rules file is left");
    }

    /// A reset queued before a child entered L leaves the snapshot without the
    /// child. A folder made anew around the child is not the destination of a
    /// rename, so it is not a replacement, and the child keeps its node.
    #[test]
    fn a_synced_child_keeps_its_node_when_a_stale_reset_hid_it_and_its_folder_is_made_anew() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("space");
        std::fs::create_dir_all(root.join("d")).unwrap();
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let (_dir, mut engine) = mapper();
        let folder = Observed::Created {
            path: "d".into(),
            kind: EntryKind::Dir,
            id: snapshot.entries[Path::new("d")].id,
            linked: false,
            content: None,
        };
        engine.observe(&[folder], 0);
        let stale = engine.resets([PathBuf::from("d")]);
        std::fs::write(root.join("d/x"), b"abc").unwrap();
        snapshot.rescan(&root, [PathBuf::new()]).unwrap();
        let id = snapshot.entries[Path::new("d/x")].id;
        engine.observe(&[created("d/x", id, false, 3)], 1);
        let local: Vec<_> = (engine.trees.local().iter())
            .map(|(id, node)| (id.clone(), node.shape.clone()))
            .collect();
        for (id, shape) in local {
            engine.trees.set_synced(id, Some(shape));
        }
        let (synced, _, _) = holding(&engine, "d/x").unwrap();

        for reset in stale {
            snapshot.reset(&reset.scope, reset.known);
        }
        std::fs::rename(root.join("d/x"), root.join("x")).unwrap();
        std::fs::rename(root.join("d"), outer.path().join("away")).unwrap();
        std::fs::create_dir(root.join("d")).unwrap();
        std::fs::rename(root.join("x"), root.join("d/x")).unwrap();
        let changes = snapshot.rescan(&root, [PathBuf::new()]).unwrap().changes;
        assert_eq!(
            changes,
            [flarebox_fs::Change::Created {
                path: "d/x".into(),
                kind: EntryKind::File,
            }]
        );
        engine.observe(&[created("d/x", id, false, 3)], 2);
        assert_eq!(
            holding(&engine, "d/x").map(|(node, _, _)| node),
            Some(synced)
        );
    }

    /// A reset queued before a child entered L leaves the snapshot without the
    /// child. A folder renamed over the child's folder while the child is away
    /// replaces it, and the child that comes back keeps its node.
    #[test]
    fn a_synced_child_keeps_its_node_when_a_stale_reset_hid_it_and_its_folder_is_replaced() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("space");
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::create_dir(root.join("incoming")).unwrap();
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let (_dir, mut engine) = mapper();
        for path in ["d", "incoming"] {
            let folder = Observed::Created {
                path: path.into(),
                kind: EntryKind::Dir,
                id: snapshot.entries[Path::new(path)].id,
                linked: false,
                content: None,
            };
            engine.observe(&[folder], 0);
        }
        let stale = engine.resets([PathBuf::from("d")]);
        std::fs::write(root.join("d/x"), b"abc").unwrap();
        snapshot.rescan(&root, [PathBuf::new()]).unwrap();
        let id = snapshot.entries[Path::new("d/x")].id;
        engine.observe(&[created("d/x", id, false, 3)], 1);
        let local: Vec<_> = (engine.trees.local().iter())
            .map(|(id, node)| (id.clone(), node.shape.clone()))
            .collect();
        for (id, shape) in local {
            engine.trees.set_synced(id, Some(shape));
        }
        let (synced, _, _) = holding(&engine, "d/x").unwrap();
        let (incoming, _, _) = holding(&engine, "incoming").unwrap();

        for reset in stale {
            snapshot.reset(&reset.scope, reset.known);
        }
        std::fs::rename(root.join("d/x"), outer.path().join("x")).unwrap();
        std::fs::rename(root.join("incoming"), root.join("d")).unwrap();
        std::fs::rename(outer.path().join("x"), root.join("d/x")).unwrap();
        let changes = snapshot.rescan(&root, [PathBuf::new()]).unwrap().changes;
        let observed: Vec<_> = (changes.into_iter())
            .map(|change| match change {
                flarebox_fs::Change::Removed { path, kind } => Observed::Removed { path, kind },
                flarebox_fs::Change::Renamed { from, to, kind } => Observed::Renamed {
                    id: snapshot.entries[&to].id,
                    from,
                    to,
                    kind,
                    linked: false,
                },
                flarebox_fs::Change::Created { path, kind } => Observed::Created {
                    id: snapshot.entries[&path].id,
                    content: (kind == EntryKind::File).then(|| bytes(3)),
                    path,
                    kind,
                    linked: false,
                },
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert!(
            matches!(
                &observed[..],
                [
                    Observed::Removed { .. },
                    Observed::Renamed { .. },
                    Observed::Created { .. }
                ]
            ),
            "{observed:?}"
        );
        engine.observe(&observed, 2);
        assert_eq!(
            holding(&engine, "d/x").map(|(node, _, _)| node),
            Some(synced.clone())
        );
        assert_eq!(
            holding(&engine, "d").map(|(node, _, _)| node),
            Some(incoming)
        );
        assert_eq!(engine.path_of(&synced), Some(PathBuf::from("d/x")));
    }

    /// A node a batch takes out of L and then places again elsewhere is not taken back by an entry created later at
    /// its old path with its old identity, so the entry it was placed for stays in L.
    #[test]
    fn a_node_placed_again_in_a_batch_stays_where_it_was_placed() {
        let (_dir, mut engine) = mapper();
        let folder = |path: &str, id: u64| Observed::Created {
            path: path.into(),
            kind: EntryKind::Dir,
            id: ino(id),
            linked: false,
            content: None,
        };
        engine.observe(&[folder("a", 1)], 0);
        let (node, _, _) = holding(&engine, "a").unwrap();

        let batch = [
            Observed::Removed {
                path: "a".into(),
                kind: EntryKind::Dir,
            },
            Observed::Renamed {
                from: "a".into(),
                to: "b".into(),
                kind: EntryKind::Dir,
                id: ino(2),
                linked: false,
            },
            folder("a", 1),
        ];
        engine.observe(&batch, 1);
        assert_eq!(
            holding(&engine, "b").map(|(id, _, _)| id),
            Some(node.clone())
        );
        assert!(holding(&engine, "a").is_some_and(|(id, _, _)| id != node));
        assert_eq!(engine.trees.local().len(), 2);
    }

    #[test]
    fn a_file_saved_by_rename_over_another_keeps_the_node_of_the_file_it_replaces() {
        let (_dir, mut engine) = mapper();
        engine.observe(&[created("notes.txt", ino(1), false, 1)], 0);
        let (notes, _, _) = holding(&engine, "notes.txt").unwrap();
        let shape = engine.trees.local().get(&notes).unwrap().shape.clone();
        engine.trees.set_synced(notes.clone(), Some(shape));

        engine.observe(&[created(".notes.txt.swp", ino(2), false, 2)], 0);
        engine.observe(&[renamed(".notes.txt.swp", "notes.txt", ino(2), false)], 0);
        assert_eq!(
            holding(&engine, "notes.txt"),
            Some((notes.clone(), file_identity(ino(2)), 2))
        );
        assert_eq!(holding(&engine, ".notes.txt.swp"), None);
        assert_eq!(engine.trees.local().len(), 1, "L holds the file alone");

        engine.observe(&[created("draft.txt", ino(3), false, 3)], 0);
        let (draft, _, _) = holding(&engine, "draft.txt").unwrap();
        let shape = engine.trees.local().get(&draft).unwrap().shape.clone();
        engine.trees.set_synced(draft.clone(), Some(shape));
        engine.observe(&[renamed("draft.txt", "notes.txt", ino(3), false)], 0);
        assert_eq!(
            holding(&engine, "notes.txt"),
            Some((draft, file_identity(ino(3)), 3)),
            "a file that synced keeps its own node when it moves over another"
        );
        assert!(engine.trees.local().get(&notes).is_none());
    }

    #[test]
    fn a_second_hard_link_is_a_node_of_its_own() {
        let (_dir, mut engine) = mapper();
        engine.observe(&[created("one.txt", ino(1), false, 1)], 0);
        let (one, _, _) = holding(&engine, "one.txt").unwrap();

        engine.observe(&[created("two.txt", ino(1), true, 1)], 0);
        let (two, _, _) = holding(&engine, "two.txt").unwrap();
        assert_ne!(one, two);
        assert_eq!(
            holding(&engine, "one.txt"),
            Some((one.clone(), file_identity(ino(1)), 1))
        );

        let edited = Observed::Modified {
            path: "one.txt".into(),
            id: ino(1),
            linked: true,
            content: Some(bytes(4)),
        };
        engine.observe(&[edited, renamed("two.txt", "three.txt", ino(1), true)], 0);
        assert_eq!(
            holding(&engine, "one.txt"),
            Some((one, file_identity(ino(1)), 4))
        );
        assert_eq!(
            holding(&engine, "three.txt"),
            Some((two, file_identity(ino(1)), 1))
        );
        assert_eq!(holding(&engine, "two.txt"), None);

        engine.observe(&[created("four.txt", ino(5), false, 5)], 0);
        let (four, _, _) = holding(&engine, "four.txt").unwrap();
        engine.observe(&[created("five.txt", ino(5), false, 5)], 0);
        assert_eq!(
            holding(&engine, "five.txt").map(|(id, _, _)| id),
            Some(four),
            "a created entry of one link with a known identity is that node"
        );
        assert_eq!(holding(&engine, "four.txt"), None);
    }

    #[test]
    fn a_file_replaced_by_a_hard_link_to_another_keeps_its_node() {
        let (_dir, mut engine) = mapper();
        engine.observe(
            &[
                created("one.txt", ino(1), false, 1),
                created("two.txt", ino(2), false, 2),
            ],
            0,
        );
        let (one, _, _) = holding(&engine, "one.txt").unwrap();
        let (two, _, _) = holding(&engine, "two.txt").unwrap();
        let replaced = Observed::Modified {
            path: "two.txt".into(),
            id: ino(1),
            linked: true,
            content: Some(bytes(1)),
        };
        engine.observe(&[replaced], 0);
        assert_eq!(
            holding(&engine, "one.txt"),
            Some((one, file_identity(ino(1)), 1))
        );
        assert_eq!(
            holding(&engine, "two.txt"),
            Some((two, file_identity(ino(1)), 1))
        );
    }

    #[test]
    fn the_last_link_of_a_file_keeps_its_node_as_it_is_edited_and_moved() {
        let (_dir, mut engine) = mapper();
        engine.observe(
            &[
                created("one", ino(1), false, 1),
                created("two", ino(1), true, 1),
            ],
            0,
        );
        let (one, _, _) = holding(&engine, "one").unwrap();
        let (two, _, _) = holding(&engine, "two").unwrap();
        let edited = Observed::Modified {
            path: "two".into(),
            id: ino(1),
            linked: false,
            content: Some(bytes(2)),
        };
        let removed = Observed::Removed {
            path: "one".into(),
            kind: EntryKind::File,
        };
        engine.observe(&[removed, edited], 0);
        assert!(engine.trees.local().get(&one).is_none());
        assert_eq!(
            holding(&engine, "two"),
            Some((two.clone(), file_identity(ino(1)), 2))
        );
        engine.observe(&[created("three", ino(1), false, 2)], 0);
        assert_eq!(
            holding(&engine, "three"),
            Some((two, file_identity(ino(1)), 2))
        );
        assert_eq!(holding(&engine, "two"), None);
    }

    #[test]
    fn two_links_renamed_in_one_batch_keep_their_nodes() {
        let (_dir, mut engine) = mapper();
        engine.observe(
            &[
                created("one", ino(1), false, 1),
                created("two", ino(1), true, 1),
            ],
            0,
        );
        let one = holding(&engine, "one").unwrap();
        let two = holding(&engine, "two").unwrap();
        engine.observe(
            &[
                renamed("one", "three", ino(1), true),
                renamed("two", "four", ino(1), true),
            ],
            0,
        );
        assert_eq!(holding(&engine, "three"), Some(one));
        assert_eq!(holding(&engine, "four"), Some(two));
    }
}
