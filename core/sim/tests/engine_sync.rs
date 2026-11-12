// SPDX-License-Identifier: Apache-2.0

//! The engine's commits and effects against the reference server (docs/design/sync-engine.md
//! section 7.2, planner property). A device with offline local changes meets another device's
//! concurrent commits, then plans until nothing is left. It must end with L = S = R equal to the
//! server's tree, and every write either device made must still be on the server.
//!
//! Both devices also restore from the trash and restore earlier versions (section 6.3). The other
//! device commits its restores; this one asks its engine for them, as an app does, and every
//! request must end with the result the server gave its `Restore` op, sent once.

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::classify::Trees;
use koidrive_engine::commit::{Commits, Failure, Halt, MAX_OPS, Reconnect};
use koidrive_engine::effects::{Effect, Effects};
use koidrive_engine::ids::IdGen;
use koidrive_engine::ignore::{LocalRules, RuleFile};
use koidrive_engine::names::name_key;
use koidrive_engine::question::{Answer, Question, QuestionKind};
use koidrive_engine::restore::{Outcome, Rejection, Resolved, RestoreIntent};
use koidrive_engine::state::StateError;
use koidrive_engine::tree::{FileIdentity, Kind, LocalNode, Names, RemoteNode, same};
use koidrive_proto::v1::{
    ChangeRecord, ChunkRef, Commit, Content, CreateDir, CreateFile, Delete, Error, ListVersions,
    Move, NodeState, Op, OpResult, PrepareUpload, Restore, Resume, UpdateContent, UploadReceipt,
    VersionInfo, change_record, chunk_plan, error, op, op_result,
};
use koidrive_sim::space::{Role, Session, SpaceDo};
use proptest::prelude::*;
use sha2::{Digest, Sha256};

const NOW: i64 = 1_794_268_740_000;
const NAMES: &[&str] = &["a", "A", "b", "c.txt", "C.TXT", "ß", "ss"];
const STEPS: usize = 400;

/// Three bytes that say which device wrote them, so writes of different devices never compare equal.
fn content(device: u8, text: u8) -> Content {
    let bytes = [device, text, text];
    let chunks = vec![ChunkRef {
        hash: Sha256::digest(bytes).to_vec(),
        size: 3,
        encoding: 0,
    }];
    let list: Vec<u8> = chunks
        .iter()
        .flat_map(|c| c.hash.iter().copied().chain(c.size.to_le_bytes()))
        .collect();
    Content {
        size: 3,
        chunk_count: 1,
        manifest_digest: Sha256::digest(list).to_vec(),
        chunks,
        ..Default::default()
    }
}

fn session(device: &str) -> Session {
    Session {
        device_id: device.into(),
        user_id: "user".into(),
        device_name: device.into(),
        role: Role::Owner,
        max_format: 1,
    }
}

#[derive(Clone, Debug)]
struct Action {
    kind: u8,
    target: usize,
    parent: usize,
    name: usize,
    text: u8,
}

fn action(kinds: u8) -> impl Strategy<Value = Action> {
    (
        0..kinds,
        any::<usize>(),
        any::<usize>(),
        0..NAMES.len(),
        0..4u8,
    )
        .prop_map(|(kind, target, parent, name, text)| Action {
            kind,
            target,
            parent,
            name,
            text,
        })
}

fn upload(space: &mut SpaceDo, session: &Session, ops: &[Op]) -> Vec<UploadReceipt> {
    let chunks: Vec<ChunkRef> = ops
        .iter()
        .filter_map(|op| match &op.op {
            Some(op::Op::CreateFile(create)) => create.content.as_ref(),
            Some(op::Op::UpdateContent(update)) => update.content.as_ref(),
            _ => None,
        })
        .flat_map(|content| content.chunks.clone())
        .collect();
    if chunks.is_empty() {
        return Vec::new();
    }
    let plan = space
        .prepare_upload(session, NOW, &PrepareUpload { chunks })
        .unwrap();
    plan.chunks
        .iter()
        .filter(|chunk| chunk.action == chunk_plan::Action::Upload as i32)
        .map(|chunk| space.put_chunk(&chunk.hash, 3, NOW))
        .collect()
}

/// The versions of a node, newest first, as an app lists them.
fn versions(space: &SpaceDo, node_id: &str) -> Vec<VersionInfo> {
    let request = ListVersions {
        node_id: node_id.to_owned(),
        ..Default::default()
    };
    space.list_versions(&request).unwrap().versions
}

/// The server's nodes with this status (1 live, 2 trashed) and, when given, this kind (1 folder,
/// 2 file), without the root.
fn nodes_with(space: &SpaceDo, status: i32, kind: Option<i32>) -> Vec<String> {
    let nodes = space.nodes().into_iter().filter(|n| {
        n.status == status
            && kind.is_none_or(|kind| n.kind == kind)
            && n.node_id != space.space_id()
    });
    nodes.map(|n| n.node_id).collect()
}

/// One commit of one op by another device that has seen everything, on its live nodes. Kind 5
/// restores a node from the trash and kind 6 an earlier version of a file.
fn remote_action(space: &mut SpaceDo, ids: &mut IdGen, device: u8, action: &Action) {
    let root = space.space_id().to_owned();
    let live: Vec<_> = space
        .nodes()
        .into_iter()
        .filter(|n| n.status == 1 && n.node_id != root)
        .collect();
    let dirs: Vec<String> = [root.clone()]
        .into_iter()
        .chain(
            live.iter()
                .filter(|n| n.kind == 1)
                .map(|n| n.node_id.clone()),
        )
        .collect();
    let files: Vec<String> = live
        .iter()
        .filter(|n| n.kind == 2)
        .map(|n| n.node_id.clone())
        .collect();
    let parent = dirs[action.parent % dirs.len()].clone();
    let name = NAMES[action.name].to_string();
    let target =
        |list: &[String]| (!list.is_empty()).then(|| list[action.target % list.len()].clone());
    let all: Vec<String> = live.iter().map(|n| n.node_id.clone()).collect();
    let body = match action.kind {
        0 => op::Op::CreateDir(CreateDir {
            node_id: ids.uuid_v7(NOW as u64),
            parent_id: parent,
            name,
        }),
        1 => op::Op::CreateFile(CreateFile {
            node_id: ids.uuid_v7(NOW as u64),
            parent_id: parent,
            name,
            content: Some(content(device, action.text)),
        }),
        2 => match target(&files) {
            Some(node_id) => op::Op::UpdateContent(UpdateContent {
                node_id,
                content: Some(content(device, action.text)),
            }),
            None => return,
        },
        3 => match target(&all) {
            Some(node_id) => op::Op::Move(Move {
                node_id,
                new_parent_id: parent,
                new_name: name,
            }),
            None => return,
        },
        5 => match target(&nodes_with(space, 2, None)) {
            Some(node_id) => op::Op::Restore(Restore {
                node_id,
                ..Default::default()
            }),
            None => return,
        },
        6 => {
            let earlier = target(&files).map(|node_id| (versions(space, &node_id), node_id));
            match earlier {
                Some((versions, node_id)) if versions.len() > 1 => op::Op::Restore(Restore {
                    node_id,
                    version_id: versions[1 + action.name % (versions.len() - 1)]
                        .version_id
                        .clone(),
                    ..Default::default()
                }),
                _ => return,
            }
        }
        _ => match target(&all) {
            Some(node_id) => op::Op::Delete(Delete {
                node_id,
                requires: None,
            }),
            None => return,
        },
    };
    let session = session("other");
    let ops = vec![Op { op: Some(body) }];
    let receipts = upload(space, &session, &ops);
    let commit = Commit {
        commit_id: ids.uuid_v7(NOW as u64),
        base_seq: space.head_seq(),
        ops,
        receipts,
        history_id: space.history_id().to_owned(),
        device_seq: space.device_seq(&session.device_id) + 1,
        ..Default::default()
    };
    space.commit(&session, NOW, &commit).unwrap();
}

/// A restore the person asks this device's engine for (kinds 5 and 6), chosen from the server's
/// trash or from a file's versions, as an app lists them. With text 1 a node from the trash gets a
/// position, which may be one the server refuses. With text 2 the version is of a file in the
/// trash. With text 3 the node or the version is one the server does not have, as after a purge.
/// False when there was nothing to choose.
fn request_restore(
    space: &SpaceDo,
    commits: &mut Commits,
    ids: &mut IdGen,
    action: &Action,
) -> bool {
    let target =
        |list: &[String]| (!list.is_empty()).then(|| list[action.target % list.len()].clone());
    let unknown = action.text == 3;
    let intent = if action.kind == 5 {
        let node = match unknown {
            true => Some(ids.uuid_v7(NOW as u64)),
            false => target(&nodes_with(space, 2, None)),
        };
        let places: Vec<String> = [space.space_id().to_owned()]
            .into_iter()
            .chain(nodes_with(space, 1, None))
            .collect();
        let position = (action.text == 1).then(|| {
            let parent = places[action.parent % places.len()].clone();
            (parent, NAMES[action.name].to_owned())
        });
        let (parent, name) = position.unzip();
        node.map(|node| RestoreIntent::from_trash(node, parent, name))
    } else {
        let status = if action.text == 2 { 2 } else { 1 };
        target(&nodes_with(space, status, Some(2))).map(|node| {
            let versions = versions(space, &node);
            let version = match unknown {
                true => ids.uuid_v7(NOW as u64),
                false => versions[action.name % versions.len()].version_id.clone(),
            };
            RestoreIntent::version(node, version)
        })
    };
    let Some(intent) = intent else {
        return false;
    };
    assert!(commits.restore(ids.uuid_v7(NOW as u64), intent.unwrap()));
    true
}

/// The outcome the engine must report for a restore whose op the server answered with `result`.
fn outcome_of(result: &OpResult) -> Outcome {
    use op_result::Code;
    match Code::try_from(result.code) {
        Ok(Code::Applied) => Outcome::Restored,
        Ok(Code::AppliedRenamed) => Outcome::Renamed {
            name: result.name.clone(),
        },
        Ok(Code::ConflictCopy) => Outcome::Copied {
            node: result.node_id.clone(),
            name: result.name.clone(),
        },
        _ => Outcome::Rejected(Rejection::from_reason(&result.reject_reason)),
    }
}

/// What each `Restore` op of `commit` brings back if the server applies it to the space as it
/// is, by the op's index: the file with the content of the version the op names, or every file
/// that went to the trash with the op's node, the node included, with the content it holds
/// there. Empty for a restore from the trash of a node that is not in the trash.
fn restorable(space: &SpaceDo, commit: &Commit) -> BTreeMap<usize, Vec<(String, Vec<u8>)>> {
    let nodes = space.nodes();
    let mut found = BTreeMap::new();
    for (index, op) in commit.ops.iter().enumerate() {
        let Some(op::Op::Restore(restore)) = &op.op else {
            continue;
        };
        let files = if restore.version_id.is_empty() {
            let trashed = || nodes.iter().filter(|n| n.status == 2);
            let node = trashed().find(|n| n.node_id == restore.node_id);
            let mut group: Vec<&NodeState> = node.into_iter().collect();
            let mut next = 0;
            while let Some(parent) = group.get(next).copied() {
                let with_it = |n: &&NodeState| {
                    n.parent_id == parent.node_id && n.trash_group == parent.trash_group
                };
                group.extend(trashed().filter(with_it));
                next += 1;
            }
            let content = |n: &NodeState| n.current.clone()?.content;
            let held = |n: &NodeState| Some((n.node_id.clone(), content(n)?.manifest_digest));
            group.into_iter().filter_map(held).collect()
        } else {
            let request = ListVersions {
                node_id: restore.node_id.clone(),
                ..Default::default()
            };
            let versions = space.list_versions(&request).map(|list| list.versions);
            let version = versions.unwrap_or_default().into_iter();
            let version = version.filter(|v| v.version_id == restore.version_id);
            let content = version.filter_map(|v| v.content);
            content
                .map(|c| (restore.node_id.clone(), c.manifest_digest))
                .collect()
        };
        found.insert(index, files);
    }
    found
}

