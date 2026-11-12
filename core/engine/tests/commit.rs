// SPDX-License-Identifier: Apache-2.0

//! Building commits from local changes and acknowledging their results (docs/design/sync-engine.md
//! sections 2.4, 2.6, 2.7, 2.8 and 4.2, and RFC 0002), on hand-built trees and results.

use koidrive_engine::classify::{Class, Origin, Trees};
use koidrive_engine::commit::{
    Commits, Failure, Halt, Lift, LiftError, MAX_COMMIT_BYTES, MAX_OPS, Reconnect,
};
use koidrive_engine::effects::{Effect, EffectError, Effects, Location};
use koidrive_engine::ids::IdGen;
use koidrive_engine::names::{MAX_NAME_BYTES, nfc, normalize_name};
use koidrive_engine::question::Answer;
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};
use koidrive_proto::v1::{self as proto, CommitResult, OpResult, Welcome, error, op, op_result};
use prost::Message;

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

/// R's node with its position from the record at `rev`.
fn remote_at(shape: Shape, rev: u64) -> RemoteNode {
    RemoteNode {
        meta_rev: rev,
        ..remote(shape)
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
            birth_ns: Some(0),
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
        ..Default::default()
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

/// Two files that swapped names.
fn swapped() -> Vec<Row> {
    vec![
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
    ]
}

/// Neither move of a swap is valid before the other, so one node moves to a pending name first and to its own name
/// last, in one commit, and each node keeps its node and its exact name.
#[test]
fn a_name_swap_goes_through_a_pending_name() {
    let mut trees = trees(&swapped());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [
            mv("x", ROOT, "b (pending)"),
            mv("y", ROOT, "a"),
            mv("x", ROOT, "b")
        ]
    );
}

/// A swap that the rest of the commit leaves no room for waits for the next commit, so its pending name never
/// reaches the server.
#[test]
fn a_name_swap_that_does_not_fit_the_commit_waits_for_the_next_one() {
    let mut rows = swapped();
    rows.push(("n", Some(file(ROOT, "new", 3)), None, None));
    let mut trees = trees(&rows);
    let state = koidrive_engine::state::CommitsState {
        cap: Some(2),
        ..Commits::default().state()
    };
    let (mut commits, mut ids) = (Commits::from_state(state).unwrap(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(ops(&commit).as_slice(), [op::Op::CreateFile(create)] if create.name == "new"),
        "{:?}",
        ops(&commit)
    );
}

/// A move onto a name that a node keeps is no cycle, and is sent for the server to resolve (R1).
#[test]
fn a_move_onto_a_name_another_node_keeps_is_sent_for_the_server_to_resolve() {
    let mut trees = trees(&[
        (
            "x",
            Some(file(ROOT, "b", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        synced("y", file(ROOT, "b", 2)),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("x", ROOT, "b")]);
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
            birth_ns: Some(0),
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
    trees.settle();
    assert!(trees.work().is_empty());
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
fn a_folder_created_where_a_moving_file_still_is_takes_a_pending_name_first() {
    let mut trees = trees(&[
        (
            "h",
            Some(file("n", "ß", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
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
    assert_eq!(ops(&first), [create, mv("h", "n", "ß")]);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    trees.set_remote("n".into(), remote(dir(ROOT, "a (pending)")));
    trees.set_remote("h".into(), remote(file("n", "ß", 1)));
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

/// Another device created a file at x first, so the server gives this device's new file another
/// name, and the two are a conflict. A build before the local file takes the server's name, as
/// while its move waits for a retry, keeps the question: the name L holds is not the person's move.
#[test]
fn a_conflict_about_a_renamed_create_stays_open_while_the_local_file_waits_for_its_name() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("o".into(), remote(file(ROOT, "x", 2)));
    trees.set_remote("a".into(), remote(file(ROOT, "x (conflict)", 1)));
    let renamed = OpResult {
        name: "x (conflict)".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    assert!(trees.reverted().contains("a"));
    let asked = commits.questions().to_vec();
    assert_eq!(asked.len(), 1);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(commits.questions(), asked);
}

/// Another device renames and rewrites n, and the disk takes both, so L and R agree. S cannot take
/// the new name yet, because the folder p it holds there is parked on its way to q. S still takes
/// the content both sides hold, so the other device's next edit is not taken for a concurrent one
/// and forked: once p has moved, S takes the name and the edit is a remote change.
#[test]
fn a_download_s_cannot_place_yet_is_not_forked_by_the_next_remote_edit() {
    let mut trees = trees(&[
        (
            "p",
            Some(dir(ROOT, "y")),
            Some(dir(ROOT, "y")),
            Some(remote(dir(ROOT, "q"))),
        ),
        (
            "n",
            Some(file(ROOT, "y", 2)),
            Some(file(ROOT, "x", 1)),
            Some(remote(file(ROOT, "y", 2))),
        ),
    ]);
    trees.set_local(
        "p".into(),
        LocalNode {
            parked: true,
            ..local(dir(ROOT, "y"), 1)
        },
    );
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "x", 2)));
    trees.set_remote("n".into(), remote(file(ROOT, "y", 3)));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    trees.set_local("p".into(), local(dir(ROOT, "q"), 1));
    trees.settle();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "y", 2)));
    assert_eq!(trees.class("n"), Class::Remote);
}

/// The user renames folder f to a and back to f. A commit built before the second rename was seen
/// moves f to a, and the server renames it to "a (conflict)" because another device holds a, so
/// the node is reverted. The rename back to f is the user's last word: it is sent, and nothing
/// moves the folder to the conflict name.
#[test]
fn a_folder_renamed_back_after_its_move_was_renamed_by_the_server_keeps_the_users_name() {
    for back_before_the_result in [false, true] {
        let mut trees = trees(&[(
            "n",
            Some(dir(ROOT, "a")),
            Some(dir(ROOT, "f")),
            Some(remote(dir(ROOT, "f"))),
        )]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        assert_eq!(ops(&commit), [mv("n", ROOT, "a")]);
        if back_before_the_result {
            trees.set_local("n".into(), local(dir(ROOT, "f"), 1));
        }
        trees.set_remote("n".into(), remote(dir(ROOT, "a (conflict)")));
        let renamed = OpResult {
            name: "a (conflict)".into(),
            ..outcome(op_result::Code::AppliedRenamed)
        };
        acknowledge(&mut commits, &mut trees, vec![renamed]);
        trees.set_local("n".into(), local(dir(ROOT, "f"), 1));
        trees.settle();
        assert!(!trees.reverted().contains("n"), "{back_before_the_result}");
        let mut effects = Effects::default();
        let planned = (effects.plan(&trees, &mut ids, NOW)).map(|record| record.effect.clone());
        assert_eq!(planned, None, "{back_before_the_result}");
        let commit = build(&mut commits, &mut trees, &mut ids);
        assert_eq!(
            commit.map(|commit| ops(&commit)),
            Some(vec![mv("n", ROOT, "f")]),
            "{back_before_the_result}"
        );
    }
}

/// The server lost folder albums, which is parked here, and another device deleted one in it.
/// Once one is removed, albums is not removed too: the commit layer forgets its S first, then the
/// effects move it out of the parked directory, and it is created again.
#[test]
fn a_parked_folder_the_server_lost_is_created_again_after_a_delete_empties_it() {
    let mut trees = trees(&[
        (
            "albums",
            Some(dir(ROOT, "albums")),
            Some(dir(ROOT, "albums")),
            None,
        ),
        (
            "one",
            Some(file("albums", "one", 1)),
            Some(file("albums", "one", 1)),
            Some(trashed(file(ROOT, "one", 1))),
        ),
    ]);
    trees.set_local(
        "albums".into(),
        LocalNode {
            parked: true,
            ..local(dir(ROOT, "albums"), 1)
        },
    );
    trees.forget_lost(Vec::new(), Vec::new(), 0, 0);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let removed = effects.plan(&trees, &mut ids, NOW).cloned().unwrap();
    assert_eq!(removed.node, "one");
    assert!(matches!(removed.effect, Effect::RemoveFile { .. }));
    effects.done(&mut trees, &removed.effect_id, None).unwrap();
    trees.settle();
    assert!(effects.plan(&trees, &mut ids, NOW).is_none());
    let mut commits = Commits::default();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    trees.settle();
    let unparked = effects.plan(&trees, &mut ids, NOW).cloned().unwrap();
    assert_eq!(unparked.node, "albums");
    assert!(matches!(unparked.effect, Effect::Move { .. }));
    effects.done(&mut trees, &unparked.effect_id, None).unwrap();
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateDir(c)] if c.node_id == "albums"));
}

/// Trees with `rows`, synced at seq 1. A commit of this device at slot 1 creates `pad` and is
/// applied at seq 2. The user then deletes `rows` here, and the commit at slot 2 that deletes them
/// is applied at seq 5, which trashes each row. A server restored from a backup taken at seq 2
/// counts only slot 1, so every seq up to 2 is from before the loss. Returns the delete's ops.
fn deleted_at_5_then_lost(rows: &[Row]) -> (Commits, Trees, IdGen, Vec<op::Op>) {
    let mut trees = trees(rows);
    trees.set_local("pad".into(), local(file(ROOT, "pad", 9), 99));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let pad = RemoteNode {
        content_rev: 2,
        meta_rev: 2,
        ..remote(file(ROOT, "pad", 9))
    };
    trees.set_remote("pad".into(), pad);
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    for (id, ..) in rows.iter().rev() {
        trees.remove_local(id);
    }
    trees.settle();
    let sent = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    for (id, _, shape, _) in rows {
        let trashed = RemoteNode {
            meta_rev: 5,
            trash_group: "5:0".into(),
            ..trashed(shape.clone().unwrap())
        };
        trees.set_remote((*id).into(), trashed);
    }
    let applied = vec![outcome(op_result::Code::Applied); sent.len()];
    commits.received(result(5, applied)).unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle();
    assert_eq!(trees.work(), vec![]);
    assert_eq!(commits.held_through(), 5);
    let welcome = Welcome {
        history_id: HISTORY.into(),
        device_seq: 1,
        ..Default::default()
    };
    assert_eq!(commits.connected(&welcome), Reconnect::AheadOfServer);
    assert_eq!(commits.held_through(), 2);
    (commits, trees, ids, sent)
}

/// The server lost the commit that deleted folder d with file a in it, and a bootstrap lists both
/// live as they were deleted, at seq 1. The delete is sent again, not taken for a remote create.
#[test]
fn a_delete_the_server_lost_is_sent_again() {
    let rows = [synced("d", dir(ROOT, "d")), synced("a", file("d", "a", 1))];
    let (mut commits, mut trees, mut ids, sent) = deleted_at_5_then_lost(&rows);
    assert_eq!(sent, [delete("d")]);
    let trashed = trees.trashed_here();
    trees.set_remote("d".into(), remote(dir(ROOT, "d")));
    trees.set_remote("a".into(), remote(file("d", "a", 1)));
    trees.forget_lost(
        trashed,
        Vec::new(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert_eq!(
        (trees.class("d"), trees.class("a")),
        (Class::Local, Class::Local)
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete("d")]);
}

/// Of three files this device deleted at seq 5, a bootstrap from a server restored from seq 2 lists
/// `same` as it was deleted, `edited` with other content, and `restored` as it was deleted but
/// restored after the loss at seq 3, a seq below the lost delete's that the lost commits had. Only
/// `same` is deleted again; the other two are downloaded.
#[test]
fn a_delete_the_server_lost_is_sent_again_only_for_what_it_saw() {
    let rows = [
        synced("same", file(ROOT, "same", 1)),
        synced("edited", file(ROOT, "edited", 1)),
        synced("restored", file(ROOT, "restored", 1)),
    ];
    let (mut commits, mut trees, mut ids, _) = deleted_at_5_then_lost(&rows);
    let trashed = trees.trashed_here();
    assert_eq!(trashed.len(), 3);
    trees.set_remote("same".into(), remote(file(ROOT, "same", 1)));
    let edited = RemoteNode {
        content_rev: 2,
        meta_rev: 2,
        ..remote(file(ROOT, "edited", 2))
    };
    trees.set_remote("edited".into(), edited);
    let restored = RemoteNode {
        meta_rev: 3,
        ..remote(file(ROOT, "restored", 1))
    };
    trees.set_remote("restored".into(), restored);
    trees.forget_lost(
        trashed,
        Vec::new(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let classes = ["same", "edited", "restored"].map(|id| (id, trees.class(id)));
    assert_eq!(
        classes,
        [
            ("same", Class::Local),
            ("edited", Class::Remote),
            ("restored", Class::Remote)
        ]
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete("same")]);
}

/// File a synced with content 1 at seq 1. A commit of this device at slot 1 creates `pad` and is
/// applied at seq 2, and the commit at slot 2 that sets a's content to 2 is applied at seq 5. A
/// server restored from a backup taken at seq 2 counts only slot 1.
fn edited_at_5_then_lost() -> (Commits, Trees, IdGen) {
    let mut trees = trees(&[synced("a", file(ROOT, "a", 1))]);
    trees.set_local("pad".into(), local(file(ROOT, "pad", 9), 99));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let pad = RemoteNode {
        content_rev: 2,
        meta_rev: 2,
        ..remote(file(ROOT, "pad", 9))
    };
    trees.set_remote("pad".into(), pad);
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.set_local("a".into(), local(file(ROOT, "a", 2), 1));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("a", 2)]);
    let edited = RemoteNode {
        content_rev: 5,
        ..remote(file(ROOT, "a", 2))
    };
    trees.set_remote("a".into(), edited);
    commits
        .received(result(5, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle();
    assert_eq!(trees.work(), vec![]);
    let welcome = Welcome {
        history_id: HISTORY.into(),
        device_seq: 1,
        ..Default::default()
    };
    assert_eq!(commits.connected(&welcome), Reconnect::AheadOfServer);
    (commits, trees, ids)
}

/// A bootstrap from the server that lost the edit lists a with content 1 of seq 1, which the server
/// held before the loss. The edit is sent again as an edit, not kept beside a as a conflict copy.
#[test]
fn an_edit_the_server_lost_is_sent_again() {
    let (mut commits, mut trees, mut ids) = edited_at_5_then_lost();
    let edited = trees.edited_here();
    assert_eq!(edited, ["a"]);
    trees.set_remote("a".into(), remote(file(ROOT, "a", 1)));
    trees.forget_lost(
        Vec::new(),
        edited,
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert_eq!(trees.class("a"), Class::Local);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("a", 2)]);
}

/// As above, but another device wrote content 3 into a after the loss, at seq 3, a seq the lost
/// commits had. The edit is not sent over it: a stays a concurrent change, as before.
#[test]
fn an_edit_the_server_lost_is_not_sent_again_over_a_later_edit() {
    let (commits, mut trees, _) = edited_at_5_then_lost();
    let edited = trees.edited_here();
    let later = RemoteNode {
        content_rev: 3,
        ..remote(file(ROOT, "a", 3))
    };
    trees.set_remote("a".into(), later);
    trees.forget_lost(
        Vec::new(),
        edited,
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert_eq!(trees.class("a"), Class::Concurrent);
}

/// As above, but another device that never saw the edit trashed a after the loss, at seq 3. An
/// edit beats a delete (R8), so the edit is sent again and revives a.
#[test]
fn an_edit_the_server_lost_is_sent_again_over_a_later_delete() {
    let (mut commits, mut trees, mut ids) = edited_at_5_then_lost();
    let edited = trees.edited_here();
    let deleted = RemoteNode {
        meta_rev: 3,
        ..trashed(file(ROOT, "a", 1))
    };
    trees.set_remote("a".into(), deleted);
    trees.forget_lost(
        Vec::new(),
        edited,
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("a", 2)]);
}

#[test]
fn a_local_edit_on_a_reverted_file_is_kept_on_disk_and_uploaded() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let renamed = OpResult {
        name: "x (conflict)".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    trees.set_remote("a".into(), remote(file(ROOT, "x (conflict)", 1)));
    trees.set_local("a".into(), local(file(ROOT, "x", 2), 1));
    let mut effects = Effects::default();
    let record = effects.plan(&trees, &mut ids, NOW).unwrap().clone();
    assert!(
        matches!(record.effect, Effect::Move { .. }),
        "{:?}",
        record.effect
    );
    effects.done(&mut trees, &record.effect_id, None).unwrap();
    trees.settle();
    assert_eq!(
        trees.local().get("a").unwrap().shape,
        file(ROOT, "x (conflict)", 2)
    );
    assert!(trees.reverted().is_empty());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("a", 2)]);
}

/// Folder a is merged into b, which L holds elsewhere, so a is reverted and what is created in it
/// goes into b. The user then renames a, which ends the revert: a is created on its own, and a
/// file created in it afterwards goes into a, not into b.
#[test]
fn a_merged_folder_the_user_renames_takes_what_is_created_in_it() {
    let mut trees = trees(&[
        ("a", Some(dir(ROOT, "d")), None, None),
        synced("b", dir(ROOT, "e")),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&first)[..], [op::Op::CreateDir(c)] if c.node_id == "a"));
    trees.set_remote("b".into(), remote(dir(ROOT, "d")));
    let merged = OpResult {
        node_id: "b".into(),
        ..outcome(op_result::Code::MergedInto)
    };
    acknowledge(&mut commits, &mut trees, vec![merged]);
    assert!(trees.reverted().contains("a"));
    trees.set_local("a".into(), local(dir(ROOT, "user"), 1));
    trees.settle();
    let next = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&next)[..], [op::Op::CreateDir(c)] if c.node_id == "a" && c.name == "user")
    );
    trees.set_remote("a".into(), remote(dir(ROOT, "user")));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    trees.set_local("g".into(), local(file("a", "new", 2), 9));
    trees.settle();
    let child = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&child)[..], [op::Op::CreateFile(c)] if c.parent_id == "a"),
        "{:?}",
        ops(&child)
    );
}

