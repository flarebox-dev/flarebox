// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use flarebox_engine::commit::{Commits, Halt, InFlight, Reconnect};
use flarebox_engine::effects::{Base, Dir, Displaced, Effect, EffectRecord, Location};
use flarebox_engine::ids::uuid_v7;
use flarebox_engine::restore::{Outcome, Rejection, Resolved, RestoreIntent};
use flarebox_engine::state::{CommitsState, EffectsState, TreesState};
use flarebox_engine::tree::{FileIdentity, Kind, Shape as EngineShape};
use flarebox_fs::FileId;
use flarebox_proto::v1::{
    ChunkRef, Commit, CommitResult, Content, CreateFile, Delete, NodeKind, NodeState, NodeStatus,
    Op, OpResult, VersionInfo, Welcome, op, op_result,
};
use flarebox_store::{
    CachedChunk, Cursor, Durability, EngineState, Error, GivenAnswer, Link, LocalNode,
    SCHEMA_VERSION, Shape, Store, StoredQuestion, SyncedNode,
};
use rusqlite::Connection;

fn content(size: u64) -> Content {
    Content {
        size,
        mtime_ns: 1_700_000_000_000_000_000,
        executable: true,
        chunks: vec![ChunkRef {
            hash: vec![7; 32],
            size: size as u32,
            encoding: 0,
        }],
        chunk_count: 1,
        manifest_digest: vec![9; 32],
        chunker: "fastcdc2020-1m-4m-16m".into(),
    }
}

fn file_shape(parent_id: &str, name: &str) -> Shape {
    Shape {
        parent_id: parent_id.into(),
        name: name.into(),
        kind: NodeKind::File as i32,
        content: Some(content(3)),
    }
}

fn dir_shape(parent_id: &str, name: &str) -> Shape {
    Shape {
        parent_id: parent_id.into(),
        name: name.into(),
        kind: NodeKind::Dir as i32,
        content: None,
    }
}

#[test]
fn migrations_apply_from_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    drop(store);

    let conn = Connection::open(&path).unwrap();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        tables,
        [
            "chunk_cache",
            "commit_in_flight",
            "commit_in_flight_delete_members",
            "commit_in_flight_deletes",
            "commit_slots",
            "commit_state",
            "effect_state",
            "engine_state",
            "failed_effects",
            "link",
            "local_nodes",
            "node_maps",
            "node_sets",
            "questions",
            "remote_cursor",
            "remote_nodes",
            "restore_intents",
            "restore_requests",
            "synced_nodes",
            "trashed_here",
            "waiting_updates",
            "watched_contents",
        ]
    );
    let journal: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(journal, "wal");
}

#[test]
fn connection_uses_durable_commits() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("state.db")).unwrap();
    let durability = store.durability().unwrap();
    assert_eq!(durability, Durability::required());
    assert_eq!(durability.journal_mode, "wal");
    assert_eq!(durability.synchronous, 2);
    assert_eq!(durability.fullfsync, cfg!(target_os = "macos"));
}

#[test]
fn reopening_keeps_data_and_schema_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let cursor = Cursor {
        history_id: "h1".into(),
        last_seq: 7,
        ahead_of_server: true,
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_cursor(&cursor))
        .unwrap();

    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(store.transaction(|tx| tx.cursor()).unwrap(), Some(cursor));
}

#[test]
fn newer_schema_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    drop(Store::open(&path).unwrap());
    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();

    match Store::open(&path) {
        Err(Error::SchemaTooNew { found, supported }) => {
            assert_eq!((found, supported), (SCHEMA_VERSION + 1, SCHEMA_VERSION));
        }
        other => panic!("expected SchemaTooNew, got {:?}", other.err()),
    }
}

