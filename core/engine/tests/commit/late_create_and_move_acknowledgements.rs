// SPDX-License-Identifier: Apache-2.0

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
            edit_rev: 8,
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
        edit_rev: 9,
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
        edit_rev: 3,
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
        edit_rev: 9,
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
                    edit_rev: 4,
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
        edit_rev: content_rev,
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
