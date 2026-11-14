// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_move_out_of_the_trash_holds_a_create_onto_a_member_its_user_removed_until_its_delete() {
    let mut trees = trees(&[
        ("p", None, Some(dir(ROOT, "p")), Some(trashed(dir(ROOT, "p")))),
        ("a", Some(dir(ROOT, "a")), Some(dir("p", "a")), Some(trashed(dir("p", "a")))),
        ("g", None, Some(file("a", "g", 1)), Some(trashed(file("a", "g", 1)))),
        ("h", Some(file("a", "g", 2)), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("a", ROOT, "a")]);

    trees.set_remote_made_here("a".into(), remote_at(dir(ROOT, "a"), 2));
    trees.set_remote_made_here("g".into(), remote_at(file("a", "g", 1), 2));
    use op_result::Code;
    commits.received(result(2, vec![outcome(Code::Applied)])).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let create = op::Op::CreateFile(proto::CreateFile {
        node_id: "h".into(),
        parent_id: "a".into(),
        name: "g".into(),
        content: Some(content(2)),
    });
    assert_eq!(ops(&commit), [delete("g"), create]);
}

#[test]
fn a_delete_its_own_move_out_of_the_trash_superseded_is_sent_again() {
    let mut trees = trees(&[
        ("a", Some(dir(ROOT, "A")), Some(dir(ROOT, "a")), Some(remote(dir(ROOT, "a")))),
        ("g", None, Some(file("a", "g", 1)), Some(remote(file("a", "g", 1)))),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("a", ROOT, "A"), delete("g")]);

    let at = |node: RemoteNode, rev: u64| RemoteNode {
        meta_rev: rev,
        ..node
    };
    trees.set_remote("a".into(), at(trashed(dir(ROOT, "a")), 2));
    trees.set_remote("g".into(), at(trashed(file("a", "g", 1)), 2));
    trees.set_remote_made_here("a".into(), remote_at(dir(ROOT, "A"), 3));
    trees.set_remote_made_here("g".into(), remote_at(file("a", "g", 1), 3));
    use op_result::Code;
    let results = vec![outcome(Code::Applied), outcome(Code::Superseded)];
    commits.received(result(3, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [delete("g")]);
}

#[test]
fn a_rename_out_of_the_trash_holds_a_create_onto_a_member_its_user_removed_until_its_delete() {
    let mut trees = trees(&[
        ("t", Some(dir(ROOT, "T2")), Some(dir(ROOT, "T")), Some(trashed(dir(ROOT, "T")))),
        ("g", None, Some(file("t", "g", 1)), Some(trashed(file("t", "g", 1)))),
        ("x", Some(file("t", "g", 2)), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("t", ROOT, "T2")]);

    trees.set_remote_made_here("t".into(), remote_at(dir(ROOT, "T2"), 2));
    trees.set_remote_made_here("g".into(), remote_at(file("t", "g", 1), 2));
    use op_result::Code;
    commits.received(result(2, vec![outcome(Code::Applied)])).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    let create = op::Op::CreateFile(proto::CreateFile {
        node_id: "x".into(),
        parent_id: "t".into(),
        name: "g".into(),
        content: Some(content(2)),
    });
    assert_eq!(ops(&commit), [delete("g"), create]);
}

fn listed_while_renaming_out_of_the_trash(listed_rename: &str, results: Vec<OpResult>, result_seq: u64, answer_first: bool) -> Option<Vec<op::Op>> {
    let mut trees = trees(&[
        ("t", Some(dir(ROOT, "T2")), Some(dir(ROOT, "T")), Some(trashed(dir(ROOT, "T")))),
        ("g", None, Some(file("t", "g", 1)), Some(trashed(file("t", "g", 1)))),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("t", ROOT, "T2")]);
    let mut answer = Some(result(result_seq, results));
    if answer_first {
        commits.received(answer.take().unwrap()).unwrap();
    }
    let removed = trees.removed_here();
    trees.set_listed_remote("t".into(), remote_at(dir(ROOT, listed_rename), 2));
    trees.set_listed_remote("g".into(), remote_at(file("t", "g", 1), 2));
    trees.listed_removals(removed, |seq| commits.listed_here(seq));
    if let Some(answer) = answer {
        commits.received(answer).unwrap();
    }
    commits.acknowledge(&mut trees, result_seq);
    trees.settle();
    build(&mut commits, &mut trees, &mut ids).map(|commit| ops(&commit))
}

#[test]
fn a_listing_after_a_rename_out_of_the_trash_was_answered_deletes_the_member_its_user_removed_again() {
    use op_result::Code;
    let sent = listed_while_renaming_out_of_the_trash("T2", vec![outcome(Code::Applied)], 2, true);
    assert_eq!(sent, Some(vec![delete("g")]));
}

#[test]
fn a_listing_of_another_devices_rename_out_of_the_trash_downloads_the_member() {
    use op_result::Code;
    for answer_first in [true, false] {
        let sent = listed_while_renaming_out_of_the_trash("T3", vec![outcome(Code::Superseded)], 3, answer_first);
        let deletes = (sent.as_deref().unwrap_or_default().iter())
            .filter(|op| **op == delete("g"))
            .count();
        assert_eq!(deletes, 0, "answer first: {answer_first}");
    }
}