#[test]
fn three_trees_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let remote_root = NodeState {
        node_id: "root".into(),
        kind: NodeKind::Dir as i32,
        status: NodeStatus::Live as i32,
        content_rev: 1,
        meta_rev: 1,
        ..Default::default()
    };
    let remote_file = NodeState {
        node_id: "f".into(),
        parent_id: "root".into(),
        name: "b.txt".into(),
        kind: NodeKind::File as i32,
        status: NodeStatus::Trashed as i32,
        content_rev: 4,
        meta_rev: 3,
        trash_group: "4:0".into(),
        current: Some(VersionInfo {
            version_id: "v1".into(),
            seq: 4,
            content: Some(content(3)),
            device_id: "d1".into(),
            user_id: "u1".into(),
            created_at_ms: 1_700_000_000_000,
        }),
    };
    let synced = [
        SyncedNode {
            node_id: "f".into(),
            shape: file_shape("root", "b.txt"),
        },
        SyncedNode {
            node_id: "root".into(),
            shape: dir_shape("", ""),
        },
    ];
    let local = [
        LocalNode {
            node_id: "f".into(),
            shape: file_shape("root", "b\u{301}.txt"),
            identity: FileId {
                dev: u64::MAX,
                ino: 1 << 63,
                birth_ns: Some(1_700_000_000_123_456_789),
            },
            parked: false,
        },
        LocalNode {
            node_id: "p".into(),
            shape: dir_shape("root", "swap"),
            identity: FileId {
                dev: 16_777_232,
                ino: 42,
                birth_ns: None,
            },
            parked: true,
        },
    ];

    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_remote_node(&remote_file)?;
            tx.put_remote_node(&remote_root)?;
            for node in &synced {
                tx.put_synced_node(node)?;
            }
            for node in &local {
                tx.put_local_node(node)?;
            }
            Ok::<_, Error>(())
        })
        .unwrap();

    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            assert_eq!(
                tx.remote_nodes()?,
                [remote_file.clone(), remote_root.clone()]
            );
            assert_eq!(tx.synced_nodes()?, synced);
            assert_eq!(tx.local_nodes()?, local);

            assert!(tx.remove_remote_node("f")?);
            assert!(tx.remove_synced_node("f")?);
            assert!(tx.remove_local_node("p")?);
            assert!(!tx.remove_local_node("p")?);
            assert_eq!(tx.remote_nodes()?, std::slice::from_ref(&remote_root));
            assert_eq!(tx.synced_nodes()?, synced[1..]);
            assert_eq!(tx.local_nodes()?, local[..1]);
            Ok::<_, Error>(())
        })
        .unwrap();
}

fn identity(ino: u64) -> FileIdentity {
    FileIdentity {
        dev: u64::MAX,
        ino,
        birth_ns: ino.is_multiple_of(2).then_some(-1),
    }
}

fn engine_shape(parent: &str, name: &str) -> EngineShape {
    EngineShape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::File,
        content: Some(content(3)),
    }
}

fn nodes<const N: usize>(ids: [&str; N]) -> BTreeSet<String> {
    ids.into_iter().map(String::from).collect()
}

/// A node, version or restore ID.
fn id(n: u128) -> String {
    uuid_v7(1, n)
}

fn from_trash(node: u128) -> RestoreIntent {
    RestoreIntent::from_trash(id(node), None, None).unwrap()
}

/// A restore from the trash to a named position, and one of a version.
fn placed_and_version() -> [RestoreIntent; 2] {
    let placed = RestoreIntent::from_trash(id(2), Some(id(3)), Some("back.txt".into()));
    [
        placed.unwrap(),
        RestoreIntent::version(id(4), id(5)).unwrap(),
    ]
}

