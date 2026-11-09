// SPDX-License-Identifier: Apache-2.0

use flarebox_fs::FileId;
use flarebox_proto::v1::{
    ChunkRef, Commit, Content, CreateFile, Delete, NodeKind, NodeState, NodeStatus, Op,
    VersionInfo, op,
};
use flarebox_store::{
    CachedChunk, Cursor, Error, InFlightCommit, LocalNode, Problem, SCHEMA_VERSION, Shape, Store,
    SyncedNode,
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
            "commit_in_flight_shapes",
            "engine_state",
            "local_nodes",
            "problems",
            "remote_cursor",
            "remote_nodes",
            "synced_nodes",
        ]
    );
    let journal: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(journal, "wal");
}

#[test]
fn reopening_keeps_data_and_schema_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let cursor = Cursor {
        history_id: "h1".into(),
        last_seq: 7,
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

#[test]
fn commit_in_flight_keeps_ops_and_sent_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let in_flight = InFlightCommit {
        commit: Commit {
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
            ],
            receipts: vec![],
        },
        sent: vec![Some(file_shape("root", "new.txt")), None],
    };

    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.commit_in_flight()).unwrap(), None);
    store
        .transaction(|tx| tx.set_commit_in_flight(Some(&in_flight)))
        .unwrap();
    drop(store);

    let mut store = Store::open(&path).unwrap();
    assert_eq!(
        store.transaction(|tx| tx.commit_in_flight()).unwrap(),
        Some(in_flight.clone())
    );

    let next = InFlightCommit {
        commit: Commit {
            commit_id: "c2".into(),
            base_seq: 13,
            ops: in_flight.commit.ops[1..].to_vec(),
            receipts: vec![],
        },
        sent: vec![None],
    };
    store
        .transaction(|tx| {
            tx.set_commit_in_flight(Some(&next))?;
            assert_eq!(tx.commit_in_flight()?, Some(next.clone()));
            tx.set_commit_in_flight(None)?;
            assert_eq!(tx.commit_in_flight()?, None);
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn problems_and_id_generator_persist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let problem = Problem {
        node_id: "n".into(),
        reason: "REJECTED: INVALID_NAME".into(),
    };
    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            assert_eq!(tx.id_generator()?, None);
            tx.set_id_generator(&[1, 2, 3])?;
            tx.put_problem(&problem)
        })
        .unwrap();
    drop(store);

    let mut store = Store::open(&path).unwrap();
    store
        .transaction(|tx| {
            assert_eq!(tx.id_generator()?, Some(vec![1, 2, 3]));
            assert_eq!(tx.problems()?, std::slice::from_ref(&problem));
            assert!(tx.remove_problem("n")?);
            assert_eq!(tx.problems()?, []);
            Ok::<_, Error>(())
        })
        .unwrap();
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
