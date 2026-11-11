// SPDX-License-Identifier: Apache-2.0

//! The mass-delete guard (design section 6.1) as the commit planner applies it.

use flarebox_engine::classify::{Class, Trees};
use flarebox_engine::commit::Commits;
use flarebox_engine::guard::{Limit, WINDOW_MS, Window};
use flarebox_engine::ids::IdGen;
use flarebox_engine::question::{Answer, Held, Question, QuestionKind, RecoveryGroup};
use flarebox_engine::state::CommitsState;
use flarebox_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};
use flarebox_proto::v1::{CommitResult, OpResult, op, op_result};

const ROOT: &str = "root";
const NOW: u64 = 1_794_268_740_000;

fn shape(parent: &str, name: &str, kind: Kind) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind,
        content: (kind == Kind::File).then(|| Content {
            size: 3,
            manifest_digest: vec![1; 32],
            ..Default::default()
        }),
    }
}

/// A space in sync: `loose` files at the root and a folder `d` holding `inside` files. Node IDs
/// are the paths.
fn space(loose: usize, inside: usize) -> Trees {
    let mut trees = Trees::new(ROOT);
    let mut nodes = vec![("d".to_owned(), shape(ROOT, "d", Kind::Dir))];
    nodes.extend((0..loose).map(|i| (format!("f{i}"), shape(ROOT, &format!("f{i}"), Kind::File))));
    nodes
        .extend((0..inside).map(|i| (format!("d/g{i}"), shape("d", &format!("g{i}"), Kind::File))));
    for (ino, (id, shape)) in nodes.into_iter().enumerate() {
        put_back(&mut trees, &id, shape.clone(), ino as u64 + 1);
        trees.set_synced(id.clone(), Some(shape.clone()));
        trees.set_remote(
            id,
            RemoteNode {
                shape,
                trashed: false,
                content_rev: 1,
                meta_rev: 1,
                trash_group: String::new(),
            },
        );
    }
    trees.settle();
    trees
}

fn put_back(trees: &mut Trees, id: &str, shape: Shape, ino: u64) {
    let identity = FileIdentity {
        dev: 1,
        ino,
        birth_ns: Some(0),
    };
    trees.set_local(
        id.into(),
        LocalNode {
            shape,
            identity,
            parked: false,
        },
    );
}

/// Removes `ids` from the disk, each with everything below it.
fn remove(trees: &mut Trees, ids: &[&str]) {
    for id in ids {
        let below: Vec<String> = (trees.local().iter())
            .filter(|(other, _)| other.starts_with(&format!("{id}/")))
            .map(|(other, _)| other.clone())
            .collect();
        for other in below {
            trees.remove_local(&other);
        }
        trees.remove_local(id);
    }
    trees.settle();
}

fn root_files(range: std::ops::Range<usize>) -> Vec<String> {
    range.map(|i| format!("f{i}")).collect()
}

fn build_at(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen, now: u64) -> Vec<op::Op> {
    let commit = commits.build(trees, ids, now, "history", 7).cloned();
    trees.settle();
    commit.map_or_else(Vec::new, |commit| {
        commit.ops.into_iter().map(|op| op.op.unwrap()).collect()
    })
}

fn deleted(ops: &[op::Op]) -> Vec<String> {
    let mut deleted: Vec<String> = (ops.iter())
        .filter_map(|op| match op {
            op::Op::Delete(delete) => Some(delete.node_id.clone()),
            _ => None,
        })
        .collect();
    deleted.sort();
    deleted
}

fn acknowledge(commits: &mut Commits, trees: &mut Trees, seq: u64) {
    let ops = commits.in_flight().unwrap().commit.ops.len();
    let ok = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    commits
        .received(CommitResult {
            seq,
            results: vec![ok; ops],
            changes: vec![],
        })
        .unwrap();
    assert!(commits.acknowledge(trees, seq));
    trees.settle();
}

/// The guard's question, the only one these tests expect.
fn asked(commits: &Commits) -> Option<&Question> {
    match commits.questions() {
        [] => None,
        [question] => Some(question),
        more => panic!("one question at most: {more:?}"),
    }
}

/// What the question counted.
fn window(question: &Question) -> Window {
    let QuestionKind::MassDelete {
        deletions,
        affected_files,
        space_files,
    } = question.kind
    else {
        panic!("a mass-delete question: {question:?}");
    };
    Window {
        deletions,
        affected_files,
        space_files,
    }
}

