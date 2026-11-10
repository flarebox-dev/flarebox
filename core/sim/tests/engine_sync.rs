// SPDX-License-Identifier: Apache-2.0

//! The engine's commits and effects against the reference server (docs/design/sync-engine.md
//! section 7.2, planner property). A device with offline local changes meets another device's
//! concurrent commits, then plans until nothing is left. It must end with L = S = R equal to the
//! server's tree, and every write either device made must still be on the server.

use std::collections::BTreeSet;

use flarebox_engine::classify::Trees;
use flarebox_engine::commit::{Commits, Failure, MAX_OPS};
use flarebox_engine::effects::Effects;
use flarebox_engine::ids::IdGen;
use flarebox_engine::names::name_key;
use flarebox_engine::state::StateError;
use flarebox_engine::tree::{FileIdentity, Kind, LocalNode, Names, RemoteNode, same};
use flarebox_proto::v1::{
    ChangeRecord, ChunkRef, Commit, Content, CreateDir, CreateFile, Delete, Error, Move, Op,
    PrepareUpload, Resume, UpdateContent, UploadReceipt, change_record, chunk_plan, error, op,
};
use flarebox_sim::space::{Role, Session, SpaceDo};
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

/// One commit of one op by another device that has seen everything, on its live nodes.
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
        _ => match target(&all) {
            Some(node_id) => op::Op::Delete(Delete { node_id }),
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

fn identity(ino: u64) -> FileIdentity {
    FileIdentity {
        dev: 1,
        ino,
        birth_ns: 0,
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

struct Device {
    trees: Trees,
    ids: IdGen,
    inos: u64,
    /// Whether the folders the user creates take names nothing else has, so none is merged.
    fresh_dir_names: bool,
}

impl Device {
    fn identity(&mut self) -> FileIdentity {
        self.inos += 1;
        identity(self.inos)
    }

    /// One change the user makes on disk, as the next rescan reports it. The user sees no parked
    /// node and nothing under one.
    fn act(&mut self, action: &Action) {
        let root = self.trees.local().root.clone();
        let local = self.trees.local();
        let visible = |id: &str| {
            let mut current = id;
            for _ in 0..=local.len() {
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
            nodes.iter().any(|(other, n)| {
                other != id && n.shape.parent == parent && name_key(&n.shape.name) == name_key(name)
            })
        };
        let target = (!nodes.is_empty()).then(|| nodes[action.target % nodes.len()].clone());
        match (action.kind, target) {
            (0 | 1, _) if !taken("") => {
                let (kind, content) = match action.kind {
                    0 => (Kind::Dir, None),
                    _ => (Kind::File, Some(content(1, action.text))),
                };
                let shape = flarebox_engine::tree::Shape {
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
                node.shape.content = Some(content(1, action.text));
                self.trees.set_local(id, node);
            }
            (3, Some((id, mut node))) if !taken(&id) && !self.holds(&id, &parent) => {
                node.shape.parent = parent;
                node.shape.name = name.into();
                self.trees.set_local(id, node);
            }
            (4, Some((id, _))) => {
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
    let synced = |id: &str| {
        let shape = match trees.waiting().get(id) {
            Some(waiting) => waiting.as_ref(),
            None => trees.synced().live(id),
        }?;
        Some(shape.content.as_ref()?.manifest_digest.clone())
    };
    after
        .difference(before)
        .filter(|(id, digest)| synced(id).as_ref() != Some(digest))
        .map(|(_, digest)| digest.clone())
        .collect()
}

/// Rebuilds the engine state the way the store does after a restart, and checks that each part
/// comes back equal.
fn reload(
    trees: &Trees,
    commits: &Commits,
    effects: &Effects,
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
    let restored_commits = Commits::from_state(commits.state()).map_err(refused)?;
    let restored_effects = Effects::from_state(effects.state()).map_err(refused)?;
    prop_assert_eq!(&loaded, trees);
    prop_assert_eq!(&restored_commits, commits);
    prop_assert_eq!(&restored_effects, effects);
    Ok((loaded, restored_commits, restored_effects))
}

/// `fits` is the most ops a commit may hold before the server refuses it with `LIMIT_EXCEEDED`,
/// standing in for the 1 MiB message limit (spec v0 section 7.2).
fn run(
    seed: &[Action],
    local: &[Action],
    remote: &[Action],
    fits: usize,
    fresh_dir_names: bool,
    during: &[(bool, Vec<Action>)],
) -> Result<usize, TestCaseError> {
    let mut space = SpaceDo::new(5, NOW);
    let mut other = IdGen::new(6);
    for action in seed {
        remote_action(&mut space, &mut other, 0, action);
    }
    let root = space.space_id().to_owned();
    let history = space.history_id().to_owned();
    let mut device = Device {
        trees: Trees::new(root.clone()),
        ids: IdGen::new(7),
        inos: 0,
        fresh_dir_names,
    };
    for state in space.nodes().iter().filter(|n| n.node_id != root) {
        let (id, node) = RemoteNode::from_state(state).unwrap();
        if !node.trashed {
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

    for action in local {
        device.act(action);
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
        })
        .unwrap();
    for record in backlog {
        prop_assert_eq!(record.seq, last_seq + 1);
        apply(&mut device.trees, &record.changes);
        last_seq = record.seq;
    }

    let me = session("me");
    let (mut effects, mut commits) = (Effects::default(), Commits::default());
    let (mut steps, mut quiet, mut due, mut parked_reverted) = (0, false, false, 0);
    let mut during = during.iter();
    while !quiet {
        steps += 1;
        prop_assert!(steps <= STEPS, "no convergence within {} steps", STEPS);
        device.trees.settle();
        if (due || steps % 2 == 0)
            && let Some((on_device, actions)) = during.next()
        {
            due = false;
            if *on_device {
                let before = held(&device.trees);
                for action in actions {
                    device.act(action);
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
        (device.trees, commits, effects) = reload(&device.trees, &commits, &effects)?;
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
        let Some(commit) = built else {
            quiet = device.trees == before && during.len() == 0;
            continue;
        };
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
        let receipts = upload(&mut space, &me, &commit.ops);
        let committed = space.commit(&me, NOW, &Commit { receipts, ..commit });
        let committed = committed.map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        prop_assert_eq!(committed.result.seq, last_seq + 1);
        apply(&mut device.trees, &committed.result.changes);
        last_seq = committed.result.seq;
        commits.received(committed.result).unwrap();
        prop_assert!(commits.acknowledge(&mut device.trees, last_seq));
        due = true;
    }

    prop_assert_eq!(device.trees.settle(), vec![]);
    prop_assert!(commits.problems().is_empty(), "{:?}", commits.problems());
    prop_assert!(effects.blocked().is_empty(), "{:?}", effects.blocked());
    prop_assert!(device.trees.local().iter().all(|(_, n)| !n.parked));
    let server: Vec<(String, Option<flarebox_engine::tree::Shape>)> = space
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
    let kept = written(
        server
            .iter()
            .map(|(_, s)| s.clone().and_then(|s| s.content)),
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
    Ok(parked_reverted)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|cases| cases.parse().ok()).unwrap_or(1024),
    ))]

    #[test]
    fn a_device_with_local_changes_converges_with_the_server_and_keeps_every_write(
        seed in prop::collection::vec(action(2), 0..12),
        local in prop::collection::vec(action(5), 0..8),
        remote in prop::collection::vec(action(5), 0..8),
        fits in prop_oneof![Just(MAX_OPS), 1usize..4],
        fresh_dir_names in any::<bool>(),
        during in prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(5), 1..4)),
            0..6,
        ),
    ) {
        run(&seed, &local, &remote, fits, fresh_dir_names, &during)?;
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