/// The server renames a create or move of n to "é (conflict)". Before the result arrives, a scan
/// reports n under the decomposed spelling of the name the commit sent. That is the same name, not
/// a user move, so n stays reverted and the refused name is not sent again.
#[test]
fn a_scan_that_respells_the_sent_name_keeps_a_renamed_result_reverted() {
    for is_create in [true, false] {
        let before = (!is_create).then(|| dir(ROOT, "old"));
        let mut trees = trees(&[(
            "n",
            Some(dir(ROOT, "é")),
            before.clone(),
            before.map(remote),
        )]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        build(&mut commits, &mut trees, &mut ids).unwrap();
        trees.set_local("n".into(), local(dir(ROOT, "e\u{301}"), 1));
        trees.set_remote("n".into(), remote(dir(ROOT, "é (conflict)")));
        let renamed = OpResult {
            name: "é (conflict)".into(),
            ..outcome(op_result::Code::AppliedRenamed)
        };
        acknowledge(&mut commits, &mut trees, vec![renamed]);
        assert!(trees.reverted().contains("n"), "create={is_create}");
        let retry = build(&mut commits, &mut trees, &mut ids);
        assert!(retry.is_none(), "create={is_create}, retry={retry:?}");
    }
}

#[test]
fn a_reverted_file_deleted_remotely_keeps_its_unuploaded_edit_as_a_new_file() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let renamed = OpResult {
        name: "x (conflict)".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    trees.set_local("a".into(), local(file(ROOT, "x", 2), 1));
    trees.set_remote("a".into(), trashed(file(ROOT, "x (conflict)", 1)));
    trees.settle();
    assert!(trees.reverted().contains("a"));
    let mut effects = Effects::default();
    assert_eq!(effects.plan(&trees, &mut ids, NOW), None);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let [op::Op::CreateFile(create)] = &ops(&commit)[..] else {
        panic!("{:?}", ops(&commit));
    };
    assert_eq!(
        (create.parent_id.as_str(), create.name.as_str()),
        (ROOT, "x")
    );
    assert_eq!(create.content, Some(content(2)));
    assert!(trees.local().get("a").is_none());
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    trees.set_remote(create.node_id.clone(), remote(file(ROOT, "x", 2)));
    trees.settle();
    assert_eq!(trees.work(), vec![]);
    assert!(trees.reverted().is_empty());
}

#[test]
fn a_renamed_file_the_user_deletes_before_it_moves_is_deleted() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let renamed = OpResult {
        name: "x (conflict)".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    trees.set_remote("a".into(), remote(file(ROOT, "x (conflict)", 1)));
    trees.remove_local("a");
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        ops(&commit),
        [op::Op::Delete(proto::Delete {
            node_id: "a".into()
        })]
    );
}

#[test]
fn two_reverted_files_that_block_each_other_are_placed_through_parking() {
    let mut trees = trees(&[
        ("a", Some(file(ROOT, "x", 1)), None, None),
        ("b", Some(file(ROOT, "y", 2)), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let renamed = |name: &str| OpResult {
        name: name.into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(
        &mut commits,
        &mut trees,
        vec![renamed("x (conflict)"), renamed("y (conflict)")],
    );
    trees.set_remote("a".into(), remote(file(ROOT, "y", 1)));
    trees.set_remote("b".into(), remote(file(ROOT, "x", 2)));
    trees.settle();
    let mut effects = Effects::default();
    for _ in 0..8 {
        let Some(record) = effects.plan(&trees, &mut ids, NOW).cloned() else {
            break;
        };
        effects.done(&mut trees, &record.effect_id, None).unwrap();
        trees.settle();
    }
    assert_eq!(trees.local().get("a").unwrap().shape, file(ROOT, "y", 1));
    assert_eq!(trees.local().get("b").unwrap().shape, file(ROOT, "x", 2));
    assert!(trees.reverted().is_empty());
}

#[test]
fn a_folder_deleted_around_a_parked_node_is_brought_back_for_it() {
    for name in ["f", "F"] {
        let mut trees = trees(&[
            (
                "d",
                None,
                Some(dir(ROOT, "d")),
                Some(remote(dir(ROOT, "d"))),
            ),
            (
                "f",
                Some(file("d", "F", 1)),
                Some(file("d", "F", 1)),
                Some(remote(file("d", name, 1))),
            ),
        ]);
        trees.set_local(
            "f".into(),
            LocalNode {
                parked: true,
                ..local(file("d", "F", 1), 2)
            },
        );
        trees.settle();
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        assert_eq!(build(&mut commits, &mut trees, &mut ids), None, "{name}");
        assert!(trees.reverted().contains("d"), "{name}");
        let mut effects = Effects::default();
        for ino in 10..18 {
            trees.settle();
            let Some(record) = effects.plan(&trees, &mut ids, NOW).cloned() else {
                break;
            };
            let identity = FileIdentity {
                dev: 1,
                ino,
                birth_ns: Some(0),
            };
            effects
                .done(&mut trees, &record.effect_id, Some(identity))
                .unwrap();
        }
        trees.settle();
        assert_eq!(trees.work(), vec![], "{name}");
        assert_eq!(trees.local().get("d").unwrap().shape, dir(ROOT, "d"));
        let f = trees.local().get("f").unwrap();
        assert_eq!((&f.shape, f.parked), (&file("d", name, 1), false));
        assert_eq!(build(&mut commits, &mut trees, &mut ids), None, "{name}");
    }
}

#[test]
fn a_parked_file_rekeyed_for_its_edit_is_moved_out_from_where_it_was_parked() {
    let mut trees = trees(&[
        (
            "a",
            Some(file(ROOT, "x", 2)),
            Some(file(ROOT, "x (conflict)", 1)),
            Some(remote(file(ROOT, "y", 1))),
        ),
        (
            "c",
            Some(file(ROOT, "y", 4)),
            Some(file(ROOT, "y (conflict)", 4)),
            Some(remote(file(ROOT, "x", 4))),
        ),
    ]);
    trees.revert("a".into());
    trees.revert("c".into());
    let (mut commits, mut ids, mut effects) =
        (Commits::default(), IdGen::new(1), Effects::default());
    let parking = effects.plan(&trees, &mut ids, NOW).unwrap().clone();
    assert!(
        matches!(&parking.effect, Effect::Move { to: Location::Parked(id), .. } if id == "a"),
        "{parking:?}"
    );
    effects.done(&mut trees, &parking.effect_id, None).unwrap();
    trees.set_remote("a".into(), trashed(file(ROOT, "y", 1)));
    trees.set_remote("c".into(), trashed(file(ROOT, "x", 4)));
    trees.set_local("b".into(), local(file(ROOT, "x", 3), 3));
    trees.settle();
    let remove = effects.plan(&trees, &mut ids, NOW).unwrap().clone();
    assert_eq!(remove.node, "c");
    effects.done(&mut trees, &remove.effect_id, None).unwrap();
    trees.settle();
    assert!(effects.plan(&trees, &mut ids, NOW).is_none());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(commit.ops.len(), 2);
    let rekeyed = trees
        .local()
        .iter()
        .find(|(_, node)| node.parked)
        .unwrap()
        .0
        .clone();
    assert_ne!(rekeyed, "a");
    assert_eq!(trees.parked_name(&rekeyed), "a");
    trees.set_remote(rekeyed.clone(), remote(file(ROOT, "x", 2)));
    trees.set_remote("b".into(), remote(file(ROOT, "x (new conflict)", 3)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![
            outcome(op_result::Code::Applied),
            OpResult {
                name: "x (new conflict)".into(),
                ..outcome(op_result::Code::AppliedRenamed)
            },
        ],
    );
    let mut unparked = false;
    for ino in 10..16 {
        trees.settle();
        let Some(record) = effects.plan(&trees, &mut ids, NOW).cloned() else {
            break;
        };
        if record.node == rekeyed {
            let Effect::Move { from, .. } = &record.effect else {
                panic!("{record:?}");
            };
            assert_eq!(from, &Location::Parked("a".into()));
            unparked = true;
        }
        let identity = FileIdentity {
            dev: 1,
            ino,
            birth_ns: Some(0),
        };
        effects
            .done(&mut trees, &record.effect_id, Some(identity))
            .unwrap();
    }
    assert!(unparked);
    trees.settle();
    assert_eq!(trees.work(), vec![]);
    let node = trees.local().get(&rekeyed).unwrap();
    assert_eq!((&node.shape, node.parked), (&file(ROOT, "x", 2), false));
    assert_eq!(trees.parked_name(&rekeyed), rekeyed);
    let b = &trees.local().get("b").unwrap().shape;
    assert_eq!(b, &file(ROOT, "x (new conflict)", 3));
}

/// The server renamed the create of x to y, and the file is reverted until the effects move it
/// there. Then the server loses the create. Once S forgets the file, it is no longer reverted, so
/// it is created again under its own ID at x, instead of as a new node beside the one another
/// device creates again (section 4.4).
#[test]
fn a_reverted_file_the_server_lost_is_created_again_under_its_id() {
    let renamed = file(ROOT, "y", 1);
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "x", 1)),
        Some(renamed.clone()),
        Some(remote(renamed)),
    )]);
    trees.revert("h".into());
    trees.remove_remote("h");
    trees.forget_lost(Vec::new(), Vec::new(), 0, 0);
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "h" && c.name == "x"),
        "{:?}",
        ops(&commit)
    );
}

#[test]
fn a_reverted_file_whose_s_update_waits_is_created_again_under_its_id_once_the_server_lost_it() {
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "y", 9)),
        Some(file(ROOT, "y", 9)),
        Some(trashed(file(ROOT, "y", 9))),
    )]);
    trees.set_local("n".into(), local(file(ROOT, "x", 1), 2));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("n".into(), remote(file(ROOT, "y", 1)));
    let renamed = OpResult {
        name: "y".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    acknowledge(&mut commits, &mut trees, vec![renamed]);
    assert!(trees.waiting().contains_key("n"), "S still has h at y");
    assert!(trees.reverted().contains("n"));
    trees.remove_remote("n");
    trees.forget_lost(Vec::new(), Vec::new(), 0, 0);
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n" && c.name == "x"),
        "{:?}",
        ops(&commit)
    );
}

#[test]
fn a_file_whose_s_update_waits_is_removed_or_uploaded_anew_after_a_remote_delete() {
    for (edit, parked) in [(None, false), (None, true), (Some(2), false)] {
        let mut trees = trees(&[synced("h", file(ROOT, "x", 9))]);
        trees.set_remote("h".into(), remote(file(ROOT, "y", 9)));
        trees.set_local(
            "h".into(),
            LocalNode {
                parked: true,
                ..local(file(ROOT, "x", 9), 1)
            },
        );
        trees.set_local("n".into(), local(file(ROOT, "x", 1), 2));
        trees.settle();
        let (mut commits, mut ids, mut effects) =
            (Commits::default(), IdGen::new(1), Effects::default());
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
        trees.set_remote("n".into(), remote(file(ROOT, "x", 1)));
        acknowledge(
            &mut commits,
            &mut trees,
            vec![outcome(op_result::Code::Applied)],
        );
        assert!(trees.waiting().contains_key("n"), "S still has h at x");
        if let Some(digest) = edit {
            trees.set_local("n".into(), local(file(ROOT, "x", digest), 3));
        }
        if parked {
            trees.set_local(
                "n".into(),
                LocalNode {
                    parked: true,
                    ..local(file(ROOT, "x", 1), 2)
                },
            );
            trees.set_local("b".into(), local(file(ROOT, "x", 5), 4));
        }
        trees.set_remote("n".into(), trashed(file(ROOT, "x", 1)));
        trees.settle();
        let mut removed = false;
        for _ in 0..4 {
            let Some(record) = effects.plan(&trees, &mut ids, NOW).cloned() else {
                break;
            };
            if record.node == "h" {
                effects.failed(&record.effect_id).unwrap();
                continue;
            }
            assert_eq!(record.node, "n", "{record:?}");
            removed = matches!(record.effect, Effect::RemoveFile { .. });
            effects.done(&mut trees, &record.effect_id, None).unwrap();
            trees.settle();
        }
        let commit = build(&mut commits, &mut trees, &mut ids);
        match edit {
            None => {
                assert!(
                    removed,
                    "a file with no edit follows the delete, parked={parked}"
                );
                assert!(trees.local().get("n").is_none());
                let ops = commit.map(|commit| ops(&commit)).unwrap_or_default();
                assert!(
                    ops.iter()
                        .all(|op| matches!(op, op::Op::CreateFile(c) if c.node_id == "b")),
                    "{ops:?}"
                );
            }
            Some(digest) => {
                assert!(!removed, "an edit is never removed");
                let ops = ops(&commit.expect("the edit is uploaded"));
                assert!(
                    matches!(&ops[..], [op::Op::CreateFile(c)]
                        if c.node_id != "n" && c.content == Some(content(digest))),
                    "{ops:?}"
                );
            }
        }
    }
}

#[test]
fn a_parked_reverted_folder_the_server_lacks_is_removed_even_when_its_name_is_taken() {
    let mut trees = trees(&[synced("h", dir(ROOT, "x"))]);
    trees.set_local(
        "n".into(),
        LocalNode {
            parked: true,
            ..local(dir(ROOT, "x"), 2)
        },
    );
    trees.revert("n".into());
    trees.settle();
    let mut effects = Effects::default();
    let record = effects.plan(&trees, &mut IdGen::new(1), NOW).cloned();
    let record = record.expect("an effect for the parked folder");
    assert!(
        matches!(record.effect, Effect::RemoveDir { .. }),
        "{record:?}"
    );
    effects.done(&mut trees, &record.effect_id, None).unwrap();
    trees.settle();
    assert_eq!(trees.work(), vec![]);
    assert!(trees.local().get("n").is_none());
    assert!(trees.reverted().is_empty());
}