/// What the restores of a commit this device sent did to the writes the property tracks, with
/// the server's content before and after the commit and what each restore was to bring back
/// ([`restorable`], taken before the commit).
///
/// A version restore the server applied replaces the file's content because the person asked for
/// that: the content the server held, and an edit of the file this commit sent ahead of the
/// restore, are versions now and no lost writes.
///
/// The content a restore made current again is a write of the device that wrote it and must be
/// kept from here on: the version's content on the file, or on the copy the server made of it,
/// and the content of each file the restore brought back from the trash. Nothing else that came
/// back in the same commit counts. A move or an edit of a trashed node revives it with its
/// folders (R8), and what that revives is content the device already acknowledged.
///
/// A file a restore brought back from the trash is left out while the device holds an unsent
/// edit of it made on that same content. The person asked for the file, not for its old content
/// over the edit, so the edit replaces it next, as an edit beats a delete. A version restore has
/// no such exception: it asks for that content, and an edit made before it must not replace it.
fn restored(
    commit: &Commit,
    results: &[OpResult],
    bringing: &BTreeMap<usize, Vec<(String, Vec<u8>)>>,
    before: &BTreeSet<(String, Vec<u8>)>,
    after: &BTreeSet<(String, Vec<u8>)>,
    trees: &Trees,
    [local, remote]: [&mut BTreeSet<Vec<u8>>; 2],
) {
    use op_result::Code;
    let applied = |result: &OpResult| {
        result.code == Code::Applied as i32 || result.code == Code::AppliedRenamed as i32
    };
    for (index, (op, result)) in commit.ops.iter().zip(results).enumerate() {
        let version_restore = match &op.op {
            Some(op::Op::Restore(restore)) if !restore.version_id.is_empty() => restore,
            _ => continue,
        };
        if !applied(result) {
            continue;
        }
        let node = &version_restore.node_id;
        let held = before.iter().filter(|(id, _)| id == node);
        let held = held.map(|(_, digest)| digest.clone());
        let sent = commit.ops[..index].iter().zip(results);
        let sent = sent.filter_map(|(op, result)| match &op.op {
            Some(op::Op::UpdateContent(update)) if update.node_id == *node && applied(result) => {
                Some(update.content.as_ref()?.manifest_digest.clone())
            }
            _ => None,
        });
        for digest in held.chain(sent) {
            if !after.contains(&(node.clone(), digest.clone())) {
                local.remove(&digest);
                remote.remove(&digest);
            }
        }
    }
    let mut current = Vec::new();
    for (index, files) in bringing {
        let result = &results[*index];
        let copied = result.code == Code::ConflictCopy as i32;
        if !applied(result) && !copied {
            continue;
        }
        let from_trash =
            matches!(&commit.ops[*index].op, Some(op::Op::Restore(r)) if r.version_id.is_empty());
        for (node, digest) in files {
            let node = if copied { &result.node_id } else { node };
            let on_disk = (trees.local().get(node)).and_then(|n| n.shape.content.as_ref());
            let edited = from_trash
                && on_disk.is_some_and(|c| c.manifest_digest != *digest)
                && acknowledged(trees, node).as_ref() == Some(digest);
            if after.contains(&(node.clone(), digest.clone())) && !edited {
                current.push(digest.clone());
            }
        }
    }
    local.extend(own(current.iter().cloned(), 1));
    remote.extend(own(current.into_iter(), 2));
}

/// Where the process stops and starts again, with the engine state as the store hands it back
/// (the crash table of docs/design/sync-engine.md section 2.8). Without one, the state is still
/// reloaded at the top of every step, which is the stop after a request was taken and the stop
/// after an acknowledgement.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Cut {
    None,
    /// After a commit that carries a restore was built and saved, before it left.
    Built,
    /// After the server applied such a commit, before its response arrived.
    Applied,
    /// After the result of such a commit was saved, before R held its records.
    Answered,
    /// While an effect is in flight.
    Effect,
}

/// An identity with inode `ino`. Entries with an odd inode stand for a filesystem that reports no
/// birth time.
fn identity(ino: u64) -> FileIdentity {
    FileIdentity {
        dev: 1,
        ino: ino.into(),
        birth_ns: ino.is_multiple_of(2).then_some(1_700_000_000_000_000_000),
    }
}

/// Applies change records to R, as the receiving side does for records in seq order.
fn apply(trees: &mut Trees, changes: &[ChangeRecord]) {
    for change in changes {
        match &change.change {
            Some(change_record::Change::Node(state)) if state.node_id != trees.remote().root => {
                let (id, node) = RemoteNode::from_state(state).unwrap();
                trees.set_remote(id, node);
            }
            Some(change_record::Change::Purged(purged)) => trees.remove_remote(&purged.node_id),
            _ => {}
        }
    }
}