/// An engine state with every field set, and every kind of effect, location, restore and
/// restore outcome.
fn engine_state() -> EngineState {
    let in_docs = |name: &str| Location::At {
        base: Base::Root,
        dirs: vec![Dir {
            name: "docs".into(),
            identity: identity(2),
        }],
        name: name.into(),
    };
    let in_parked = Location::At {
        base: Base::Parked {
            node: "p".into(),
            identity: identity(3),
        },
        dirs: vec![],
        name: "inner.txt".into(),
    };
    let displaced = Displaced {
        at: in_docs("old.txt"),
        identity: identity(4),
        content: content(5),
    };
    let commit = Commit {
        commit_id: "c1".into(),
        base_seq: 12,
        ops: vec![
            Op {
                op: Some(op::Op::CreateFile(CreateFile {
                    node_id: "n".into(),
                    parent_id: "root".into(),
                    name: "new.txt".into(),
                    content: Some(content(3)),
                })),
            },
            Op {
                op: Some(op::Op::Delete(Delete {
                    node_id: "old".into(),
                })),
            },
            Op {
                op: Some(op::Op::Restore(from_trash(1).op().clone())),
            },
        ],
        history_id: "h1".into(),
        device_seq: 5,
        require_head: true,
        ..Default::default()
    };
    let [placed, version] = placed_and_version();
    let outcomes = [
        Outcome::Restored,
        Outcome::Renamed {
            name: "back (conflict).txt".into(),
        },
        Outcome::Copied {
            node: id(6),
            name: "copy.txt".into(),
        },
        Outcome::Rejected(Rejection::NotFound),
        Outcome::Rejected(Rejection::ReadOnly),
        Outcome::Rejected(Rejection::Other("LATER_REASON".into())),
        Outcome::DoesNotFit,
    ];
    let resolved = outcomes
        .into_iter()
        .enumerate()
        .map(|(n, outcome)| Resolved {
            id: id(900 - n as u128),
            intent: [&placed, &version][n % 2].clone(),
            outcome,
        });
    let applied = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    EngineState {
        trees: TreesState {
            dirty: nodes(["a", "b"]),
            unsearched: nodes(["b"]),
            reverted: nodes(["r"]),
            parked_as: BTreeMap::from([("p".into(), "p-before".into())]),
            waiting: BTreeMap::from([
                ("w".into(), Some(engine_shape("root", "w.txt"))),
                ("gone".into(), None),
            ]),
            watched: BTreeMap::from([("edited".into(), true), ("kept".into(), false)]),
            trashed_here: BTreeMap::from([("t".into(), 5)]),
        },
        commits: CommitsState {
            accepted: 4,
            in_flight: Some(InFlight {
                commit,
                deletes: BTreeMap::from([
                    (
                        "old".into(),
                        vec!["old".into(), "old-child".into(), "old-grandchild".into()],
                    ),
                    ("empty".into(), vec![]),
                ]),
                result: Some(CommitResult {
                    seq: 30,
                    results: vec![applied.clone(), applied.clone(), applied],
                    changes: vec![],
                }),
            }),
            problems: nodes(["rejected"]),
            halted: Some(Halt::Policy { code: 42 }),
            cap: Some(8),
            split: nodes(["big"]),
            aliases: BTreeMap::from([("twin".into(), "kept".into())]),
            restores: BTreeMap::from([
                (id(100), from_trash(1)),
                (id(101), placed.clone()),
                (id(102), version.clone()),
            ]),
            resolved: resolved.collect(),
            slots: BTreeMap::from([(3, 7), (4, 9)]),
        },
        effects: EffectsState {
            in_flight: Some(EffectRecord {
                effect_id: "e1".into(),
                node: "n".into(),
                counter: 3,
                effect: Effect::WriteFile {
                    at: in_parked,
                    content: content(7),
                    replaces: Some(displaced.clone()),
                },
                after: Some(engine_shape("p", "inner.txt")),
            }),
            cleared: 2,
            failed: BTreeMap::from([
                (
                    "c".into(),
                    Effect::CreateDir {
                        at: Location::Parked("c".into()),
                    },
                ),
                (
                    "m".into(),
                    Effect::Move {
                        from: in_docs("a"),
                        identity: identity(6),
                        to: Location::Parked("m".into()),
                    },
                ),
                ("f".into(), Effect::RemoveFile { displaced }),
                (
                    "d".into(),
                    Effect::RemoveDir {
                        at: in_docs("empty"),
                        identity: identity(8),
                    },
                ),
            ]),
        },
    }
}

#[test]
fn engine_state_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let state = engine_state();
    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.transaction(|tx| tx.engine_state()).unwrap(),
        EngineState::default()
    );
    store.transaction(|tx| tx.set_engine_state(&state)).unwrap();
    drop(store);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
    store
        .transaction(|tx| {
            tx.set_engine_state(&EngineState::default())?;
            assert_eq!(tx.engine_state()?, EngineState::default());
            Ok::<_, Error>(())
        })
        .unwrap();
}

/// The tables of the engine state besides the nodes of R, L and S.
const ENGINE_TABLES: [&str; 13] = [
    "node_sets",
    "node_maps",
    "waiting_updates",
    "commit_state",
    "commit_in_flight",
    "commit_in_flight_deletes",
    "commit_in_flight_delete_members",
    "watched_contents",
    "trashed_here",
    "commit_slots",
    "effect_state",
    "failed_effects",
    "restore_intents",
];