#[test]
fn a_pending_name_stays_within_the_name_limit() {
    let pending = |name: &str, taken: Option<&str>| {
        let mut rows = vec![
            (
                "h",
                Some(dir("n", name)),
                Some(dir(ROOT, name)),
                Some(remote(dir(ROOT, name))),
            ),
            ("n", Some(dir(ROOT, name)), None, None),
        ];
        rows.extend(taken.map(|taken| synced("p", dir(ROOT, taken))));
        let mut trees = trees(&rows);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        let op::Op::CreateDir(create) = &ops(&commit)[0] else {
            panic!("{:?}", ops(&commit));
        };
        normalize_name(&create.name).expect("a valid name")
    };
    for name in ["a".repeat(255), "é".repeat(127), "e\u{301}".repeat(85)] {
        let first = pending(&name, None);
        let second = pending(&name, Some(&first));
        for (pending, suffix) in [(&first, " (pending)"), (&second, " (pending 2)")] {
            let stem = pending.strip_suffix(suffix).expect(suffix);
            assert!(nfc(&name).starts_with(stem), "{pending}");
            assert!(pending.len() <= MAX_NAME_BYTES, "{pending}");
            assert!(pending.len() + "é".len() > MAX_NAME_BYTES || stem == nfc(&name));
        }
    }
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

/// The answer to a conflict keeps the copy, a file of 8,192 chunks, so the original takes the
/// copy's content in an op of about 330 KiB. It counts toward the commit's change records, and
/// the files queued after it wait for the next commit.
#[test]
fn a_conflict_answer_counts_toward_the_change_records_of_its_commit() {
    let mut trees = trees(&[(
        "f",
        Some(file(ROOT, "f", 2)),
        Some(file(ROOT, "f", 1)),
        Some(remote(file(ROOT, "f", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let chunk = proto::ChunkRef {
        hash: vec![7; 32],
        size: 4 << 20,
        encoding: 0,
    };
    let copied = Content {
        chunks: vec![chunk; 8192],
        chunk_count: 8192,
        ..content(2)
    };
    let copy = Shape {
        content: Some(copied.clone()),
        ..file(ROOT, "f (conflict)", 2)
    };
    trees.set_remote("f".into(), remote(file(ROOT, "f", 3)));
    trees.set_remote("y".into(), remote(copy.clone()));
    let made = OpResult {
        node_id: "y".into(),
        ..outcome(op_result::Code::ConflictCopy)
    };
    acknowledge(&mut commits, &mut trees, vec![made]);
    for (id, shape, ino) in [("f", file(ROOT, "f", 3), 1), ("y", copy, 2)] {
        trees.set_local(id.into(), local(shape.clone(), ino));
        trees.set_synced(id.into(), Some(shape));
    }
    for i in 0..4 {
        trees.set_local(format!("g{i}"), local(large(ROOT, &format!("{i}")), i + 3));
    }
    trees.settle();
    let question = commits.questions()[0].id.clone();
    assert!(commits.decide(&question, Answer::KeepCopy));

    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(commit.encoded_len() <= MAX_COMMIT_BYTES);
    let answer = op::Op::UpdateContent(proto::UpdateContent {
        node_id: "f".into(),
        content: Some(copied.clone()),
    });
    let sent = ops(&commit);
    assert!(sent.contains(&answer));
    assert_eq!(sent.len(), 2);

    let mut took = remote(file(ROOT, "f", 3));
    took.shape.content = Some(copied);
    took.content_rev = 2;
    trees.set_remote("f".into(), took);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    let next = ops(&build(&mut commits, &mut trees, &mut ids).unwrap());
    assert_eq!(next.len(), 3);
    assert!(next.iter().all(|op| matches!(op, op::Op::CreateFile(_))));
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
            commits.failed(&refusal(error::Code::LimitExceeded, 0)),
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
        commits.failed(&refusal(error::Code::LimitExceeded, 0)),
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
    commits
        .failed(&refusal(error::Code::LimitExceeded, 0))
        .unwrap();
    for _ in 0..3 {
        let Some(commit) = build(&mut commits, &mut trees, &mut ids) else {
            break;
        };
        assert_eq!(commit.ops.len(), 1);
        commits
            .failed(&refusal(error::Code::LimitExceeded, 0))
            .unwrap();
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
    trees.settle();
    assert!(trees.work().is_empty());
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

/// The device's own commit makes the conflict copy y, and its answer is lost. A log replay after
/// the reconnect brings y into R before the stored result arrives, and the effects download y.
/// The result then finds y on disk already: that entry stays y's, f takes the server's content,
/// and nothing is left to upload.
#[test]
fn a_conflict_copy_downloaded_before_its_result_is_kept_once() {
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
    trees.settle();
    let mut effects = Effects::default();
    let identity = |ino| FileIdentity {
        dev: 1,
        ino,
        birth_ns: Some(0),
    };
    let download = effects.plan(&trees, &mut ids, NOW).cloned().unwrap();
    assert_eq!(download.node, "y");
    effects
        .done(&mut trees, &download.effect_id, Some(identity(9)))
        .unwrap();
    trees.settle();
    let copy = OpResult {
        node_id: "y".into(),
        ..outcome(op_result::Code::ConflictCopy)
    };
    acknowledge(&mut commits, &mut trees, vec![copy]);
    let mut ino = 10;
    while let Some(record) = effects.plan(&trees, &mut ids, NOW).cloned() {
        effects
            .done(&mut trees, &record.effect_id, Some(identity(ino)))
            .unwrap();
        ino += 1;
        trees.settle();
    }
    let y = trees.local().get("y").unwrap();
    assert_eq!(
        (&y.shape, y.identity.ino),
        (&file(ROOT, "f (conflict)", 2), 9)
    );
    assert_eq!(trees.local().get("f").unwrap().shape, file(ROOT, "f", 3));
    assert_eq!(trees.local().iter().count(), 2);
    trees.settle();
    assert!(trees.work().is_empty());
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

/// As above, but the result arrives while y's download is in flight. The result makes the local
/// file y, so L holds y before the download is done. L does not take the download's outcome, and
/// the local file stays y's: it is moved to y's name, and f is downloaded again.
#[test]
fn a_conflict_copy_whose_result_arrives_during_its_download_keeps_the_local_file() {
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
    trees.settle();
    let mut effects = Effects::default();
    let identity = |ino| FileIdentity {
        dev: 1,
        ino,
        birth_ns: Some(0),
    };
    let download = effects.plan(&trees, &mut ids, NOW).cloned().unwrap();
    assert_eq!(download.node, "y");
    let copy = OpResult {
        node_id: "y".into(),
        ..outcome(op_result::Code::ConflictCopy)
    };
    acknowledge(&mut commits, &mut trees, vec![copy]);
    assert_eq!(trees.local().get("y").unwrap().identity.ino, 1);
    assert!(!download.applies(&trees));
    assert_eq!(
        effects.done(&mut trees, &download.effect_id, Some(identity(9))),
        Err(EffectError::DoesNotApply(download.effect_id.clone()))
    );
    effects.failed(&download.effect_id).unwrap();
    effects.unblock("y");
    trees.settle();
    let mut ino = 10;
    while let Some(record) = effects.plan(&trees, &mut ids, NOW).cloned() {
        effects
            .done(&mut trees, &record.effect_id, Some(identity(ino)))
            .unwrap();
        ino += 1;
        trees.settle();
    }
    let y = trees.local().get("y").unwrap();
    assert_eq!(
        (&y.shape, y.identity.ino),
        (&file(ROOT, "f (conflict)", 2), 1)
    );
    assert_eq!(trees.local().get("f").unwrap().shape, file(ROOT, "f", 3));
    assert_eq!(trees.local().iter().count(), 2);
    trees.settle();
    assert!(trees.work().is_empty());
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

/// On a disk that tells "A" from "a", the user edits the file "A" and creates the folder "a" while
/// another device edits the file and renames it to "b". The commit makes the edit the conflict copy
/// y, whose S update at "A" waits behind the folder: S cannot hold both names. The effects still
/// move y to the server's name, which ends the wait, and the trees come into sync.
#[test]
fn a_conflict_copy_whose_s_update_waits_behind_a_new_folder_is_moved_to_its_name() {
    let mut trees = trees(&[
        (
            "n",
            Some(file(ROOT, "A", 2)),
            Some(file(ROOT, "A", 1)),
            Some(remote(file(ROOT, "A", 1))),
        ),
        ("d", Some(dir(ROOT, "a")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit).len(), 2);
    trees.set_remote("n".into(), remote(file(ROOT, "b", 3)));
    trees.set_remote("y".into(), remote(file(ROOT, "b (conflict)", 2)));
    trees.set_remote("d".into(), remote(dir(ROOT, "a")));
    let results = commit
        .ops
        .iter()
        .map(|op| match &op.op {
            Some(op::Op::UpdateContent(_)) => OpResult {
                node_id: "y".into(),
                ..outcome(op_result::Code::ConflictCopy)
            },
            _ => outcome(op_result::Code::Applied),
        })
        .collect();
    acknowledge(&mut commits, &mut trees, results);
    assert!(trees.waiting().contains_key("y"));
    let mut effects = Effects::default();
    let mut ino = 10;
    while let Some(record) = effects.plan(&trees, &mut ids, NOW).cloned() {
        let identity = FileIdentity {
            dev: 1,
            ino,
            birth_ns: Some(0),
        };
        effects
            .done(&mut trees, &record.effect_id, Some(identity))
            .unwrap();
        ino += 1;
        trees.settle();
        assert!(ino < 20, "the effects do not end");
    }
    assert_eq!(
        trees.local().get("y").unwrap().shape,
        file(ROOT, "b (conflict)", 2)
    );
    assert_eq!(trees.local().get("n").unwrap().shape, file(ROOT, "b", 3));
    assert!(trees.waiting().is_empty());
    trees.settle();
    assert!(trees.work().is_empty());
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
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

fn refusal(code: error::Code, retry_after_ms: u64) -> proto::Error {
    proto::Error {
        code: code as i32,
        retry_after_ms,
        ..Default::default()
    }
}

#[test]
fn a_policy_refusal_keeps_the_commit_and_outlasts_any_welcome() {
    use error::Code;
    let unknown = proto::Error {
        code: 99,
        ..Default::default()
    };
    let policy = [
        unknown,
        refusal(Code::Unspecified, 0),
        refusal(Code::QuotaExceeded, 0),
        refusal(Code::Forbidden, 0),
        refusal(Code::InvalidArgument, 0),
        refusal(Code::NotFound, 0),
        refusal(Code::ConfirmationRequired, 0),
    ];
    for error in policy {
        let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
        let halt = Halt::Policy { code: error.code };
        assert_eq!(
            commits.failed(&error),
            Ok(Failure::Halted(halt)),
            "{error:?}"
        );
        let welcomes = [
            welcome(0),
            welcome(1),
            Welcome {
                read_only: true,
                ..welcome(0)
            },
            Welcome {
                history_id: "another".into(),
                ..welcome(0)
            },
        ];
        for welcome in welcomes {
            assert_eq!(commits.connected(&welcome), Reconnect::Idle, "{error:?}");
            assert_eq!(commits.halted(), Some(halt), "{error:?}");
        }
        assert!(!commits.abandon(), "{error:?}");
        assert_eq!(commits.in_flight().unwrap().commit, sent, "{error:?}");
        assert_eq!(build(&mut commits, &mut trees, &mut ids), None, "{error:?}");
    }
    for error in [
        refusal(Code::QuotaExceeded, 5_000),
        refusal(Code::RetryLater, 0),
    ] {
        let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        build(&mut commits, &mut trees, &mut ids).unwrap();
        assert_eq!(commits.failed(&error), Ok(Failure::Resend), "{error:?}");
        assert_eq!(
            commits.connected(&welcome(0)),
            Reconnect::Resend,
            "{error:?}"
        );
    }
}

/// A device under a policy halt keeps the refused commit whatever the `Welcome` says, until a
/// server counts fewer of its commits. That loss is handled as without the halt, once: the
/// counter takes the server's, the refused commit and the slots of the lost commits are dropped,
/// and R is listed again. The halt stays, and the next `Welcome` finds nothing lost.
#[test]
fn a_policy_halt_takes_a_server_loss_once() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let a = RemoteNode {
        content_rev: 4,
        meta_rev: 4,
        ..remote(file(ROOT, "x", 1))
    };
    trees.set_remote("a".into(), a);
    commits
        .received(result(4, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 4));
    trees.settle();
    trees.set_local("b".into(), local(file(ROOT, "y", 2), 98));
    trees.settle();
    let refused = build(&mut commits, &mut trees, &mut ids).unwrap();
    commits.failed(&refusal(error::Code::Forbidden, 0)).unwrap();
    let halt = Some(Halt::Policy {
        code: error::Code::Forbidden as i32,
    });
    assert_eq!(commits.held_through(), 4);
    assert_eq!(commits.connected(&welcome(1)), Reconnect::Idle);
    assert_eq!(commits.held_through(), 4);
    assert_eq!(commits.in_flight().unwrap().commit, refused);
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    assert_eq!(commits.held_through(), 0);
    assert_eq!((commits.accepted(), commits.in_flight()), (0, None));
    assert_eq!(commits.halted(), halt);
    assert_eq!(commits.connected(&welcome(0)), Reconnect::Idle);
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_policy_halt_lifted_to_resend_sends_the_same_commit_again() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    commits.failed(&refusal(error::Code::Forbidden, 0)).unwrap();
    assert_eq!(commits.lift(Lift::Resend), Ok(()));
    assert_eq!(commits.halted(), None);
    assert_eq!(commits.connected(&welcome(0)), Reconnect::Resend);
    assert_eq!(commits.in_flight().unwrap().commit, sent);
}

#[test]
fn a_policy_halt_lifted_to_replan_plans_the_changes_into_a_new_commit_in_the_same_slot() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let refused = build(&mut commits, &mut trees, &mut ids).unwrap();
    commits
        .failed(&refusal(error::Code::InvalidArgument, 0))
        .unwrap();
    assert_eq!(commits.lift(Lift::Replan), Ok(()));
    assert_eq!((commits.halted(), commits.in_flight()), (None, None));
    assert_eq!(commits.connected(&welcome(0)), Reconnect::Idle);
    let planned = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_ne!(planned.commit_id, refused.commit_id);
    assert_eq!(planned.device_seq, refused.device_seq);
    assert_eq!(ops(&planned), ops(&refused));
}

#[test]
fn a_lift_needs_a_policy_halt_and_leaves_one_that_asks_for_confirmation() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    assert_eq!(commits.lift(Lift::Resend), Err(LiftError::NotHalted));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    commits
        .failed(&refusal(error::Code::ConfirmationRequired, 0))
        .unwrap();
    let halt = commits.halted();
    for lift in [Lift::Resend, Lift::Replan] {
        assert_eq!(commits.lift(lift), Err(LiftError::NeedsConfirmation));
        assert_eq!(commits.halted(), halt);
        assert_eq!(commits.in_flight().unwrap().commit, sent);
    }
    let read_only = Welcome {
        read_only: true,
        ..welcome(0)
    };
    let mut commits = Commits::default();
    commits.connected(&read_only);
    assert_eq!(commits.halted(), Some(Halt::ReadOnly));
    assert_eq!(commits.lift(Lift::Replan), Err(LiftError::NotHalted));
    assert_eq!(commits.halted(), Some(Halt::ReadOnly));
}

fn welcome(device_seq: u64) -> Welcome {
    Welcome {
        history_id: HISTORY.into(),
        device_seq,
        ..Default::default()
    }
}

/// GUN-379. The server counts fewer commits of this device than the store accepted, so it lost some it had
/// accepted. The store continues from the server's count and drops the commit in flight, also on a read-only
/// device.
#[test]
fn a_server_count_below_the_accepted_one_is_taken_and_drops_the_commit_in_flight() {
    for read_only in [false, true] {
        let mut trees = trees(&[("n", Some(file(ROOT, "n", 1)), None, None)]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        build(&mut commits, &mut trees, &mut ids).unwrap();
        acknowledge(
            &mut commits,
            &mut trees,
            vec![outcome(op_result::Code::Applied)],
        );
        assert_eq!(commits.accepted(), 1);
        trees.set_local("m".into(), local(file(ROOT, "m", 2), 2));
        trees.settle();
        let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
        assert_eq!(commit.device_seq, 2);
        let welcome = Welcome {
            read_only,
            ..welcome(0)
        };
        assert_eq!(commits.connected(&welcome), Reconnect::AheadOfServer);
        assert_eq!(commits.accepted(), 0);
        assert!(commits.in_flight().is_none());
    }
}

/// GUN-444: a commit's result arrives, but before it is acknowledged the server loses it, and
/// its `Welcome` counts *w* = *a*. That is a loss, so the commit in flight is dropped and R is
/// listed again, instead of recovery starting.
#[test]
fn a_held_result_of_a_commit_the_server_lost_is_a_loss() {
    let mut trees = trees(&[("n", Some(file(ROOT, "n", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let applied = vec![outcome(op_result::Code::Applied)];
    commits.received(result(3, applied)).unwrap();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    assert_eq!(commits.halted(), None);
    assert!(commits.in_flight().is_none());
    assert_eq!(commits.accepted(), 0);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
}

#[test]
fn the_device_slot_is_kept_by_a_retry_and_reused_after_a_refusal() {
    let mut trees = trees(&[("a", Some(file(ROOT, "x", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        commits.failed(&refusal(error::Code::MissingChunks, 0)),
        Ok(Failure::Resend)
    );
    assert_eq!(commits.in_flight().unwrap().commit, first);
    assert_eq!(
        commits.failed(&refusal(error::Code::AheadOfServer, 0)),
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
        (Code::Unauthenticated, Failure::Resend, true),
        (Code::Internal, Failure::Resend, true),
        (Code::HeadMoved, Failure::Abandoned, false),
        (Code::UnsupportedVersion, Failure::Abandoned, false),
        (Code::AheadOfServer, Failure::Resync, false),
        (Code::ReadOnly, Failure::Halted(Halt::ReadOnly), false),
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
        assert_eq!(commits.failed(&refusal(code, 0)), Ok(failure), "{code:?}");
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

/// A parked file h holds root/x in S while R has it at y. n moves to x with new content; the
/// server applies both ops, but S cannot take n at x yet, so its update waits with content 1.
fn a_move_and_edit_whose_s_update_waits() -> (Trees, Commits, IdGen) {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        (
            "n",
            Some(file(ROOT, "x", 1)),
            Some(file(ROOT, "old", 0)),
            Some(remote(file(ROOT, "old", 0))),
        ),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "x"), update("n", 1)]);
    trees.set_remote("n".into(), remote(file(ROOT, "x", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    assert_eq!(
        trees.waiting().get("n").map(|waiting| &waiting.shape),
        Some(&Some(file(ROOT, "x", 1)))
    );
    (trees, commits, ids)
}

/// The user moves and edits n again while its update waits. Compared with the waiting shape, only
/// L changed, so both ops are sent for n itself, which is not forked into a new file.
#[test]
fn a_node_whose_update_waits_is_moved_and_edited_again_under_its_id() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "z", 2), 2));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "z"), update("n", 2)]);
    assert!(trees.local().get("n").is_some());
}

/// The user takes n back to its S shape while the update to x waits. That is a change of L
/// against the waiting shape, not a remote change, so it is sent.
#[test]
fn a_node_whose_update_waits_taken_back_to_its_s_shape_is_sent() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "old", 0), 2));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "old"), update("n", 0)]);
}

/// The other device edits n while its update waits, and the user moves it. The edit does not hold
/// back the move (R4).
#[test]
fn a_remote_edit_of_a_node_whose_update_waits_does_not_hold_back_its_local_move() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "z", 1), 2));
    trees.set_remote("n".into(), remote(file(ROOT, "x", 3)));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "z")]);
}

/// As in `a_move_and_edit_whose_s_update_waits`, but a commit of this device that creates `pad`
/// is applied at seq 2 first, and the move and edit of n at seq 5. A server restored from a backup
/// taken at seq 2 counts only the first, and a bootstrap lists n back at root/old with content 0
/// of seq 1, as S holds it.
fn a_move_and_edit_whose_s_update_waits_lost_at_5() -> (Trees, Commits, IdGen) {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        (
            "n",
            Some(file(ROOT, "old", 0)),
            Some(file(ROOT, "old", 0)),
            Some(remote(file(ROOT, "old", 0))),
        ),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.set_local("pad".into(), local(file(ROOT, "pad", 7), 99));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    let pad = RemoteNode {
        content_rev: 2,
        meta_rev: 2,
        ..remote(file(ROOT, "pad", 7))
    };
    trees.set_remote("pad".into(), pad);
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.set_local("n".into(), local(file(ROOT, "x", 1), 2));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "x"), update("n", 1)]);
    let moved = RemoteNode {
        content_rev: 5,
        meta_rev: 5,
        ..remote(file(ROOT, "x", 1))
    };
    trees.set_remote("n".into(), moved);
    let applied = vec![outcome(op_result::Code::Applied); 2];
    commits.received(result(5, applied)).unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle();
    assert_eq!(
        trees.waiting().get("n").map(|waiting| &waiting.shape),
        Some(&Some(file(ROOT, "x", 1)))
    );
    assert_eq!(commits.connected(&welcome(1)), Reconnect::AheadOfServer);
    assert_eq!(commits.held_through(), 2);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("n".into(), remote(file(ROOT, "old", 0)));
    trees.forget_lost(trashed, edited, commits.held_through(), commits.accepted());
    trees.settle();
    (trees, commits, ids)
}

