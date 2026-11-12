// SPDX-License-Identifier: Apache-2.0

//! Matching a first link's local entries against the server's tree (docs/design/sync-engine.md
//! section 6.4).

use koidrive_engine::classify::Trees;
use koidrive_engine::commit::{Commits, Failure, Reconnect};
use koidrive_engine::effects::{Effect, Effects};
use koidrive_engine::first_link::adopt;
use koidrive_engine::ids::IdGen;
use koidrive_engine::ignore::RuleFile;
use koidrive_engine::question::{Answer, Question, QuestionKind, RecoveryGroup};
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};
use koidrive_proto::v1::{CommitResult, Error, OpResult, Welcome, error, op, op_result};

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

/// Trees with `local` in L and `remote` in R, and nothing in S, as on a first link.
fn trees(local: &[(&str, Shape)], remote: &[(&str, Shape)]) -> Trees {
    let mut trees = Trees::new(ROOT);
    for (ino, (id, shape)) in local.iter().enumerate() {
        let identity = FileIdentity {
            dev: 1,
            ino: ino as u128 + 1,
            birth_ns: Some(0),
        };
        let node = LocalNode {
            shape: shape.clone(),
            identity,
            parked: false,
        };
        trees.set_local((*id).into(), node);
    }
    for (id, shape) in remote {
        let node = RemoteNode {
            shape: shape.clone(),
            trashed: false,
            content_rev: 1,
            meta_rev: 1,
            trash_group: String::new(),
        };
        trees.set_remote((*id).into(), node);
    }
    trees.settle();
    trees
}

/// The local nodes by ID with their parent and name, in order.
fn local(trees: &Trees) -> Vec<(String, String, String)> {
    let nodes = trees.local().iter();
    nodes
        .map(|(id, node)| {
            (
                id.clone(),
                node.shape.parent.clone(),
                node.shape.name.clone(),
            )
        })
        .collect()
}

#[test]
fn an_entry_the_server_holds_with_the_same_name_and_bytes_is_adopted() {
    let mut trees = trees(
        &[
            ("l-docs", dir(ROOT, "docs")),
            ("l-a", file("l-docs", "a", 1)),
        ],
        &[
            ("r-docs", dir(ROOT, "docs")),
            ("r-a", file("r-docs", "a", 1)),
        ],
    );
    assert_eq!(adopt(&mut trees), Vec::<String>::new());
    assert_eq!(
        local(&trees),
        [
            ("r-a".into(), "r-docs".into(), "a".into()),
            ("r-docs".into(), ROOT.into(), "docs".into())
        ]
    );
    trees.settle();
    assert!(trees.dirty().is_empty(), "{:?}", trees.dirty());
}

/// A folder both hold is adopted although its contents differ, and its children are matched one
/// by one: the file with other bytes and the file the server lacks are held, and the server's
/// other file is left for the effects to download.
#[test]
fn a_shared_folder_is_adopted_and_its_children_are_matched_one_by_one() {
    let mut trees = trees(
        &[
            ("l-docs", dir(ROOT, "docs")),
            ("l-same", file("l-docs", "same", 1)),
            ("l-edited", file("l-docs", "edited", 2)),
            ("l-mine", file("l-docs", "mine", 3)),
        ],
        &[
            ("r-docs", dir(ROOT, "docs")),
            ("r-same", file("r-docs", "same", 1)),
            ("r-edited", file("r-docs", "edited", 9)),
            ("r-theirs", file("r-docs", "theirs", 4)),
        ],
    );
    let mut held = adopt(&mut trees);
    held.sort();
    assert_eq!(held, ["l-edited", "l-mine"]);
    assert!(trees.local().get("r-docs").is_some());
    assert!(trees.local().get("r-same").is_some());
    assert_eq!(
        trees.local().get("l-edited").unwrap().shape.parent,
        "r-docs"
    );
    assert!(trees.synced().get("r-edited").is_none());
}

/// A local folder the server lacks is held whole, as one item; a name that differs only by case,
/// and an entry of another kind, are held too.
#[test]
fn a_new_folder_a_name_that_only_folds_alike_and_another_kind_are_held() {
    let mut trees = trees(
        &[
            ("l-new", dir(ROOT, "new")),
            ("l-inside", file("l-new", "inside", 1)),
            ("l-report", file(ROOT, "Report.txt", 2)),
            ("l-notes", file(ROOT, "notes", 3)),
        ],
        &[
            ("r-report", file(ROOT, "report.txt", 2)),
            ("r-notes", dir(ROOT, "notes")),
        ],
    );
    let mut held = adopt(&mut trees);
    held.sort();
    assert_eq!(held, ["l-new", "l-notes", "l-report"]);
    assert!(trees.synced().iter().next().is_none());
}