fn held(commits: &Commits) -> Vec<String> {
    asked(commits)
        .map(|question| question.held.keys().cloned().collect())
        .unwrap_or_default()
}

#[test]
fn a_removed_folder_holding_most_of_the_space_is_held_while_the_rest_syncs() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    put_back(&mut trees, "new", shape(ROOT, "new", Kind::File), 99);
    trees.settle();

    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW);

    assert_eq!(deleted(&ops), Vec::<String>::new(), "nothing is deleted");
    assert!(
        matches!(&ops[..], [op::Op::CreateFile(create)] if create.node_id == "new"),
        "the new file is still uploaded: {ops:?}"
    );
    let question = asked(&commits).expect("a question");
    assert_eq!(window(question).limit(), Some(Limit::Share));
    assert_eq!(
        window(question),
        Window {
            deletions: 1,
            affected_files: 15,
            space_files: 25,
        },
        "a folder is one deletion of the files it holds"
    );
    assert_eq!(held(&commits), ["d"]);
    assert_eq!(question.held["d"].members.len(), 16);
}

#[test]
fn deletions_below_the_limits_are_committed() {
    let mut trees = space(25, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["f0", "f1", "f2"]);

    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW);

    assert_eq!(deleted(&ops), root_files(0..3));
    assert_eq!(asked(&commits), None);
}

#[test]
fn half_of_the_space_is_not_more_than_half() {
    let mut trees = space(20, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let half = root_files(0..10);
    remove(
        &mut trees,
        &half.iter().map(String::as_str).collect::<Vec<_>>(),
    );

    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW);

    assert_eq!(deleted(&ops).len(), 10);
    assert_eq!(asked(&commits), None);
}

#[test]
fn a_space_of_fewer_than_20_files_may_lose_all_of_them() {
    let mut trees = space(19, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let all = root_files(0..19);
    remove(
        &mut trees,
        &all.iter().map(String::as_str).collect::<Vec<_>>(),
    );

    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW);

    assert_eq!(deleted(&ops).len(), 19);
    assert_eq!(asked(&commits), None);
}

#[test]
fn more_than_1000_deletions_are_held_in_a_large_space_and_1000_are_not() {
    let names = root_files(0..1001);
    let names: Vec<&str> = names.iter().map(String::as_str).collect();

    let mut trees = space(5000, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &names[..1000]);
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW);
    assert_eq!(deleted(&ops).len(), 1000);
    assert_eq!(asked(&commits), None);

    let mut trees = space(5000, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &names);
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW);
    assert_eq!(deleted(&ops), Vec::<String>::new());
    let question = asked(&commits).expect("a question");
    assert_eq!(window(question).limit(), Some(Limit::Deletions));
    assert_eq!(question.held.len(), 1001, "every pending deletion is held");
}