/// Once h leaves x, the waiting update could be made, and R's rollback would read as a remote
/// change that moves n back and writes content 0 over the edit. The update is dropped instead, so
/// the move and the edit are sent again.
#[test]
fn a_move_and_edit_the_server_lost_while_its_s_update_waits_is_sent_again() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits_lost_at_5();
    trees.set_local("h".into(), local(file(ROOT, "y", 9), 1));
    trees.settle();
    assert_eq!(trees.class("n"), Class::Local);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "x"), update("n", 1)]);
}

/// As in `a_move_and_edit_whose_s_update_waits`, but the server loses every commit of this device,
/// and a bootstrap lists n at root/old with content 0 of seq 1, a seq the lost commit had. The
/// update came from the lost commit, so it is dropped. R holds n as S does, so R's content is not
/// written over the edit: the move and the edit are sent again.
#[test]
fn a_move_and_edit_whose_s_update_waits_is_not_overwritten_by_a_change_after_the_loss() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("n".into(), remote(file(ROOT, "old", 0)));
    trees.forget_lost(trashed, edited, commits.held_through(), commits.accepted());
    trees.settle();
    trees.set_local("h".into(), local(file(ROOT, "y", 9), 1));
    trees.settle();
    assert_eq!(trees.class("n"), Class::Local);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "x"), update("n", 1)]);
}

/// A parked file h holds root/x in S while R has it at y, and the user creates n at x. The server
/// applies the create, but S cannot take n at x, so its update waits. The server then loses the
/// commit, and a bootstrap lists no n. Once h leaves x, the waiting update would put n in S, and
/// R's rollback would read as a remote delete of the only copy. The update is dropped instead, so
/// n is created again.
#[test]
fn a_create_the_server_lost_while_its_s_update_waits_is_sent_again() {
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "x", 9)),
        Some(file(ROOT, "x", 9)),
        Some(remote(file(ROOT, "y", 9))),
    )]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.set_local("n".into(), local(file(ROOT, "x", 1), 2));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
    trees.set_remote("n".into(), remote(file(ROOT, "x", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert!(trees.waiting().contains_key("n"));
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    trees.remove_remote("n");
    trees.forget_lost(trashed, edited, commits.held_through(), commits.accepted());
    trees.settle();
    trees.set_local("h".into(), local(file(ROOT, "y", 9), 1));
    trees.settle();
    assert_eq!(trees.class("n"), Class::Local);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
}

/// Commits a new file `id`, and acknowledges it at `seq` with R holding the file.
fn commit_file(trees: &mut Trees, commits: &mut Commits, ids: &mut IdGen, id: &str, seq: u64) {
    trees.set_local(id.into(), local(file(ROOT, id, 7), 100 + seq));
    trees.settle();
    let commit = build(commits, trees, ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == id),
        "{:?}",
        ops(&commit)
    );
    let created = RemoteNode {
        content_rev: seq,
        meta_rev: seq,
        ..remote(file(ROOT, id, 7))
    };
    trees.set_remote(id.into(), created);
    let applied = vec![outcome(op_result::Code::Applied)];
    commits.received(result(seq, applied)).unwrap();
    assert!(commits.acknowledge(trees, seq));
    trees.settle();
}

/// Whether `ops` upload content `digest`, to any node.
fn uploads(ops: &[op::Op], digest: u8) -> bool {
    ops.iter().any(|op| match op {
        op::Op::CreateFile(create) => create.content == Some(content(digest)),
        op::Op::UpdateContent(update) => update.content == Some(content(digest)),
        _ => false,
    })
}

/// GUN-418: the move and edit of n at slot 1 wait behind h. Another device moves n back at seq 2,
/// this device's commit at slot 2 and seq 3 is kept, and the one at slot 3 is lost. The bootstrap
/// lists n as S holds it. The update came from a kept commit, so it stays, and once h leaves x, n
/// takes the other device's change: nothing is sent for n.
#[test]
fn a_kept_move_that_another_device_reverses_before_a_loss_is_not_sent_again() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    let reversed = RemoteNode {
        content_rev: 2,
        meta_rev: 2,
        ..remote(file(ROOT, "old", 0))
    };
    trees.set_remote("n".into(), reversed);
    trees.settle();
    commit_file(&mut trees, &mut commits, &mut ids, "p", 3);
    commit_file(&mut trees, &mut commits, &mut ids, "q", 4);
    assert_eq!(commits.connected(&welcome(2)), Reconnect::AheadOfServer);
    assert_eq!(commits.held_through(), 3);
    let (gone, edited) = (trees.trashed_here(), trees.edited_here());
    trees.remove_remote("q");
    trees.forget_lost(gone, edited, commits.held_through(), commits.accepted());
    trees.settle();
    assert!(trees.waiting().contains_key("n"));
    trees.set_local("h".into(), local(file(ROOT, "y", 9), 1));
    trees.settle();
    let sent =
        build(&mut commits, &mut trees, &mut ids).map_or_else(Vec::new, |commit| ops(&commit));
    assert!(
        !sent.contains(&mv("n", ROOT, "x")) && !uploads(&sent, 1),
        "{sent:?}"
    );
    assert_eq!(trees.class("n"), Class::Remote);
}

/// GUN-418: the create of n at slot 1 waits behind h, and the commit at slot 2 is lost. Another
/// device then deletes n and the trash is purged, so the bootstrap lists no n. The create was
/// kept, so its update stays, and once h leaves x, n is a remote delete instead of being created
/// again.
#[test]
fn a_kept_create_that_another_device_deletes_after_a_loss_stays_deleted() {
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "x", 9)),
        Some(file(ROOT, "x", 9)),
        Some(remote(file(ROOT, "y", 9))),
    )]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.set_local("n".into(), local(file(ROOT, "x", 1), 2));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("n".into(), remote(file(ROOT, "x", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert!(trees.waiting().contains_key("n"));
    commit_file(&mut trees, &mut commits, &mut ids, "p", 2);
    assert_eq!(commits.connected(&welcome(1)), Reconnect::AheadOfServer);
    let (gone, edited) = (trees.trashed_here(), trees.edited_here());
    trees.remove_remote("n");
    trees.remove_remote("p");
    trees.forget_lost(gone, edited, commits.held_through(), commits.accepted());
    trees.settle();
    assert!(trees.waiting().contains_key("n"));
    trees.set_local("h".into(), local(file(ROOT, "y", 9), 1));
    trees.settle();
    let sent =
        build(&mut commits, &mut trees, &mut ids).map_or_else(Vec::new, |commit| ops(&commit));
    assert!(
        !sent
            .iter()
            .any(|op| matches!(op, op::Op::CreateFile(c) if c.node_id == "n")),
        "{sent:?}"
    );
    assert_eq!(trees.class("n"), Class::Remote);
}

/// GUN-418: the move and edit of n at slot 2 wait behind h. Another device moves n to root/z,
/// which file k holds in S while both sides move k away, and the effect gives L R's position. S
/// cannot take it yet, so the update waits again, and it keeps slot 2.
/// The server then loses slot 2, so the update is dropped, and the move and the edit are sent
/// again instead of R's rollback content being written over the edit.
#[test]
fn an_effect_on_a_node_whose_update_waits_keeps_its_slot_for_a_loss() {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        (
            "n",
            Some(file(ROOT, "old", 0)),
            Some(file(ROOT, "old", 0)),
            Some(remote(file(ROOT, "old", 0))),
        ),
        (
            "k",
            Some(file(ROOT, "z", 8)),
            Some(file(ROOT, "z", 8)),
            Some(remote(file(ROOT, "z", 8))),
        ),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    commit_file(&mut trees, &mut commits, &mut ids, "p", 2);
    trees.set_local("n".into(), local(file(ROOT, "x", 1), 2));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "x"), update("n", 1)]);
    let moved = RemoteNode {
        content_rev: 5,
        meta_rev: 5,
        ..remote(file(ROOT, "x", 1))
    };
    trees.set_remote("n".into(), moved);
    let applied = vec![outcome(op_result::Code::Applied); 2];
    commits.received(result(5, applied)).unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle();
    assert_eq!(trees.waiting()["n"].origin, Origin::Commit(2));
    trees.set_local("k".into(), local(file(ROOT, "z2", 8), 3));
    let k_moved = RemoteNode {
        meta_rev: 6,
        ..remote(file(ROOT, "w", 8))
    };
    trees.set_remote("k".into(), k_moved);
    let moved_again = RemoteNode {
        content_rev: 5,
        meta_rev: 7,
        ..remote(file(ROOT, "z", 1))
    };
    trees.set_remote("n".into(), moved_again);
    trees.settle();
    assert!(!effects_for(&mut trees, &mut ids, "n").is_empty());
    trees.settle();
    assert_eq!(
        trees
            .waiting()
            .get("n")
            .map(|waiting| (&waiting.shape, waiting.origin)),
        Some((&Some(file(ROOT, "z", 1)), Origin::Commit(2)))
    );
    assert_eq!(commits.connected(&welcome(1)), Reconnect::AheadOfServer);
    let (gone, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("n".into(), remote(file(ROOT, "old", 0)));
    trees.set_remote("k".into(), remote(file(ROOT, "z", 8)));
    trees.forget_lost(gone, edited, commits.held_through(), commits.accepted());
    trees.settle();
    let sent =
        build(&mut commits, &mut trees, &mut ids).map_or_else(Vec::new, |commit| ops(&commit));
    assert!(
        sent.contains(&mv("n", ROOT, "z")) && sent.contains(&update("n", 1)),
        "{sent:?}"
    );
}

/// GUN-418: a waiting update taken by a rebase and waited again keeps the slot of its commit.
#[test]
fn a_waiting_update_keeps_its_slot_through_a_rebase() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    assert_eq!(trees.waiting()["n"].origin, Origin::Commit(1));
    trees.set_local("p".into(), local(file(ROOT, "p", 7), 50));
    trees.settle();
    build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(trees.waiting()["n"].origin, Origin::Commit(1));
}

/// As in `a_move_and_edit_whose_s_update_waits`, but the server loses the commit, and the
/// bootstrap lists n in the trash. The update is dropped as for any node.
fn a_move_and_edit_lost_while_r_holds_it_in_the_trash() -> (Trees, Commits, IdGen) {
    let (mut trees, mut commits, ids) = a_move_and_edit_whose_s_update_waits();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (gone, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("n".into(), trashed(file(ROOT, "old", 0)));
    trees.forget_lost(gone, edited, commits.held_through(), commits.accepted());
    trees.settle();
    assert!(!trees.waiting().contains_key("n"));
    (trees, commits, ids)
}

/// GUN-418: another device restores n with other content, and h leaves x, where the lost update
/// would fit. S never takes it, so L's edit, which only L holds, is uploaded rather than replaced
/// by R's content.
#[test]
fn a_lost_update_is_not_made_when_another_device_restores_the_node() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_lost_while_r_holds_it_in_the_trash();
    let restored = RemoteNode {
        content_rev: 2,
        meta_rev: 2,
        ..remote(file(ROOT, "old", 7))
    };
    trees.set_remote("n".into(), restored);
    trees.set_local("h".into(), local(file(ROOT, "y", 9), 1));
    trees.settle();
    let sent =
        build(&mut commits, &mut trees, &mut ids).map_or_else(Vec::new, |commit| ops(&commit));
    assert!(!trees.waiting().contains_key("n"));
    assert!(uploads(&sent, 1), "{sent:?}");
}

/// GUN-418: as above, but h stays at x in R and S, so the lost update could never fit, and the
/// device is read-only, so nothing is sent. Another device restores n with other content. The
/// effects compare L with S, not with the lost shape, so n is not read as changed only remotely,
/// and L's edit is not replaced by R's content.
#[test]
fn a_lost_update_is_not_the_effects_base_while_it_cannot_fit() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (gone, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("h".into(), remote(file(ROOT, "x", 9)));
    trees.set_remote("n".into(), trashed(file(ROOT, "old", 0)));
    trees.forget_lost(gone, edited, commits.held_through(), commits.accepted());
    trees.settle();
    let mut state = commits.state();
    state.halted = Some(Halt::ReadOnly);
    let mut commits = Commits::from_state(state).unwrap();
    let restored = RemoteNode {
        content_rev: 2,
        meta_rev: 2,
        ..remote(file(ROOT, "old", 7))
    };
    trees.set_remote("n".into(), restored);
    trees.settle();
    let mut effects = Effects::default();
    let identity = FileIdentity {
        dev: 1,
        ino: 100,
        birth_ns: Some(0),
    };
    for _ in 0..3 {
        assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
        let Some(record) = effects.plan(&trees, &mut ids, NOW).cloned() else {
            break;
        };
        if record.node == "h" {
            effects.failed(&record.effect_id).unwrap();
            continue;
        }
        effects
            .done(&mut trees, &record.effect_id, Some(identity))
            .unwrap();
        trees.settle();
    }
    assert_eq!(
        trees.local().get("n").map(|node| &node.shape.content),
        Some(&Some(content(1)))
    );
}

/// GUN-418: this device moves file m out of folder d to root/x and deletes d. A parked file h
/// holds root/x in S while R has it at y, so m's update waits, S still holds m under d, and the
/// removal of d waits too. The server loses the commit and lists d and m as S holds them. Both
/// updates are dropped, so once h leaves x, the move and the delete are sent again instead of R's
/// d reading as a remote create.
#[test]
fn a_removal_the_server_lost_while_it_waits_is_sent_again() {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        (
            "d",
            None,
            Some(dir(ROOT, "d")),
            Some(remote(dir(ROOT, "d"))),
        ),
        (
            "m",
            Some(file(ROOT, "x", 1)),
            Some(file("d", "m", 1)),
            Some(remote(file("d", "m", 1))),
        ),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 1)
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let applied = vec![outcome(op_result::Code::Applied); ops(&commit).len()];
    trees.remove_remote("d");
    trees.set_remote("m".into(), remote(file(ROOT, "x", 1)));
    acknowledge(&mut commits, &mut trees, applied);
    assert_eq!(
        trees.waiting().get("d").map(|waiting| &waiting.shape),
        Some(&None)
    );
    assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
    let (gone, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("d".into(), remote(dir(ROOT, "d")));
    trees.set_remote("m".into(), remote(file("d", "m", 1)));
    trees.forget_lost(gone, edited, commits.held_through(), commits.accepted());
    trees.settle();
    assert!(trees.waiting().is_empty());
    trees.set_local("h".into(), local(file(ROOT, "y", 9), 1));
    trees.settle();
    let sent =
        build(&mut commits, &mut trees, &mut ids).map_or_else(Vec::new, |commit| ops(&commit));
    assert!(
        sent.contains(&mv("m", ROOT, "x")) && sent.contains(&delete("d")),
        "{sent:?}"
    );
}