/// An excluded entry is left out of the match: it is neither adopted nor held, even where the
/// server holds the same name.
#[test]
fn an_excluded_entry_is_neither_adopted_nor_held() {
    let mut trees = trees(
        &[
            ("l-logs", dir(ROOT, "logs")),
            ("l-old", file("l-logs", "old.log", 1)),
            ("l-cache", file(ROOT, "cache.log", 2)),
        ],
        &[("r-logs", dir(ROOT, "logs"))],
    );
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"*.log\n")));
    assert_eq!(adopt(&mut trees), Vec::<String>::new());
    assert!(trees.local().get("r-logs").is_some());
    assert!(trees.local().get("l-cache").is_some());
    assert!(trees.synced().get("l-cache").is_none());
    assert!(trees.synced().get("l-old").is_none());
}

/// The ops of the next commit, each as its kind and node.
fn ops(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen) -> Vec<(&'static str, String)> {
    let Some(commit) = commits.build(trees, ids, NOW, HISTORY, 1) else {
        return Vec::new();
    };
    (commit.ops.iter())
        .map(|op| match &op.op {
            Some(op::Op::CreateFile(create)) => ("create file", create.node_id.clone()),
            Some(op::Op::CreateDir(create)) => ("create dir", create.node_id.clone()),
            Some(op::Op::Move(moved)) => ("move", moved.node_id.clone()),
            Some(op::Op::UpdateContent(update)) => ("update", update.node_id.clone()),
            Some(op::Op::Delete(delete)) => ("delete", delete.node_id.clone()),
            other => panic!("unexpected op {other:?}"),
        })
        .collect()
}

/// The one open question, which must be about entries created on this device.
fn question(commits: &Commits) -> Question {
    let [question] = commits.questions() else {
        panic!("one question: {:?}", commits.questions());
    };
    assert_eq!(
        question.kind,
        QuestionKind::Recovery {
            group: RecoveryGroup::CreatedOnThisDevice
        }
    );
    question.clone()
}

/// The space holds `docs/a`, and the folder linked to it holds the same `docs/a`, its own
/// `docs/b`, a `notes` file whose name the space gives other bytes, and a folder `new` with
/// one file.
fn linked() -> (Commits, Trees, IdGen) {
    let mut trees = trees(
        &[
            ("l-docs", dir(ROOT, "docs")),
            ("l-a", file("l-docs", "a", 1)),
            ("l-b", file("l-docs", "b", 2)),
            ("l-notes", file(ROOT, "notes", 3)),
            ("l-new", dir(ROOT, "new")),
            ("l-inside", file("l-new", "inside", 4)),
        ],
        &[
            ("r-docs", dir(ROOT, "docs")),
            ("r-a", file("r-docs", "a", 1)),
            ("r-notes", file(ROOT, "notes", 9)),
        ],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
    (commits, trees, ids)
}

#[test]
fn a_first_link_holds_what_matches_nothing_under_one_question_and_sends_nothing_for_it() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    let tops: Vec<&str> = asked.held.keys().map(String::as_str).collect();
    assert_eq!(tops, ["l-b", "l-new", "l-notes"]);
    let mut held: Vec<&str> = asked.holds().map(String::as_str).collect();
    held.sort();
    assert_eq!(held, ["l-b", "l-inside", "l-new", "l-notes"]);
    assert_eq!(asked.held["l-new"].files, 1);
    assert!(held.iter().all(|id| trees.held(id)));
    assert!(!trees.held("r-docs") && !trees.held("r-a"));

    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert_eq!(
        question(&commits).id,
        asked.id,
        "nothing changed, so it is the same question"
    );
}

/// The space's `notes` has other bytes than the folder's, which is held: the effects neither
/// download the space's `notes` over it nor park it to make room.
#[test]
fn a_remote_node_whose_name_a_held_entry_takes_is_not_downloaded() {
    let (_, trees, mut ids) = linked();
    let mut effects = Effects::default();
    assert_eq!(effects.plan(&trees, &mut ids, NOW), None);
}

#[test]
fn a_first_link_to_a_space_with_nothing_live_holds_nothing() {
    let mut trees = trees(&[("l-mine", file(ROOT, "mine", 1))], &[]);
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
    assert!(commits.questions().is_empty());
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create file", "l-mine".to_owned())]
    );
}