/// A change to what this device ignores, made while it runs (section 5.10).
#[derive(Clone, Debug)]
enum Ruling {
    /// The lines of the `.koiignore` file at the root.
    File(Vec<&'static str>),
    /// The device-local exclude list.
    Exclude(Vec<&'static str>),
    /// The device-local include list.
    Include(Vec<&'static str>),
}

const PATTERNS: &[&str] = &["a", "/b", "*.txt", "ss/", "local-*", "!a", "c.txt"];
const INCLUDES: &[&str] = &["a", "b/", "ß", "local-1/"];

fn ruling() -> impl Strategy<Value = Ruling> {
    let lines =
        |pool: &'static [&'static str]| prop::collection::vec(prop::sample::select(pool), 0..3);
    prop_oneof![
        lines(PATTERNS).prop_map(Ruling::File),
        lines(PATTERNS).prop_map(Ruling::Exclude),
        lines(INCLUDES).prop_map(Ruling::Include),
    ]
}

/// The rules as the disk and the configuration hold them. They are not part of the stored
/// state, so the device reads them again after every restart.
#[derive(Clone, Debug, Default)]
struct Rules {
    file: Vec<&'static str>,
    exclude: Vec<&'static str>,
    include: Vec<&'static str>,
}

impl Rules {
    fn read(&self, trees: &mut Trees) {
        let root: [&str; 0] = [];
        let file = RuleFile::parse(self.file.join("\n").as_bytes());
        trees.set_rules(&root, (!self.file.is_empty()).then_some(file));
        trees.set_local_rules(LocalRules::new(&self.include, &self.exclude).unwrap());
        trees.rules_scanned();
    }
}

/// The names from the root to `id` in L.
fn local_path(trees: &Trees, id: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut current = id;
    while let Some(node) = trees.local().get(current) {
        names.push(node.shape.name.clone());
        current = &node.shape.parent;
    }
    names.reverse();
    names
}

struct Device {
    trees: Trees,
    ids: IdGen,
    inos: u64,
    /// Whether the folders the user creates take names nothing else has, so none is merged.
    fresh_dir_names: bool,
    rules: Rules,
}

impl Device {
    fn identity(&mut self) -> FileIdentity {
        self.inos += 1;
        identity(self.inos)
    }

    fn rule(&mut self, ruling: &Ruling) {
        match ruling {
            Ruling::File(lines) => self.rules.file = lines.clone(),
            Ruling::Exclude(lines) => self.rules.exclude = lines.clone(),
            Ruling::Include(paths) => self.rules.include = paths.clone(),
        }
        self.rules.read(&mut self.trees);
    }

    /// One change the user makes on disk, as the next rescan reports it. The user sees no parked
    /// node and nothing under one. A scan leaves out ignored paths, so a change at one is not
    /// reported, except a move across the boundary: a node moved to an ignored name keeps its
    /// identity and is reported at that name, one moved into an ignored folder is found there by
    /// its identity, and one moved out is reported where it now is. L keeps the entries of
    /// ignored nodes, so here it still stands for the disk; a node moved into an ignored folder
    /// is out of the user's reach after that, and the user puts nothing where it was: an entry
    /// that takes the path of such a node replaces it, which is a deletion the user made.
    fn act(&mut self, action: &Action) {
        let root = self.trees.local().root.clone();
        let local = self.trees.local();
        let hidden = self.trees.ignored_local();
        let visible = |id: &str| {
            let mut current = id;
            for _ in 0..=local.len() {
                if hidden.contains(current) {
                    return false;
                }
                match local.get(current) {
                    Some(node) if node.parked => return false,
                    Some(node) => current = &node.shape.parent,
                    None => return true,
                }
            }
            false
        };
        let nodes: Vec<(String, LocalNode)> = local
            .iter()
            .filter(|(id, _)| visible(id))
            .map(|(id, node)| (id.clone(), node.clone()))
            .collect();
        let dirs: Vec<String> = [root.clone()]
            .into_iter()
            .chain(
                nodes
                    .iter()
                    .filter(|(_, n)| n.shape.kind == Kind::Dir)
                    .map(|(id, _)| id.clone()),
            )
            .collect();
        let parent = dirs[action.parent % dirs.len()].clone();
        let unique = format!("local-{}", self.inos);
        let name = if action.kind == 0 && self.fresh_dir_names {
            unique.as_str()
        } else {
            NAMES[action.name]
        };
        let taken = |id: &str| {
            local.iter().any(|(other, n)| {
                other != id
                    && !n.parked
                    && n.shape.parent == parent
                    && name_key(&n.shape.name) == name_key(name)
            })
        };
        let target = (!nodes.is_empty()).then(|| nodes[action.target % nodes.len()].clone());
        let unseen = |id: &str, kind| self.trees.ignores(&local_path(&self.trees, id), kind);
        let into_ignored = parent != root && unseen(&parent, Kind::Dir);
        let lands_unseen = |kind| {
            let mut path = local_path(&self.trees, &parent);
            path.push(name.to_owned());
            into_ignored || self.trees.ignores(&path, kind)
        };
        match (action.kind, target) {
            (0 | 1, _) if !taken("") => {
                let (kind, content) = match action.kind {
                    0 => (Kind::Dir, None),
                    _ => (Kind::File, Some(content(1, action.text))),
                };
                if lands_unseen(kind) {
                    return;
                }
                let shape = koidrive_engine::tree::Shape {
                    parent,
                    name: name.into(),
                    kind,
                    content,
                };
                let node = LocalNode {
                    shape,
                    identity: self.identity(),
                    parked: false,
                };
                self.trees.set_local(self.ids.uuid_v7(NOW as u64), node);
            }
            (2, Some((id, mut node))) if node.shape.kind == Kind::File => {
                if unseen(&id, Kind::File) {
                    return;
                }
                node.shape.content = Some(content(1, action.text));
                self.trees.set_local(id, node);
            }
            (3, Some((id, mut node))) if !taken(&id) && !self.holds(&id, &parent) => {
                let kind = node.shape.kind;
                match (unseen(&id, kind), lands_unseen(kind)) {
                    (true, true) => {}
                    (false, true) if into_ignored => self.trees.ignore_local(&id),
                    _ => {
                        node.shape.parent = parent;
                        node.shape.name = name.into();
                        self.trees.set_local(id, node);
                    }
                }
            }
            (4, Some((id, node))) if !unseen(&id, node.shape.kind) => {
                let doomed: Vec<String> = nodes
                    .iter()
                    .filter(|(other, _)| self.holds(&id, other))
                    .map(|(other, _)| other.clone())
                    .collect();
                for other in doomed {
                    self.trees.remove_local(&other);
                }
            }
            _ => {}
        }
    }

    /// True when `ancestor` is `id` or holds it in L.
    fn holds(&self, ancestor: &str, id: &str) -> bool {
        let mut current = id;
        loop {
            if current == ancestor {
                return true;
            }
            match self.trees.local().get(current) {
                Some(node) => current = &node.shape.parent,
                None => return false,
            }
        }
    }
}

fn written(contents: impl Iterator<Item = Option<Content>>, device: u8) -> BTreeSet<Vec<u8>> {
    own(contents.flatten().map(|c| c.manifest_digest), device)
}

/// The digests among `digests` that `device` wrote.
fn own(digests: impl Iterator<Item = Vec<u8>>, device: u8) -> BTreeSet<Vec<u8>> {
    let mine: BTreeSet<Vec<u8>> = (0..4)
        .map(|text| content(device, text).manifest_digest)
        .collect();
    digests.filter(|digest| mine.contains(digest)).collect()
}

/// The live server nodes that hold content, each with its manifest digest.
fn stored(space: &SpaceDo) -> BTreeSet<(String, Vec<u8>)> {
    space
        .nodes()
        .into_iter()
        .filter(|n| n.status == 1)
        .filter_map(|n| Some((n.node_id, n.current?.content?.manifest_digest)))
        .collect()
}

/// The L nodes that hold content, each with its manifest digest.
/// The content the server acknowledged for a file: its waiting S update's while one waits, and
/// S's otherwise.
fn acknowledged(trees: &Trees, id: &str) -> Option<Vec<u8>> {
    let shape = match trees.waiting().get(id) {
        Some(waiting) => waiting.shape.as_ref(),
        None => trees.synced().live(id),
    }?;
    Some(shape.content.as_ref()?.manifest_digest.clone())
}

fn held(trees: &Trees) -> BTreeSet<(String, Vec<u8>)> {
    trees
        .local()
        .iter()
        .filter_map(|(id, node)| {
            Some((
                id.clone(),
                node.shape.content.as_ref()?.manifest_digest.clone(),
            ))
        })
        .collect()
}

/// Forgets the writes that left a node holding content in `before` by `after`, which the user
/// or the other device overwrote or deleted, and returns the digests that nodes in `after` hold
/// anew. Content equal to the node's acknowledged content, its waiting S update's while one
/// waits and S's otherwise, is not a write the engine can tell apart from no change at all, so it
/// is left out.
fn rewritten(
    before: &BTreeSet<(String, Vec<u8>)>,
    after: &BTreeSet<(String, Vec<u8>)>,
    trees: &Trees,
    mut writes: [&mut BTreeSet<Vec<u8>>; 2],
) -> Vec<Vec<u8>> {
    for (_, gone) in before.difference(after) {
        for written in writes.iter_mut() {
            written.remove(gone);
        }
    }
    after
        .difference(before)
        .filter(|(id, digest)| acknowledged(trees, id).as_ref() != Some(digest))
        .map(|(_, digest)| digest.clone())
        .collect()
}

/// Rebuilds the engine state the way the store does after a restart, with the rules and the moves
/// into ignored folders that the first scan reports again, and checks that each part comes back
/// equal.
fn reload(
    trees: &Trees,
    commits: &Commits,
    effects: &Effects,
    rules: &Rules,
) -> Result<(Trees, Commits, Effects), TestCaseError> {
    let mut loaded = Trees::new(trees.remote().root.clone());
    for (id, node) in trees.remote().iter() {
        loaded.set_remote(id.clone(), node.clone());
    }
    for (id, node) in trees.local().iter() {
        loaded.set_local(id.clone(), node.clone());
    }
    for (id, shape) in trees.synced().iter() {
        loaded.set_synced(id.clone(), Some(shape.clone()));
    }
    let refused = |error: StateError| TestCaseError::fail(format!("refused: {error:?}"));
    loaded.restore(trees.state()).map_err(refused)?;
    prop_assert!(!loaded.rules_known());
    rules.read(&mut loaded);
    let restored_commits = Commits::from_state(commits.state()).map_err(refused)?;
    restored_commits.hold(&mut loaded);
    let restored_effects = Effects::from_state(effects.state()).map_err(refused)?;
    prop_assert_eq!(&loaded, trees);
    prop_assert_eq!(&restored_commits, commits);
    prop_assert_eq!(&restored_effects, effects);
    Ok((loaded, restored_commits, restored_effects))
}

/// The server as [`run`] seeds it.
fn seeded(seed: &[Action]) -> (SpaceDo, IdGen) {
    let mut space = SpaceDo::new(5, NOW);
    let mut other = IdGen::new(6);
    for action in seed {
        remote_action(&mut space, &mut other, 0, action);
    }
    (space, other)
}

/// How a run ended.
struct Ran {
    /// The most reverted nodes that were parked at one time.
    parked_reverted: usize,
    /// The restores this device asked for, as they ended.
    resolved: Vec<Resolved>,
    /// The `Delete` ops the device sent.
    deletes: usize,
    /// The effects that removed a file or a folder from the device's disk, not counting a folder
    /// the server never held: one created here and merged into another, removed once empty.
    removals: usize,
    /// The nodes the server holds trashed at the end.
    trashed: usize,
    /// The questions the run answered.
    asked: usize,
    space: SpaceDo,
}

/// `fits` is the most ops a commit may hold before the server refuses it with `LIMIT_EXCEEDED`,
/// standing in for the 1 MiB message limit (spec v0 section 7.2). Returns the most reverted nodes
/// that were parked at once.
fn run(
    seed: &[Action],
    local: &[Action],
    remote: &[Action],
    fits: usize,
    fresh_dir_names: bool,
    during: &[(bool, Vec<Action>)],
) -> Result<usize, TestCaseError> {
    let ran = run_cut(
        seed,
        local,
        remote,
        fits,
        fresh_dir_names,
        during,
        Cut::None,
    )?;
    Ok(ran.parked_reverted)
}

/// Every node a question holds.
fn members(question: &Question) -> impl Iterator<Item = String> + '_ {
    (question.held.values()).flat_map(|held| held.members.keys().cloned())
}

/// Whether the effects cannot place `id` because of an excluded node: on the way from the node's
/// R position to the root, a name is held in L by an excluded node, whose local copy stays where
/// it is, or by a node that waits the same way, or a folder on the way waits itself or is
/// excluded and not where R has it. A folder R
/// no longer holds waits when L still holds such a node in it, since only an empty folder is
/// removed. Such a node stays as it is, and the driver reports it as a problem (section 5.1).
fn waits_on_excluded(trees: &Trees, id: &str, waiting: &BTreeSet<String>) -> bool {
    let mut current = id;
    for _ in 0..=trees.remote().len() {
        let Some(remote) = trees.remote().live(current) else {
            let stays = |other: &str| trees.ignored(other) || waiting.contains(other);
            return current == id
                && (trees.local().iter())
                    .any(|(other, node)| node.shape.parent == id && stays(other));
        };
        let placed = |node: &LocalNode| !node.parked && node.shape.same_position(remote);
        let unplaced = !trees.local().get(current).is_some_and(placed);
        if current != id && (waiting.contains(current) || (unplaced && trees.ignored(current))) {
            return true;
        }
        let key = name_key(&remote.name);
        let holder = trees.local().iter().find(|(other, node)| {
            *other != current
                && !node.parked
                && node.shape.parent == remote.parent
                && name_key(&node.shape.name) == key
        });
        if holder.is_some_and(|(other, _)| trees.excluded(other) || waiting.contains(other)) {
            return true;
        }
        if remote.parent == trees.remote().root {
            return false;
        }
        current = &remote.parent;
    }
    false
}

/// Whether S cannot take `id` at its R position because the S entry of a node that waits still
/// holds that name. The entry moves only once its node is placed, which the excluded node keeps
/// from happening.
fn waits_on_synced(trees: &Trees, id: &str, waiting: &BTreeSet<String>) -> bool {
    let Some(remote) = trees.remote().live(id) else {
        return false;
    };
    let key = name_key(&remote.name);
    trees.synced().iter().any(|(other, shape)| {
        other != id
            && waiting.contains(other)
            && shape.parent == remote.parent
            && name_key(&shape.name) == key
    })
}

/// [`run`], with the process stopping at `cut`. An action of kind 5 or 6 on this device asks its
/// engine for a restore.
fn run_cut(
    seed: &[Action],
    local: &[Action],
    remote: &[Action],
    fits: usize,
    fresh_dir_names: bool,
    during: &[(bool, Vec<Action>)],
    cut: Cut,
) -> Result<Ran, TestCaseError> {
    run_ignoring(
        seed,
        local,
        remote,
        fits,
        fresh_dir_names,
        during,
        cut,
        &[],
        Answer::Delete,
        false,
    )
}

/// [`run_cut`] with changes to the ignore rules: the first of `rulings` is made before the offline
/// local changes, and one more before each of the `during` changes. At the end a node the device
/// excludes, or cannot place because of an excluded one ([`waits_on_excluded`]), must have R equal
/// to the server's node, and every other node L = S = R. Every write must be on the server or on
/// the local copy of such a node, which keeps it unsent.
#[allow(clippy::too_many_arguments)]
fn run_ignoring(
    seed: &[Action],
    local: &[Action],
    remote: &[Action],
    fits: usize,
    fresh_dir_names: bool,
    during: &[(bool, Vec<Action>)],
    cut: Cut,
    rulings: &[Ruling],
    answer: Answer,
    first_link: bool,
) -> Result<Ran, TestCaseError> {
    let (mut space, mut other) = seeded(seed);
    let root = space.space_id().to_owned();
    let history = space.history_id().to_owned();
    let mut device = Device {
        trees: Trees::new(root.clone()),
        ids: IdGen::new(7),
        inos: 0,
        fresh_dir_names,
        rules: Rules::default(),
    };
    for state in space.nodes().iter().filter(|n| n.node_id != root) {
        let (id, node) = RemoteNode::from_state(state).unwrap();
        if !node.trashed && !first_link {
            device
                .trees
                .set_synced(id.clone(), Some(node.shape.clone()));
            let local = LocalNode {
                shape: node.shape.clone(),
                identity: device.identity(),
                parked: false,
            };
            device.trees.set_local(id.clone(), local);
        }
        device.trees.set_remote(id, node);
    }
    let mut last_seq = space.head_seq();

    let mut commits = Commits::default();
    let (mut requested, mut sent) = (0, 0);
    let mut rulings = rulings.iter();
    if let Some(ruling) = rulings.next() {
        device.rule(ruling);
    }
    for action in local {
        match action.kind {
            5 | 6 => {
                let asked = request_restore(&space, &mut commits, &mut device.ids, action);
                requested += usize::from(asked);
            }
            _ => device.act(action),
        }
    }
    if first_link {
        device.trees.settle();
        commits.first_link(&mut device.trees, &mut device.ids, NOW as u64);
    }
    let mut local_writes = written(
        device
            .trees
            .local()
            .iter()
            .map(|(_, n)| n.shape.content.clone()),
        1,
    );
    for action in remote {
        remote_action(&mut space, &mut other, 2, action);
    }
    let mut remote_writes = written(
        space
            .nodes()
            .into_iter()
            .filter(|n| n.status == 1)
            .map(|n| n.current.and_then(|v| v.content)),
        2,
    );
    let backlog = space
        .resume(&Resume {
            history_id: history.clone(),
            since_seq: last_seq,
            ..Default::default()
        })
        .unwrap();
    for record in backlog {
        prop_assert_eq!(record.seq, last_seq + 1);
        apply(&mut device.trees, &record.changes);
        last_seq = record.seq;
    }

    let me = session("me");
    let mut effects = Effects::default();
    let (mut steps, mut quiet, mut due, mut parked_reverted) = (0, false, false, 0);
    let (mut deletes, mut removals) = (0, 0);
    let (mut asked, mut held_once, mut confirmed) = (0, BTreeSet::new(), BTreeSet::new());
    let mut discarded: BTreeMap<String, koidrive_engine::tree::Shape> = BTreeMap::new();
    let mut revived: BTreeSet<String> = BTreeSet::new();
    let mut pending: Option<Question> = None;
    let mut during = during.iter();
    while !quiet {
        steps += 1;
        prop_assert!(steps <= STEPS, "no convergence within {} steps", STEPS);
        device.trees.settle();
        if (due || steps % 2 == 0)
            && let Some((on_device, actions)) = during.next()
        {
            due = false;
            if let Some(ruling) = rulings.next() {
                device.rule(ruling);
            }
            if *on_device {
                let before = held(&device.trees);
                for action in actions {
                    match action.kind {
                        5 | 6 => {
                            let ids = &mut device.ids;
                            let asked = request_restore(&space, &mut commits, ids, action);
                            requested += usize::from(asked);
                        }
                        _ => device.act(action),
                    }
                }
                let after = held(&device.trees);
                let writes = [&mut local_writes, &mut remote_writes];
                let new = rewritten(&before, &after, &device.trees, writes);
                local_writes.extend(own(new.into_iter(), 1));
            } else {
                let before = stored(&space);
                for action in actions {
                    remote_action(&mut space, &mut other, 2, action);
                }
                let after = stored(&space);
                let writes = [&mut local_writes, &mut remote_writes];
                let new = rewritten(&before, &after, &device.trees, writes);
                remote_writes.extend(own(new.into_iter(), 2));
                let backlog = space
                    .resume(&Resume {
                        history_id: history.clone(),
                        since_seq: last_seq,
                        ..Default::default()
                    })
                    .unwrap();
                for record in backlog {
                    apply(&mut device.trees, &record.changes);
                    last_seq = record.seq;
                }
            }
        }
        device.trees.settle();
        prop_assert_eq!(device.trees.check(), Ok(()));
        (device.trees, commits, effects) =
            reload(&device.trees, &commits, &effects, &device.rules)?;
        let reverted = device.trees.reverted();
        parked_reverted = parked_reverted.max(
            device
                .trees
                .local()
                .iter()
                .filter(|(id, node)| node.parked && reverted.contains(*id))
                .count(),
        );
        if let Some(record) = effects
            .plan(&device.trees, &mut device.ids, NOW as u64)
            .cloned()
        {
            let known = device.trees.remote().get(&record.node).is_some()
                || device.trees.synced().live(&record.node).is_some();
            let removes = match record.effect {
                Effect::RemoveFile { .. } => true,
                Effect::RemoveDir { .. } => known,
                _ => false,
            };
            removals += usize::from(removes);
            if cut == Cut::Effect {
                (device.trees, commits, effects) =
                    reload(&device.trees, &commits, &effects, &device.rules)?;
                prop_assert_eq!(effects.in_flight(), Some(&record));
            }
            let identity = device.identity();
            effects
                .done(&mut device.trees, &record.effect_id, Some(identity))
                .unwrap();
            continue;
        }
        let before = device.trees.clone();
        let built = commits.build(
            &mut device.trees,
            &mut device.ids,
            NOW as u64,
            &history,
            last_seq,
        );
        let built = built.cloned();
        prop_assert!(
            device.trees.synced().check(Names::ByKey).is_ok(),
            "S is not a valid tree"
        );
        let open = |question: &Question| commits.questions().iter().any(|o| o.id == question.id);
        if let Some(question) = pending.take_if(|question| !open(question)) {
            let asked_again = (commits.questions().iter().flat_map(Question::holds))
                .any(|id| question.shown.contains_key(id));
            if !asked_again {
                let contents = (question.shown.values()).map(|shape| shape.content.clone());
                let contents = written(contents, 1);
                local_writes.retain(|write| !contents.contains(write));
                discarded.extend(question.shown);
            }
        }
        // A discarded entry the user changed, or a discarded folder that holds an entry the answer
        // did not discard there, is the user's, and so is every discarded folder above it.
        let trees = &device.trees;
        let reclaimed = |id: &&String| {
            let changed =
                (trees.local().get(id)).is_some_and(|node| !node.shape.same(&discarded[*id]));
            changed
                || (trees.local_children(id)).any(|child| {
                    !trees.excluded(child)
                        && discarded
                            .get(child)
                            .is_none_or(|shown| &shown.parent != *id)
                })
        };
        for id in discarded.keys().filter(reclaimed) {
            let mut next = Some(id.clone());
            while let Some(id) = next.take_if(|id| discarded.contains_key(id)) {
                next = (trees.local().get(&id)).map(|node| node.shape.parent.clone());
                revived.insert(id);
            }
        }
        let guarded = (commits.questions().iter())
            .find(|question| !matches!(question.kind, QuestionKind::Conflict { .. }))
            .cloned();
        if let Some(question) = &guarded {
            held_once.extend(members(question));
        }
        let Some(commit) = built else {
            if let Some(question) = guarded {
                if matches!(answer, Answer::Delete | Answer::Publish) {
                    confirmed.extend(members(&question));
                }
                prop_assert!(commits.decide(&question.id, answer));
                if answer == Answer::Discard {
                    pending = Some(question.clone());
                }
                asked += 1;
                continue;
            }
            quiet = device.trees == before && during.len() == 0;
            continue;
        };
        for op in &commit.ops {
            let id = match &op.op {
                Some(op::Op::Delete(delete)) => &delete.node_id,
                Some(op::Op::CreateFile(create)) => &create.node_id,
                Some(op::Op::CreateDir(create)) => &create.node_id,
                Some(op::Op::Move(moved)) => &moved.node_id,
                Some(op::Op::UpdateContent(update)) => &update.node_id,
                _ => continue,
            };
            // A discarded entry the user changed, or a discarded folder the user put an entry in,
            // before the effects moved it to the local trash is the user's new change, which syncs
            // like any other from then on (section 5.2).
            let changed = |shown: &koidrive_engine::tree::Shape| {
                revived.contains(id)
                    || (device.trees.local().get(id)).is_some_and(|node| !node.shape.same(shown))
            };
            if held_once.contains(id) && !confirmed.contains(id) {
                prop_assert!(
                    discarded.get(id).is_some_and(changed),
                    "{} was held and sent before the person said so",
                    id
                );
                confirmed.insert(id.clone());
            }
        }
        let delete = |op: &&Op| matches!(op.op, Some(op::Op::Delete(_)));
        deletes += commit.ops.iter().filter(delete).count();
        if commit.ops.len() > fits {
            prop_assert_eq!(
                commits.failed(&Error {
                    code: error::Code::LimitExceeded as i32,
                    ..Default::default()
                }),
                Ok(Failure::Split)
            );
            continue;
        }
        let restores: Vec<usize> = (0..commit.ops.len())
            .filter(|index| matches!(commit.ops[*index].op, Some(op::Op::Restore(_))))
            .collect();
        let cut = if restores.is_empty() { Cut::None } else { cut };
        let in_flight = |commits: &Commits| commits.in_flight().map(|f| f.commit.clone());
        if cut == Cut::Built {
            (device.trees, commits, effects) =
                reload(&device.trees, &commits, &effects, &device.rules)?;
            prop_assert_eq!(commits.connected(&space.welcome(&me)), Reconnect::Resend);
            prop_assert_eq!(in_flight(&commits), Some(commit.clone()));
        }
        let receipts = upload(&mut space, &me, &commit.ops);
        let request = Commit {
            receipts,
            ..commit.clone()
        };
        let failed = |e| TestCaseError::fail(format!("{e:?}"));
        let bringing = restorable(&space, &commit);
        let before = stored(&space);
        let mut committed = space.commit(&me, NOW, &request).map_err(failed)?;
        let after = stored(&space);
        if cut == Cut::Applied {
            // The response is lost. The server counts the commit, so the device sends the same
            // commit again and gets the stored result: nothing is applied a second time.
            (device.trees, commits, effects) =
                reload(&device.trees, &commits, &effects, &device.rules)?;
            prop_assert_eq!(commits.connected(&space.welcome(&me)), Reconnect::Resend);
            prop_assert_eq!(in_flight(&commits), Some(commit.clone()));
            let head = space.head_seq();
            let again = space.commit(&me, NOW, &request).map_err(failed)?;
            prop_assert_eq!(&again.result, &committed.result);
            prop_assert_eq!((space.head_seq(), stored(&space)), (head, after.clone()));
            committed = again;
        }
        let result = committed.result;
        prop_assert_eq!(result.seq, last_seq + 1);
        let ended = commits.resolved().len();
        if cut == Cut::Answered {
            commits.received(result.clone()).unwrap();
            (device.trees, commits, effects) =
                reload(&device.trees, &commits, &effects, &device.rules)?;
            prop_assert_eq!(commits.connected(&space.welcome(&me)), Reconnect::Idle);
            prop_assert!(!commits.acknowledge(&mut device.trees, last_seq));
            prop_assert_eq!(commits.resolved().len(), ended);
            apply(&mut device.trees, &result.changes);
        } else {
            apply(&mut device.trees, &result.changes);
            commits.received(result.clone()).unwrap();
        }
        last_seq = result.seq;
        prop_assert!(commits.acknowledge(&mut device.trees, last_seq));
        due = true;
        if restores.is_empty() {
            continue;
        }
        sent += restores.len();
        let ended = &commits.resolved()[ended..];
        prop_assert_eq!(ended.len(), restores.len());
        for (ended, index) in ended.iter().zip(&restores) {
            let asked = Some(op::Op::Restore(ended.intent.op().clone()));
            prop_assert_eq!(&asked, &commit.ops[*index].op);
            prop_assert_eq!(&ended.outcome, &outcome_of(&result.results[*index]));
        }
        let writes = [&mut local_writes, &mut remote_writes];
        let results = &result.results;
        restored(
            &commit,
            results,
            &bringing,
            &before,
            &after,
            &device.trees,
            writes,
        );
    }
    prop_assert!(commits.restores().is_empty(), "{:?}", commits.restores());
    prop_assert_eq!(
        (commits.resolved().len(), sent),
        (requested, requested),
        "every restore asked for ends, and its op is sent once"
    );

    device.trees.settle();
    let unsettled: Vec<String> = (device.trees.work().into_iter())
        .map(|(id, _)| id)
        .filter(|id| !device.trees.ignored(id))
        .collect();
    let mut waiting: BTreeSet<String> = BTreeSet::new();
    loop {
        let more: Vec<String> = (unsettled.iter())
            .filter(|id| {
                !waiting.contains(*id)
                    && (waits_on_excluded(&device.trees, id, &waiting)
                        || waits_on_synced(&device.trees, id, &waiting))
            })
            .cloned()
            .collect();
        if more.is_empty() {
            break;
        }
        waiting.extend(more);
    }
    let unsettled: Vec<&String> = (unsettled.iter())
        .filter(|id| !waiting.contains(*id))
        .collect();
    prop_assert_eq!(unsettled, Vec::<&String>::new());
    prop_assert!(commits.problems().is_empty(), "{:?}", commits.problems());
    prop_assert!(effects.blocked().is_empty(), "{:?}", effects.blocked());
    let stays_parked = |id: &str| device.trees.ignored(id) || waiting.contains(id);
    prop_assert!(
        (device.trees.local().iter()).all(|(id, node)| !node.parked || stays_parked(id)),
        "a node the rules do not leave out is still parked"
    );
    let server: Vec<(String, Option<koidrive_engine::tree::Shape>)> = space
        .nodes()
        .iter()
        .filter(|n| n.node_id != root)
        .map(|state| {
            let (id, node) = RemoteNode::from_state(state).unwrap();
            (id, (!node.trashed).then_some(node.shape))
        })
        .collect();
    let mut ids: BTreeSet<&String> = server.iter().map(|(id, _)| id).collect();
    ids.extend(device.trees.local().iter().map(|(id, _)| id));
    ids.extend(device.trees.synced().iter().map(|(id, _)| id));
    for id in ids {
        let on_server = server
            .iter()
            .find(|(other, _)| other == id)
            .and_then(|(_, shape)| shape.as_ref());
        if device.trees.ignored(id) || waiting.contains(id) {
            prop_assert!(
                same(device.trees.remote().live(id), on_server),
                "R of excluded {}",
                id
            );
            continue;
        }
        let local = device.trees.local().get(id).map(|n| &n.shape);
        prop_assert!(
            same(local, on_server),
            "L of {} is {:?}, the server has {:?}",
            id,
            local,
            on_server
        );
        prop_assert!(
            same(device.trees.synced().live(id), on_server),
            "S of {}",
            id
        );
        prop_assert!(
            same(device.trees.remote().live(id), on_server),
            "R of {}",
            id
        );
    }
    let unsent = (device.trees.local().iter())
        .filter(|(id, _)| device.trees.ignored(id) || waiting.contains(*id))
        .map(|(_, node)| node.shape.content.clone());
    let kept = written(
        server
            .iter()
            .map(|(_, s)| s.clone().and_then(|s| s.content))
            .chain(unsent),
        1,
    );
    prop_assert!(local_writes.is_subset(&kept), "a local write was lost");
    let kept = written(
        server
            .iter()
            .map(|(_, s)| s.clone().and_then(|s| s.content)),
        2,
    );
    prop_assert!(remote_writes.is_subset(&kept), "a remote write was lost");
    Ok(Ran {
        parked_reverted,
        resolved: commits.resolved().to_vec(),
        deletes,
        removals,
        trashed: server.iter().filter(|(_, shape)| shape.is_none()).count(),
        asked,
        space,
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|cases| cases.parse().ok()).unwrap_or(1024),
    ))]

    #[test]
    fn a_device_with_local_changes_converges_with_the_server_and_keeps_every_write(
        seed in prop::collection::vec(action(2), 0..12),
        local in prop::collection::vec(action(7), 0..8),
        remote in prop::collection::vec(action(7), 0..8),
        fits in prop_oneof![Just(MAX_OPS), 1usize..4],
        fresh_dir_names in any::<bool>(),
        during in prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(7), 1..4)),
            0..6,
        ),
        cut in prop::sample::select(vec![
            Cut::None,
            Cut::Built,
            Cut::Applied,
            Cut::Answered,
            Cut::Effect,
        ]),
    ) {
        run_cut(&seed, &local, &remote, fits, fresh_dir_names, &during, cut)?;
    }

    /// Section 5.10: becoming ignored never deletes anything, on either side. Neither device
    /// deletes here, so whatever the rules and the moves into and out of ignored paths do, the
    /// device sends no `Delete`, removes nothing from its disk, and the server trashes nothing,
    /// across the restart the run makes at every step.
    #[test]
    fn a_device_whose_rules_change_deletes_nothing_on_either_side(
        seed in prop::collection::vec(action(2), 0..12),
        local in prop::collection::vec(action(4), 0..8),
        remote in prop::collection::vec(action(4), 0..8),
        fits in prop_oneof![Just(MAX_OPS), 1usize..4],
        during in prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(4), 1..4)),
            0..6,
        ),
        rulings in prop::collection::vec(ruling(), 0..7),
    ) {
        let ran = run_ignoring(
            &seed,
            &local,
            &remote,
            fits,
            true,
            &during,
            Cut::None,
            &rulings,
            Answer::Delete,
            false,
        )?;
        prop_assert_eq!((ran.deletes, ran.removals, ran.trashed), (0, 0, 0));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|cases| cases.parse().ok()).unwrap_or(128),
    ))]

    /// Section 6.1: removing a folder that holds more than half of a space of at least 20 files
    /// is held, among other local changes that still sync. No `Delete` of a held node goes out
    /// before the person answers, across the restart the run makes at every step. After
    /// `restore` the files come back on disk and the server trashes nothing; after `delete` the
    /// server trashes the folder and every file in it.
    #[test]
    fn removing_most_of_a_space_is_held_until_the_person_answers(
        inside in 11usize..20,
        loose in 9usize..11,
        local in prop::collection::vec(action(3), 0..4),
        fits in prop_oneof![Just(MAX_OPS), 1usize..4],
        answer in prop_oneof![Just(Answer::Delete), Just(Answer::Restore)],
        cut in prop::sample::select(CUTS.to_vec()),
    ) {
        let mut seed = vec![act(0, 0, 0, 0, 0)];
        seed.extend((0..inside).map(|i| act(1, 0, 1, i % NAMES.len(), 0)));
        seed.extend((0..loose).map(|i| act(1, 0, 0, i % NAMES.len(), 0)));
        let (space, _) = seeded(&seed);
        let root = space.space_id().to_owned();
        let mut live: Vec<_> = (space.nodes().into_iter())
            .filter(|node| node.status == 1 && node.node_id != root)
            .collect();
        live.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        prop_assert_eq!(live.len(), inside + loose + 1, "every seeded file has its own name");
        let folder = live.iter().position(|node| node.kind == 1).unwrap();
        let mut actions = vec![act(4, folder, 0, 0, 0)];
        actions.extend(local);

        let ran = run_ignoring(&seed, &actions, &[], fits, true, &[], cut, &[], answer, false)?;

        prop_assert!(ran.asked >= 1, "the removal was asked about");
        match answer {
            Answer::Restore => prop_assert_eq!((ran.deletes, ran.removals, ran.trashed), (0, 0, 0)),
            Answer::Delete => prop_assert_eq!(ran.trashed, inside + 1),
            other => unreachable!("the guard does not offer {other}"),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|cases| cases.parse().ok()).unwrap_or(256),
    ))]

    /// Section 6.4: on the first link of a folder to a space that holds something, what both hold
    /// alike is adopted and the rest of the folder is held. No create of a held entry goes out
    /// before the person publishes it, across the restart the run makes at every step, while the
    /// user and the other device go on changing things. After `publish` every local write is on the
    /// server; after `discard` the discarded entries are in the local trash and every other write
    /// is on the server.
    #[test]
    fn a_first_link_sends_nothing_it_holds_until_the_person_publishes_it(
        seed in prop::collection::vec(action(2), 1..12),
        local in prop::collection::vec(action(2), 1..8),
        fits in prop_oneof![Just(MAX_OPS), 1usize..4],
        during in prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(5), 1..4)),
            0..6,
        ),
        answer in prop_oneof![Just(Answer::Publish), Just(Answer::Discard)],
        cut in prop::sample::select(CUTS.to_vec()),
    ) {
        run_ignoring(&seed, &local, &[], fits, false, &during, cut, &[], answer, true)?;
    }

    #[test]
    fn a_first_link_sends_nothing_the_rules_leave_out_for_a_while_until_the_person_publishes_it(
        seed in prop::collection::vec(action(2), 1..12),
        local in prop::collection::vec(action(2), 1..8),
        fits in prop_oneof![Just(MAX_OPS), 1usize..4],
        during in prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(4), 1..4)),
            0..6,
        ),
        rulings in prop::collection::vec(ruling(), 0..7),
        cut in prop::sample::select(CUTS.to_vec()),
    ) {
        let answer = Answer::Publish;
        run_ignoring(&seed, &local, &[], fits, false, &during, cut, &rulings, answer, true)?;
    }
}

