// SPDX-License-Identifier: Apache-2.0

//! Kills a real process in the middle of store transactions and checks what
//! the next open sees. The test binary re-runs itself as the child process;
//! the environment variable tells the child which database to use.

use std::env;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

use flarebox_proto::v1::{NodeKind, NodeState, NodeStatus};
use flarebox_store::{Cursor, Error, Store};
use rusqlite::Connection;

const CHILD_DB: &str = "FLAREBOX_STORE_CRASH_CHILD_DB";
const NODES_PER_COMMIT: u64 = 500;

fn node(n: u64) -> NodeState {
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

/// Commit `seq` writes nodes `(seq - 1) * NODES_PER_COMMIT .. seq * NODES_PER_COMMIT`
/// and moves the cursor to `seq`, so a consistent database always holds
/// exactly `last_seq * NODES_PER_COMMIT` nodes.
fn write_commit(store: &mut Store, seq: u64) -> Result<(), Error> {
    store.transaction(|tx| {
        for n in (seq - 1) * NODES_PER_COMMIT..seq * NODES_PER_COMMIT {
            tx.put_node(&node(n))?;
        }
        tx.set_cursor(&Cursor {
            history_id: "history".into(),
            last_seq: seq,
        })
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
    let last_seq = store
        .transaction(|tx| tx.cursor())
        .unwrap()
        .map_or(0, |c| c.last_seq);
    let expected = last_seq * NODES_PER_COMMIT;
    store
        .transaction(|tx| {
            assert_eq!(tx.children("root")?.len() as u64, expected);
            if expected > 0 {
                assert_eq!(
                    tx.node(&node(expected - 1).node_id)?,
                    Some(node(expected - 1))
                );
            }
            assert_eq!(tx.node(&node(expected).node_id)?, None);
            Ok::<_, Error>(())
        })
        .unwrap();
    last_seq
}

/// The child aborts the process inside a large transaction, after its pages
/// have been written to the WAL but before the commit.
#[test]
fn abort_inside_transaction_keeps_last_commit() {
    if let Ok(db) = env::var(CHILD_DB) {
        let mut store = Store::open(&db).unwrap();
        let _ = store.transaction(|tx| {
            for n in NODES_PER_COMMIT..20 * NODES_PER_COMMIT {
                tx.put_node(&node(n))?;
            }
            tx.set_cursor(&Cursor {
                history_id: "history".into(),
                last_seq: 20,
            })?;
            std::process::abort();
            #[allow(unreachable_code)]
            Ok::<_, Error>(())
        });
        unreachable!();
    }

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    write_commit(&mut Store::open(&db).unwrap(), 1).unwrap();

    let status = run_child("abort_inside_transaction_keeps_last_commit", &db)
        .wait()
        .unwrap();
    assert!(!status.success(), "the child must die, not exit cleanly");

    assert_eq!(check_consistent(&db), 1);
}

/// The child commits in a loop and reports each commit; the parent kills it
/// with SIGKILL at an arbitrary point. Every reported commit must survive and
/// the database must hold exactly the commits up to its cursor.
#[test]
fn kill_during_commits_keeps_an_atomic_prefix() {
    if let Ok(db) = env::var(CHILD_DB) {
        let mut store = Store::open(&db).unwrap();
        let mut seq = 0;
        loop {
            seq += 1;
            write_commit(&mut store, seq).unwrap();
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
    assert!(last_seq >= last_reported, "a reported commit was lost");
}