#[test]
fn a_held_entry_that_is_edited_or_moved_stays_held_under_a_new_question() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);

    let mut edited = trees.local().get("l-b").unwrap().clone();
    edited.shape.content = Some(content(5));
    trees.set_local("l-b".into(), edited);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let after_edit = question(&commits);
    assert_ne!(after_edit.id, asked.id);
    assert_eq!(
        after_edit.held.keys().collect::<Vec<_>>(),
        asked.held.keys().collect::<Vec<_>>()
    );

    let mut moved = trees.local().get("l-notes").unwrap().clone();
    moved.shape.parent = "l-new".into();
    trees.set_local("l-notes".into(), moved);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let after_move = question(&commits);
    assert_ne!(after_move.id, after_edit.id);
    let tops: Vec<&str> = after_move.held.keys().map(String::as_str).collect();
    assert_eq!(tops, ["l-b", "l-new"], "notes is held inside new now");
    assert!(trees.held("l-notes"));
}

#[test]
fn an_entry_moved_out_of_a_held_folder_stays_held_as_an_item_of_its_own() {
    let (mut commits, mut trees, mut ids) = linked();
    let mut moved = trees.local().get("l-inside").unwrap().clone();
    moved.shape.parent = "r-docs".into();
    trees.set_local("l-inside".into(), moved);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let tops: Vec<String> = question(&commits).held.into_keys().collect();
    assert_eq!(tops, ["l-b", "l-inside", "l-new", "l-notes"]);
    assert!(trees.held("l-inside"));
}

#[test]
fn a_held_entry_the_user_deletes_leaves_the_question_and_the_last_one_ends_it() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    trees.remove_local("l-inside");
    trees.remove_local("l-new");
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let after = question(&commits);
    assert_ne!(after.id, asked.id);
    let tops: Vec<&str> = after.held.keys().map(String::as_str).collect();
    assert_eq!(tops, ["l-b", "l-notes"]);

    trees.remove_local("l-b");
    trees.remove_local("l-notes");
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(commits.questions().is_empty());
}

#[test]
fn a_held_entry_the_rules_leave_out_for_a_while_stays_held_until_the_person_answers() {
    let (mut commits, mut trees, mut ids) = linked();
    let root: [&str; 0] = [];
    let held = ["l-b", "l-inside", "l-new", "l-notes"];
    trees.set_rules(&root, Some(RuleFile::parse(b"notes\nnew/\n")));
    assert!(trees.excluded("l-notes") && trees.excluded("l-new"));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);

    trees.set_rules(&root, None);
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [],
        "nothing held is sent once the rules let it in again"
    );
    assert!(held.iter().all(|id| trees.held(id)));

    assert!(commits.decide(&question(&commits).id, Answer::Publish));
    let mut sent = ops(&mut commits, &mut trees, &mut ids);
    sent.sort();
    assert_eq!(
        sent,
        [
            ("create dir", "l-new".to_owned()),
            ("create file", "l-b".to_owned()),
            ("create file", "l-inside".to_owned()),
            ("create file", "l-notes".to_owned()),
        ]
    );
}

/// An entry created after the first link, beside the held ones, is an ordinary create.
#[test]
fn an_entry_created_after_the_first_link_is_sent_at_once() {
    let (mut commits, mut trees, mut ids) = linked();
    let node = LocalNode {
        shape: file("r-docs", "later", 6),
        identity: FileIdentity {
            dev: 1,
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("l-later".into(), node);
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create file", "l-later".to_owned())]
    );
}

#[test]
fn publishing_sends_every_held_entry_as_a_create() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    assert!(!commits.decide(&asked.id, Answer::Delete));
    assert!(commits.decide(&asked.id, Answer::Publish));
    assert!(
        !commits.decide(&asked.id, Answer::Publish),
        "it took an answer"
    );
    let mut sent = ops(&mut commits, &mut trees, &mut ids);
    sent.sort();
    assert_eq!(
        sent,
        [
            ("create dir", "l-new".to_owned()),
            ("create file", "l-b".to_owned()),
            ("create file", "l-inside".to_owned()),
            ("create file", "l-notes".to_owned()),
        ]
    );
    assert!(commits.questions().is_empty());
    assert!(!trees.held("l-b"));
}