/// Folder h at root/x is trashed remotely, and the user moves and edits its file n to root/X. The
/// server applies both ops, but S cannot take n at X while h holds the name key x, so the update
/// waits.
fn a_file_moved_out_of_a_trashed_folder_whose_update_waits() -> (Trees, Commits, IdGen) {
    let mut trees = trees(&[
        (
            "h",
            Some(dir(ROOT, "x")),
            Some(dir(ROOT, "x")),
            Some(trashed(dir(ROOT, "x"))),
        ),
        (
            "n",
            Some(file(ROOT, "X", 1)),
            Some(file("h", "old", 0)),
            Some(trashed(file("h", "old", 0))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "X"), update("n", 1)]);
    trees.set_remote("n".into(), remote(file(ROOT, "X", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    assert_eq!(
        trees.waiting().get("n").map(|waiting| &waiting.shape),
        Some(&Some(file(ROOT, "X", 1)))
    );
    (trees, commits, ids)
}

/// The user moves n back into h. Its content agrees with the waiting shape, not with S, so S does
/// not take that content as both sides' and drop the waiting update, and the move is sent instead
/// of the effects undoing it.
#[test]
fn a_file_moved_back_into_a_trashed_folder_while_its_update_waits_stays_there() {
    let (mut trees, mut commits, mut ids) =
        a_file_moved_out_of_a_trashed_folder_whose_update_waits();
    trees.set_local("n".into(), local(file("h", "old", 1), 2));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", "h", "old")]);
    effects_for(&mut trees, &mut ids, "n");
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.local().live("n"), Some(&file("h", "old", 1)));
}

/// As above, with an edit of n on the other device and a move of n into h on this one. The move is
/// sent, so neither n's update nor h's removal waits for the other for good.
#[test]
fn a_file_moved_into_a_trashed_folder_while_its_update_waits_and_it_is_edited_remotely_is_sent() {
    let (mut trees, mut commits, mut ids) =
        a_file_moved_out_of_a_trashed_folder_whose_update_waits();
    trees.set_local("n".into(), local(file("h", "z", 1), 2));
    trees.set_remote("n".into(), remote(file(ROOT, "X", 3)));
    trees.settle();
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", "h", "z")]);
    assert_eq!(trees.check(), Ok(()));
}

/// Plans and does every effect, failing none, and returns those planned for `node`. An entry an
/// effect creates or writes gets inode 100.
fn effects_for(trees: &mut Trees, ids: &mut IdGen, node: &str) -> Vec<Effect> {
    let mut effects = Effects::default();
    let mut planned = Vec::new();
    for _ in 0..8 {
        let Some(record) = effects.plan(trees, ids, NOW).cloned() else {
            break;
        };
        if record.node == node {
            planned.push(record.effect.clone());
        }
        let written = trees.local().get(&record.node).is_none()
            || matches!(record.effect, Effect::WriteFile { .. });
        let identity = written.then_some(FileIdentity {
            dev: 1,
            ino: 100,
            birth_ns: Some(0),
        });
        effects.done(trees, &record.effect_id, identity).unwrap();
        trees.settle();
    }
    planned
}

/// The user puts n back where S still holds it, with the content S still holds, and the server
/// deletes n. Against the acknowledged update that is an edit, so it is uploaded, whether the
/// effects or a commit come first, and whether n is parked or not.
#[test]
fn a_waiting_file_put_back_as_s_still_holds_it_is_uploaded_after_a_remote_delete() {
    for (effects_first, parked) in [(true, false), (true, true), (false, false)] {
        let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
        trees.set_local(
            "n".into(),
            LocalNode {
                parked,
                ..local(file(ROOT, "old", 0), 2)
            },
        );
        trees.set_remote("n".into(), trashed(file(ROOT, "x", 1)));
        trees.settle();
        assert_eq!(trees.class("n"), Class::Remote, "L matches S");
        assert!(trees.edited("n"));
        if effects_first {
            let effects = effects_for(&mut trees, &mut ids, "n");
            assert!(
                effects
                    .iter()
                    .all(|effect| matches!(effect, Effect::Move { .. })),
                "{effects:?}"
            );
            assert_eq!(trees.local().get("n").map(|node| node.parked), Some(false));
            let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
            assert_eq!(ops(&commit), [mv("n", ROOT, "old"), update("n", 0)]);
            continue;
        }
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
            (ROOT, "old", Some(content(0)))
        );
        assert_eq!(effects_for(&mut trees, &mut ids, &create.node_id), []);
        assert_eq!(trees.local().get(&create.node_id).unwrap().identity.ino, 2);
    }
}

#[test]
fn a_waiting_update_both_sides_moved_past_keeps_its_content_as_the_base_of_a_later_edit() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "z", 0), 2));
    trees.set_remote("n".into(), remote(file(ROOT, "z", 1)));
    trees.settle();
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 0)]);
}

/// The other device moves n on and puts a new folder m where n's update would place it, while the
/// user moves n back. The update's position is stale, so it no longer holds n out of planning, and
/// n follows the remote move with the acknowledged content.
#[test]
fn a_waiting_update_whose_place_r_gave_away_does_not_hold_the_node_for_good() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "old", 1), 2));
    trees.set_remote("n".into(), remote(file(ROOT, "q", 1)));
    trees.set_remote("m".into(), remote(dir(ROOT, "x")));
    trees.settle();
    for _ in 0..3 {
        effects_for(&mut trees, &mut ids, "");
        assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    }
    assert_eq!(trees.waiting(), &Default::default());
    assert_eq!(
        trees.local().get("n").map(|node| &node.shape),
        Some(&file(ROOT, "q", 1))
    );
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "q", 1)));
    assert_eq!(trees.synced().live("m"), Some(&dir(ROOT, "x")));
}

/// n is parked as it was before the update, with the content S still holds, while R has n as the
/// update left it. Against the acknowledged update the old content is an edit, so n is only moved
/// out of the parked directory, and the content is uploaded once the update is made.
#[test]
fn a_parked_waiting_file_with_the_content_s_still_holds_keeps_it() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local(
        "n".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "old", 0), 2)
        },
    );
    trees.settle();
    assert_eq!(trees.class("n"), Class::Remote, "L matches S");
    let effects = effects_for(&mut trees, &mut ids, "n");
    assert!(
        effects
            .iter()
            .all(|effect| matches!(effect, Effect::Move { .. })),
        "{effects:?}"
    );
    assert_eq!(
        trees
            .local()
            .get("n")
            .map(|node| (&node.shape, node.parked)),
        Some((&file(ROOT, "x", 0), false))
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 0)]);
}

/// n is parked with the content the server acknowledged, which its waiting update holds, while
/// the other device moves n to q and changes its content, and committing is halted. Against the
/// acknowledged update only R changed, so the effects write R's content at q, although no commit
/// rebases n while committing is halted.
#[test]
fn a_parked_waiting_file_takes_remote_content_while_committing_is_halted() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local(
        "n".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 1), 2)
        },
    );
    trees.set_remote("n".into(), remote(file(ROOT, "q", 3)));
    trees.set_local("p".into(), local(file(ROOT, "p", 3), 3));
    trees.settle();
    build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(
        commits.failed(&refusal(error::Code::ReadOnly, 0)),
        Ok(Failure::Halted(Halt::ReadOnly))
    );
    effects_for(&mut trees, &mut ids, "n");
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    effects_for(&mut trees, &mut ids, "n");
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "q", 3)));
}

/// As above, but the user edited n while it was parked. That edit is the user's, so n is only
/// moved to q, with the edited content, and the edit is uploaded as a conflict copy beside R's.
#[test]
fn a_parked_waiting_file_the_user_edited_keeps_the_edit_when_the_other_device_moves_it() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local(
        "n".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 2), 2)
        },
    );
    trees.set_remote("n".into(), remote(file(ROOT, "q", 3)));
    trees.settle();
    let effects = effects_for(&mut trees, &mut ids, "n");
    assert!(
        effects
            .iter()
            .all(|effect| matches!(effect, Effect::Move { .. })),
        "{effects:?}"
    );
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "q", 2)));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let [op::Op::CreateFile(copy)] = <[op::Op; 1]>::try_from(ops(&commit)).unwrap() else {
        panic!("{commit:?}");
    };
    assert_eq!(
        (copy.parent_id.as_str(), copy.content),
        (ROOT, Some(content(2)))
    );
}

/// Both sides move n to z with the same new content while its update waits. Settling makes S
/// take that shape, which replaces the update, so nothing is uploaded and nothing is forked.
#[test]
fn a_waiting_update_settling_replaced_is_not_made_afterwards() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "z", 2), 2));
    trees.set_remote("n".into(), remote(file(ROOT, "z", 2)));
    trees.settle();
    assert_eq!(trees.waiting(), &Default::default());
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "z", 2)));
}

/// The server renames n's move, so n is reverted as well as waiting, and then the user puts n back
/// as S still holds it. That move is the user's last word, so n is not moved to its R name, and
/// its content, an edit against the acknowledged update, is neither replaced nor removed. Both are
/// uploaded once the update is made.
#[test]
fn a_renamed_waiting_file_put_back_as_s_still_holds_it_keeps_its_content() {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "x (2)", 9)),
            Some(file(ROOT, "x (2)", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        (
            "n",
            Some(file(ROOT, "x", 1)),
            Some(file(ROOT, "old", 0)),
            Some(remote(file(ROOT, "old", 0))),
        ),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x (2)", 9), 1)
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "x"), update("n", 1)]);
    trees.set_remote("n".into(), remote(file(ROOT, "x (2)", 1)));
    trees.set_remote("z".into(), remote(file(ROOT, "x", 8)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![
            OpResult {
                name: "x (2)".into(),
                ..outcome(op_result::Code::AppliedRenamed)
            },
            outcome(op_result::Code::Applied),
        ],
    );
    assert!(trees.waiting().contains_key("n") && trees.reverted().contains("n"));
    trees.set_local("n".into(), local(file(ROOT, "old", 0), 2));
    trees.settle();
    assert_eq!(trees.class("n"), Class::Remote, "L matches S");
    assert!(!trees.reverted().contains("n"));
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    assert_eq!(
        trees.local().get("n").map(|node| &node.shape),
        Some(&file(ROOT, "old", 0))
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "old"), update("n", 0)]);
}

/// n's update waits and h needs n's place on disk, so n is parked. When the driver fails to park
/// it, the same move is not planned again until something changes.
#[test]
fn a_waiting_blocker_whose_parking_failed_is_not_parked_again_unchanged() {
    let (mut trees, _, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "old", 0), 2));
    trees.set_remote("h".into(), remote(file(ROOT, "old", 9)));
    trees.settle();
    let mut effects = Effects::default();
    let first = effects.plan(&trees, &mut ids, NOW).unwrap().clone();
    assert_eq!(first.node, "n");
    assert!(matches!(
        first.effect,
        Effect::Move {
            to: Location::Parked(_),
            ..
        }
    ));
    effects.failed(&first.effect_id).unwrap();
    assert_eq!(effects.plan(&trees, &mut ids, NOW), None);
    assert!(effects.blocked().contains_key("n"));
}

/// Committing is halted, read-only, while n's update waits. Once the update fits, building makes it
/// anyway, so S stays in step while downloads go on.
#[test]
fn a_waiting_update_is_made_while_committing_is_halted() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("p".into(), local(file(ROOT, "p", 3), 3));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "p"));
    assert_eq!(
        commits.failed(&refusal(error::Code::ReadOnly, 0)),
        Ok(Failure::Halted(Halt::ReadOnly))
    );
    effects_for(&mut trees, &mut ids, "");
    assert_eq!(trees.synced().live("h"), Some(&file(ROOT, "y", 9)));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(trees.waiting(), &Default::default());
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "x", 1)));
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
}

/// As above, with an edit the user made after the commit: L leaves the waiting shape, so nothing
/// but building makes the update. The edit is planned against the waiting shape and goes out with
/// the commit that is refused. Once the update fits, building makes it while committing is halted,
/// and the edit stays the user's.
#[test]
fn a_waiting_update_is_made_while_committing_is_halted_under_a_local_edit() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_local("n".into(), local(file(ROOT, "x", 2), 2));
    trees.set_local("p".into(), local(file(ROOT, "p", 3), 3));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::UpdateContent(u), op::Op::CreateFile(c)] if u.node_id == "n" && c.node_id == "p"),
        "{:?}",
        ops(&commit)
    );
    assert_eq!(
        commits.failed(&refusal(error::Code::ReadOnly, 0)),
        Ok(Failure::Halted(Halt::ReadOnly))
    );
    effects_for(&mut trees, &mut ids, "");
    assert_eq!(trees.synced().live("h"), Some(&file(ROOT, "y", 9)));
    assert!(trees.waiting().contains_key("n"));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert!(trees.waiting().is_empty());
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "x", 1)));
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "x", 2)));
}

/// The other device moves n on while n's update waits behind h. Against the waiting shape that is
/// a remote change, so the effects move n at once, and the update then takes R's position.
#[test]
fn a_remote_move_of_a_node_whose_update_waits_reaches_the_disk() {
    let (mut trees, mut commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    trees.set_remote("n".into(), remote(file(ROOT, "q", 1)));
    trees.settle();
    let planned = effects_for(&mut trees, &mut ids, "n");
    assert!(matches!(&planned[..], [Effect::Move { .. }]), "{planned:?}");
    assert_eq!(
        trees.local().get("n").map(|node| &node.shape),
        Some(&file(ROOT, "q", 1))
    );
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(trees.waiting(), &Default::default());
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "q", 1)));
}

/// The user creates folder d at X, where S still holds parked file h at x, and file m in d, so the
/// updates of both wait. The other device moves h into d, and the user deletes d with m. d is
/// brought back for h, and m's delete is uploaded once its update fits.
#[test]
fn a_waiting_folder_the_user_deleted_is_brought_back_for_a_remote_move_into_it() {
    let mut trees = trees(&[
        ("d", Some(dir(ROOT, "X")), None, None),
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        ("m", Some(file("d", "c", 1)), None, None),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 2)
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(commit.ops.len(), 2);
    trees.set_remote("d".into(), remote(dir(ROOT, "X")));
    trees.set_remote("m".into(), remote(file("d", "c", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied); 2],
    );
    assert!(trees.waiting().contains_key("d") && trees.waiting().contains_key("m"));
    trees.set_remote("h".into(), remote(file("d", "h", 9)));
    trees.remove_local("m");
    trees.remove_local("d");
    trees.settle();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    effects_for(&mut trees, &mut ids, "");
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete("m")]);
    assert_eq!(
        trees.local().get("d").map(|node| &node.shape),
        Some(&dir(ROOT, "X"))
    );
    let h = trees.local().get("h").unwrap();
    assert_eq!((&h.shape, h.parked), (&file("d", "h", 9), false));
}

/// GUN-350. The user moves f out of folder d while the other device removes both. The removal of
/// the empty d is in flight when R restores d with f in it. S cannot drop d while it holds f, so
/// that update waits instead of being lost. The commit sends only the user's move, and once it is
/// acknowledged S drops d, the effects bring d back, and the trees end in sync.
#[test]
fn a_removal_r_undid_whose_s_update_does_not_fit_yet_waits() {
    let mut trees = trees(&[
        ("d", Some(dir(ROOT, "d")), Some(dir(ROOT, "d")), None),
        ("f", Some(file(ROOT, "f", 1)), Some(file("d", "f", 1)), None),
    ]);
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let record = effects.plan(&trees, &mut ids, NOW).unwrap().clone();
    assert_eq!(record.node, "d");
    assert!(
        matches!(record.effect, Effect::RemoveDir { .. }),
        "{record:?}"
    );
    trees.set_remote("d".into(), remote(dir(ROOT, "d")));
    trees.set_remote("f".into(), remote(file("d", "f", 1)));
    effects.done(&mut trees, &record.effect_id, None).unwrap();
    trees.settle();

    assert_eq!(effects_for(&mut trees, &mut ids, "d"), []);

    let mut commits = Commits::default();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", ROOT, "f")]);
    trees.set_remote("f".into(), remote(file(ROOT, "f", 1)));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(trees.waiting(), &Default::default());
    let effects = effects_for(&mut trees, &mut ids, "d");
    assert!(
        matches!(effects[..], [Effect::CreateDir { .. }]),
        "{effects:?}"
    );
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(trees.class("d"), Class::InSync);
}