/// A space of 25 files that lost 10 of them in a commit acknowledged at `NOW`, and 3 more since.
fn ten_deleted_then_three() -> (Trees, Commits, IdGen) {
    let mut trees = space(25, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = root_files(0..10);
    remove(
        &mut trees,
        &first.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert_eq!(
        deleted(&build_at(&mut commits, &mut trees, &mut ids, NOW)).len(),
        10
    );
    acknowledge(&mut commits, &mut trees, 1);
    remove(&mut trees, &["f10", "f11", "f12"]);
    (trees, commits, ids)
}

#[test]
fn deletions_add_up_within_the_window() {
    let (mut trees, mut commits, mut ids) = ten_deleted_then_three();

    let late = NOW + WINDOW_MS - 1;
    assert_eq!(
        deleted(&build_at(&mut commits, &mut trees, &mut ids, late)),
        Vec::<String>::new()
    );
    let question = asked(&commits).expect("13 of 25 files in the window");
    assert_eq!(
        window(question),
        Window {
            deletions: 13,
            affected_files: 13,
            space_files: 25,
        },
        "the space is counted as it was before the window's deletions"
    );
    let id = question.id.clone();

    let much_later = NOW + 10 * WINDOW_MS;
    assert_eq!(
        deleted(&build_at(&mut commits, &mut trees, &mut ids, much_later)),
        Vec::<String>::new()
    );
    assert_eq!(
        asked(&commits).unwrap().id,
        id,
        "a question stays until it is answered"
    );
}

#[test]
fn deletions_after_the_window_count_on_their_own() {
    let (mut trees, mut commits, mut ids) = ten_deleted_then_three();

    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + WINDOW_MS);

    assert_eq!(deleted(&ops), root_files(10..13));
    assert_eq!(asked(&commits), None);
}

#[test]
fn a_deletion_planned_again_counts_once() {
    let mut trees = space(25, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let some = root_files(0..12);
    remove(
        &mut trees,
        &some.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert_eq!(
        deleted(&build_at(&mut commits, &mut trees, &mut ids, NOW)).len(),
        12
    );

    let forgotten = CommitsState {
        in_flight: None,
        ..commits.state()
    };
    let mut commits = Commits::from_state(forgotten).unwrap();
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(deleted(&ops).len(), 12);
    assert_eq!(asked(&commits), None);
    assert_eq!(commits.state().guard.ledger.len(), 12);
}

#[test]
fn a_clock_that_moved_back_keeps_deletions_in_the_window_longer() {
    let mut trees = space(25, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = root_files(0..10);
    remove(
        &mut trees,
        &first.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    build_at(&mut commits, &mut trees, &mut ids, NOW + WINDOW_MS);
    acknowledge(&mut commits, &mut trees, 1);

    remove(&mut trees, &["f10", "f11", "f12"]);
    let late = NOW + WINDOW_MS - 1;
    assert_eq!(
        deleted(&build_at(&mut commits, &mut trees, &mut ids, late)),
        Vec::<String>::new()
    );
    assert!(
        asked(&commits).is_some(),
        "counted as made at the earlier time"
    );
}

#[test]
fn deleting_answers_the_question_and_commits_without_counting_again() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let id = asked(&commits).unwrap().id.clone();

    assert!(commits.decide(&id, Answer::Delete));
    assert!(!commits.decide(&id, Answer::Delete), "answered once");
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(deleted(&ops), ["d"]);
    assert_eq!(asked(&commits), None);
    acknowledge(&mut commits, &mut trees, 1);

    remove(&mut trees, &["f0"]);
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 2);
    assert_eq!(
        deleted(&ops),
        ["f0"],
        "the confirmed deletion does not count"
    );
    assert!(
        commits.state().guard.confirmed.is_empty(),
        "done once acknowledged"
    );
}

#[test]
fn restoring_answers_the_question_and_downloads_the_held_nodes_again() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let id = asked(&commits).unwrap().id.clone();

    assert!(commits.decide(&id, Answer::Restore));
    assert_eq!(
        deleted(&build_at(&mut commits, &mut trees, &mut ids, NOW + 1)),
        Vec::<String>::new()
    );

    for node in ["d", "d/g0", "d/g14"] {
        assert_eq!(trees.synced().live(node), None, "{node} left S");
        assert_eq!(trees.class(node), Class::Remote, "{node} is downloaded");
    }
    assert_eq!(asked(&commits), None);
    assert!(commits.state().guard.ledger.is_empty());
}

#[test]
fn more_deletions_replace_the_question_and_an_answer_to_the_old_one_does_nothing() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let first = asked(&commits).unwrap().id.clone();

    remove(&mut trees, &["f0"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(held(&commits), ["d", "f0"]);
    assert_ne!(asked(&commits).unwrap().id, first);
    assert!(!commits.decide(&first, Answer::Delete));
    assert_eq!(held(&commits), ["d", "f0"]);
}

#[test]
fn putting_the_files_back_ends_the_question() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    assert!(asked(&commits).is_some());

    put_back(&mut trees, "d", shape(ROOT, "d", Kind::Dir), 1);
    for i in 0..15 {
        put_back(
            &mut trees,
            &format!("d/g{i}"),
            shape("d", &format!("g{i}"), Kind::File),
            100 + i,
        );
    }
    trees.settle();
    build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(asked(&commits), None);
    assert!(commits.state().guard.ledger.is_empty());
}

#[test]
fn a_held_file_changed_on_the_server_leaves_the_question_and_is_downloaded() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(
        &mut trees,
        &[
            "f0", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "d/g0", "d/g1", "d/g2",
        ],
    );
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    assert_eq!(held(&commits).len(), 13);

    let mut edited = shape(ROOT, "f0", Kind::File);
    edited.content.as_mut().unwrap().manifest_digest = vec![2; 32];
    trees.set_remote(
        "f0".into(),
        RemoteNode {
            shape: edited,
            trashed: false,
            content_rev: 2,
            meta_rev: 1,
            trash_group: String::new(),
        },
    );
    trees.settle();
    build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    let question = asked(&commits).expect("12 of 25 files are still held");
    assert!(!question.held.contains_key("f0"));
    assert_eq!(question.held.len(), 12);
    assert_ne!(
        trees.class("f0"),
        Class::Local,
        "f0 is not a local delete any more"
    );
}

#[test]
fn nothing_is_counted_while_committing_has_stopped() {
    let mut trees = space(10, 15);
    let mut commits = Commits::from_state(CommitsState {
        halted: Some(flarebox_engine::commit::Halt::Recovery),
        ..Default::default()
    })
    .unwrap();
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut IdGen::new(1), NOW);
    assert_eq!(asked(&commits), None);
    assert!(commits.state().guard.ledger.is_empty());
}