/// The server gives this device's create of `id` the conflict name `name`: R holds the file under
/// it at `seq`, and the commit's result names it.
fn renamed_create(commits: &mut Commits, trees: &mut Trees, id: &str, name: &str, seq: u64) {
    let shape = trees.local().get(id).unwrap().shape.clone();
    let node = RemoteNode {
        shape: Shape {
            name: name.into(),
            ..shape
        },
        trashed: false,
        content_rev: seq,
        meta_rev: seq,
        trash_group: String::new(),
    };
    trees.set_remote(id.into(), node);
    let renamed = OpResult {
        code: op_result::Code::AppliedRenamed as i32,
        name: name.into(),
        ..Default::default()
    };
    let result = CommitResult {
        seq,
        results: vec![renamed],
        changes: vec![],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(trees, seq));
    trees.settle();
}

/// Publishing a first link's `notes`, whose name the space holds with other bytes, keeps both
/// files: its create takes a conflict name and asks nothing, also when the first commit that
/// carried it was forgotten and the next one was stored and loaded again before its result. A
/// file created later that takes a conflict name is asked about.
#[test]
fn a_published_create_that_takes_a_conflict_name_asks_nothing_and_a_later_create_asks() {
    let mut trees = trees(
        &[("l-notes", file(ROOT, "notes", 3))],
        &[("r-notes", file(ROOT, "notes", 9))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
    assert!(commits.decide(&question(&commits).id, Answer::Publish));
    let created = [("create file", "l-notes".to_owned())];
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), created);
    let head_moved = Error {
        code: error::Code::HeadMoved as i32,
        ..Default::default()
    };
    assert_eq!(commits.failed(&head_moved), Ok(Failure::Abandoned));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), created);
    let mut commits = Commits::from_state(commits.state()).unwrap();
    renamed_create(&mut commits, &mut trees, "l-notes", "notes (conflict)", 2);
    assert_eq!(commits.questions(), []);

    let later = LocalNode {
        shape: file(ROOT, "later", 6),
        identity: FileIdentity {
            dev: 1,
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("l-later".into(), later);
    trees.settle();
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create file", "l-later".to_owned())]
    );
    let theirs = RemoteNode {
        shape: file(ROOT, "later", 7),
        trashed: false,
        content_rev: 3,
        meta_rev: 3,
        trash_group: String::new(),
    };
    trees.set_remote("r-later".into(), theirs);
    renamed_create(&mut commits, &mut trees, "l-later", "later (conflict)", 3);
    let [asked] = commits.questions() else {
        panic!("one question: {:?}", commits.questions());
    };
    let QuestionKind::Conflict { original, copy, .. } = &asked.kind else {
        panic!("a conflict: {asked:?}");
    };
    assert_eq!(
        (original.node_id.as_str(), copy.node_id.as_str()),
        ("r-later", "l-later")
    );
}

/// The effects of discarding `linked`'s entries and of the downloads that follow, applied as if
/// they succeeded, until none is left. Returns them, by node.
fn discarded(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen) -> Vec<(String, Effect)> {
    let asked = question(commits);
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(
        ops(commits, trees, ids),
        [],
        "nothing is sent for a discarded entry"
    );
    assert!(commits.questions().is_empty());
    let mut effects = Effects::default();
    let mut done = Vec::new();
    for ino in 100.. {
        trees.settle();
        let Some(record) = effects.plan(trees, ids, NOW).cloned() else {
            break;
        };
        let identity = FileIdentity {
            dev: 1,
            ino,
            birth_ns: Some(0),
        };
        effects
            .done(trees, &record.effect_id, Some(identity))
            .unwrap();
        done.push((record.node, record.effect));
    }
    trees.settle();
    done
}

/// Discarding sets S to the held entries, which R does not hold, so the effects move them to the
/// local trash, children before their folder, and then download what R holds at their paths.
#[test]
fn discarding_moves_the_held_entries_to_the_local_trash_and_downloads_the_space() {
    let (mut commits, mut trees, mut ids) = linked();
    let done = discarded(&mut commits, &mut trees, &mut ids);
    let removed: Vec<&str> = (done.iter())
        .filter(|(_, effect)| {
            matches!(effect, Effect::RemoveFile { .. } | Effect::RemoveDir { .. })
        })
        .map(|(node, _)| node.as_str())
        .collect();
    assert_eq!(removed.len(), 4, "{done:?}");
    let position = |id: &str| removed.iter().position(|node| *node == id);
    assert!(position("l-inside") < position("l-new"));
    let notes = done.iter().position(|(node, _)| node == "l-notes");
    let download = done.iter().position(|(node, _)| node == "r-notes");
    assert!(notes < download && download.is_some(), "{done:?}");
    assert_eq!(
        local(&trees),
        [
            ("r-a".into(), "r-docs".into(), "a".into()),
            ("r-docs".into(), ROOT.into(), "docs".into()),
            ("r-notes".into(), ROOT.into(), "notes".into()),
        ]
    );
    assert!(trees.work().is_empty(), "{:?}", trees.work());
}