#[test]
fn a_server_file_moved_into_a_held_folder_while_its_s_update_waits_is_not_held() {
    let seed = [
        act(0, 0, 0, 2, 0),
        act(1, 0, 0, 0, 0),
        act(1, 0, 0, 0, 0),
        act(1, 0, 0, 0, 0),
    ];
    let local = [
        act(0, 0, 0, 0, 0),
        act(0, 0, 0, 2, 0),
        act(0, 0, 0, 3, 0),
        act(0, 0, 3, 0, 0),
    ];
    let during = [
        (
            false,
            vec![act(4, 0, 0, 0, 0), act(4, 1, 0, 0, 0), act(1, 0, 0, 2, 0)],
        ),
        (true, vec![act(3, 0, 1, 0, 0), act(2, 1, 0, 0, 0)]),
        (true, vec![act(4, 0, 0, 0, 0)]),
    ];
    for answer in [Answer::Publish, Answer::Discard] {
        run_ignoring(
            &seed,
            &local,
            &[],
            MAX_OPS,
            false,
            &during,
            Cut::None,
            &[],
            answer,
            true,
        )
        .unwrap();
    }
}

#[test]
fn a_first_link_holds_a_file_whose_name_the_space_holds_until_the_person_answers() {
    for answer in [Answer::Publish, Answer::Discard] {
        let seed = [act(1, 0, 0, 0, 0)];
        let local = [act(1, 0, 0, 0, 0), act(0, 0, 0, 2, 0)];
        let ran = run_ignoring(
            &seed,
            &local,
            &[],
            MAX_OPS,
            false,
            &[],
            Cut::None,
            &[],
            answer,
            true,
        )
        .unwrap();
        assert_eq!(ran.asked, 1, "{answer}");
        let live = (ran.space.nodes().into_iter())
            .filter(|node| node.status == 1 && node.node_id != ran.space.space_id())
            .count();
        let expected = if answer == Answer::Publish { 3 } else { 1 };
        assert_eq!(live, expected, "{answer}");
        assert_eq!(
            ran.removals,
            if answer == Answer::Discard { 2 } else { 0 },
            "{answer}"
        );
    }
}