/// An open question of another kind, as GUN-324's first-link hold will ask, holding `d` and
/// everything below it.
fn recovery_question(trees: &Trees) -> CommitsState {
    let members = (trees.synced().iter())
        .filter(|(id, _)| id.as_str() == "d" || id.starts_with("d/"))
        .map(|(id, _)| (id.clone(), None))
        .collect();
    CommitsState {
        questions: vec![Question {
            id: "other".into(),
            asked_at_ms: NOW,
            kind: QuestionKind::Recovery {
                group: RecoveryGroup::CreatedOnThisDevice,
            },
            held: [("d".into(), Held { members, files: 15 })].into(),
            answer: None,
        }],
        ..Default::default()
    }
}

#[test]
fn nodes_an_open_question_of_any_kind_holds_get_no_op_and_are_not_counted() {
    let mut trees = space(10, 15);
    let mut commits = Commits::from_state(recovery_question(&trees)).unwrap();
    let mut ids = IdGen::new(1);
    remove(&mut trees, &["d", "f0"]);

    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW);

    assert_eq!(deleted(&ops), ["f0"], "only the deletion no question holds");
    assert_eq!(commits.questions().len(), 1, "the guard does not ask");
    assert_eq!(commits.state().guard.ledger.len(), 1);
}

#[test]
fn an_answer_to_a_kind_the_engine_does_not_take_yet_changes_nothing() {
    let trees = space(10, 15);
    let state = recovery_question(&trees);
    let mut commits = Commits::from_state(state.clone()).unwrap();

    assert!(!commits.decide("other", Answer::Publish));
    assert!(!commits.decide("other", Answer::Delete));
    assert_eq!(commits.state(), state);
}

#[test]
fn an_answer_the_kind_does_not_offer_changes_nothing() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let id = asked(&commits).unwrap().id.clone();

    assert!(!commits.decide(&id, Answer::Publish));
    assert_eq!(held(&commits), ["d"]);
}

#[test]
fn a_held_node_carries_the_revisions_r_held_when_it_was_asked() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);

    let members = &asked(&commits).unwrap().held["d"].members;
    assert!(
        members.values().all(|revisions| revisions
            .is_some_and(|revisions| revisions.content_rev == 1 && revisions.meta_rev == 1)),
        "{members:?}"
    );
}

#[test]
fn an_unanswered_question_keeps_holding_what_is_left_when_it_shrinks() {
    let mut trees = space(25, 0);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let gone = root_files(0..13);
    remove(
        &mut trees,
        &gone.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let first = asked(&commits).unwrap().id.clone();

    for i in 0..7 {
        put_back(
            &mut trees,
            &format!("f{i}"),
            shape(ROOT, &format!("f{i}"), Kind::File),
            100 + i as u64,
        );
    }
    trees.settle();
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(
        deleted(&ops),
        Vec::<String>::new(),
        "6 of 25 files are still held"
    );
    let question = asked(&commits).expect("a new question");
    assert_ne!(question.id, first);
    assert_eq!(window(question).limit(), None);
    let mut left = root_files(7..13);
    left.sort();
    assert_eq!(held(&commits), left);
}

#[test]
fn a_delete_answer_covers_only_the_deletions_its_question_listed() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let first = asked(&commits).unwrap().id.clone();

    remove(&mut trees, &["f0"]);
    assert!(commits.decide(&first, Answer::Delete));
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(
        deleted(&ops),
        Vec::<String>::new(),
        "the answer was about d alone"
    );
    let question = asked(&commits).expect("a new question");
    assert_ne!(question.id, first);
    assert_eq!(question.answer, None, "the old answer is dropped");
    assert_eq!(held(&commits), ["d", "f0"]);
}