/// Counts each row inserted, updated or deleted in the engine's tables from now on in the table
/// `writes`. The triggers are stored in the database, so they count the writes of every connection.
fn count_engine_writes(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch("CREATE TABLE writes (n INTEGER NOT NULL); INSERT INTO writes VALUES (0);")
        .unwrap();
    for table in ENGINE_TABLES {
        for event in ["INSERT", "UPDATE", "DELETE"] {
            conn.execute_batch(&format!(
                "CREATE TRIGGER {table}_{event} AFTER {event} ON {table}
                 BEGIN UPDATE writes SET n = n + 1; END;"
            ))
            .unwrap();
        }
    }
}

fn engine_writes(path: &Path) -> u64 {
    let conn = Connection::open(path).unwrap();
    conn.query_row("SELECT n FROM writes", [], |row| row.get(0))
        .unwrap()
}

/// [`engine_state`] with `n` more nodes in each node set, node map, the waiting updates and the
/// failed effects.
fn engine_state_of(n: usize) -> EngineState {
    let mut state = engine_state();
    for i in 0..n {
        let node = format!("node-{i}");
        state.trees.dirty.insert(node.clone());
        state.trees.unsearched.insert(node.clone());
        state.trees.reverted.insert(node.clone());
        state
            .trees
            .parked_as
            .insert(node.clone(), format!("parked-{i}"));
        let shape = engine_shape("p", &format!("{i}.txt"));
        state.trees.waiting.insert(node.clone(), Some(shape));
        state.commits.problems.insert(node.clone());
        state.commits.split.insert(node.clone());
        state
            .commits
            .aliases
            .insert(node.clone(), format!("alias-{i}"));
        let effect = Effect::CreateDir {
            at: Location::Parked(node.clone()),
        };
        state.effects.failed.insert(node, effect);
    }
    state
}

#[test]
fn an_engine_state_update_writes_rows_for_the_change_alone() {
    let mut writes = Vec::new();
    for n in [100, 1000] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let before = engine_state_of(n);
        let mut after = before.clone();
        after.trees.dirty.remove("node-0");
        after.trees.dirty.insert("node-new".into());
        after
            .trees
            .parked_as
            .insert("node-1".into(), "parked-new".into());
        after.trees.waiting.remove("node-2");
        after.trees.waiting.insert("node-3".into(), None);
        after.commits.problems.insert("node-new".into());
        after.commits.accepted += 1;
        after.commits.in_flight = None;
        after.commits.resolved.pop();
        after.effects.cleared += 1;
        after.effects.in_flight = None;
        after.effects.failed.remove("node-4");
        Store::open(&path)
            .unwrap()
            .transaction(|tx| tx.set_engine_state(&before))
            .unwrap();
        count_engine_writes(&path);

        let mut store = Store::open(&path).unwrap();
        store
            .transaction(|tx| tx.update_engine_state(&before, &after))
            .unwrap();
        writes.push(engine_writes(&path));
        assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
        store
            .transaction(|tx| tx.update_engine_state(&after, &after))
            .unwrap();
        assert_eq!(
            engine_writes(&path),
            writes[writes.len() - 1],
            "an unchanged state writes no row"
        );
    }
    assert!(writes[0] > 0);
    assert_eq!(writes[0], writes[1], "rows written for 100 and 1000 nodes");
}