const CUTS: [Cut; 5] = [
    Cut::None,
    Cut::Built,
    Cut::Applied,
    Cut::Answered,
    Cut::Effect,
];

fn act(kind: u8, target: usize, parent: usize, name: usize, text: u8) -> Action {
    Action {
        kind,
        target,
        parent,
        name,
        text,
    }
}

/// The folder `a` with the file `c.txt` in it.
fn a_folder_with_a_file() -> Vec<Action> {
    vec![act(0, 0, 0, 0, 0), act(1, 0, 1, 3, 0)]
}

/// The digests of a file's versions on the server, newest first.
fn version_digests(space: &SpaceDo, node_id: &str) -> Vec<Vec<u8>> {
    let digest = |version: VersionInfo| version.content.unwrap().manifest_digest;
    versions(space, node_id).into_iter().map(digest).collect()
}

/// The other device deletes a folder with a file in it, and the person restores the folder from
/// the trash on this device. Wherever the process stops on the way, the folder and the file come
/// back on the server and on disk as the nodes they were, and the file keeps its one version.
#[test]
fn a_deleted_folder_comes_back_with_the_same_node_ids() {
    let seed = a_folder_with_a_file();
    let (space, _) = seeded(&seed);
    let nodes = nodes_with(&space, 1, None);
    let folder = nodes_with(&space, 1, Some(1))[0].clone();
    let file = nodes_with(&space, 1, Some(2))[0].clone();
    let folder_at = nodes.iter().position(|id| *id == folder).unwrap();
    for cut in CUTS {
        let delete = [act(4, folder_at, 0, 0, 0)];
        let restore = (true, vec![act(5, folder_at, 0, 0, 0)]);
        let ran = run_cut(&seed, &[], &delete, MAX_OPS, false, &[restore], cut).unwrap();
        assert_eq!(ran.resolved.len(), 1, "{cut:?}");
        assert_eq!(ran.resolved[0].intent.node(), folder, "{cut:?}");
        assert_eq!(ran.resolved[0].outcome, Outcome::Restored, "{cut:?}");
        assert_eq!(nodes_with(&ran.space, 1, None), nodes, "{cut:?}");
        assert_eq!(nodes_with(&ran.space, 2, None), Vec::<String>::new());
        assert_eq!(version_digests(&ran.space, &file).len(), 1, "{cut:?}");
    }
}

/// The person edits a file and then restores an earlier version of it, before the edit was sent.
/// The edit goes first and becomes a version, the earlier version becomes current, and the file
/// keeps its node. Nothing is a conflict copy.
#[test]
fn an_edited_file_restored_to_an_earlier_version_keeps_the_edit_as_a_version() {
    let seed = [act(1, 0, 0, 3, 0)];
    let remote_edit = [act(2, 0, 0, 0, 1)];
    // Versions are listed newest first, so name 1 picks the first of the two.
    let edit_then_restore = (true, vec![act(2, 0, 0, 0, 2), act(6, 0, 0, 1, 0)]);
    for cut in CUTS {
        let during = [edit_then_restore.clone()];
        let ran = run_cut(&seed, &[], &remote_edit, MAX_OPS, false, &during, cut).unwrap();
        let files = nodes_with(&ran.space, 1, Some(2));
        assert_eq!(files.len(), 1, "{cut:?}");
        assert_eq!(ran.resolved[0].outcome, Outcome::Restored, "{cut:?}");
        let digest = |device, text| content(device, text).manifest_digest;
        assert_eq!(
            version_digests(&ran.space, &files[0]),
            [digest(0, 0), digest(1, 2), digest(2, 1), digest(0, 0)],
            "{cut:?}"
        );
    }
}

