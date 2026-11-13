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