/// S still holds parked h at x when d's move to X is acknowledged, so d's update waits, and S
/// still holds m in d. The user deletes d and parks m, and the other device moves h into d. The
/// keep rule needs d but cannot clear it from S yet. d's acknowledged move keeps waiting, so the
/// effects bring d back at X for h.
#[test]
fn a_needed_folder_whose_move_waits_is_brought_back() {
    let mut trees = trees(&[
        (
            "d",
            Some(dir(ROOT, "X")),
            Some(dir(ROOT, "old")),
            Some(remote(dir(ROOT, "old"))),
        ),
        (
            "h",
            Some(file(ROOT, "x", 9)),
            Some(file(ROOT, "x", 9)),
            Some(remote(file(ROOT, "y", 9))),
        ),
        synced("m", file("d", "c", 1)),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 9), 2)
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("d", ROOT, "X")]);
    trees.set_remote("d".into(), remote(dir(ROOT, "X")));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(
        trees.waiting().get("d").map(|waiting| &waiting.shape),
        Some(&Some(dir(ROOT, "X")))
    );
    trees.set_local(
        "m".into(),
        LocalNode {
            parked: true,
            ..local(file("d", "c", 1), 3)
        },
    );
    trees.remove_local("d");
    trees.set_remote("h".into(), remote(file("d", "h", 9)));
    trees.settle();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    let effects = effects_for(&mut trees, &mut ids, "d");
    assert!(
        matches!(effects[..], [Effect::CreateDir { .. }]),
        "{effects:?}"
    );
    assert_eq!(trees.local().live("d"), Some(&dir(ROOT, "X")));
}

#[test]
fn a_removal_that_waits_after_both_sides_dropped_the_node_is_kept_until_it_fits() {
    let mut trees = trees(&[
        ("d", Some(dir(ROOT, "d")), Some(dir(ROOT, "d")), None),
        ("f", Some(file(ROOT, "f", 1)), Some(file("d", "f", 1)), None),
    ]);
    let (mut effects, mut ids, mut commits) =
        (Effects::default(), IdGen::new(1), Commits::default());
    assert_eq!(
        commits.connected(&Welcome {
            read_only: true,
            ..welcome(0)
        }),
        Reconnect::Idle
    );
    let record = effects.plan(&trees, &mut ids, NOW).unwrap().clone();
    assert_eq!(record.node, "d");
    assert!(matches!(record.effect, Effect::RemoveDir { .. }));
    effects.done(&mut trees, &record.effect_id, None).unwrap();
    trees.settle();
    assert_eq!(
        trees.waiting().get("d").map(|waiting| &waiting.shape),
        Some(&None)
    );
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    trees.set_remote("d".into(), remote(dir(ROOT, "d")));
    trees.set_remote("f".into(), remote(file("d", "f", 1)));
    trees.settle();
    assert_eq!(commits.connected(&welcome(0)), Reconnect::Idle);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("f", ROOT, "f")]);
}

#[test]
fn a_kept_removal_is_made_once_s_no_longer_holds_anything_under_it() {
    let mut trees = trees(&[
        ("d", Some(dir(ROOT, "d")), Some(dir(ROOT, "d")), None),
        ("f", Some(file(ROOT, "f", 1)), Some(file("d", "f", 1)), None),
    ]);
    let (mut effects, mut ids, mut commits) =
        (Effects::default(), IdGen::new(1), Commits::default());
    assert_eq!(
        commits.connected(&Welcome {
            read_only: true,
            ..welcome(0)
        }),
        Reconnect::Idle
    );
    let record = effects.plan(&trees, &mut ids, NOW).unwrap().clone();
    effects.done(&mut trees, &record.effect_id, None).unwrap();
    trees.settle();
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert_eq!(commits.connected(&welcome(0)), Reconnect::Idle);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(ops(&commit)[..], [op::Op::CreateFile(_)]));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
    assert!(trees.waiting().is_empty());
    assert_eq!(trees.synced().live("d"), None);
}

/// GUN-375. n's edit to 1 is in flight when the user and the other device both write 2, so S
/// takes 2. The user writes 1 again before the commit is acknowledged, and the other device then
/// writes 3, before the acknowledgement with `overtaken` and after it otherwise. S keeps 2, which
/// it took after the commit was built, so the user's 1 is an edit of its own and is kept beside
/// the other device's 3. With `restart`, the commit side is restored from its stored state
/// between the send and the acknowledgement.
fn a_late_acknowledgement_keeps_the_content_s_took_since(restart: bool, overtaken: bool) {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "a", 1)),
        Some(file(ROOT, "a", 0)),
        Some(remote(file(ROOT, "a", 0))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 1)]);
    trees.set_local("n".into(), local(file(ROOT, "a", 2), 1));
    trees.set_remote("n".into(), remote(file(ROOT, "a", 2)));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "a", 2)));
    trees.set_local("n".into(), local(file(ROOT, "a", 1), 1));
    if overtaken {
        trees.set_remote("n".into(), remote(file(ROOT, "a", 3)));
        trees.settle();
    }
    if restart {
        commits = Commits::from_state(commits.state()).unwrap();
        trees.restore(trees.state()).unwrap();
        trees.rules_scanned();
    }
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "a", 2)));
    trees.set_remote("n".into(), remote(file(ROOT, "a", 3)));
    trees.settle();
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(copy)] if copy.content == Some(content(1))),
        "{:?}",
        ops(&commit)
    );
}

#[test]
fn a_late_acknowledgement_keeps_the_content_s_took_after_the_commit_was_built() {
    a_late_acknowledgement_keeps_the_content_s_took_since(false, false);
}

#[test]
fn a_late_acknowledgement_after_a_restart_keeps_the_content_s_took_after_the_commit_was_built() {
    a_late_acknowledgement_keeps_the_content_s_took_since(true, false);
}

#[test]
fn a_late_acknowledgement_overtaken_by_the_next_remote_edit_keeps_the_content_s_took() {
    a_late_acknowledgement_keeps_the_content_s_took_since(false, true);
}

#[test]
fn a_late_acknowledgement_overtaken_by_the_next_remote_edit_after_a_restart_keeps_the_content_s_took()
 {
    a_late_acknowledgement_keeps_the_content_s_took_since(true, true);
}

/// n's edit to 1 is in flight when the user and the other device both write 2, so S takes 2. The
/// other device then writes 0 again, which is downloaded, so S is back at the content it held
/// when the commit was built. The user writes 1 again before the acknowledgement. S keeps 0,
/// since it changed after the commit was built, and the user's 1 is uploaded. With \`restart\`,
/// the commit side and the trees are restored from their stored state before the acknowledgement.
fn a_late_acknowledgement_keeps_the_content_s_took_back(restart: bool) {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "a", 1)),
        Some(file(ROOT, "a", 0)),
        Some(remote(file(ROOT, "a", 0))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 1)]);
    trees.set_local("n".into(), local(file(ROOT, "a", 2), 1));
    trees.set_remote("n".into(), remote(file(ROOT, "a", 2)));
    trees.settle();
    trees.set_remote("n".into(), remote(file(ROOT, "a", 0)));
    trees.settle();
    let download = effects_for(&mut trees, &mut ids, "n");
    assert!(
        matches!(&download[..], [Effect::WriteFile { content: written, .. }] if *written == content(0)),
        "{download:?}"
    );
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "a", 0)));
    trees.set_local("n".into(), local(file(ROOT, "a", 1), 100));
    if restart {
        commits = Commits::from_state(commits.state()).unwrap();
        trees.restore(trees.state()).unwrap();
        trees.rules_scanned();
    }
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "a", 0)));
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 1)]);
}

#[test]
fn a_late_acknowledgement_keeps_the_content_s_took_back_to_what_it_held_at_the_build() {
    a_late_acknowledgement_keeps_the_content_s_took_back(false);
}

#[test]
fn a_late_acknowledgement_after_a_restart_keeps_the_content_s_took_back_to_what_it_held_at_the_build()
 {
    a_late_acknowledgement_keeps_the_content_s_took_back(true);
}

/// n's edit to 1 is in flight when the other device writes 0 again. S never took anything after
/// the commit was built, so it takes the 1 that was sent, and the other device's 0 is downloaded.
#[test]
fn a_late_acknowledgement_takes_what_was_sent_when_the_other_device_reverts_it() {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "a", 1)),
        Some(file(ROOT, "a", 0)),
        Some(remote(file(ROOT, "a", 0))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 1)]);
    trees.set_remote("n".into(), remote(file(ROOT, "a", 1)));
    trees.set_remote("n".into(), remote(file(ROOT, "a", 0)));
    trees.settle();
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "a", 1)));
    let effects = effects_for(&mut trees, &mut ids, "n");
    assert!(
        matches!(&effects[..], [Effect::WriteFile { content: written, .. }] if *written == content(0)),
        "{effects:?}"
    );
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "a", 0)));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

/// This device creates a lost file n again while another device does the same, and the other
/// create comes first. R holds n when the result comes, and the server refused this create with
/// ID_EXISTS. n is no problem, and the next commit sends L's name as a move.
#[test]
fn a_create_refused_because_another_device_created_the_node_again_is_no_problem() {
    let mut trees = trees(&[("n", Some(file(ROOT, "a", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
    trees.set_remote("n".into(), remote(file(ROOT, "b", 1)));
    let exists = OpResult {
        reject_reason: "ID_EXISTS".into(),
        ..outcome(op_result::Code::Rejected)
    };
    acknowledge(&mut commits, &mut trees, vec![exists]);
    assert!(commits.problems().is_empty());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "a")]);
}

/// GUN-378. n's move to b is in flight when the user and the other device both move it to c, so S
/// takes c. The user moves n back to b, and the commit is acknowledged at seq 2 while R already
/// holds the other device's move to c at seq 3, as after a log replay. S keeps c, the position the
/// server took after the commit. When the other device then moves n to d, the user's move back to b
/// is a local move of its own: the disk keeps n at b and the move is sent, instead of n being moved
/// to d. With `restart`, the commit side and the trees are restored from their stored state before
/// the acknowledgement. With `renamed`, the server gave the move another name: n is not reverted
/// to it, since S keeps the newer position.
fn a_late_acknowledgement_keeps_the_position_s_took_after_the_commit(restart: bool, renamed: bool) {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "b", 1)),
        Some(file(ROOT, "a", 1)),
        Some(remote(file(ROOT, "a", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 3));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "c", 1)));
    trees.set_local("n".into(), local(file(ROOT, "b", 1), 1));
    if restart {
        commits = Commits::from_state(commits.state()).unwrap();
        trees.restore(trees.state()).unwrap();
        trees.rules_scanned();
    }
    let applied = match renamed {
        true => OpResult {
            name: "b 2".into(),
            ..outcome(op_result::Code::AppliedRenamed)
        },
        false => outcome(op_result::Code::Applied),
    };
    commits.received(result(2, vec![applied])).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "c", 1)));
    assert!(!trees.reverted().contains("n"));
    trees.set_remote("n".into(), remote_at(file(ROOT, "d", 1), 4));
    trees.settle();
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "b", 1)));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
}

#[test]
fn a_late_acknowledgement_keeps_the_position_s_took_after_the_commit_was_built() {
    a_late_acknowledgement_keeps_the_position_s_took_after_the_commit(false, false);
}

#[test]
fn a_late_acknowledgement_after_a_restart_keeps_the_position_s_took_after_the_commit_was_built() {
    a_late_acknowledgement_keeps_the_position_s_took_after_the_commit(true, false);
}

#[test]
fn a_late_acknowledgement_renamed_by_the_server_keeps_the_position_s_took_after_the_commit_was_built()
 {
    a_late_acknowledgement_keeps_the_position_s_took_after_the_commit(false, true);
}

/// n's move to b is in flight with m's move to d, which the server rejects, so m is a problem and
/// S keeps it at c. The other device moves m to e, and both sides move n to c, so n's agreement
/// waits behind m. The user then edits n: the edit is uploaded while the position waits.
#[test]
fn an_edit_of_a_node_waiting_behind_a_problem_node_is_uploaded() {
    let mut trees = trees(&[
        (
            "n",
            Some(file(ROOT, "b", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        (
            "m",
            Some(file(ROOT, "d", 2)),
            Some(file(ROOT, "c", 2)),
            Some(remote(file(ROOT, "c", 2))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("m", ROOT, "d"), mv("n", ROOT, "b")]);
    let rejected = OpResult {
        reject_reason: "NAME_INVALID".into(),
        ..outcome(op_result::Code::Rejected)
    };
    trees.set_remote("m".into(), remote_at(file(ROOT, "e", 2), 3));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 4));
    trees.settle();
    let results = vec![rejected, outcome(op_result::Code::Applied)];
    commits.received(result(2, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 4));
    trees.settle();
    trees.set_local("n".into(), local(file(ROOT, "c", 9), 1));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids);
    assert_eq!(commit.as_ref().map(ops), Some(vec![update("n", 9)]));
}

/// As in `an_edit_of_a_node_waiting_behind_a_problem_node_is_uploaded`, n's agreement waits behind the problem node m, but the other device has
/// moved n on to x, and the user edits n at c before the effects run. The edit must be uploaded.
#[test]
fn an_edit_of_a_waiting_node_r_moved_on_from_is_uploaded() {
    let mut trees = trees(&[
        (
            "n",
            Some(file(ROOT, "b", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        (
            "m",
            Some(file(ROOT, "d", 2)),
            Some(file(ROOT, "c", 2)),
            Some(remote(file(ROOT, "c", 2))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("m", ROOT, "d"), mv("n", ROOT, "b")]);
    let rejected = OpResult {
        reject_reason: "NAME_INVALID".into(),
        ..outcome(op_result::Code::Rejected)
    };
    trees.set_remote("m".into(), remote_at(file(ROOT, "e", 2), 3));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 4));
    trees.settle();
    trees.set_remote("n".into(), remote_at(file(ROOT, "x", 1), 5));
    trees.settle();
    let results = vec![rejected, outcome(op_result::Code::Applied)];
    commits.received(result(2, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle();
    trees.set_local("n".into(), local(file(ROOT, "c", 9), 1));
    trees.settle();
    let mut sent = Vec::new();
    for seq in 10..14 {
        if let Some(commit) = build(&mut commits, &mut trees, &mut ids) {
            sent.extend(ops(&commit));
            let applied = vec![outcome(op_result::Code::Applied); commit.ops.len()];
            commits.received(result(seq, applied)).unwrap();
            assert!(commits.acknowledge(&mut trees, seq));
            trees.settle();
        }
        effects_for(&mut trees, &mut ids, "n");
    }
    assert!(sent.contains(&update("n", 9)), "{sent:?}");
}

/// n's move to b is in flight. The user and the other device move m from c to d with different
/// content, so S keeps m at c, and then both move n to c. The other device then moves n on to x.
/// S cannot take the agreed c while m holds it, so the agreement waits: n is not planned, and once
/// the rebase moves m off c, S takes c and the other device's move to x reaches the disk.
#[test]
fn an_agreed_position_another_nodes_stale_s_entry_holds_waits_for_it() {
    let mut trees = trees(&[
        (
            "n",
            Some(file(ROOT, "b", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        synced("m", file(ROOT, "c", 2)),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
    trees.set_local("m".into(), local(file(ROOT, "d", 3), 2));
    trees.set_remote("m".into(), remote_at(file(ROOT, "d", 4), 3));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 4));
    trees.settle();
    assert_eq!(trees.synced().live("m"), Some(&file(ROOT, "c", 2)));
    trees.set_remote("n".into(), remote_at(file(ROOT, "x", 1), 5));
    trees.settle();
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids);
    let sent: Vec<op::Op> = commit.iter().flat_map(ops).collect();
    assert!(
        !sent
            .iter()
            .any(|op| matches!(op, op::Op::Move(moved) if moved.node_id == "n")),
        "{sent:?}"
    );
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "c", 1)));
    assert_eq!(trees.class("n"), Class::Remote);
}

/// The commit moves n to p, m to q and k to r. Before its acknowledgement, the user and the other
/// device swap n and m, so they agree on n at q and m at p, and both move k to g, which S still
/// gives to h, since L and R moved h to h2 with different content. The other device then moves n on
/// to x. S cannot hold k at g yet, but the swap fits without k, so S takes it, k's agreement waits,
/// and n is not sent back to q over the other device's move.
#[test]
fn agreed_positions_that_fit_together_are_taken_when_another_agreement_is_refused() {
    let mut trees = trees(&[
        (
            "n",
            Some(file(ROOT, "p", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        (
            "m",
            Some(file(ROOT, "q", 2)),
            Some(file(ROOT, "b", 2)),
            Some(remote(file(ROOT, "b", 2))),
        ),
        (
            "k",
            Some(file(ROOT, "r", 3)),
            Some(file(ROOT, "e", 3)),
            Some(remote(file(ROOT, "e", 3))),
        ),
        synced("h", file(ROOT, "g", 4)),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let mut sent = ops(&commit);
    sent.sort_by_key(|op| format!("{op:?}"));
    let mut expected = [mv("k", ROOT, "r"), mv("m", ROOT, "q"), mv("n", ROOT, "p")];
    expected.sort_by_key(|op| format!("{op:?}"));
    assert_eq!(sent, expected);
    trees.set_local("h".into(), local(file(ROOT, "h2", 5), 4));
    trees.set_remote("h".into(), remote_at(file(ROOT, "h2", 6), 3));
    trees.set_local("n".into(), local(file(ROOT, "q", 1), 1));
    trees.set_local("m".into(), local(file(ROOT, "p", 2), 2));
    trees.set_local("k".into(), local(file(ROOT, "g", 3), 3));
    trees.set_remote("n".into(), remote_at(file(ROOT, "q", 1), 4));
    trees.set_remote("m".into(), remote_at(file(ROOT, "p", 2), 4));
    trees.set_remote("k".into(), remote_at(file(ROOT, "g", 3), 4));
    trees.settle();
    assert_eq!(trees.synced().live("h"), Some(&file(ROOT, "g", 4)));
    trees.set_remote("n".into(), remote_at(file(ROOT, "x", 1), 5));
    trees.settle();
    let applied = vec![outcome(op_result::Code::Applied); 3];
    commits.received(result(2, applied)).unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "q", 1)));
    assert_eq!(trees.synced().live("m"), Some(&file(ROOT, "p", 2)));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids);
    let sent: Vec<op::Op> = commit.iter().flat_map(ops).collect();
    assert!(
        !sent
            .iter()
            .any(|op| matches!(op, op::Op::Move(moved) if moved.node_id == "n")),
        "{sent:?}"
    );
    assert_eq!(trees.class("n"), Class::Remote);
    assert_eq!(trees.synced().live("k"), Some(&file(ROOT, "g", 3)));
}

/// n's move to b is in flight when the user and the other device both move it to c, in a commit
/// the server applied before this one, so S takes c. A log replay then brings the commit at seq 3
/// and the other device's move back to c at seq 4, which L and S already hold. The user moves n
/// back to b, and the commit is acknowledged. S keeps c, which the server holds after the commit,
/// so the user's move back to b is sent when the other device then moves n to d.
#[test]
fn a_late_acknowledgement_keeps_a_position_r_took_again_after_the_commit() {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "b", 1)),
        Some(file(ROOT, "a", 1)),
        Some(remote(file(ROOT, "a", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 2));
    trees.settle();
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 3));
    trees.settle();
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 4));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "c", 1)));
    trees.set_local("n".into(), local(file(ROOT, "b", 1), 1));
    commits
        .received(result(3, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 4));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "c", 1)));
    trees.set_remote("n".into(), remote_at(file(ROOT, "d", 1), 5));
    trees.settle();
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "b", 1)));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
}

