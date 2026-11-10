// SPDX-License-Identifier: Apache-2.0

//! Building commits from local changes and acknowledging their results (docs/design/sync-engine.md
//! sections 2.4, 2.6, 2.7, 2.8 and 4.2, and RFC 0002), on hand-built trees and results.

use flarebox_engine::classify::{Class, Trees};
use flarebox_engine::commit::{Commits, Failure, Halt, MAX_OPS, Reconnect};
use flarebox_engine::effects::Effects;
use flarebox_engine::ids::IdGen;
use flarebox_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};
use flarebox_proto::v1::{self as proto, CommitResult, OpResult, Welcome, error, op, op_result};

const ROOT: &str = "root";
const NOW: u64 = 1_794_268_740_000;
const HISTORY: &str = "history";

fn content(digest: u8) -> Content {
    Content {
        size: 3,
        manifest_digest: vec![digest; 32],
        ..Default::default()
    }
}

fn file(parent: &str, name: &str, digest: u8) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::File,
        content: Some(content(digest)),
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

fn remote(shape: Shape) -> RemoteNode {
    RemoteNode {
        shape,
        trashed: false,
        content_rev: 1,
        meta_rev: 1,
        trash_group: String::new(),
    }
}

fn trashed(shape: Shape) -> RemoteNode {
    RemoteNode {
        trashed: true,
        trash_group: "2:0".into(),
        ..remote(shape)
    }
}

fn local(shape: Shape, ino: u64) -> LocalNode {
    LocalNode {
        shape,
        identity: FileIdentity {
            dev: 1,
            ino,
            birth_ns: 0,
        },
        parked: false,
    }
}

/// A node's L, S and R.
type Row = (
    &'static str,
    Option<Shape>,
    Option<Shape>,
    Option<RemoteNode>,
);

/// Trees from (node, L, S, R) rows.
fn trees(nodes: &[Row]) -> Trees {
    let mut trees = Trees::new(ROOT);
    for (i, (id, l, s, r)) in nodes.iter().enumerate() {
        if let Some(shape) = l {
            trees.set_local((*id).into(), local(shape.clone(), i as u64 + 1));
        }
        trees.set_synced((*id).into(), s.clone());
        if let Some(node) = r {
            trees.set_remote((*id).into(), node.clone());
        }
    }
    trees.settle();
    trees
}

/// The same shape in L, S and R.
fn synced(id: &'static str, shape: Shape) -> Row {
    (
        id,
        Some(shape.clone()),
        Some(shape.clone()),
        Some(remote(shape)),
    )
}

fn build(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen) -> Option<proto::Commit> {
    commits.build(trees, ids, NOW, HISTORY, 7).cloned()
}

fn ops(commit: &proto::Commit) -> Vec<op::Op> {
    commit.ops.iter().map(|op| op.op.clone().unwrap()).collect()
}

fn mv(node: &str, parent: &str, name: &str) -> op::Op {
    op::Op::Move(proto::Move {
        node_id: node.into(),
        new_parent_id: parent.into(),
        new_name: name.into(),
    })
}

fn delete(node: &str) -> op::Op {
    op::Op::Delete(proto::Delete {
        node_id: node.into(),
    })
}

fn update(node: &str, digest: u8) -> op::Op {
    op::Op::UpdateContent(proto::UpdateContent {
        node_id: node.into(),
        content: Some(content(digest)),
    })
}

fn outcome(code: op_result::Code) -> OpResult {
    OpResult {
        code: code as i32,
        ..Default::default()
    }
}

fn result(seq: u64, results: Vec<OpResult>) -> CommitResult {
    CommitResult {
        seq,
        results,
        changes: vec![],
    }
}

/// Receives `results` for the commit in flight and acknowledges it with R at `seq`.
fn acknowledge(commits: &mut Commits, trees: &mut Trees, results: Vec<OpResult>) {
    commits.received(result(1, results)).unwrap();
    assert!(commits.acknowledge(trees, 1));
    trees.settle();
}