/// The response to a commit that carries a restore is lost. The device sends the same commit
/// again, and the server lists exactly one new version for the restore.
#[test]
fn a_restore_whose_response_was_lost_is_applied_once() {
    let seed = [act(1, 0, 0, 3, 0)];
    let remote_edit = [act(2, 0, 0, 0, 1)];
    let restore = (true, vec![act(6, 0, 0, 1, 0)]);
    let ran = run_cut(
        &seed,
        &[],
        &remote_edit,
        MAX_OPS,
        false,
        &[restore],
        Cut::Applied,
    )
    .unwrap();
    let file = &nodes_with(&ran.space, 1, Some(2))[0];
    assert_eq!(version_digests(&ran.space, file).len(), 3);
    assert_eq!(ran.resolved.len(), 1);
    assert_eq!(ran.resolved[0].outcome, Outcome::Restored);
}

/// A version that expired on the server, or was never there, cannot be restored. The request ends
/// as rejected, and a file created on this device meanwhile is uploaded in the same commit.
#[test]
fn a_restore_of_a_version_the_server_does_not_have_is_refused_and_sync_goes_on() {
    let seed = [act(1, 0, 0, 3, 0)];
    let create_and_restore = (true, vec![act(1, 0, 0, 0, 1), act(6, 0, 0, 0, 3)]);
    for cut in CUTS {
        let during = [create_and_restore.clone()];
        let ran = run_cut(&seed, &[], &[], MAX_OPS, false, &during, cut).unwrap();
        let rejected = Outcome::Rejected(Rejection::NotFound);
        assert_eq!(ran.resolved.len(), 1, "{cut:?}");
        assert_eq!(ran.resolved[0].outcome, rejected, "{cut:?}");
        assert_eq!(nodes_with(&ran.space, 1, Some(2)).len(), 2, "{cut:?}");
    }
}

/// The store is replaced by a copy from before a restore was sent, after the server applied it.
/// The server's count of this device's commits is then one ahead of the store, which no healthy
/// store shows. The space enters recovery and the restore, still queued in the old copy, is
/// neither sent again nor dropped.
#[test]
fn a_store_rolled_back_past_a_sent_restore_starts_recovery_and_sends_nothing() {
    let (mut space, mut other) = seeded(&a_folder_with_a_file());
    remote_action(&mut space, &mut other, 2, &act(4, 0, 0, 0, 0));
    let trashed = nodes_with(&space, 2, None);
    let mut trees = Trees::new(space.space_id().to_owned());
    for state in space.nodes().iter().skip(1) {
        let (id, node) = RemoteNode::from_state(state).unwrap();
        trees.set_remote(id, node);
    }
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    let intent = RestoreIntent::from_trash(trashed[0].clone(), None, None).unwrap();
    commits.restore(ids.uuid_v7(NOW as u64), intent.clone());
    let backup = commits.state();

    let (history, head) = (space.history_id().to_owned(), space.head_seq());
    let commit = commits.build(&mut trees, &mut ids, NOW as u64, &history, head);
    let me = session("me");
    space.commit(&me, NOW, &commit.cloned().unwrap()).unwrap();
    let head = space.head_seq();

    let mut commits = Commits::from_state(backup).unwrap();
    assert_eq!(commits.connected(&space.welcome(&me)), Reconnect::Recovery);
    for _ in 0..2 {
        assert_eq!(commits.halted(), Some(Halt::Recovery));
        assert_eq!(commits.restores().values().collect::<Vec<_>>(), [&intent]);
        let built = commits.build(&mut trees, &mut ids, NOW as u64, &history, head);
        assert_eq!(built, None);
        commits = Commits::from_state(commits.state()).unwrap();
    }
    assert_eq!(space.head_seq(), head);
}

/// A minimal interleaving the property found. The user moves and edits a file this device
/// created, and the other device deletes it. A commit that fits two ops carries the restore of
/// another file's version and the move, which revives the file with the content the server
/// already acknowledged (R8). The edit follows in the next commit. What the move revived is no
/// restored write: the edit replaces it, and it stays a version of the file.
#[test]
fn content_a_move_revives_next_to_a_restore_is_replaced_by_the_unsent_edit() {
    let seed = [act(1, 0, 0, 0, 0), act(0, 0, 0, 3, 0)];
    let local = [
        act(4, 652086880384956978, 0, 0, 0),
        act(1, 0, 9536159111846681508, 2, 0),
        act(0, 0, 0, 5, 0),
    ];
    let remote = [act(1, 0, 2753156350273366001, 0, 0)];
    let during = [
        (false, vec![act(5, 0, 0, 0, 0)]),
        (
            true,
            vec![
                act(0, 0, 7165637464455973765, 0, 0),
                act(3, 3958708204467096970, 3613361925314318621, 3, 0),
            ],
        ),
        (
            true,
            vec![
                act(2, 455309143188881385, 0, 0, 1),
                act(6, 3741528846974593209, 0, 0, 0),
            ],
        ),
        (
            false,
            vec![
                act(1, 0, 4063425664772464998, 0, 0),
                act(4, 2784706346093506431, 0, 0, 0),
                act(1, 0, 0, 0, 0),
            ],
        ),
        (true, vec![act(6, 206315280794903461, 0, 0, 0)]),
    ];
    let ran = run_cut(&seed, &local, &remote, 2, false, &during, Cut::None).unwrap();
    let mut files = ran.space.nodes().into_iter().filter(|n| n.kind == 2);
    let moved = files.find(|n| n.name == NAMES[3]);
    let digest = |text| content(1, text).manifest_digest;
    assert_eq!(
        version_digests(&ran.space, &moved.unwrap().node_id),
        [digest(1), digest(0)]
    );
}

/// A minimal interleaving the property found. This device creates the file b, and its S update
/// waits behind a delete and a move that are still unsent, so the file is left out of planning.
/// The user edits b and then asks for its first version. One op fits a commit. The restore
/// waits until the edit is sent, so the first version is current at the end and the edit is a
/// version under it.
#[test]
fn a_version_restore_follows_an_edit_of_a_file_left_out_of_planning() {
    let seed = [
        act(0, 0, 0, 0, 0),
        act(0, 0, 2960225115312676489, 0, 0),
        act(1, 0, 482761575999273747, 2, 0),
    ];
    let local = [
        act(4, 3338552224821716477, 0, 0, 0),
        act(3, 4169982272558885297, 686010811002822325, 0, 0),
        act(1, 0, 10528470701804065178, 2, 1),
    ];
    let during = [
        (false, vec![act(3, 2253923601960195439, 0, 3, 0)]),
        (true, vec![act(2, 6132494219199068644, 0, 0, 0)]),
        (true, vec![act(6, 7039772437741740600, 0, 0, 0)]),
    ];
    for cut in CUTS {
        let ran = run_cut(&seed, &local, &[], 1, false, &during, cut).unwrap();
        assert_eq!(ran.resolved.len(), 1, "{cut:?}");
        assert_eq!(ran.resolved[0].outcome, Outcome::Restored, "{cut:?}");
        let node = ran.resolved[0].intent.node();
        let digest = |text| content(1, text).manifest_digest;
        assert_eq!(
            version_digests(&ran.space, node),
            [digest(1), digest(0), digest(1)],
            "{cut:?}"
        );
    }
}

/// A minimal interleaving the property found. The other device deletes a folder, and the user
/// edits a file in it before the delete reaches the disk and then asks for the folder back from
/// the trash. One op fits a commit, so the restore brings the file back with its old content
/// first and the edit follows. The edit is current at the end, as an edit beats a delete, and
/// the content the restore brought back is the version under it.
#[test]
fn an_unsent_edit_of_a_file_in_a_restored_folder_stays_current() {
    let seed = [
        act(0, 0, 0, 0, 0),
        act(0, 0, 101854363273155071, 2, 0),
        act(0, 0, 3503316990192837406, 0, 0),
        act(1, 0, 2272586388003352129, 0, 0),
        act(1, 0, 9015892922196451787, 2, 0),
        act(1, 0, 110837118209456033, 0, 0),
        act(0, 0, 11223625915557445946, 0, 0),
        act(1, 0, 12002327795321761196, 0, 0),
        act(1, 0, 5987160327725455170, 0, 0),
        act(0, 0, 5314422076761796984, 0, 0),
    ];
    let local = [
        act(4, 989225936368150641, 0, 0, 0),
        act(0, 0, 0, 0, 0),
        act(4, 1897456816994156894, 0, 0, 0),
    ];
    let remote = [act(2, 11371124727216740042, 0, 0, 0)];
    let during = [
        (false, vec![act(4, 6116793111143655890, 0, 0, 0)]),
        (
            true,
            vec![
                act(2, 16490628292596182229, 0, 0, 0),
                act(5, 13447724379140298103, 0, 0, 0),
            ],
        ),
    ];
    for cut in CUTS {
        let ran = run_cut(&seed, &local, &remote, 1, true, &during, cut).unwrap();
        assert_eq!(ran.resolved.len(), 1, "{cut:?}");
        assert_eq!(ran.resolved[0].outcome, Outcome::Restored, "{cut:?}");
        let folder = ran.resolved[0].intent.node();
        let mut inside = ran
            .space
            .nodes()
            .into_iter()
            .filter(|n| n.parent_id == folder);
        let file = inside.find(|n| n.kind == 2).unwrap();
        let versions = version_digests(&ran.space, &file.node_id);
        let (edit, restored) = (content(1, 0), content(2, 0));
        assert_eq!(
            versions[..2],
            [edit.manifest_digest, restored.manifest_digest],
            "{cut:?}"
        );
    }
}

/// A minimal interleaving the property found. The user asks for an earlier version of a file
/// the other device renamed, and the disk gets the version under the new name while S still
/// waits for a parked folder to leave that name. Then the user asks for the newer version back.
/// Sent at once, that restore would leave L with the earlier version and S without it, and the
/// engine would upload the earlier version over the restored one as if the user had written
/// it. The restore waits for S, so the version asked for last is current at the end.
#[test]
fn a_second_version_restore_waits_until_s_has_the_first() {
    let seed = [
        act(0, 0, 0, 2, 0),
        act(0, 0, 10193068882185749263, 0, 0),
        act(0, 0, 15322717350651070711, 2, 0),
        act(0, 0, 7458645619147002727, 0, 0),
        act(1, 0, 5780529924190365195, 0, 0),
        act(1, 0, 1139158932485848356, 0, 0),
        act(0, 0, 16173846703536834397, 0, 0),
    ];
    let local = [
        act(4, 6149728382812320256, 0, 0, 0),
        act(1, 0, 0, 3, 0),
        act(0, 0, 4089352059523475543, 0, 0),
        act(0, 0, 0, 0, 0),
    ];
    let remote = [
        act(4, 10301859415978009313, 0, 0, 0),
        act(2, 0, 0, 0, 0),
        act(2, 0, 0, 0, 0),
    ];
    let during = [
        (false, vec![act(3, 0, 0, 0, 0)]),
        (true, vec![act(6, 0, 0, 2, 0)]),
        (false, vec![act(3, 0, 0, 2, 0)]),
        (true, vec![act(6, 0, 0, 1, 0)]),
    ];
    for cut in CUTS {
        let ran = run_cut(&seed, &local, &remote, 2, true, &during, cut).unwrap();
        let outcomes: Vec<_> = ran.resolved.iter().map(|r| &r.outcome).collect();
        assert_eq!(outcomes, [&Outcome::Restored; 2], "{cut:?}");
        let last = &ran.resolved[1].intent;
        let versions = versions(&ran.space, last.node());
        let asked = versions
            .iter()
            .find(|v| Some(v.version_id.as_str()) == last.version_id());
        assert_eq!(versions[0].content, asked.unwrap().content, "{cut:?}");
    }
}

