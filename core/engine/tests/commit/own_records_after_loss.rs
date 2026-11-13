// SPDX-License-Identifier: Apache-2.0

fn served_at_5_then_lost(
    rows: &[Row],
    change: impl FnOnce(&mut Trees),
    applied: RemoteNode,
    listed: &[(&str, RemoteNode)],
    received: bool,
    restart: bool,
) -> (Vec<Effect>, Vec<op::Op>) {
    let mut trees = trees(rows);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    commit_file(&mut trees, &mut commits, &mut ids, "p", 2);
    commit_file(&mut trees, &mut commits, &mut ids, "q", 4);
    change(&mut trees);
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(commits.own_record(5, &commit.commit_id));
    if commits.edits_in_flight(&commit.commit_id, "n") {
        trees.set_remote_edited_here("n".into(), applied);
    } else {
        trees.set_remote_made_here("n".into(), applied);
    }
    assert!(!commits.acknowledge(&mut trees, 5));
    trees.settle();
    if received {
        let results = vec![outcome(op_result::Code::Applied); commit.ops.len()];
        commits.received(result(5, results)).unwrap();
        assert!(commits.acknowledge(&mut trees, 5));
        trees.settle();
    }
    if restart {
        commits = Commits::from_state(commits.state()).unwrap();
        trees.restore(trees.state()).unwrap();
        trees.rules_scanned();
    }
    assert_eq!(commits.connected(&welcome(1)), Reconnect::AheadOfServer);
    let (trashed, edited, moved) = (
        trees.trashed_here(),
        trees.edited_here(),
        trees.moved_here(),
    );
    let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
    let untaken = trees.untaken_edits(|seq| commits.lost_here(seq));
    let p = RemoteNode {
        content_rev: 2,
        edit_rev: 2,
        meta_rev: 2,
        ..remote(file(ROOT, "p", 7))
    };
    let listed: BTreeMap<String, RemoteNode> = (listed.iter())
        .map(|(id, node)| ((*id).to_owned(), node.clone()))
        .chain([("p".to_owned(), p)])
        .collect();
    let gone: Vec<String> = (trees.remote().iter())
        .map(|(id, _)| id.clone())
        .filter(|id| !listed.contains_key(id))
        .collect();
    for id in gone {
        trees.remove_remote(&id);
    }
    for (id, node) in listed {
        trees.set_listed_remote(id, node);
    }
    trees.forget_lost(
        trashed,
        edited,
        synced,
        moved,
        untaken,
        commits.held_through(),
        commits.accepted(),
    );
    commits.forget_lost_slots();
    trees.settle();
    let effects = effects_for(&mut trees, &mut ids, "n");
    let sent = build(&mut commits, &mut trees, &mut ids).map_or_else(Vec::new, |commit| ops(&commit));
    (effects, sent)
}

fn served_at_5_then_lost_with_or_without_its_result(
    rows: &[Row],
    change: impl Fn(&mut Trees),
    applied: RemoteNode,
    listed: &[(&str, RemoteNode)],
) -> (Vec<Effect>, Vec<op::Op>) {
    let run = |received, restart| {
        served_at_5_then_lost(rows, &change, applied.clone(), listed, received, restart)
    };
    let received = run(true, false);
    assert_eq!(run(false, false), received, "the dropped result changed the outcome");
    assert_eq!(run(false, true), received, "the restart changed the outcome");
    received
}

fn sent_for<'a>(sent: &'a [op::Op], node: &str) -> Vec<&'a op::Op> {
    let node_of = |op: &op::Op| match op {
        op::Op::CreateDir(op) => op.node_id.clone(),
        op::Op::CreateFile(op) => op.node_id.clone(),
        op::Op::UpdateContent(op) => op.node_id.clone(),
        op::Op::Move(op) => op.node_id.clone(),
        op::Op::Delete(op) => op.node_id.clone(),
        op::Op::Restore(op) => op.node_id.clone(),
    };
    sent.iter().filter(|op| node_of(op) == node).collect()
}

#[test]
fn a_create_the_server_lost_is_sent_again_into_a_later_deleted_folder_whether_or_not_its_result_arrived() {
    let created = RemoteNode {
        content_rev: 5,
        edit_rev: 5,
        meta_rev: 5,
        ..remote(file("f", "n", 3))
    };
    let deleted = RemoteNode {
        meta_rev: 3,
        ..trashed(dir(ROOT, "f"))
    };
    let (effects, sent) = served_at_5_then_lost_with_or_without_its_result(
        &[synced("f", dir(ROOT, "f"))],
        |trees| trees.set_local("n".into(), local(file("f", "n", 3), 50)),
        created,
        &[("f", deleted)],
    );
    assert_eq!(effects, []);
    assert!(
        matches!(&sent_for(&sent, "n")[..], [op::Op::CreateFile(create)] if create.content == Some(content(3))),
        "{sent:?}"
    );
}

#[test]
fn an_edit_the_server_lost_is_sent_again_whether_or_not_its_result_arrived() {
    let edited = RemoteNode {
        content_rev: 5,
        edit_rev: 5,
        ..remote(file(ROOT, "n", 2))
    };
    let (effects, sent) = served_at_5_then_lost_with_or_without_its_result(
        &[synced("n", file(ROOT, "n", 1))],
        |trees| trees.set_local("n".into(), local(file(ROOT, "n", 2), 1)),
        edited,
        &[("n", remote(file(ROOT, "n", 1)))],
    );
    assert_eq!(effects, []);
    assert_eq!(sent_for(&sent, "n"), [&update("n", 2)]);
}

#[test]
fn an_edit_the_server_lost_is_sent_again_over_a_later_delete_whether_or_not_its_result_arrived() {
    let edited = RemoteNode {
        content_rev: 5,
        edit_rev: 5,
        ..remote(file(ROOT, "n", 2))
    };
    let deleted = RemoteNode {
        meta_rev: 3,
        ..trashed(file(ROOT, "n", 1))
    };
    let (effects, sent) = served_at_5_then_lost_with_or_without_its_result(
        &[synced("n", file(ROOT, "n", 1))],
        |trees| trees.set_local("n".into(), local(file(ROOT, "n", 2), 1)),
        edited,
        &[("n", deleted)],
    );
    assert_eq!(effects, []);
    assert_eq!(sent_for(&sent, "n"), [&update("n", 2)]);
}

#[test]
fn a_delete_the_server_lost_is_sent_again_whether_or_not_its_result_arrived() {
    let deleted = RemoteNode {
        meta_rev: 5,
        trash_group: "5:0".into(),
        ..trashed(file(ROOT, "n", 1))
    };
    let (effects, sent) = served_at_5_then_lost_with_or_without_its_result(
        &[synced("n", file(ROOT, "n", 1))],
        |trees| trees.remove_local("n"),
        deleted,
        &[("n", remote(file(ROOT, "n", 1)))],
    );
    assert_eq!(effects, []);
    assert_eq!(sent_for(&sent, "n"), [&delete("n")]);
}

#[test]
fn a_move_the_server_lost_is_sent_again_whether_or_not_its_result_arrived() {
    let (effects, sent) = served_at_5_then_lost_with_or_without_its_result(
        &[synced("n", file(ROOT, "a", 1))],
        |trees| trees.set_local("n".into(), local(file(ROOT, "b", 1), 1)),
        remote_at(file(ROOT, "b", 1), 5),
        &[("n", remote(file(ROOT, "a", 1)))],
    );
    assert_eq!(effects, []);
    assert_eq!(sent_for(&sent, "n"), [&mv("n", ROOT, "b")]);
}