/// Each part of the state reaches the store through an update: from a store that never held one,
/// through changes of each part, back to the default state.
#[test]
fn engine_state_updates_store_every_part() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("state.db")).unwrap();
    let full = engine_state();
    let mut changed = full.clone();
    changed.commits.halted = Some(Halt::Policy { code: 3 });
    changed.commits.cap = Some(2);
    changed.commits.in_flight.as_mut().unwrap().result = None;
    changed.commits.restores.clear();
    changed.effects.in_flight = None;
    changed.trees.waiting.clear();
    let states = [full.clone(), changed, full, EngineState::default()];
    let mut stored = EngineState::default();
    store
        .transaction(|tx| {
            for state in &states {
                tx.update_engine_state(&stored, state)?;
                assert_eq!(&tx.engine_state()?, state);
                stored = state.clone();
            }
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn a_halt_survives_a_reopen_with_its_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    for halted in [
        None,
        Some(Halt::ReadOnly),
        Some(Halt::Recovery),
        Some(Halt::Policy { code: 7 }),
    ] {
        let mut state = engine_state();
        state.commits.halted = halted;
        Store::open(&path)
            .unwrap()
            .transaction(|tx| tx.set_engine_state(&state))
            .unwrap();
        let stored = Store::open(&path)
            .unwrap()
            .transaction(|tx| tx.engine_state())
            .unwrap();
        assert_eq!(stored.commits.halted, halted);
        assert_eq!(stored.commits.in_flight, state.commits.in_flight);
    }
}

#[test]
fn a_policy_halt_outlasts_a_restart_and_resends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let halt = Halt::Policy { code: 42 };
    let state = EngineState {
        commits: CommitsState {
            accepted: 4,
            in_flight: Some(InFlight {
                commit: Commit {
                    commit_id: "c1".into(),
                    history_id: "h1".into(),
                    device_seq: 5,
                    ..Default::default()
                },
                deletes: BTreeMap::new(),
                result: None,
            }),
            halted: Some(halt),
            ..Default::default()
        },
        ..Default::default()
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&state))
        .unwrap();

    let stored = Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.engine_state())
        .unwrap();
    let mut commits = Commits::from_state(stored.commits).unwrap();
    assert_eq!(commits.halted(), Some(halt));
    for device_seq in [4, 5] {
        let welcome = Welcome {
            history_id: "h1".into(),
            device_seq,
            read_only: false,
            ..Default::default()
        };
        assert_eq!(commits.connected(&welcome), Reconnect::Idle);
        assert_eq!(commits.halted(), Some(halt));
    }
}

#[test]
fn a_restore_request_waits_until_it_is_taken() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let [placed, version] = placed_and_version();
    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            tx.request_restore(&id(101), &version)?;
            tx.request_restore(&id(100), &placed)
        })
        .unwrap();
    let lost = store.transaction(|tx| {
        tx.request_restore(&id(102), &from_trash(1))?;
        Err::<(), _>(Error::InvalidEngineState("the process stopped here".into()))
    });
    assert!(lost.is_err());
    drop(store);

    let requests = vec![(id(100), placed), (id(101), version)];
    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            assert_eq!(tx.restore_requests()?, requests, "in request order");
            // Saving the engine state leaves the requests the engine has not taken.
            tx.set_engine_state(&engine_state())?;
            assert_eq!(tx.restore_requests()?, requests);
            assert_eq!(tx.engine_state()?, engine_state());

            assert_eq!(tx.take_restore_requests()?, requests);
            assert_eq!(tx.restore_requests()?, vec![]);
            assert_eq!(tx.take_restore_requests()?, vec![]);
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn the_engines_restores_are_read_without_the_rest_of_its_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("state.db")).unwrap();
    let state = engine_state();
    store
        .transaction(|tx| {
            assert_eq!(tx.restores()?, (BTreeMap::new(), vec![]));
            tx.set_engine_state(&state)?;
            let (queued, resolved) = tx.restores()?;
            assert_eq!(queued, state.commits.restores);
            assert_eq!(resolved, state.commits.resolved);
            assert!(!state.commits.problems.is_empty());
            assert_eq!(tx.problem_nodes()?, state.commits.problems);
            Ok::<_, Error>(())
        })
        .unwrap();
}

/// A database of the schema before the cursor records a server that is behind keeps its cursor,
/// which records none.
#[test]
fn a_cursor_stored_before_it_could_record_a_server_behind_it_records_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let cursor = Cursor {
        history_id: "h1".into(),
        last_seq: 7,
        ahead_of_server: true,
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_cursor(&cursor))
        .unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP TABLE trashed_here; DROP TABLE commit_slots; DROP TABLE watched_contents;
         ALTER TABLE remote_cursor DROP COLUMN ahead_of_server;",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 4).unwrap();
    drop(conn);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    let stored = store.transaction(|tx| tx.cursor()).unwrap();
    assert_eq!(
        stored,
        Some(Cursor {
            ahead_of_server: false,
            ..cursor
        })
    );
}

/// A database of the schema before S's content is watched for the commit in flight keeps its
/// engine state, with nothing watched.
#[test]
fn a_store_from_before_contents_were_watched_opens_with_nothing_watched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP TABLE trashed_here; DROP TABLE commit_slots; DROP TABLE watched_contents;",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 5).unwrap();
    drop(conn);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    before.trees.watched.clear();
    before.trees.trashed_here.clear();
    before.commits.slots.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
}