/// n's move to b is in flight when the user and the other device both move it to c, in a commit
/// the server applied before this one, so S takes c. The result then puts n at b at seq 3, since
/// the server applies the later move. S takes the b that was sent, so the user's move to c is a
/// local move again: the disk keeps n at c and the move is sent.
#[test]
fn a_late_acknowledgement_takes_the_sent_position_over_one_s_took_from_before_the_commit() {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "b", 1)),
        Some(file(ROOT, "a", 1)),
        Some(remote(file(ROOT, "a", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 2));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "c", 1)));
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 3));
    commits
        .received(result(3, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "b", 1)));
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "c", 1)));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "c")]);
}

/// n's move to b is in flight when a log replay brings the commit at seq 2 and the other device's
/// move back to a, through each of `stops` first, settling after each record but the commit's. S still holds
/// the a it held when the commit was built, and L never took a again, so S takes the b that was
/// sent. The other device's later move then reaches the disk, and the move to b is not sent again.
fn a_late_acknowledgement_takes_the_sent_position_when_only_r_returned(stops: &[&str]) {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "b", 1)),
        Some(file(ROOT, "a", 1)),
        Some(remote(file(ROOT, "a", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 2));
    for (rev, name) in (3..).zip(stops.iter().chain(&["a"])) {
        trees.set_remote("n".into(), remote_at(file(ROOT, name, 1), rev));
        trees.settle();
    }
    let last = 3 + stops.len() as u64;
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, last));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "b", 1)));
    let effects = effects_for(&mut trees, &mut ids, "n");
    assert!(matches!(&effects[..], [Effect::Move { .. }]), "{effects:?}");
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "a", 1)));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_late_acknowledgement_takes_the_sent_position_when_only_r_returned_to_the_one_s_holds() {
    a_late_acknowledgement_takes_the_sent_position_when_only_r_returned(&[]);
}

#[test]
fn a_late_acknowledgement_takes_the_sent_position_when_r_returned_through_another_position() {
    a_late_acknowledgement_takes_the_sent_position_when_only_r_returned(&["x"]);
}

/// GUN-450. n is created at b, and the commit is in flight when the user and the other device both
/// move n to c. A log replay brings the commit at seq 2 and the other device's move at seq 3, so S
/// takes c, the position L and R agree on. The user moves n back to b, and the commit is
/// acknowledged. S keeps c, so when the other device then moves n to d, the user's move back to b
/// is sent instead of n being moved to d. With `settled`, the trees settle after the commit's
/// record as well. With `restart`, the commit side and the trees are restored from their stored
/// state before the acknowledgement. With `renamed`, the server created n as "b 2".
fn a_late_create_acknowledgement_keeps_the_position_s_took_after_the_commit(
    kind: Kind,
    settled: bool,
    restart: bool,
    renamed: bool,
) {
    let at = |name: &str| match kind {
        Kind::File => file(ROOT, name, 1),
        Kind::Dir => dir(ROOT, name),
    };
    let mut trees = trees(&[("n", Some(at("b")), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n")
            || matches!(&ops(&commit)[..], [op::Op::CreateDir(c)] if c.node_id == "n"),
        "{:?}",
        ops(&commit)
    );
    let created = if renamed { "b 2" } else { "b" };
    trees.set_remote("n".into(), remote_at(at(created), 2));
    if settled {
        trees.settle();
        assert_eq!(trees.synced().live("n"), Some(&at("b")));
    }
    trees.set_local("n".into(), local(at("c"), 1));
    trees.set_remote("n".into(), remote_at(at("c"), 3));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&at("c")));
    trees.set_local("n".into(), local(at("b"), 1));
    if restart {
        commits = Commits::from_state(commits.state()).unwrap();
        trees.restore(trees.state()).unwrap();
        trees.rules_scanned();
    }
    let applied = match renamed {
        true => OpResult {
            name: created.into(),
            ..outcome(op_result::Code::AppliedRenamed)
        },
        false => outcome(op_result::Code::Applied),
    };
    commits.received(result(2, vec![applied])).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&at("c")));
    assert!(!trees.reverted().contains("n"));
    trees.set_remote("n".into(), remote_at(at("d"), 4));
    trees.settle();
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    assert_eq!(trees.local().live("n"), Some(&at("b")));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
}

#[test]
fn a_late_file_create_acknowledgement_keeps_the_position_s_took_after_the_commit_was_built() {
    a_late_create_acknowledgement_keeps_the_position_s_took_after_the_commit(
        Kind::File,
        false,
        false,
        false,
    );
}

#[test]
fn a_late_folder_create_acknowledgement_keeps_the_position_s_took_after_the_commit_was_built() {
    a_late_create_acknowledgement_keeps_the_position_s_took_after_the_commit(
        Kind::Dir,
        false,
        false,
        false,
    );
}

#[test]
fn a_late_create_acknowledgement_keeps_the_position_s_took_after_settling_the_commits_record() {
    a_late_create_acknowledgement_keeps_the_position_s_took_after_the_commit(
        Kind::File,
        true,
        false,
        false,
    );
}

#[test]
fn a_late_create_acknowledgement_after_a_restart_keeps_the_position_s_took_after_the_commit_was_built()
 {
    a_late_create_acknowledgement_keeps_the_position_s_took_after_the_commit(
        Kind::File,
        false,
        true,
        false,
    );
}

#[test]
fn a_late_create_acknowledgement_renamed_by_the_server_keeps_the_position_s_took_after_the_commit()
{
    a_late_create_acknowledgement_keeps_the_position_s_took_after_the_commit(
        Kind::Dir,
        false,
        false,
        true,
    );
}

/// GUN-450. n's create at b is in flight when a log replay brings the commit at seq 2 and the
/// other device's move to c at seq 3. L never took c, so S takes the b that was sent. The other
/// device's move then reaches the disk, and nothing is sent.
#[test]
fn a_late_create_acknowledgement_takes_the_sent_position_when_only_r_moved_after_the_commit() {
    let mut trees = trees(&[("n", Some(file(ROOT, "b", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 2));
    trees.settle();
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 3));
    trees.settle();
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "b", 1)));
    let effects = effects_for(&mut trees, &mut ids, "n");
    assert!(matches!(&effects[..], [Effect::Move { .. }]), "{effects:?}");
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "c", 1)));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

/// GUN-450. n is synced at b with content 2 when the user edits it to content 3 and the server
/// purges it, so the next commit creates n again at b with content 3 while S still holds content 2.
/// The user reverts n to content 2 while the create is in flight, and a log replay brings the
/// create at seq 8 and the other device's moves to c and back to b. S keeps b, the position L and
/// R came back to, and takes the content that was sent, so the user's revert is sent and nothing
/// is downloaded.
#[test]
fn a_late_create_acknowledgement_that_keeps_a_position_takes_the_sent_content() {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "b", 3)),
        Some(file(ROOT, "b", 2)),
        Some(remote(file(ROOT, "b", 2))),
    )]);
    trees.remove_remote("n");
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n" && c.content == Some(content(3))),
        "{:?}",
        ops(&commit)
    );
    trees.set_local("n".into(), local(file(ROOT, "b", 2), 1));
    for (rev, name) in [(8, "b"), (9, "c"), (10, "b")] {
        let node = RemoteNode {
            content_rev: 8,
            ..remote_at(file(ROOT, name, 3), rev)
        };
        trees.set_remote("n".into(), node);
        trees.settle();
    }
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 10));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "b", 3)));
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "b", 2)));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::UpdateContent(u)] if u.node_id == "n" && u.content == Some(content(2))),
        "{:?}",
        ops(&commit)
    );
}

/// GUN-450. n is created at b with content 1, and before the acknowledgement the user and the
/// other device both move n to c and edit it to content 2 at seq 9, so S takes c with content 2.
/// The user moves n back to b and reverts it to content 1. S keeps the position and the content it
/// took after the create was sent, so the next commit sends both the move back and the revert.
#[test]
fn a_late_create_acknowledgement_keeps_the_content_s_took_after_the_commit() {
    let mut trees = trees(&[("n", Some(file(ROOT, "b", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n" && c.content == Some(content(1))),
        "{:?}",
        ops(&commit)
    );
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 8));
    trees.settle();
    trees.set_local("n".into(), local(file(ROOT, "c", 2), 1));
    let node = RemoteNode {
        content_rev: 9,
        ..remote_at(file(ROOT, "c", 2), 9)
    };
    trees.set_remote("n".into(), node);
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "c", 2)));
    trees.set_local("n".into(), local(file(ROOT, "b", 1), 1));
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 9));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "c", 2)));
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let sent = ops(&commit);
    assert!(sent.contains(&mv("n", ROOT, "b")), "{sent:?}");
    assert!(
        sent.iter().any(|op| matches!(op, op::Op::UpdateContent(u) if u.node_id == "n" && u.content == Some(content(1)))),
        "{sent:?}"
    );
}

/// GUN-453. n's `op` is in flight when the user moves n to c, and the other device moves n to c
/// and edits it to content 2 at seq 3, so L and R agree on the position but not on the content.
/// With `later`, the replay also holds the other device's later move to x at seq 4. After the
/// acknowledgement the effects and the commits take turns: the disk ends with R's content at R's
/// position, and the user's move to c, which the server already holds, is never sent again.
fn a_late_acknowledgement_converges_on_a_remote_edit_made_at_the_position_l_took(
    created: bool,
    later: bool,
) {
    let mut trees = match created {
        true => trees(&[("n", Some(file(ROOT, "b", 1)), None, None)]),
        false => trees(&[(
            "n",
            Some(file(ROOT, "b", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        )]),
    };
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if created && c.node_id == "n")
            || (!created && ops(&commit) == [mv("n", ROOT, "b")]),
        "{:?}",
        ops(&commit)
    );
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 2));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    let edited = |name: &str, rev: u64| RemoteNode {
        content_rev: 3,
        ..remote_at(file(ROOT, name, 2), rev)
    };
    trees.set_remote("n".into(), edited("c", 3));
    let (at, last) = match later {
        true => ("x", 4),
        false => ("c", 3),
    };
    if later {
        trees.set_remote("n".into(), edited("x", 4));
    }
    trees.settle();
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, last));
    trees.settle();
    for _ in 0..3 {
        effects_for(&mut trees, &mut ids, "n");
        let commit = build(&mut commits, &mut trees, &mut ids);
        assert_eq!(commit.as_ref().map(ops), None);
    }
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, at, 2)));
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, at, 2)));
}

#[test]
fn a_late_create_acknowledgement_converges_on_a_remote_edit_made_at_the_position_l_took() {
    a_late_acknowledgement_converges_on_a_remote_edit_made_at_the_position_l_took(true, false);
}

#[test]
fn a_late_create_acknowledgement_keeps_a_later_remote_move_after_a_remote_edit() {
    a_late_acknowledgement_converges_on_a_remote_edit_made_at_the_position_l_took(true, true);
}

#[test]
fn a_late_move_acknowledgement_converges_on_a_remote_edit_made_at_the_position_l_took() {
    a_late_acknowledgement_converges_on_a_remote_edit_made_at_the_position_l_took(false, false);
}

#[test]
fn a_late_move_acknowledgement_keeps_a_later_remote_move_after_a_remote_edit() {
    a_late_acknowledgement_converges_on_a_remote_edit_made_at_the_position_l_took(false, true);
}

/// GUN-453. n's move to b is in flight when the other device moves n to c and edits it at seq 3,
/// and the user then moves n to c too, before the other device moves n on to x at seq 4. L took
/// the position R already held, so that is where they agreed. After the acknowledgement the disk
/// follows n to x with R's content, and the user's move to c is never sent again.
#[test]
fn a_late_move_acknowledgement_keeps_a_later_remote_move_after_l_took_the_position_r_held() {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "b", 1)),
        Some(file(ROOT, "a", 1)),
        Some(remote(file(ROOT, "a", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
    let edited = |name: &str, rev: u64| RemoteNode {
        content_rev: 3,
        ..remote_at(file(ROOT, name, 2), rev)
    };
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 2));
    trees.set_remote("n".into(), edited("c", 3));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), edited("x", 4));
    trees.settle();
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 4));
    trees.settle();
    for _ in 0..3 {
        effects_for(&mut trees, &mut ids, "n");
        let commit = build(&mut commits, &mut trees, &mut ids);
        assert_eq!(commit.as_ref().map(ops), None);
    }
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "x", 2)));
}

/// GUN-453. n's move to b is in flight when the user and the other device both move n to c, and
/// the other device then moves n on to x and m to c, which the user does too. The user moves n back
/// to a. S cannot take c for n, since m holds it, so S takes the b that was sent, and the user's
/// move back to a is sent rather than the disk following n to x.
#[test]
fn a_late_move_acknowledgement_takes_the_sent_position_when_another_node_took_the_agreed_one() {
    let mut trees = trees(&[
        (
            "n",
            Some(file(ROOT, "b", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        (
            "m",
            Some(file(ROOT, "z", 2)),
            Some(file(ROOT, "z", 2)),
            Some(remote(file(ROOT, "z", 2))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 2));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 3));
    trees.set_remote("n".into(), remote_at(file(ROOT, "x", 1), 4));
    trees.set_local("n".into(), local(file(ROOT, "a", 1), 1));
    trees.set_local("m".into(), local(file(ROOT, "c", 2), 2));
    trees.set_remote("m".into(), remote_at(file(ROOT, "c", 2), 5));
    trees.settle();
    assert_eq!(trees.synced().live("m"), Some(&file(ROOT, "c", 2)));
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "b", 1)));
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "a", 1)));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "a")]);
}

/// GUN-453. One commit moves n to b and m to c. Before the acknowledgement, the user and the other
/// device both move n to c, which the commit gives to m, and the other device moves n on to x. The
/// user moves n back to a. S cannot hold both n and m at c, so n takes the b that was sent, and the
/// user's move back to a is sent rather than the disk following n to x.
#[test]
fn a_late_move_acknowledgement_takes_the_sent_position_when_the_commit_gives_the_agreed_one_away() {
    let mut trees = trees(&[
        (
            "n",
            Some(file(ROOT, "b", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        (
            "m",
            Some(file(ROOT, "c", 2)),
            Some(file(ROOT, "z", 2)),
            Some(remote(file(ROOT, "z", 2))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let sent = ops(&commit);
    assert!(
        sent.contains(&mv("n", ROOT, "b")) && sent.contains(&mv("m", ROOT, "c")),
        "{sent:?}"
    );
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 2));
    trees.set_remote("m".into(), remote_at(file(ROOT, "c", 2), 2));
    trees.set_local("m".into(), local(file(ROOT, "w", 2), 2));
    trees.set_remote("m".into(), remote_at(file(ROOT, "v", 2), 3));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 4));
    trees.set_remote("n".into(), remote_at(file(ROOT, "x", 1), 5));
    trees.set_local("n".into(), local(file(ROOT, "a", 1), 1));
    trees.settle();
    let applied = sent
        .iter()
        .map(|_| outcome(op_result::Code::Applied))
        .collect();
    commits.received(result(2, applied)).unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "b", 1)));
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "a", 1)));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let sent = ops(&commit);
    assert!(sent.contains(&mv("n", ROOT, "a")), "{sent:?}");
}

