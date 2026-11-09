// SPDX-License-Identifier: Apache-2.0

use flarebox_proto::v1::{Content, NodeKind, NodeState, NodeStatus, Op, VersionInfo, op};
use flarebox_store::{CachedChunk, Cursor, Error, SCHEMA_VERSION, Store};
use rusqlite::Connection;

fn node(node_id: &str, parent_id: &str, name: &str) -> NodeState {
    NodeState {
        node_id: node_id.into(),
        parent_id: parent_id.into(),
        name: name.into(),
        kind: NodeKind::Dir as i32,
        status: NodeStatus::Live as i32,
        content_rev: 1,
        meta_rev: 1,
        ..Default::default()
    }
}

fn delete_op(node_id: &str) -> Op {
    Op {
        op: Some(op::Op::Delete(flarebox_proto::v1::Delete {
            node_id: node_id.into(),
        })),
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
    let mut tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    tables.sort();
    assert_eq!(tables, ["chunk_cache", "cursor", "nodes", "pending_ops"]);
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
fn tree_round_trips_node_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("state.db")).unwrap();
    let root = node("root", "", "");
    let file = NodeState {
        kind: NodeKind::File as i32,
        content_rev: 4,
        meta_rev: 3,
        trash_group: "4:0".into(),
        status: NodeStatus::Trashed as i32,
        current: Some(VersionInfo {
            version_id: "v1".into(),
            seq: 4,
            content: Some(Content {
                size: 3,
                mtime_ns: 1_700_000_000_000_000_000,
                executable: true,
                chunk_count: 1,
                manifest_digest: vec![9; 32],
                ..Default::default()
            }),
            device_id: "d1".into(),
            user_id: "u1".into(),
            created_at_ms: 1_700_000_000_000,
        }),
        ..node("f", "root", "b.txt")
    };
    let sub = node("s", "root", "a");

    store
        .transaction(|tx| {
            tx.put_node(&root)?;
            tx.put_node(&file)?;
            tx.put_node(&sub)
        })
        .unwrap();

    store
        .transaction(|tx| {
            assert_eq!(tx.node("root")?, Some(root.clone()));
            assert_eq!(tx.node("f")?, Some(file.clone()));
            assert_eq!(tx.node("missing")?, None);
            assert_eq!(tx.children("root")?, vec![sub.clone(), file.clone()]);
            assert!(tx.remove_node("s")?);
            assert!(!tx.remove_node("s")?);
            assert_eq!(tx.children("root")?, vec![file.clone()]);
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn failed_transaction_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("state.db")).unwrap();

    let result: Result<(), Error> = store.transaction(|tx| {
        tx.put_node(&node("root", "", ""))?;
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
            tx.put_node(&node("root", "", ""))?;
            panic!("crash inside the transaction");
            #[allow(unreachable_code)]
            Ok::<_, Error>(())
        });
    }));
    assert!(panicked.is_err());

    store
        .transaction(|tx| {
            assert_eq!(tx.node("root")?, None);
            assert_eq!(tx.cursor()?, None);
            Ok::<_, Error>(())
        })
        .unwrap();
}

#[test]
fn pending_ops_keep_order_and_commit_assignment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut store = Store::open(&path).unwrap();

    let ids = store
        .transaction(|tx| {
            Ok::<_, Error>([
                tx.push_op(&delete_op("a"))?,
                tx.push_op(&delete_op("b"))?,
                tx.push_op(&delete_op("c"))?,
            ])
        })
        .unwrap();
    assert!(ids[0] < ids[1] && ids[1] < ids[2]);

    store
        .transaction(|tx| {
            tx.assign_commit(&ids[..2], "commit-1")?;
            assert!(tx.replace_op(ids[1], &delete_op("b2"))?);
            assert!(!tx.replace_op(ids[2] + 100, &delete_op("x"))?);
            Ok::<_, Error>(())
        })
        .unwrap();
    drop(store);

    let mut store = Store::open(&path).unwrap();
    let pending = store.transaction(|tx| tx.pending_ops()).unwrap();
    let summary: Vec<_> = pending
        .iter()
        .map(|p| (p.id, p.op.clone(), p.commit_id.as_deref()))
        .collect();
    assert_eq!(
        summary,
        [
            (ids[0], delete_op("a"), Some("commit-1")),
            (ids[1], delete_op("b2"), Some("commit-1")),
            (ids[2], delete_op("c"), None),
        ]
    );

    assert_eq!(
        store
            .transaction(|tx| tx.remove_commit("commit-1"))
            .unwrap(),
        2
    );
    let next = store.transaction(|tx| tx.push_op(&delete_op("d"))).unwrap();
    assert!(next > ids[2], "ids are never reused");
    let left: Vec<_> = store
        .transaction(|tx| tx.pending_ops())
        .unwrap()
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(left, [ids[2], next]);
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