#[test]
fn local_changes_become_ops_with_parents_first() {
    let mut trees = trees(&[
        ("a", Some(file("z", "f", 1)), None, None),
        ("z", Some(dir(ROOT, "d")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [
            op::Op::CreateDir(proto::CreateDir {
                node_id: "z".into(),
                parent_id: ROOT.into(),
                name: "d".into(),
            }),
            op::Op::CreateFile(proto::CreateFile {
                node_id: "a".into(),
                parent_id: "z".into(),
                name: "f".into(),
                content: Some(content(1)),
            }),
        ]
    );
    assert_eq!(
        (
            commit.history_id.as_str(),
            commit.base_seq,
            commit.device_seq
        ),
        (HISTORY, 7, 1)
    );
    assert_eq!(
        build(&mut commits, &mut trees, &mut ids),
        None,
        "one commit in flight"
    );
}

#[test]
fn a_name_swap_is_sent_for_the_server_to_resolve() {
    let mut trees = trees(&[
        (
            "x",
            Some(file(ROOT, "b", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        (
            "y",
            Some(file(ROOT, "a", 2)),
            Some(file(ROOT, "b", 2)),
            Some(remote(file(ROOT, "b", 2))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("x", ROOT, "b"), mv("y", ROOT, "a")]);
}

#[test]
fn a_delete_waits_until_a_child_is_moved_out_and_covers_its_subtree() {
    let mut trees = trees(&[
        synced("a", file("d", "keep", 1)),
        (
            "d",
            None,
            Some(dir(ROOT, "d")),
            Some(remote(dir(ROOT, "d"))),
        ),
        (
            "e",
            None,
            Some(file("d", "gone", 2)),
            Some(remote(file("d", "gone", 2))),
        ),
    ]);
    let mut moved = trees.local().get("a").unwrap().clone();
    moved.shape.parent = ROOT.into();
    trees.set_local("a".into(), moved);
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("a", ROOT, "keep"), delete("d")]);
    assert_eq!(
        commits.in_flight().unwrap().deletes["d"],
        ["d".to_string(), "e".to_string()]
    );
}

#[test]
fn a_delete_keeps_what_changed_remotely_and_the_folders_it_needs() {
    let mut trees = trees(&[
        (
            "c",
            None,
            Some(file("d", "c", 1)),
            Some(remote(file("d", "c", 2))),
        ),
        (
            "d",
            None,
            Some(dir(ROOT, "d")),
            Some(remote(dir(ROOT, "d"))),
        ),
        (
            "e",
            None,
            Some(file("d", "e", 1)),
            Some(remote(file("d", "e", 1))),
        ),
        (
            "n",
            None,
            Some(dir(ROOT, "n")),
            Some(remote(dir(ROOT, "n"))),
        ),
        ("m", None, None, Some(remote(file("n", "new", 3)))),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete("e")]);
    for id in ["c", "n", "m"] {
        assert_eq!(trees.synced().live(id), None, "{id}");
        assert_eq!(trees.class(id), Class::Remote, "{id}");
    }
    assert_eq!(trees.synced().live("d"), Some(&dir(ROOT, "d")));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(trees.synced().live("e"), None);
    assert!(build(&mut commits, &mut trees, &mut ids).is_none());
    assert_eq!(trees.synced().live("d"), None);
    assert_eq!(trees.class("d"), Class::Remote);
}

#[test]
fn content_edited_on_both_sides_is_forked_into_a_new_node() {
    let mut trees = trees(&[(
        "f",
        Some(file(ROOT, "f", 2)),
        Some(file(ROOT, "f", 1)),
        Some(remote(file(ROOT, "f", 3))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let [op::Op::CreateFile(create)] = <[op::Op; 1]>::try_from(ops(&commit)).unwrap() else {
        panic!("{commit:?}");
    };
    assert_eq!(
        (
            create.parent_id.as_str(),
            create.name.as_str(),
            create.content
        ),
        (ROOT, "f", Some(content(2)))
    );
    assert_eq!(trees.local().get(&create.node_id).unwrap().identity.ino, 1);
    assert_eq!(trees.local().get("f"), None);
    assert_eq!(trees.class("f"), Class::Remote);
}

#[test]
fn an_edit_against_a_remote_move_is_sent_alone() {
    let mut trees = trees(&[(
        "f",
        Some(file(ROOT, "a", 2)),
        Some(file(ROOT, "a", 1)),
        Some(remote(file(ROOT, "b", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("f", 2)]);
}

#[test]
fn an_edit_and_a_move_against_a_remote_delete_are_sent_move_first() {
    let mut trees = trees(&[(
        "f",
        Some(file(ROOT, "b", 2)),
        Some(file(ROOT, "a", 1)),
        Some(trashed(file(ROOT, "a", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", ROOT, "b"), update("f", 2)]);
}

#[test]
fn a_move_that_closes_a_cycle_against_r_is_reverted() {
    let mut trees = trees(&[
        (
            "p",
            Some(dir("q", "p")),
            Some(dir(ROOT, "p")),
            Some(remote(dir(ROOT, "p"))),
        ),
        (
            "q",
            Some(dir(ROOT, "q")),
            Some(dir(ROOT, "q")),
            Some(remote(dir("p", "q"))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(trees.synced().live("p"), Some(&dir(ROOT, "p")));
    assert!(trees.reverted().contains("p"));
    moves_back(&mut trees, &mut ids, "p");
}

/// Applies the effect that moves the reverted `node` back to its R position.
fn moves_back(trees: &mut Trees, ids: &mut IdGen, node: &str) {
    let mut effects = Effects::default();
    let record = effects.plan(trees, ids, NOW).unwrap().clone();
    assert_eq!(record.node, node);
    effects.done(trees, &record.effect_id, None).unwrap();
    trees.settle();
    assert_eq!(
        trees.local().get(node).map(|n| &n.shape),
        trees.remote().live(node)
    );
    assert!(trees.reverted().is_empty());
}

#[test]
fn a_reverted_move_brings_back_the_folders_l_deleted_on_its_way() {
    let mut trees = trees(&[
        (
            "d",
            None,
            Some(dir(ROOT, "d")),
            Some(remote(dir(ROOT, "d"))),
        ),
        (
            "p",
            Some(dir("q", "p")),
            Some(dir("d", "p")),
            Some(remote(dir("d", "p"))),
        ),
        (
            "q",
            Some(dir(ROOT, "q")),
            Some(dir(ROOT, "q")),
            Some(remote(dir("p", "q"))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    let reverted: Vec<&str> = trees.reverted().iter().map(String::as_str).collect();
    assert_eq!(reverted, ["d", "p"]);
    let mut effects = Effects::default();
    while let Some(record) = effects.plan(&trees, &mut ids, NOW).cloned() {
        let identity = FileIdentity {
            dev: 1,
            ino: 99,
            birth_ns: 0,
        };
        effects
            .done(&mut trees, &record.effect_id, Some(identity))
            .unwrap();
        trees.settle();
    }
    for id in ["d", "p", "q"] {
        assert_eq!(
            trees.local().get(id).map(|n| &n.shape),
            trees.remote().live(id),
            "{id}"
        );
    }
    assert!(trees.settle().is_empty());
}

#[test]
fn a_folder_a_new_one_may_be_merged_into_is_not_deleted_in_the_same_commit() {
    let mut trees = trees(&[
        (
            "o",
            None,
            Some(dir(ROOT, "a")),
            Some(remote(dir(ROOT, "a"))),
        ),
        (
            "f",
            Some(file("n", "f", 1)),
            Some(file("o", "f", 1)),
            Some(remote(file("o", "f", 1))),
        ),
        ("n", Some(dir(ROOT, "a")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let create = op::Op::CreateDir(proto::CreateDir {
        node_id: "n".into(),
        parent_id: ROOT.into(),
        name: "a".into(),
    });
    assert_eq!(ops(&commit), [create, mv("f", "n", "f")]);
}

#[test]
fn a_folder_created_where_a_moving_folder_still_is_takes_a_pending_name_first() {
    let mut trees = trees(&[
        (
            "h",
            Some(dir("n", "a")),
            Some(dir(ROOT, "a")),
            Some(remote(dir(ROOT, "a"))),
        ),
        ("n", Some(dir(ROOT, "a")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    let create = op::Op::CreateDir(proto::CreateDir {
        node_id: "n".into(),
        parent_id: ROOT.into(),
        name: "a (pending)".into(),
    });
    assert_eq!(ops(&first), [create, mv("h", "n", "a")]);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    trees.set_remote("n".into(), remote(dir(ROOT, "a (pending)")));
    trees.set_remote("h".into(), remote(dir("n", "a")));
    let second = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&second), [mv("n", ROOT, "a")]);
}

#[test]
fn a_renamed_result_reverts_the_local_name_to_the_server_one() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let renamed = OpResult {
        name: "x (conflict)".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    trees.set_remote("a".into(), remote(file(ROOT, "x (conflict)", 1)));
    assert_eq!(
        trees.synced().live("a"),
        Some(&file(ROOT, "x (conflict)", 1))
    );
    assert!(trees.reverted().contains("a"));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    moves_back(&mut trees, &mut ids, "a");
}

#[test]
fn a_commit_holds_at_most_the_op_limit_with_parents_first() {
    let mut rows = vec![("d".to_string(), dir(ROOT, "d"))];
    rows.extend((0..MAX_OPS).map(|i| (format!("f{i:04}"), file("d", &format!("{i}"), 1))));
    let mut trees = Trees::new(ROOT);
    for (i, (id, shape)) in rows.into_iter().enumerate() {
        trees.set_local(id, local(shape, i as u64 + 1));
    }
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(commit.ops.len(), MAX_OPS);
    assert!(matches!(ops(&commit)[0], op::Op::CreateDir(_)));
}

/// A file of 4,096 chunks, whose change record takes about 150 KiB.
fn large(parent: &str, name: &str) -> Shape {
    let chunk = proto::ChunkRef {
        hash: vec![7; 32],
        size: 4 << 20,
        encoding: 0,
    };
    Shape {
        content: Some(Content {
            chunks: vec![chunk; 4096],
            chunk_count: 4096,
            ..content(1)
        }),
        ..file(parent, name, 1)
    }
}

#[test]
fn a_commit_stops_before_its_change_records_outgrow_one_message() {
    let mut trees = Trees::new(ROOT);
    for i in 0..8 {
        trees.set_local(format!("f{i}"), local(large(ROOT, &format!("{i}")), i + 1));
    }
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(commit.ops.len(), 3);
}

#[test]
fn a_delete_too_large_for_one_commit_deletes_the_folder_contents_first() {
    let mut rows = vec![(
        "d",
        None,
        Some(dir(ROOT, "d")),
        Some(remote(dir(ROOT, "d"))),
    )];
    for id in ["f0", "f1", "f2", "f3", "f4"] {
        rows.push((id, None, Some(large("d", id)), Some(remote(large("d", id)))));
    }
    let mut trees = trees(&rows);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let ops = ops(&commit);
    assert_eq!(ops.len(), 3);
    assert!(
        ops.iter()
            .all(|op| op != &delete("d") && matches!(op, op::Op::Delete(_)))
    );
}

#[test]
fn a_commit_that_does_not_fit_is_planned_again_in_halves_and_grows_back() {
    let mut trees = Trees::new(ROOT);
    for i in 0..12 {
        trees.set_local(
            format!("f{i:02}"),
            local(file(ROOT, &format!("{i}"), 1), i + 1),
        );
    }
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let mut sizes = vec![build(&mut commits, &mut trees, &mut ids).unwrap().ops.len()];
    for _ in 0..2 {
        assert_eq!(
            commits.failed(error::Code::LimitExceeded),
            Ok(Failure::Split)
        );
        assert_eq!(commits.in_flight(), None);
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        assert_eq!(commit.device_seq, 1);
        sizes.push(commit.ops.len());
    }
    for _ in 0..2 {
        let applied = vec![outcome(op_result::Code::Applied); *sizes.last().unwrap()];
        acknowledge(&mut commits, &mut trees, applied);
        sizes.push(build(&mut commits, &mut trees, &mut ids).unwrap().ops.len());
    }
    assert_eq!(sizes, [12, 6, 3, 6, 3]);
}

#[test]
fn a_delete_that_does_not_fit_alone_deletes_the_folder_contents_first() {
    let mut trees = trees(&[
        (
            "d",
            None,
            Some(dir(ROOT, "d")),
            Some(remote(dir(ROOT, "d"))),
        ),
        (
            "e",
            None,
            Some(file("d", "e", 1)),
            Some(remote(file("d", "e", 1))),
        ),
        ("s", None, Some(dir("d", "s")), Some(remote(dir("d", "s")))),
        (
            "t",
            None,
            Some(file("s", "t", 1)),
            Some(remote(file("s", "t", 1))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&first), [delete("d")]);
    assert_eq!(
        commits.failed(error::Code::LimitExceeded),
        Ok(Failure::Split)
    );
    let mut second = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    second.sort_by_key(|op| format!("{op:?}"));
    assert_eq!(second, [delete("e"), delete("s")]);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    for (id, shape) in [
        ("e", file("d", "e", 1)),
        ("s", dir("d", "s")),
        ("t", file("s", "t", 1)),
    ] {
        assert_eq!(trees.synced().live(id), None, "{id}");
        trees.set_remote(id.into(), trashed(shape));
    }
    let third = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&third), [delete("d")]);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(trees.synced().live("d"), None);
    assert!(commits.problems().is_empty());
}

#[test]
fn an_op_that_does_not_fit_alone_and_cannot_be_split_is_a_problem() {
    let mut trees = trees(&[
        ("a", Some(file(ROOT, "x", 1)), None, None),
        (
            "e",
            None,
            Some(file(ROOT, "e", 1)),
            Some(remote(file(ROOT, "e", 1))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    commits.failed(error::Code::LimitExceeded).unwrap();
    for _ in 0..3 {
        let Some(commit) = build(&mut commits, &mut trees, &mut ids) else {
            break;
        };
        assert_eq!(commit.ops.len(), 1);
        commits.failed(error::Code::LimitExceeded).unwrap();
    }
    assert!(build(&mut commits, &mut trees, &mut ids).is_none());
    let problems: Vec<&str> = commits.problems().iter().map(String::as_str).collect();
    assert_eq!(problems, ["a", "e"]);
}

#[test]
fn results_set_s_to_what_was_sent() {
    let mut trees = trees(&[
        ("a", Some(file(ROOT, "x", 1)), None, None),
        (
            "m",
            Some(file(ROOT, "moved", 2)),
            Some(file(ROOT, "m", 2)),
            Some(remote(file(ROOT, "m", 2))),
        ),
        (
            "u",
            Some(file(ROOT, "u", 4)),
            Some(file(ROOT, "u", 3)),
            Some(remote(file(ROOT, "u", 3))),
        ),
        (
            "d",
            None,
            Some(dir(ROOT, "d")),
            Some(remote(dir(ROOT, "d"))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("a".into(), remote(file(ROOT, "x (conflict)", 1)));
    trees.set_remote("m".into(), remote(file(ROOT, "moved", 2)));
    trees.set_remote("u".into(), remote(file(ROOT, "u", 4)));
    trees.set_remote("d".into(), remote(dir(ROOT, "d")));
    use op_result::Code;
    acknowledge(
        &mut commits,
        &mut trees,
        vec![
            outcome(Code::AppliedRenamed),
            outcome(Code::Applied),
            outcome(Code::Applied),
            outcome(Code::Superseded),
        ],
    );
    assert_eq!(trees.synced().live("a"), Some(&file(ROOT, "x", 1)));
    assert_eq!(
        trees.class("a"),
        Class::Remote,
        "the conflict name reaches the disk"
    );
    assert_eq!(trees.synced().live("d"), None);
    assert_eq!(
        trees.class("d"),
        Class::Remote,
        "the folder the server kept comes back"
    );
    assert_eq!(trees.class("m"), Class::InSync);
    assert_eq!(trees.class("u"), Class::InSync);
    assert_eq!(commits.accepted(), 1);
    assert!(commits.problems().is_empty());
}

#[test]
fn a_folder_merged_into_one_l_holds_elsewhere_is_removed_once_its_files_follow_into_it() {
    let mut trees = trees(&[
        ("a", Some(dir(ROOT, "d")), None, None),
        (
            "b",
            Some(dir(ROOT, "e")),
            Some(dir(ROOT, "e")),
            Some(remote(dir(ROOT, "e"))),
        ),
        ("c", Some(file("a", "f", 1)), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    use op_result::Code;
    let merged = OpResult {
        node_id: "b".into(),
        ..outcome(Code::MergedInto)
    };
    acknowledge(
        &mut commits,
        &mut trees,
        vec![merged, outcome(Code::Applied)],
    );
    trees.set_remote("c".into(), remote(file("b", "f", 1)));
    assert_eq!(trees.synced().live("a"), None);
    assert_eq!(trees.synced().live("c"), Some(&file("b", "f", 1)));
    trees.set_local("g".into(), local(file("a", "g", 2), 9));
    let later = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&later)[..], [op::Op::CreateFile(c)] if c.parent_id == "b"));
    acknowledge(&mut commits, &mut trees, vec![outcome(Code::Applied)]);
    trees.set_remote("g".into(), remote(file("b", "g", 2)));
    let reverted: Vec<&str> = trees.reverted().iter().map(String::as_str).collect();
    assert_eq!(reverted, ["a", "c", "g"]);
    let mut effects = Effects::default();
    while let Some(record) = effects.plan(&trees, &mut ids, NOW).cloned() {
        effects.done(&mut trees, &record.effect_id, None).unwrap();
        trees.settle();
    }
    assert_eq!(trees.local().get("a"), None);
    assert_eq!(trees.local().get("c").unwrap().shape, file("b", "f", 1));
    assert_eq!(trees.local().get("g").unwrap().shape, file("b", "g", 2));
    let b = trees.local().get("b").unwrap();
    assert_eq!((&b.shape, b.identity.ino), (&dir(ROOT, "e"), 2));
    assert!(trees.reverted().is_empty());
    assert!(trees.settle().is_empty());
}

#[test]
fn a_conflict_copy_takes_the_local_file_and_the_original_is_downloaded_again() {
    let mut trees = trees(&[(
        "f",
        Some(file(ROOT, "f", 2)),
        Some(file(ROOT, "f", 1)),
        Some(remote(file(ROOT, "f", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("f".into(), remote(file(ROOT, "f", 3)));
    trees.set_remote("y".into(), remote(file(ROOT, "f (conflict)", 2)));
    let copy = OpResult {
        node_id: "y".into(),
        ..outcome(op_result::Code::ConflictCopy)
    };
    acknowledge(&mut commits, &mut trees, vec![copy]);
    assert_eq!(trees.local().get("y").unwrap().identity.ino, 1);
    assert_eq!(trees.synced().live("y"), Some(&file(ROOT, "f", 2)));
    assert_eq!(trees.class("y"), Class::Remote);
    assert_eq!(
        (trees.local().get("f"), trees.synced().live("f")),
        (None, None)
    );
    assert_eq!(trees.class("f"), Class::Remote);
}

#[test]
fn a_rejected_op_marks_a_problem_but_a_rejected_cycle_moves_the_node_back() {
    let mut trees = trees(&[
        (
            "p",
            Some(dir("q", "p")),
            Some(dir(ROOT, "p")),
            Some(remote(dir(ROOT, "p"))),
        ),
        (
            "q",
            Some(dir(ROOT, "q")),
            Some(dir(ROOT, "q")),
            Some(remote(dir(ROOT, "q"))),
        ),
        (
            "u",
            Some(file(ROOT, "u", 2)),
            Some(file(ROOT, "u", 1)),
            Some(remote(file(ROOT, "u", 1))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("p", "q", "p"), update("u", 2)]);
    let rejected = |reason: &str| OpResult {
        reject_reason: reason.into(),
        ..outcome(op_result::Code::Rejected)
    };
    acknowledge(
        &mut commits,
        &mut trees,
        vec![rejected("CYCLE"), rejected("NOT_FOUND")],
    );
    assert_eq!(trees.synced().live("p"), Some(&dir(ROOT, "p")));
    assert!(trees.reverted().contains("p"));
    assert_eq!(commits.problems().iter().collect::<Vec<_>>(), ["u"]);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert!(commits.clear_problem("u"));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("u", 2)]);
    moves_back(&mut trees, &mut ids, "p");
}

#[test]
fn a_result_is_acknowledged_only_once_r_holds_its_records() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(!commits.acknowledge(&mut trees, 9), "no result yet");
    commits
        .received(result(5, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(!commits.acknowledge(&mut trees, 4));
    assert_eq!(trees.synced().live("a"), None);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(
        commits.connected(&welcome(1)),
        Reconnect::Idle,
        "accepted, nothing to resend"
    );
    assert!(!commits.abandon(), "an accepted commit is never forgotten");
    assert!(commits.acknowledge(&mut trees, 5));
    assert_eq!(trees.synced().live("a"), Some(&file(ROOT, "x", 1)));
    assert_eq!((commits.accepted(), commits.in_flight()), (1, None));
}

fn welcome(device_seq: u64) -> Welcome {
    Welcome {
        history_id: HISTORY.into(),
        device_seq,
        ..Default::default()
    }
}

#[test]
fn the_device_slot_is_kept_by_a_retry_and_reused_after_a_refusal() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        commits.failed(error::Code::MissingChunks),
        Ok(Failure::Resend)
    );
    assert_eq!(commits.in_flight().unwrap().commit, first);
    assert_eq!(
        commits.failed(error::Code::AheadOfServer),
        Ok(Failure::Resync)
    );
    assert_eq!(commits.in_flight(), None);
    let second = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_ne!(second.commit_id, first.commit_id);
    assert_eq!((second.device_seq, second.ops.clone()), (1, first.ops));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    trees.set_local("b".into(), local(file(ROOT, "y", 2), 9));
    let third = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(third.device_seq, 2);
}

#[test]
fn errors_forget_the_commit_only_when_nothing_was_applied() {
    use error::Code;
    let cases = [
        (Code::MissingChunks, Failure::Resend, true),
        (Code::RetryLater, Failure::Resend, true),
        (Code::QuotaExceeded, Failure::Resend, true),
        (Code::Unauthenticated, Failure::Resend, true),
        (Code::Internal, Failure::Resend, true),
        (Code::InvalidArgument, Failure::Abandoned, false),
        (Code::HeadMoved, Failure::Abandoned, false),
        (Code::UnsupportedVersion, Failure::Abandoned, false),
        (Code::AheadOfServer, Failure::Resync, false),
        (Code::ReadOnly, Failure::Halted(Halt::ReadOnly), false),
        (Code::Forbidden, Failure::Halted(Halt::ReadOnly), false),
        (Code::HistoryMismatch, Failure::Halted(Halt::Recovery), true),
        (
            Code::StaleDeviceState,
            Failure::Halted(Halt::Recovery),
            true,
        ),
    ];
    for (code, failure, kept) in cases {
        let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        build(&mut commits, &mut trees, &mut ids).unwrap();
        assert_eq!(commits.failed(code), Ok(failure), "{code:?}");
        assert_eq!(commits.in_flight().is_some(), kept, "{code:?}");
        let halted = matches!(failure, Failure::Halted(_));
        assert_eq!(
            build(&mut commits, &mut trees, &mut ids).is_some(),
            !kept && !halted,
            "{code:?}"
        );
    }
}

#[test]
fn reconnecting_compares_the_device_counter_and_the_history() {
    let fresh = || {
        let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        build(&mut commits, &mut trees, &mut ids).unwrap();
        commits
    };
    assert_eq!(Commits::default().connected(&welcome(0)), Reconnect::Idle);
    assert_eq!(
        fresh().connected(&welcome(0)),
        Reconnect::Resend,
        "not accepted"
    );
    assert_eq!(
        fresh().connected(&welcome(1)),
        Reconnect::Resend,
        "accepted, response lost"
    );
    for w in [2, 5] {
        let mut commits = fresh();
        assert_eq!(commits.connected(&welcome(w)), Reconnect::Recovery);
        assert_eq!(commits.halted(), Some(Halt::Recovery));
        assert!(commits.in_flight().is_some(), "kept as evidence");
        assert_eq!(commits.connected(&welcome(1)), Reconnect::Recovery);
    }
    let mut idle = Commits::default();
    assert_eq!(
        idle.connected(&welcome(1)),
        Reconnect::Recovery,
        "a store older than its commits"
    );
    let other = Welcome {
        history_id: "rewound".into(),
        ..welcome(0)
    };
    assert_eq!(fresh().connected(&other), Reconnect::Recovery);

    let mut commits = fresh();
    let read_only = Welcome {
        read_only: true,
        ..welcome(1)
    };
    assert_eq!(commits.connected(&read_only), Reconnect::Idle);
    assert_eq!(commits.halted(), Some(Halt::ReadOnly));
    assert_eq!(commits.connected(&welcome(1)), Reconnect::Resend);
    assert_eq!(commits.halted(), None);
}