/// GUN-453. As above, with the user moving n back to b, the position the commit sent, and the
/// server creating it as "b 2". S cannot hold c for n, so n takes the server's name and is
/// reverted, so the disk takes that name too.
#[test]
fn a_late_move_acknowledgement_renamed_by_the_server_is_reverted_when_another_node_took_the_agreed_position()
 {
    let mut trees = trees(&[
        (
            "n",
            Some(file(ROOT, "b", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        (
            "m",
            Some(file(ROOT, "z", 2)),
            Some(file(ROOT, "z", 2)),
            Some(remote(file(ROOT, "z", 2))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
    trees.set_remote("n".into(), remote_at(file(ROOT, "b 2", 1), 2));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 3));
    trees.set_remote("n".into(), remote_at(file(ROOT, "x", 1), 4));
    trees.set_local("n".into(), local(file(ROOT, "b", 1), 1));
    trees.set_local("m".into(), local(file(ROOT, "c", 2), 2));
    trees.set_remote("m".into(), remote_at(file(ROOT, "c", 2), 5));
    trees.settle();
    let renamed = OpResult {
        name: "b 2".into(),
        ..outcome(op_result::Code::AppliedRenamed)
    };
    commits.received(result(2, vec![renamed])).unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "b 2", 1)));
    assert!(trees.reverted().contains("n"));
}

/// GUN-453. n's create at b is in flight when L and R agree on c with different contents, so S
/// never takes n. The other device then deletes n and the server purges it, all before the result
/// arrives. S takes c, the position L and R agreed on, so the disk follows the purge and the create
/// is not sent again.
#[test]
fn a_late_create_acknowledgement_follows_a_purge_after_l_and_r_agreed_on_a_position() {
    let mut trees = trees(&[("n", Some(file(ROOT, "b", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 8));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    let edited = RemoteNode {
        content_rev: 9,
        ..remote_at(file(ROOT, "c", 2), 9)
    };
    trees.set_remote("n".into(), edited.clone());
    trees.set_remote(
        "n".into(),
        RemoteNode {
            meta_rev: 10,
            trashed: true,
            trash_group: "10:0".into(),
            ..edited
        },
    );
    trees.remove_remote("n");
    trees.settle();
    assert_eq!(trees.synced().get("n"), None);
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 11));
    trees.settle();
    for _ in 0..3 {
        effects_for(&mut trees, &mut ids, "n");
        let commit = build(&mut commits, &mut trees, &mut ids);
        assert_eq!(commit.as_ref().map(ops), None);
    }
    assert!(trees.local().get("n").is_none());
}

/// GUN-453. The ack of a create or move of n whose agreed position c may collide with other nodes:
/// none, a sibling S already holds at c, or another node the same commit moves to c, with its op
/// before or after n's. With `edited`, the commit also updates n's content. With `purged`, R
/// deletes and purges n before the result arrives. The
/// user moves n to y and the other node back to z after the agreement. Every applied node ends
/// with what it sent or with its agreed position, never with S from before the commit, n takes c
/// whenever no other node of S holds it, and n keeps the content the commit sent either way.
#[test]
fn a_late_acknowledgement_takes_an_agreed_position_only_where_s_can_hold_it() {
    #[derive(Clone, Copy, Debug)]
    enum Other {
        None,
        InS,
        Sent(&'static str),
    }
    let others = [Other::None, Other::InS, Other::Sent("m"), Other::Sent("q")];
    let cases = [(true, false), (false, false), (false, true)];
    for (created, edited) in cases {
        for other in others {
            for purged in [false, true] {
                let case =
                    format!("created={created} edited={edited} other={other:?} purged={purged}");
                let digest = if edited { 2 } else { 1 };
                let n = match created {
                    true => ("n", Some(file(ROOT, "b", 1)), None, None),
                    false => (
                        "n",
                        Some(file(ROOT, "b", digest)),
                        Some(file(ROOT, "a", 1)),
                        Some(remote(file(ROOT, "a", 1))),
                    ),
                };
                let o = match other {
                    Other::Sent(id) => Some((
                        id,
                        Some(file(ROOT, "c", 2)),
                        Some(file(ROOT, "z", 2)),
                        Some(remote(file(ROOT, "z", 2))),
                    )),
                    Other::InS => Some((
                        "m",
                        Some(file(ROOT, "z", 2)),
                        Some(file(ROOT, "z", 2)),
                        Some(remote(file(ROOT, "z", 2))),
                    )),
                    Other::None => None,
                };
                let mut trees = trees(&[Some(n), o].into_iter().flatten().collect::<Vec<_>>());
                let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
                let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
                let sent = ops(&commit);
                let at = |op: &op::Op| match op {
                    op::Op::Move(m) => m.node_id.clone(),
                    op::Op::CreateFile(c) => c.node_id.clone(),
                    op::Op::UpdateContent(u) => u.node_id.clone(),
                    other => panic!("{case}: {other:?}"),
                };
                let order: Vec<String> = sent.iter().map(at).collect();
                assert_eq!(
                    order.contains(&"n".to_owned()) && edited,
                    sent.contains(&update("n", 2)),
                    "{case}"
                );
                if let Other::Sent(id) = other {
                    let first = |id: &str| order.iter().position(|other| other == id);
                    assert_eq!(first(id) < first("n"), id < "n", "{case}: {order:?}");
                }
                trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 2));
                if let Other::Sent(id) = other {
                    trees.set_remote(id.into(), remote_at(file(ROOT, "c", 2), 2));
                    trees.set_local(id.into(), local(file(ROOT, "w", 2), 2));
                    trees.set_remote(id.into(), remote_at(file(ROOT, "v", 2), 3));
                }
                trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
                let edited = RemoteNode {
                    content_rev: 4,
                    ..remote_at(file(ROOT, "c", 3), 4)
                };
                trees.set_remote("n".into(), edited.clone());
                trees.set_remote(
                    "n".into(),
                    RemoteNode {
                        meta_rev: 5,
                        ..remote_at(file(ROOT, "x", 3), 5)
                    },
                );
                trees.set_local("n".into(), local(file(ROOT, "y", 1), 1));
                if let Other::Sent(id) = other {
                    trees.set_local(id.into(), local(file(ROOT, "z", 2), 2));
                }
                if let Other::InS = other {
                    trees.set_local("m".into(), local(file(ROOT, "c", 2), 2));
                    trees.set_remote("m".into(), remote_at(file(ROOT, "c", 2), 6));
                }
                if purged {
                    trees.set_remote(
                        "n".into(),
                        RemoteNode {
                            meta_rev: 7,
                            trashed: true,
                            trash_group: "7:0".into(),
                            ..remote_at(file(ROOT, "x", 3), 7)
                        },
                    );
                    trees.remove_remote("n");
                }
                trees.settle();
                let applied = sent
                    .iter()
                    .map(|_| outcome(op_result::Code::Applied))
                    .collect();
                commits.received(result(2, applied)).unwrap();
                assert!(commits.acknowledge(&mut trees, 8), "{case}");
                let held = |trees: &Trees, id: &str| match trees.waiting().get(id) {
                    Some(waiting) => waiting.shape.clone(),
                    None => trees.synced().get(id).cloned(),
                };
                let position = |shape: Option<Shape>| shape.map(|s| (s.parent, s.name));
                let s = |name: &str| Some((ROOT.to_owned(), name.to_owned()));
                if let Other::Sent(id) = other {
                    assert_eq!(position(held(&trees, id)), s("c"), "{case}: {id}");
                }
                let free = !(trees.synced().iter())
                    .any(|(id, node)| id != "n" && node.parent == ROOT && node.name == "c");
                let expected = match free {
                    true => s("c"),
                    false => s("b"),
                };
                let n = held(&trees, "n");
                assert_eq!(
                    n.as_ref().and_then(|s| s.content.clone()),
                    Some(content(digest)),
                    "{case}"
                );
                assert_eq!(position(n), expected, "{case}");
            }
        }
    }
}

/// GUN-453. One commit moves n to b and updates its content to 2. Before the acknowledgement, the
/// user and the other device both move n to c, which another node m then takes, and the user moves
/// n back to b and reverts its content to 1. S cannot hold c for n, so n keeps the b that was sent
/// with the content 2 that was sent, and the user's content 1 is uploaded rather than overwritten.
#[test]
fn a_late_acknowledgement_keeps_the_sent_content_when_s_cannot_hold_the_agreed_position() {
    let mut trees = trees(&[
        (
            "n",
            Some(file(ROOT, "b", 2)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        (
            "m",
            Some(file(ROOT, "z", 9)),
            Some(file(ROOT, "z", 9)),
            Some(remote(file(ROOT, "z", 9))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b"), update("n", 2)]);
    let at = |name: &str, digest: u8, rev: u64, content_rev: u64| RemoteNode {
        content_rev,
        ..remote_at(file(ROOT, name, digest), rev)
    };
    trees.set_remote("n".into(), at("b", 2, 2, 2));
    trees.set_local("n".into(), local(file(ROOT, "c", 2), 1));
    trees.set_remote("n".into(), at("c", 2, 3, 2));
    trees.set_remote("n".into(), at("x", 3, 4, 4));
    trees.set_local("n".into(), local(file(ROOT, "b", 1), 1));
    trees.set_local("m".into(), local(file(ROOT, "c", 9), 2));
    trees.set_remote("m".into(), remote_at(file(ROOT, "c", 9), 5));
    trees.settle();
    let applied = vec![outcome(op_result::Code::Applied); 2];
    commits.received(result(2, applied)).unwrap();
    assert!(commits.acknowledge(&mut trees, 5));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "b", 2)));
    effects_for(&mut trees, &mut ids, "n");
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "b", 1)));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let uploaded = |op: &op::Op| match op {
        op::Op::CreateFile(c) => c.content == Some(content(1)),
        op::Op::UpdateContent(u) => u.content == Some(content(1)),
        _ => false,
    };
    assert!(ops(&commit).iter().any(uploaded), "{:?}", ops(&commit));
}

/// GUN-453. n's move to b is in flight when L and R agree on c at seq 3 and the other device moves
/// n back to a at seq 4. The server then loses an unrelated change at seq 5, and a bootstrap
/// replaces R. The agreement at seq 3 is from before the loss, so it stays: S takes c and the disk
/// follows the other device's move back to a.
#[test]
fn a_late_acknowledgement_after_a_rollback_keeps_an_agreement_the_server_kept() {
    let mut trees = trees(&[(
        "n",
        Some(file(ROOT, "b", 1)),
        Some(file(ROOT, "a", 1)),
        Some(remote(file(ROOT, "a", 1))),
    )]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 2));
    trees.set_remote("n".into(), remote_at(file(ROOT, "c", 1), 3));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "a", 1), 4));
    trees.set_remote("u".into(), remote_at(file(ROOT, "u", 3), 5));
    trees.settle();
    trees.remove_remote("u");
    trees.forget_lost(Vec::new(), Vec::new(), 4, commits.accepted());
    trees.settle();
    commits
        .received(result(2, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 4));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "c", 1)));
    let effects = effects_for(&mut trees, &mut ids, "n");
    assert!(matches!(&effects[..], [Effect::Move { .. }]), "{effects:?}");
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "a", 1)));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

/// GUN-453. n's create at b is applied at seq 8, L and R agree on c at seq 10, and the user moves
/// n back to b. The server then loses commits after seq 11, none of them n's, so the bootstrap's R
/// still holds n at c from seq 10. That agreement survived, so S takes c and the user's move back
/// to b is sent rather than undone.
#[test]
fn a_late_create_acknowledgement_after_a_rollback_keeps_an_agreement_r_still_holds() {
    let mut trees = trees(&[("n", Some(file(ROOT, "b", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 8));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote(
        "n".into(),
        RemoteNode {
            content_rev: 10,
            ..remote_at(file(ROOT, "c", 2), 10)
        },
    );
    trees.set_local("n".into(), local(file(ROOT, "b", 1), 1));
    trees.settle();
    trees.forget_lost(Vec::new(), Vec::new(), 0, commits.accepted());
    trees.settle();
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 11));
    trees.settle();
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "b")]);
}

/// GUN-457. n's create at b is applied at seq 8, and L and R agree on c at seq 10. The server then
/// loses every commit after seq 7, so the bootstrap's R lacks n, and the create sent again is
/// applied at seq 9. The record that held the agreement is gone, so S takes b from the result and
/// the user's move to c is sent rather than undone.
#[test]
fn a_late_create_acknowledgement_after_a_rollback_drops_the_agreement_the_server_lost() {
    let mut trees = trees(&[("n", Some(file(ROOT, "b", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 8));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote(
        "n".into(),
        RemoteNode {
            content_rev: 10,
            ..remote_at(file(ROOT, "c", 2), 10)
        },
    );
    trees.settle();
    trees.remove_remote("n");
    trees.forget_lost(Vec::new(), Vec::new(), 7, commits.accepted());
    trees.settle();
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 9));
    commits
        .received(result(9, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 9));
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "b", 1)));
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "c", 1)));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "c")]);
}

/// GUN-457. As above, L and R agree on c at seq 10, and the server loses every commit after seq 7.
/// Its log then reaches seq 12 again with another device's edit of n at b. A revision above the
/// agreed one does not show that the record holding the agreement survived, so the agreement is
/// dropped: the late acknowledgement leaves n at b, and the user's move to c is sent.
#[test]
fn a_reused_revision_does_not_keep_an_agreement_the_server_lost() {
    let mut trees = trees(&[("n", Some(file(ROOT, "b", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
    trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 8));
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
    trees.set_remote(
        "n".into(),
        RemoteNode {
            content_rev: 10,
            ..remote_at(file(ROOT, "c", 2), 10)
        },
    );
    trees.settle();
    trees.remove_remote("n");
    trees.set_remote(
        "n".into(),
        RemoteNode {
            content_rev: 12,
            ..remote_at(file(ROOT, "b", 2), 12)
        },
    );
    trees.forget_lost(Vec::new(), Vec::new(), 7, commits.accepted());
    trees.settle();
    commits
        .received(result(9, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 12));
    trees.settle();
    assert_eq!(effects_for(&mut trees, &mut ids, "n"), []);
    let next = (commits
        .build(&mut trees, &mut ids, NOW, HISTORY, 12)
        .cloned())
    .unwrap();
    assert_eq!(ops(&next), [mv("n", ROOT, "c")]);
}

/// GUN-457: after a loss, an agreement on c at revision 10 is kept only when the server provably
/// holds its record: its revision is at or below `held_through`, or the new R holds the node live at
/// c with that very revision. A later revision at c, another name, the trash or no record drops it.
#[test]
fn an_agreement_survives_a_loss_only_on_a_kept_seq_or_its_exact_record() {
    for (held_through, rev, name, trashed, kept) in [
        (10, 12, "z", false, true),
        (9, 10, "c", false, true),
        (9, 10, "z", false, false),
        (9, 12, "c", false, false),
        (9, 10, "c", true, false),
        (9, 0, "missing", false, false),
    ] {
        let mut trees = trees(&[("n", Some(file(ROOT, "b", 1)), None, None)]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        build(&mut commits, &mut trees, &mut ids).unwrap();
        trees.set_remote("n".into(), remote_at(file(ROOT, "b", 1), 8));
        trees.set_local("n".into(), local(file(ROOT, "c", 1), 1));
        trees.set_remote("n".into(), remote_at(file(ROOT, "c", 2), 10));
        assert_eq!(trees.position_after("n", 8).unwrap().rev, 10);
        trees.set_local("n".into(), local(file(ROOT, "d", 1), 1));
        trees.remove_remote("n");
        if rev != 0 {
            trees.set_remote(
                "n".into(),
                RemoteNode {
                    trashed,
                    trash_group: if trashed { "g:0".into() } else { String::new() },
                    ..remote_at(file(ROOT, name, 2), rev)
                },
            );
        }
        trees.forget_lost(Vec::new(), Vec::new(), held_through, commits.accepted());
        trees.settle();
        assert_eq!(
            trees.position_after("n", 8).map(|agreed| agreed.rev),
            kept.then_some(10),
            "held_through={held_through} rev={rev} name={name} trashed={trashed}"
        );
    }
}