#[test]
fn a_confirmed_folder_deleted_over_several_commits_takes_one_answer() {
    let mut trees = space(10, 15);
    let state = CommitsState {
        cap: Some(1),
        split: ["d".into()].into(),
        ..Default::default()
    };
    let (mut commits, mut ids) = (Commits::from_state(state).unwrap(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let id = asked(&commits).unwrap().id.clone();
    assert!(commits.decide(&id, Answer::Delete));

    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);
    assert_eq!(deleted(&ops).len(), 1, "one op per commit");
    for node in deleted(&ops) {
        let mut remote = trees.remote().get(&node).unwrap().clone();
        remote.trashed = true;
        trees.set_remote(node, remote);
    }
    trees.settle();
    acknowledge(&mut commits, &mut trees, 8);
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 2);

    assert_eq!(asked(&commits), None, "the rest of d is still confirmed");
    let next = deleted(&ops);
    assert!(
        !next.is_empty() && next.iter().all(|node| node.starts_with("d/")),
        "{next:?}"
    );
}

#[test]
fn a_restore_answer_brings_back_its_question_and_holds_a_deletion_made_since() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let first = asked(&commits).unwrap().id.clone();

    remove(&mut trees, &["f0"]);
    assert!(commits.decide(&first, Answer::Restore));
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(deleted(&ops), Vec::<String>::new());
    assert_eq!(trees.class("d/g0"), Class::Remote, "d is downloaded");
    let question = asked(&commits).expect("a new question");
    assert_ne!(question.id, first);
    assert_eq!(held(&commits), ["f0"]);
}

#[test]
fn a_restore_answer_brings_back_held_children_whose_folder_was_put_back() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let id = asked(&commits).unwrap().id.clone();

    put_back(&mut trees, "d", shape(ROOT, "d", Kind::Dir), 100);
    trees.settle();
    assert!(commits.decide(&id, Answer::Restore));
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(deleted(&ops), Vec::<String>::new());
    for node in ["d/g0", "d/g14"] {
        assert_eq!(trees.class(node), Class::Remote, "{node} is downloaded");
    }
    assert_eq!(asked(&commits), None);
}

#[test]
fn a_restore_answer_holds_a_node_deleted_under_its_folder_since() {
    let mut trees = space(10, 15);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let id = asked(&commits).unwrap().id.clone();

    let added = shape("d", "new", Kind::File);
    trees.set_synced("d/new".into(), Some(added.clone()));
    trees.set_remote(
        "d/new".into(),
        RemoteNode {
            shape: added,
            trashed: false,
            content_rev: 1,
            meta_rev: 1,
            trash_group: String::new(),
        },
    );
    trees.settle();
    assert!(commits.decide(&id, Answer::Restore));
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1);

    assert_eq!(deleted(&ops), Vec::<String>::new());
    assert_eq!(held(&commits), ["d/new"], "the answer did not cover d/new");
}

#[test]
fn a_confirmed_folder_whose_rest_changed_after_some_commits_is_asked_about_again() {
    let mut trees = space(10, 15);
    let state = CommitsState {
        cap: Some(1),
        split: ["d".into()].into(),
        ..Default::default()
    };
    let (mut commits, mut ids) = (Commits::from_state(state).unwrap(), IdGen::new(1));
    remove(&mut trees, &["d"]);
    build_at(&mut commits, &mut trees, &mut ids, NOW);
    let id = asked(&commits).unwrap().id.clone();
    assert!(commits.decide(&id, Answer::Delete));
    for step in 0..3 {
        let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 1 + step);
        assert!(!deleted(&ops).is_empty());
        for node in deleted(&ops) {
            let mut remote = trees.remote().get(&node).unwrap().clone();
            remote.trashed = true;
            trees.set_remote(node, remote);
        }
        trees.settle();
        acknowledge(&mut commits, &mut trees, 8 + step);
    }
    let left: Vec<String> = (trees.synced().iter())
        .filter(|(id, node)| id.starts_with("d/") && node.kind == Kind::File)
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(left.len(), 8, "8 of 18 files are left, below the limits");

    let mut remote = trees.remote().get(&left[0]).unwrap().clone();
    remote.meta_rev += 1;
    trees.set_remote(left[0].clone(), remote);
    trees.settle();
    let ops = build_at(&mut commits, &mut trees, &mut ids, NOW + 5);

    assert_eq!(deleted(&ops), Vec::<String>::new());
    assert!(asked(&commits).is_some(), "the rest of d is held again");
}