/// A database of the schema before the deletes and commit slots of this device are recorded keeps
/// its engine state, with neither recorded.
#[test]
fn a_store_from_before_deletes_were_recorded_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("DROP TABLE trashed_here; DROP TABLE commit_slots;")
        .unwrap();
    conn.pragma_update(None, "user_version", SCHEMA_VERSION - 1)
        .unwrap();
    drop(conn);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    before.trees.trashed_here.clear();
    before.commits.slots.clear();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
}

/// A database of the schema before restores gains the two restore tables, empty, and keeps the
/// engine state it held.
#[test]
fn a_version_3_store_opens_with_empty_restore_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    before.commits.in_flight.as_mut().unwrap().commit.ops.pop();
    before.trees.watched.clear();
    before.trees.trashed_here.clear();
    before.commits.slots.clear();
    before.commits.restores.clear();
    before.commits.resolved.clear();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP TABLE restore_requests; DROP TABLE restore_intents;
         DROP TABLE watched_contents; DROP TABLE trashed_here; DROP TABLE commit_slots;
         ALTER TABLE remote_cursor DROP COLUMN ahead_of_server;",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 3).unwrap();
    drop(conn);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    store
        .transaction(|tx| {
            assert_eq!(tx.engine_state()?, before);
            assert_eq!(tx.restore_requests()?, vec![]);
            tx.request_restore(&id(100), &from_trash(1))?;
            tx.set_engine_state(&engine_state())?;
            assert_eq!(tx.engine_state()?, engine_state());
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn a_stored_restore_the_server_would_refuse_is_not_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    drop(Store::open(&path).unwrap());
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO restore_intents (restore_id, node_id) VALUES ('r', 'not-a-uuid')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO restore_requests (restore_id, node_id) VALUES ('r', 'not-a-uuid')",
        [],
    )
    .unwrap();
    drop(conn);

    let mut store = Store::open(&path).unwrap();
    let loaded = store.transaction(|tx| tx.engine_state());
    assert!(
        matches!(loaded, Err(Error::InvalidEngineState(_))),
        "{loaded:?}"
    );
    let taken = store.transaction(|tx| tx.take_restore_requests());
    assert!(
        matches!(taken, Err(Error::InvalidEngineState(_))),
        "{taken:?}"
    );
}

#[test]
fn id_generator_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            assert_eq!(tx.id_generator()?, None);
            tx.set_id_generator(&[1, 2, 3])
        })
        .unwrap();
    drop(store);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.transaction(|tx| tx.id_generator()).unwrap(),
        Some(vec![1, 2, 3])
    );
}