/// The answer acts on what the person saw: an edit made after the answer and before the next
/// commit is planned drops it, and the edited entry stays held under a new question.
#[test]
fn an_answer_to_entries_that_changed_since_is_not_taken() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    assert!(commits.decide(&asked.id, Answer::Discard));
    let mut edited = trees.local().get("l-b").unwrap().clone();
    edited.shape.content = Some(content(5));
    trees.set_local("l-b".into(), edited);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let again = question(&commits);
    assert_ne!(again.id, asked.id);
    assert_eq!(again.answer, None);
    assert!(trees.synced().get("l-b").is_none() && trees.held("l-b"));
}

#[test]
fn an_answer_to_a_replaced_question_changes_nothing() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    trees.remove_local("l-b");
    ops(&mut commits, &mut trees, &mut ids);
    assert!(!commits.decide(&asked.id, Answer::Discard));
    assert!(trees.held("l-notes"));
}

/// A folder the user puts a new entry in after discarding it, before the effects removed it, is
/// the user's again: it is neither removed nor parked, and it is sent with the entry.
#[test]
fn a_discarded_folder_the_user_puts_an_entry_in_is_sent_with_it() {
    let (mut commits, mut trees, mut ids) = linked();
    let asked = question(&commits);
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let node = LocalNode {
        shape: file("l-new", "later", 6),
        identity: FileIdentity {
            dev: 1,
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("l-later".into(), node);
    trees.settle();
    assert!(trees.reclaimed("l-new"));
    let mut effects = Effects::default();
    let mut removed = Vec::new();
    while let Some(record) = effects.plan(&trees, &mut ids, NOW).cloned() {
        if matches!(
            record.effect,
            Effect::RemoveFile { .. } | Effect::RemoveDir { .. }
        ) {
            removed.push(record.node.clone());
        }
        let identity = FileIdentity {
            dev: 1,
            ino: 100 + removed.len() as u128,
            birth_ns: Some(0),
        };
        effects
            .done(&mut trees, &record.effect_id, Some(identity))
            .unwrap();
        trees.settle();
    }
    removed.sort();
    assert_eq!(removed, ["l-b", "l-inside", "l-notes"], "the folder stays");
    let mut sent = ops(&mut commits, &mut trees, &mut ids);
    sent.retain(|(_, node)| node == "l-new" || node == "l-later");
    assert_eq!(
        sent,
        [
            ("create dir", "l-new".to_owned()),
            ("create file", "l-later".to_owned())
        ]
    );
}

/// A folder whose discarded subfolder the user puts a new entry in is the user's again as well:
/// the effects remove neither folder, and both are sent with the entry.
#[test]
fn a_discarded_folder_whose_subfolder_the_user_puts_an_entry_in_is_sent_with_it() {
    let mut trees = trees(
        &[
            ("l-new", dir(ROOT, "new")),
            ("l-sub", dir("l-new", "sub")),
            ("l-inside", file("l-sub", "inside", 4)),
        ],
        &[("r-notes", file(ROOT, "notes", 9))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
    let asked = question(&commits);
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let node = LocalNode {
        shape: file("l-sub", "later", 6),
        identity: FileIdentity {
            dev: 1,
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("l-later".into(), node);
    trees.settle();
    assert!(trees.reclaimed("l-sub") && trees.reclaimed("l-new"));
    let mut effects = Effects::default();
    let mut touched = Vec::new();
    while let Some(record) = effects.plan(&trees, &mut ids, NOW).cloned() {
        touched.push(record.node.clone());
        let identity = FileIdentity {
            dev: 1,
            ino: 100 + touched.len() as u128,
            birth_ns: Some(0),
        };
        effects
            .done(&mut trees, &record.effect_id, Some(identity))
            .unwrap();
        trees.settle();
    }
    touched.retain(|node| node != "r-notes");
    assert_eq!(touched, ["l-inside"], "both folders stay where they are");
    let mut sent = ops(&mut commits, &mut trees, &mut ids);
    sent.retain(|(_, node)| node != "l-inside");
    assert_eq!(
        sent,
        [
            ("create dir", "l-new".to_owned()),
            ("create dir", "l-sub".to_owned()),
            ("create file", "l-later".to_owned())
        ]
    );
}

/// A discarded file the user edits before the effects removed it is the user's again, and so is
/// the discarded folder it is in: the effects remove neither, and both are sent.
#[test]
fn a_discarded_file_the_user_edits_is_sent_with_its_folder() {
    let mut trees = trees(
        &[
            ("l-new", dir(ROOT, "new")),
            ("l-inside", file("l-new", "inside", 4)),
        ],
        &[("r-notes", file(ROOT, "notes", 9))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
    let asked = question(&commits);
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let node = LocalNode {
        shape: file("l-new", "inside", 5),
        identity: FileIdentity {
            dev: 1,
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("l-inside".into(), node);
    trees.settle();
    let mut effects = Effects::default();
    let mut touched = Vec::new();
    while let Some(record) = effects.plan(&trees, &mut ids, NOW).cloned() {
        touched.push(record.node.clone());
        let identity = FileIdentity {
            dev: 1,
            ino: 100 + touched.len() as u128,
            birth_ns: Some(0),
        };
        effects
            .done(&mut trees, &record.effect_id, Some(identity))
            .unwrap();
        trees.settle();
    }
    assert_eq!(touched, ["r-notes"], "the file and its folder stay");
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [
            ("create dir", "l-new".to_owned()),
            ("create file", "l-inside".to_owned())
        ]
    );
}

/// A matched folder whose rules a scan could not read keeps them unknown under the server's ID, so
/// nothing below it is planned until a scan reads it.
#[test]
fn an_adopted_folder_keeps_its_rules_unknown() {
    let mut trees = trees(
        &[
            ("l-docs", dir(ROOT, "docs")),
            ("l-a", file("l-docs", "a", 1)),
        ],
        &[
            ("r-docs", dir(ROOT, "docs")),
            ("r-a", file("r-docs", "a", 1)),
            ("r-new", file("r-docs", "new", 2)),
        ],
    );
    trees.set_rules_unknown("l-docs");
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
    assert!(
        trees.local().get("r-docs").is_some(),
        "the folder is adopted"
    );
    assert!(trees.held("r-new"));
    assert_eq!(Effects::default().plan(&trees, &mut ids, NOW), None);
}

/// On a first link, the person discards A, which takes the name key of the adopted a, so its S
/// update waits. A commit of this device that creates `pad` was applied at seq 2 before the
/// server lost it. The waiting update came from the person's answer, not from a commit, so the
/// rollback keeps it and A is not uploaded.
#[test]
fn a_discard_whose_s_update_waits_stays_after_a_server_loss() {
    let mut trees = trees(
        &[("l-a", file(ROOT, "a", 1)), ("l-A", file(ROOT, "A", 2))],
        &[("r-a", file(ROOT, "a", 1))],
    );
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(&mut trees, &mut ids, NOW);
    trees.settle();
    let asked = question(&commits);
    assert_eq!(asked.shown.keys().collect::<Vec<_>>(), ["l-A"]);
    let pad = LocalNode {
        shape: file(ROOT, "pad", 3),
        identity: FileIdentity {
            dev: 1,
            ino: 99,
            birth_ns: Some(0),
        },
        parked: false,
    };
    trees.set_local("pad".into(), pad);
    trees.settle();
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert_eq!(sent, [("create file", "pad".to_owned())]);
    let pad = RemoteNode {
        shape: file(ROOT, "pad", 3),
        trashed: false,
        content_rev: 2,
        meta_rev: 2,
        trash_group: String::new(),
    };
    trees.set_remote("pad".into(), pad);
    let applied = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    let result = CommitResult {
        seq: 2,
        results: vec![applied],
        changes: vec![],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    assert!(commits.decide(&asked.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(trees.waiting().contains_key("l-A"));
    let welcome = Welcome {
        history_id: HISTORY.into(),
        device_seq: 0,
        ..Default::default()
    };
    assert_eq!(commits.connected(&welcome), Reconnect::AheadOfServer);
    let (trashed, edited) = (trees.trashed_here(), trees.edited_here());
    trees.remove_remote("pad");
    trees.forget_lost(trashed, edited, commits.held_through(), commits.accepted());
    trees.settle();
    assert!(trees.waiting().contains_key("l-A"));
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(!sent.iter().any(|(_, id)| id == "l-A"), "{sent:?}");
}
