// SPDX-License-Identifier: Apache-2.0

//! Kills a real process in the middle of store transactions and checks what
//! the next open sees. The test binary re-runs itself as the child process;
//! the environment variable tells the child which database to use.

use std::collections::BTreeMap;
use std::env;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

use flarebox_engine::commit::InFlight;
use flarebox_engine::state::{CommitsState, EffectsState};
use flarebox_fs::FileId;
use flarebox_proto::v1::{Commit, NodeKind, NodeState, NodeStatus};
use flarebox_store::{Cursor, EngineState, Error, LocalNode, Shape, Store, SyncedNode, Tx};
use rusqlite::Connection;

const CHILD_DB: &str = "FLAREBOX_STORE_CRASH_CHILD_DB";
const NODES_PER_INPUT: u64 = 500;

fn remote(n: u64) -> NodeState {
    NodeState {
        node_id: format!("node-{n:08}"),
        parent_id: "root".into(),
        name: format!("file {n}"),
        kind: NodeKind::File as i32,
        status: NodeStatus::Live as i32,
        content_rev: n,
        meta_rev: n,
        ..Default::default()
    }
}

fn shape(n: u64) -> Shape {
    Shape {
        parent_id: "root".into(),
        name: format!("file {n}"),
        kind: NodeKind::File as i32,
        content: None,
    }
}

fn engine(seq: u64) -> EngineState {
    EngineState {
        commits: CommitsState {
            in_flight: Some(InFlight {
                commit: Commit {
                    commit_id: format!("commit-{seq}"),
                    base_seq: seq,
                    ..Default::default()
                },
                deletes: BTreeMap::new(),
                result: None,
            }),
            ..Default::default()
        },
        effects: EffectsState {
            cleared: seq,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Input `seq` writes, in one transaction like one engine input, the remote,
/// synced and local rows of nodes `(seq - 1) * NODES_PER_INPUT .. seq * NODES_PER_INPUT`,
/// moves the cursor to `seq` and replaces the engine state. A consistent
/// database therefore holds exactly `last_seq * NODES_PER_INPUT` nodes in each
/// tree and the engine state of `last_seq`.
fn write_input(tx: &Tx<'_>, seq: u64) -> Result<(), Error> {
    for n in (seq - 1) * NODES_PER_INPUT..seq * NODES_PER_INPUT {
        tx.put_remote_node(&remote(n))?;
        tx.put_synced_node(&SyncedNode {
            node_id: remote(n).node_id,
            shape: shape(n),
        })?;
        tx.put_local_node(&LocalNode {
            node_id: remote(n).node_id,
            shape: shape(n),
            identity: FileId {
                dev: 1,
                ino: n,
                birth_ns: None,
            },
            parked: false,
        })?;
    }
    tx.set_engine_state(&engine(seq))?;
    tx.set_cursor(&Cursor {
        history_id: "history".into(),
        last_seq: seq,
    })
}

fn run_child(test: &str, db: &Path) -> std::process::Child {
    Command::new(env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD_DB, db)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

fn check_consistent(db: &Path) -> u64 {
    let conn = Connection::open(db).unwrap();
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    drop(conn);

    let mut store = Store::open(db).unwrap();
    store
        .transaction(|tx| {
            let last_seq = tx.cursor()?.map_or(0, |cursor| cursor.last_seq);
            let expected = last_seq * NODES_PER_INPUT;
            let remote_nodes = tx.remote_nodes()?;
            assert_eq!(remote_nodes.len() as u64, expected);
            assert_eq!(
                remote_nodes.last(),
                expected.checked_sub(1).map(remote).as_ref()
            );
            assert_eq!(tx.synced_nodes()?.len() as u64, expected);
            assert_eq!(tx.local_nodes()?.len() as u64, expected);
            let expected_state = if last_seq > 0 {
                engine(last_seq)
            } else {
                EngineState::default()
            };
            assert_eq!(tx.engine_state()?, expected_state);
            Ok::<_, Error>(last_seq)
        })
        .unwrap()
}

/// The child aborts the process inside a large transaction, after its pages
/// have spilled to the WAL but before the commit.
#[test]
fn abort_inside_transaction_keeps_last_commit() {
    if let Ok(db) = env::var(CHILD_DB) {
        let mut store = Store::open(&db).unwrap();
        let _ = store.transaction(|tx| {
            for seq in 2..=20 {
                write_input(tx, seq)?;
            }
            std::process::abort();
            #[allow(unreachable_code)]
            Ok::<_, Error>(())
        });
        unreachable!();
    }

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    Store::open(&db)
        .unwrap()
        .transaction(|tx| write_input(tx, 1))
        .unwrap();

    let status = run_child("abort_inside_transaction_keeps_last_commit", &db)
        .wait()
        .unwrap();
    assert!(!status.success(), "the child must die, not exit cleanly");

    assert_eq!(check_consistent(&db), 1);
}

/// The child applies inputs in a loop and reports each commit; the parent kills
/// it with SIGKILL at an arbitrary point. Every reported input must survive and
/// the database must hold exactly the inputs up to its cursor.
#[test]
fn kill_during_commits_keeps_an_atomic_prefix() {
    if let Ok(db) = env::var(CHILD_DB) {
        let mut store = Store::open(&db).unwrap();
        let mut seq = 0;
        loop {
            seq += 1;
            store.transaction(|tx| write_input(tx, seq)).unwrap();
            println!("committed {seq}");
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    drop(Store::open(&db).unwrap());

    let mut child = run_child("kill_during_commits_keeps_an_atomic_prefix", &db);
    let mut last_reported = 0;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        if let Some(seq) = line.unwrap().strip_prefix("committed ") {
            last_reported = seq.parse().unwrap();
            if last_reported >= 25 {
                child.kill().unwrap();
                break;
            }
        }
    }
    let status = child.wait().unwrap();
    assert!(!status.success(), "the child must be killed");

    let last_seq = check_consistent(&db);
    println!("reported {last_reported}, recovered {last_seq}");
    assert!(last_seq >= last_reported, "a reported input was lost");
}