#[test]
fn failed_transaction_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("state.db")).unwrap();
    let synced = SyncedNode {
        node_id: "root".into(),
        shape: dir_shape("", ""),
    };

    let result: Result<(), Error> = store.transaction(|tx| {
        tx.put_synced_node(&synced)?;
        tx.set_cursor(&Cursor {
            history_id: "h1".into(),
            last_seq: 1,
            ahead_of_server: false,
        })?;
        Err(Error::SchemaTooNew {
            found: 0,
            supported: 0,
        })
    });
    assert!(result.is_err());

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = store.transaction(|tx| {
            tx.put_synced_node(&synced)?;
            panic!("crash inside the transaction");
            #[allow(unreachable_code)]
            Ok::<_, Error>(())
        });
    }));
    assert!(panicked.is_err());

    store
        .transaction(|tx| {
            assert_eq!(tx.synced_nodes()?, []);
            assert_eq!(tx.cursor()?, None);
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn chunk_cache_tracks_size_and_eviction_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("state.db")).unwrap();
    let (a, b, c) = ([1u8; 32], [2u8; 32], [3u8; 32]);

    store
        .transaction(|tx| {
            tx.put_chunk(&a, 100, 10)?;
            tx.put_chunk(&b, 200, 20)?;
            tx.put_chunk(&c, 300, 30)?;
            assert!(tx.touch_chunk(&a, 40)?);
            assert!(!tx.touch_chunk(&[9; 32], 40)?);
            assert_eq!(tx.chunk_cache_size()?, 600);
            assert_eq!(
                tx.chunk(&a)?,
                Some(CachedChunk {
                    hash: a,
                    size: 100,
                    last_used_ms: 40
                })
            );
            let order: Vec<_> = tx
                .least_recently_used(2)?
                .into_iter()
                .map(|chunk| chunk.hash)
                .collect();
            assert_eq!(order, [b, c]);
            assert!(tx.remove_chunk(&b)?);
            assert_eq!(tx.chunk(&b)?, None);
            assert_eq!(tx.chunk_cache_size()?, 400);
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn a_read_only_open_reads_the_link_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state #1?.db");
    let link = Link {
        space_id: "0190f0a0-0000-7000-8000-000000000001".into(),
        space_name: "Projects".into(),
        root: std::path::PathBuf::from("/home/ada/Flarebox"),
        root_dev: 66306,
        server_url: "https://files.example.com/".into(),
        device_id: "d1".into(),
        linked_at_ms: 1_700_000_000_000,
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_link(&link))
        .unwrap();
    let listing = || {
        let mut files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let meta = entry.metadata().unwrap();
                (
                    entry.file_name(),
                    meta.modified().unwrap(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect();
        files.sort();
        files
    };
    let before = listing();

    let read = Store::open_read_only(&path)
        .unwrap()
        .transaction(|tx| tx.link())
        .unwrap();

    assert_eq!(read, Some(link.clone()));
    assert_eq!(listing(), before);

    let mut writer = Store::open(&path).unwrap();
    writer
        .transaction(|tx| {
            tx.set_link(&Link {
                space_name: "Renamed".into(),
                ..link.clone()
            })
        })
        .unwrap();
    match Store::open_read_only(&path) {
        Err(Error::LogNotApplied(log)) => assert!(log.ends_with("state #1?.db-wal")),
        other => panic!("expected LogNotApplied, got {:?}", other.err()),
    }
}

#[test]
fn link_and_questions_persist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let link = Link {
        space_id: "0190f0a0-0000-7000-8000-000000000001".into(),
        space_name: "Projects".into(),
        root: std::path::PathBuf::from("/home/ada/Flarebox"),
        root_dev: 66306,
        server_url: "https://files.example.com/".into(),
        device_id: "d1".into(),
        linked_at_ms: 1_700_000_000_000,
    };
    let question = |id: &str, at: i64| StoredQuestion {
        question_id: id.into(),
        asked_at_ms: at,
        body: format!("{{\"id\":\"{id}\"}}").into_bytes(),
        answer: None,
    };
    let answer = GivenAnswer {
        answer: "restore".into(),
        answered_at_ms: 30,
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            assert_eq!(tx.link()?, None);
            tx.set_link(&link)?;
            tx.put_question(&question("q2", 20))?;
            tx.put_question(&question("q1", 10))?;
            assert!(tx.answer_question("q2", &answer)?);
            assert!(!tx.answer_question(
                "q2",
                &GivenAnswer {
                    answer: "delete".into(),
                    answered_at_ms: 40
                }
            )?);
            assert!(!tx.answer_question("missing", &answer)?);
            Ok::<_, Error>(())
        })
        .unwrap();

    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            assert_eq!(tx.link()?, Some(link.clone()));
            let questions = tx.questions()?;
            assert_eq!(questions[0], question("q1", 10));
            assert_eq!(questions[1].question_id, "q2");
            assert_eq!(questions[1].answer, Some(answer.clone()));
            assert!(tx.remove_question("q1")?);
            assert!(!tx.remove_question("q1")?);
            assert_eq!(tx.questions()?.len(), 1);
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn remote_tree_can_be_counted_and_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("state.db")).unwrap();
    store
        .transaction(|tx| {
            for id in ["a", "b", "c"] {
                tx.put_remote_node(&NodeState {
                    node_id: id.into(),
                    parent_id: "root".into(),
                    name: id.into(),
                    kind: NodeKind::Dir as i32,
                    status: NodeStatus::Live as i32,
                    ..Default::default()
                })?;
            }
            assert_eq!(tx.remote_node_count()?, 3);
            tx.clear_remote_nodes()?;
            assert_eq!(tx.remote_node_count()?, 0);
            assert!(tx.remote_nodes()?.is_empty());
            Ok::<_, Error>(())
        })
        .unwrap();
}