/// The other device creates a and b before this one does, so the server renames this device's
/// files and both are reverted. Right after that commit, the other device moves its own files away
/// and gives each renamed file the other's local name, so the two reverted files block each other.
#[test]
fn two_reverted_files_that_swap_names_converge() {
    let act = |kind, target, name, text| Action {
        kind,
        target,
        parent: 0,
        name,
        text,
    };
    let local = [act(1, 0, 0, 0), act(1, 0, 2, 1)];
    let remote = [act(1, 0, 0, 2), act(1, 0, 2, 3)];
    let swap = vec![
        act(3, 3, 3, 0),
        act(3, 0, 5, 0),
        act(3, 1, 2, 0),
        act(3, 2, 0, 0),
    ];
    let parked = run(&[], &local, &remote, MAX_OPS, false, &[(false, swap)]).unwrap();
    assert!(parked > 0, "no reverted file was parked");
}

/// A minimal interleaving the property found: a folder created here is merged into a folder this
/// device holds elsewhere, and a node the user moved into the new folder, which the server already
/// has in the merge target, must still follow it on disk.
#[test]
fn a_node_moved_into_a_merged_folder_follows_it_on_disk() {
    let seed = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 2,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 6967822464045520996,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 3,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 9526621794622326438,
            name: 1,
            text: 0,
        },
    ];
    let local = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 10553703446551236695,
            name: 5,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 16106826873077682970,
            name: 2,
            text: 0,
        },
        Action {
            kind: 3,
            target: 880737998146561905,
            parent: 5552030480946143737,
            name: 1,
            text: 0,
        },
    ];
    let remote = vec![];
    let fits = 1;
    let fresh_dir_names = false;
    let during = vec![
        (
            false,
            vec![Action {
                kind: 3,
                target: 443641311708994000,
                parent: 3064515660927608345,
                name: 5,
                text: 0,
            }],
        ),
        (
            false,
            vec![Action {
                kind: 0,
                target: 0,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

/// A minimal interleaving the property found: a file the server renamed is edited and then deleted
/// by the other device while its old name blocks another renamed file, so the edit must be
/// uploaded as a new file.
#[test]
fn an_edited_renamed_file_deleted_remotely_is_uploaded_as_a_new_file() {
    let seed = vec![Action {
        kind: 1,
        target: 0,
        parent: 0,
        name: 0,
        text: 0,
    }];
    let local = vec![
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 0,
            parent: 0,
            name: 3,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 2278650491807642941,
            parent: 9127562380887983576,
            name: 3,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 10772034385298179478,
            name: 3,
            text: 0,
        },
    ];
    let fits = 1000;
    let fresh_dir_names = false;
    let during = vec![
        (
            false,
            vec![
                Action {
                    kind: 4,
                    target: 12531602759342590661,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 3,
                    target: 283388978725368259,
                    parent: 0,
                    name: 4,
                    text: 0,
                },
                Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 1,
                },
            ],
        ),
        (
            false,
            vec![Action {
                kind: 2,
                target: 2943695200610841979,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
        (
            true,
            vec![Action {
                kind: 1,
                target: 0,
                parent: 0,
                name: 3,
                text: 0,
            }],
        ),
        (
            false,
            vec![
                Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 4,
                    target: 6986862736185568223,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

/// A minimal interleaving the property found: the user moves a folder into a folder created here,
/// which the server merges into one this device holds elsewhere, while the other device moves it
/// away, so on disk it must follow the other device's move.
#[test]
fn a_node_moved_into_a_merged_folder_follows_a_remote_move() {
    let seed = vec![Action {
        kind: 1,
        target: 0,
        parent: 0,
        name: 3,
        text: 0,
    }];
    let local = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 2,
            text: 0,
        },
        Action {
            kind: 4,
            target: 3390051005637832722,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 3573038702653260728,
            name: 5,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 3,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 4,
            target: 761663922910750675,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 5,
            text: 0,
        },
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 3824710055192453609,
            name: 0,
            text: 0,
        },
    ];
    let fits = 1;
    let fresh_dir_names = false;
    let during = vec![
        (
            true,
            vec![
                Action {
                    kind: 0,
                    target: 0,
                    parent: 7106800540313915127,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 3,
                    target: 69233815001145249,
                    parent: 11870616208155072313,
                    name: 5,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![
                Action {
                    kind: 3,
                    target: 4672328705971433533,
                    parent: 9740151841705065456,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 4,
                    target: 1845576325211218462,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ],
        ),
        (
            true,
            vec![
                Action {
                    kind: 4,
                    target: 10069492413079588418,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 3,
                    target: 17076322275721598278,
                    parent: 13906480724080066383,
                    name: 5,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![Action {
                kind: 3,
                target: 10335766898245895506,
                parent: 4894905971208633107,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

/// A minimal interleaving the property found: the other device deletes a folder holding a parked
/// node with a folder the user created under it, so the effects clear the folders above it, and
/// the create then brings them back on the server. S cannot drop them while the parked node hangs
/// from them, so they must be brought back on disk.
#[test]
fn folders_revived_above_a_parked_node_are_brought_back() {
    let seed = vec![
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 5801581838394722479,
            name: 5,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 6476457337238582140,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 4564459193690538441,
            name: 4,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 6344555226843187279,
            name: 5,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 1911679536950536619,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 10497572882504026615,
            name: 0,
            text: 0,
        },
    ];
    let local = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 7649716111743945145,
            name: 0,
            text: 0,
        },
        Action {
            kind: 2,
            target: 11369682697835578077,
            parent: 0,
            name: 0,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 3,
            target: 9257115507352902041,
            parent: 14556351700413880842,
            name: 3,
            text: 0,
        },
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 14758948430815737821,
            parent: 277029637290519871,
            name: 0,
            text: 0,
        },
    ];
    let fits = 1000;
    let fresh_dir_names = false;
    let during = vec![(
        false,
        vec![
            Action {
                kind: 0,
                target: 0,
                parent: 101087396655831711,
                name: 0,
                text: 0,
            },
            Action {
                kind: 4,
                target: 7391292373329721386,
                parent: 0,
                name: 0,
                text: 0,
            },
        ],
    )];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

#[test]
fn an_edited_reverted_file_parked_under_a_folder_deleted_remotely_is_kept() {
    let seed = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
    ];
    let local = vec![
        Action {
            kind: 2,
            target: 620503160258612587,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 6503485206416241607,
            parent: 3276521438384508601,
            name: 5,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 4,
            target: 6817887910542140471,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 2,
            text: 0,
        },
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
    ];
    let fits = 1;
    let fresh_dir_names = false;
    let during = vec![
        (
            false,
            vec![Action {
                kind: 0,
                target: 0,
                parent: 1247370204288523498,
                name: 5,
                text: 0,
            }],
        ),
        (
            false,
            vec![Action {
                kind: 3,
                target: 47517915818211563,
                parent: 1344201196089021663,
                name: 0,
                text: 0,
            }],
        ),
        (
            false,
            vec![Action {
                kind: 4,
                target: 12752295931867524494,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

/// A folder created here is merged into one the other device created, and its S update waits
/// because S still has a parked folder at that name. The user then renames it onto the name the
/// parked folder needs, so it blocks that folder and is parked too (GUN-285).
#[test]
fn a_folder_renamed_onto_the_name_a_parked_folder_needs_is_parked_too() {
    let seed = vec![Action {
        kind: 0,
        target: 0,
        parent: 0,
        name: 5,
        text: 0,
    }];
    let local = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 4,
            target: 1370376148349361013,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 1015448658534533405,
            name: 0,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 3,
            target: 0,
            parent: 4747322177713459370,
            name: 0,
            text: 0,
        },
        Action {
            kind: 4,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 5,
            text: 0,
        },
    ];
    let fits = 1000;
    let fresh_dir_names = false;
    let during = vec![
        (
            true,
            vec![Action {
                kind: 0,
                target: 0,
                parent: 0,
                name: 5,
                text: 0,
            }],
        ),
        (
            true,
            vec![Action {
                kind: 3,
                target: 0,
                parent: 5182897700414857884,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

/// A file whose S update waits blocks a remote create, and the server then deletes it. It holds
/// no edit against its waiting shape, so it is removed rather than uploaded again.
#[test]
fn a_parked_file_whose_server_update_waits_is_removed_when_the_server_deletes_it() {
    let seed = vec![
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 3,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 971099645012382092,
            name: 2,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
    ];
    let local = vec![Action {
        kind: 4,
        target: 1956793324523920083,
        parent: 0,
        name: 0,
        text: 0,
    }];
    let remote = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 2832855709947514548,
            name: 3,
            text: 0,
        },
        Action {
            kind: 4,
            target: 5522577497956667845,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 438823430391821631,
            parent: 5221277148646417130,
            name: 0,
            text: 0,
        },
    ];
    let fits = 1000;
    let fresh_dir_names = false;
    let during = vec![
        (
            false,
            vec![
                Action {
                    kind: 3,
                    target: 5190931714523467330,
                    parent: 9928977605353883034,
                    name: 3,
                    text: 0,
                },
                Action {
                    kind: 2,
                    target: 5762732690084641497,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![
                Action {
                    kind: 3,
                    target: 2270514295357040306,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 0,
                    target: 0,
                    parent: 7086831897136368260,
                    name: 3,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![Action {
                kind: 4,
                target: 14243556372242850438,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

/// A created file's S update waits, and settling then gives S the other device's content. The user
/// then writes the content the file was created with again; the stale update must not make that
/// write look uploaded.
#[test]
fn a_waiting_update_that_settling_replaced_does_not_hide_a_later_edit() {
    let seed = vec![
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 3,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 5154893513058372197,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 3121141568318315682,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 15085739982097279210,
            name: 5,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 12042687951629207403,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 805902510814196342,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 752737086628574172,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 2061236886198686334,
            name: 2,
            text: 0,
        },
    ];
    let local = vec![
        Action {
            kind: 3,
            target: 11231637698525412811,
            parent: 4780766426531151849,
            name: 0,
            text: 0,
        },
        Action {
            kind: 4,
            target: 11675596971973797087,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 7216849391709144393,
            name: 0,
            text: 0,
        },
        Action {
            kind: 4,
            target: 13401659892108216031,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 6236013527037087150,
            name: 3,
            text: 1,
        },
    ];
    let remote = vec![
        Action {
            kind: 2,
            target: 7088491083489211429,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 17401249810093626761,
            parent: 231179896921208167,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 10774319552713167365,
            parent: 2638094422400810531,
            name: 2,
            text: 0,
        },
    ];
    let fits = 1000;
    let fresh_dir_names = false;
    let during = vec![
        (
            false,
            vec![
                Action {
                    kind: 4,
                    target: 158282335639317271,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 4,
                    target: 1121437384512356461,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![Action {
                kind: 2,
                target: 3139751682532370971,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
        (
            true,
            vec![
                Action {
                    kind: 4,
                    target: 6209302477719205039,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 2,
                    target: 4326100334342803349,
                    parent: 0,
                    name: 0,
                    text: 1,
                },
            ],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

/// A move's S update waits, and the other device moves the file back before it is made. Once L
/// follows, the update is stale, so the user's edit to the file is uploaded.
#[test]
fn a_waiting_update_both_sides_moved_past_does_not_hold_back_an_edit() {
    let seed = vec![
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 5,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 978237978856208785,
            name: 2,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 4194549617926577008,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 8070810026119318711,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 17775129536871628555,
            name: 2,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 3309602263058540387,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 1280557214040556292,
            name: 0,
            text: 0,
        },
    ];
    let local = vec![
        Action {
            kind: 4,
            target: 10356890765720543216,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 1115092823749830263,
            name: 3,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 9079959440039050417,
            name: 2,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 4,
            target: 15907938580905265140,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 4,
            target: 4229855130926676766,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 6810740145664341458,
            parent: 2110593086026663230,
            name: 5,
            text: 0,
        },
    ];
    let fits = 2;
    let fresh_dir_names = false;
    let during = vec![
        (
            true,
            vec![Action {
                kind: 1,
                target: 0,
                parent: 12430656056815562337,
                name: 0,
                text: 0,
            }],
        ),
        (
            false,
            vec![Action {
                kind: 3,
                target: 291780538349244884,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
        (
            false,
            vec![Action {
                kind: 3,
                target: 1207142123533410396,
                parent: 14415471485496758628,
                name: 0,
                text: 0,
            }],
        ),
        (
            true,
            vec![Action {
                kind: 2,
                target: 4350527627605106883,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

/// A created file whose S update waits is deleted on both sides before the update fits, so the
/// update is dropped instead of holding the device out of convergence.
#[test]
fn a_waiting_update_of_a_file_both_sides_deleted_is_dropped() {
    let seed = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 5575517051411596159,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 9542798344174571143,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 1134570969992643019,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 5217341420012805030,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 5,
            text: 0,
        },
    ];
    let local = vec![
        Action {
            kind: 4,
            target: 14966158802491056936,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 2,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 3,
            target: 3639306566313460506,
            parent: 17827897833710994996,
            name: 2,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 8247849472439318298,
            name: 2,
            text: 0,
        },
    ];
    let fits = 1000;
    let fresh_dir_names = false;
    let during = vec![
        (
            false,
            vec![
                Action {
                    kind: 4,
                    target: 2494586888620959721,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 1,
                    target: 0,
                    parent: 14654176779908604874,
                    name: 0,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![Action {
                kind: 4,
                target: 7730499601774269038,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
        (
            false,
            vec![Action {
                kind: 0,
                target: 0,
                parent: 339548061254993100,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

/// The server merges the user's new folder ß into the other device's ß, whose S update waits
/// because S still holds a parked file at ss, the same name after case folding. The other device
/// has moved that file into the folder, and the user deletes the folder. The folder is brought
/// back for the file instead of its update holding both out of convergence.
#[test]
fn a_waiting_folder_the_user_deleted_is_brought_back_for_a_file_moved_into_it() {
    let seed = vec![
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 5,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 2,
            text: 0,
        },
    ];
    let local = vec![
        Action {
            kind: 2,
            target: 11079414779518565354,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 2,
            target: 5832766238116930057,
            parent: 0,
            name: 0,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 3,
            target: 351730081019129726,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 5,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 2,
            target: 554097051787814230,
            parent: 0,
            name: 0,
            text: 0,
        },
    ];
    let fits = 1000;
    let fresh_dir_names = false;
    let during = vec![
        (
            false,
            vec![
                Action {
                    kind: 2,
                    target: 3032609202697743113,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 3,
                    target: 2731698343566406169,
                    parent: 304348770870888498,
                    name: 2,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![
                Action {
                    kind: 4,
                    target: 3172150523246826267,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 3,
                    target: 9725551978640732566,
                    parent: 1209946197345948991,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 3,
                    target: 936650056549699008,
                    parent: 1061052580595229545,
                    name: 3,
                    text: 0,
                },
            ],
        ),
        (
            true,
            vec![
                Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 5,
                    text: 0,
                },
                Action {
                    kind: 0,
                    target: 0,
                    parent: 65492476661922611,
                    name: 3,
                    text: 0,
                },
            ],
        ),
        (
            true,
            vec![Action {
                kind: 4,
                target: 600126837695760128,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
        (
            false,
            vec![
                Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

/// The user moves folder A into folder B, while the other device moves B into A and then trashes
/// the folder both are in. The effects remove A's old parent C, whose removal from S waits because
/// S still holds A there. A folder the user creates in A makes the server revive A, C and the folder
/// above them (R8), and A's move would close a cycle, so A is reverted. C is created again for A to
/// go back into, instead of C, A and B each waiting for another.
#[test]
fn a_removed_folder_the_server_revives_is_created_again_while_its_removal_waits() {
    let seed = [
        act(0, 0, 0, 0, 0),
        act(1, 0, 0, 0, 0),
        act(1, 0, 0, 0, 0),
        act(1, 0, 0, 0, 0),
        act(0, 0, 980740372666386433, 2, 0),
        act(1, 0, 0, 0, 0),
        act(0, 0, 4216774371197345119, 0, 0),
        act(1, 0, 2882081015209338574, 3, 0),
        act(1, 0, 0, 0, 0),
        act(0, 0, 15439864200780996302, 0, 0),
        act(0, 0, 9289856371756041619, 0, 0),
    ];
    let local = [
        act(0, 0, 1052871606481788707, 0, 0),
        act(3, 8922599260003921418, 12502836504135289280, 0, 0),
    ];
    let remote = [
        act(3, 1802799855454195272, 10126529284027281386, 0, 0),
        act(4, 8617856007053871336, 0, 0, 0),
    ];
    let during = [(true, vec![act(0, 0, 14856899963116643210, 0, 0)])];
    run(&seed, &local, &remote, 1000, true, &during).unwrap();
}
#[test]
fn a_parked_file_forked_from_a_node_the_server_deleted_is_created_after_its_commit_is_split() {
    let seed = [act(1, 0, 0, 3, 0)];
    let local = [act(1, 0, 0, 3, 1)];
    let during = vec![
        (false, vec![act(2, 0, 0, 0, 0)]),
        (
            false,
            vec![
                act(0, 0, 0, 0, 0),
                act(3, 7779810202192050055, 202987497655873243, 5, 0),
            ],
        ),
        (
            true,
            vec![
                act(0, 0, 5081664043081684955, 5, 0),
                act(2, 6321014178578916138, 0, 0, 0),
            ],
        ),
        (false, vec![act(4, 4791664463415640549, 0, 0, 0)]),
    ];
    run_ignoring(
        &seed,
        &local,
        &[],
        1,
        false,
        &during,
        Cut::None,
        &[],
        Answer::Publish,
        true,
    )
    .unwrap();
}

#[test]
fn a_folder_the_user_deleted_comes_back_for_a_waiting_file_the_server_moves_into_it() {
    let seed = [act(0, 0, 0, 2, 0), act(0, 0, 6274166174826663049, 0, 0)];
    let local = [act(0, 0, 4991715864022573885, 0, 0)];
    let remote = [
        act(1, 0, 8366538510828719079, 0, 0),
        act(2, 0, 0, 0, 0),
        act(0, 0, 544864474965852954, 0, 0),
    ];
    let during = vec![
        (
            true,
            vec![act(6, 0, 0, 0, 0), act(0, 0, 7211273767686463164, 0, 0)],
        ),
        (
            false,
            vec![
                act(4, 16841059892813868500, 0, 0, 0),
                act(1, 0, 343720692834072452, 2, 0),
            ],
        ),
        (true, vec![act(4, 16794273080645545114, 0, 0, 0)]),
        (
            false,
            vec![
                act(0, 0, 10316403760466809166, 0, 0),
                act(3, 16377241187031633676, 6363070014188718880, 0, 0),
            ],
        ),
    ];
    run(&seed, &local, &remote, 1, true, &during).unwrap();
}

#[test]
fn a_file_the_user_deleted_while_its_server_move_waited_in_s_is_deleted_on_the_server() {
    let seed = vec![
        Action {
            kind: 0,
            target: 4614810417318265143,
            parent: 4864179259767229626,
            name: 1,
            text: 2,
        },
        Action {
            kind: 0,
            target: 12119355532117708331,
            parent: 14757475804517505032,
            name: 5,
            text: 2,
        },
        Action {
            kind: 1,
            target: 6625108946950071779,
            parent: 9918011999374063130,
            name: 3,
            text: 2,
        },
        Action {
            kind: 0,
            target: 12576383093733275581,
            parent: 4466178185947277735,
            name: 3,
            text: 2,
        },
    ];
    let local = vec![
        Action {
            kind: 2,
            target: 5819694219100739927,
            parent: 2036847718054668352,
            name: 6,
            text: 3,
        },
        Action {
            kind: 5,
            target: 2270324102383091549,
            parent: 7093835583652112558,
            name: 1,
            text: 2,
        },
        Action {
            kind: 1,
            target: 13656694315896170903,
            parent: 15331058797787251121,
            name: 1,
            text: 3,
        },
        Action {
            kind: 2,
            target: 1710332307205056776,
            parent: 7320500736078956321,
            name: 1,
            text: 2,
        },
        Action {
            kind: 0,
            target: 14507846936884681424,
            parent: 12527369109280507380,
            name: 4,
            text: 2,
        },
        Action {
            kind: 1,
            target: 8650148038972097623,
            parent: 6435848661095480790,
            name: 4,
            text: 2,
        },
    ];
    let remote = vec![
        Action {
            kind: 6,
            target: 807225587938069940,
            parent: 17863857293020369369,
            name: 5,
            text: 3,
        },
        Action {
            kind: 4,
            target: 13730757541944084961,
            parent: 11742315858831314384,
            name: 1,
            text: 1,
        },
        Action {
            kind: 0,
            target: 14107778014734974581,
            parent: 14200207690007878352,
            name: 4,
            text: 1,
        },
        Action {
            kind: 3,
            target: 5566379684988951548,
            parent: 4569525365923634314,
            name: 3,
            text: 3,
        },
    ];
    let fits = MAX_OPS;
    let fresh_dir_names = true;
    let during = vec![
        (
            false,
            vec![
                Action {
                    kind: 3,
                    target: 17702201595369440910,
                    parent: 11821905956357519784,
                    name: 0,
                    text: 2,
                },
                Action {
                    kind: 4,
                    target: 2762722298416582515,
                    parent: 8244077946071889904,
                    name: 3,
                    text: 3,
                },
            ],
        ),
        (
            true,
            vec![
                Action {
                    kind: 0,
                    target: 13793337169377006733,
                    parent: 3592739378196482913,
                    name: 5,
                    text: 3,
                },
                Action {
                    kind: 4,
                    target: 7353629600874959317,
                    parent: 3775275498239273878,
                    name: 6,
                    text: 0,
                },
                Action {
                    kind: 1,
                    target: 1401826479567066740,
                    parent: 10805880149766239377,
                    name: 6,
                    text: 0,
                },
            ],
        ),
        (
            true,
            vec![
                Action {
                    kind: 5,
                    target: 3508424595374466801,
                    parent: 13652394482325166857,
                    name: 4,
                    text: 1,
                },
                Action {
                    kind: 0,
                    target: 12196094243527756545,
                    parent: 6472941235758237528,
                    name: 1,
                    text: 1,
                },
            ],
        ),
        (
            false,
            vec![
                Action {
                    kind: 4,
                    target: 5237472451013669662,
                    parent: 3865121539495492564,
                    name: 3,
                    text: 2,
                },
                Action {
                    kind: 0,
                    target: 4275436719165289791,
                    parent: 10695791634680114475,
                    name: 1,
                    text: 2,
                },
                Action {
                    kind: 4,
                    target: 113406059743004084,
                    parent: 11482615632212085002,
                    name: 1,
                    text: 0,
                },
            ],
        ),
    ];
    let cut = Cut::Answered;
    run_cut(&seed, &local, &remote, fits, fresh_dir_names, &during, cut).unwrap();
}

#[test]
fn an_edit_the_other_device_deleted_gives_way_to_the_folder_it_made_at_that_name() {
    let seed = [
        act(1, 0, 0, 2, 0),
        act(0, 0, 0, 3, 0),
        act(0, 0, 95372568677471977, 0, 0),
    ];
    let local = [
        act(0, 0, 10639126546080908428, 0, 0),
        act(0, 0, 2083088007821781, 0, 0),
        act(5, 0, 0, 0, 3),
    ];
    let during = [
        (
            false,
            vec![act(4, 4134113597317913042, 0, 0, 0), act(3, 0, 0, 3, 0)],
        ),
        (true, vec![act(2, 0, 0, 0, 0)]),
        (false, vec![act(4, 0, 0, 0, 0), act(0, 0, 0, 3, 0)]),
    ];
    run_cut(&seed, &local, &[], 1, false, &during, Cut::None).unwrap();
}
