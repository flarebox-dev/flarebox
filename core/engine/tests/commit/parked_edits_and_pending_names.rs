// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_parked_edit_whose_name_a_new_folder_holds_is_sent() {
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "ss", 1)),
        Some(file(ROOT, "ss", 1)),
        Some(trashed(file(ROOT, "ß", 1))),
    )]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "ss", 2), 1)
        },
    );
    trees.set_local("n".into(), local(dir(ROOT, "ß"), 2));
    trees.settle();
    let (mut effects, mut commits, mut ids) =
        (Effects::default(), Commits::default(), IdGen::new(1));
    assert_eq!(
        effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW)
            .map(|record| record.effect.clone()),
        None
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(uploads(&ops(&commit), 2), "{:?}", ops(&commit));
}

#[test]
fn a_blocker_at_its_r_position_is_not_parked() {
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "ss", 1)),
        Some(file(ROOT, "ss", 1)),
        Some(trashed(file(ROOT, "ß", 1))),
    )]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "ss", 2), 1)
        },
    );
    trees.set_local("n".into(), local(dir(ROOT, "ß"), 2));
    trees.set_remote("n".into(), remote(dir(ROOT, "ß")));
    trees.settle();
    let mut state = trees.state();
    state.waiting.insert(
        "n".into(),
        Waiting {
            shape: Some(dir(ROOT, "ß")),
            origin: Origin::Commit(1),
        },
    );
    trees.restore(state).unwrap();
    trees.rules_scanned();
    assert_eq!(trees.check(), Ok(()));
    let (mut effects, mut commits, mut ids) =
        (Effects::default(), Commits::default(), IdGen::new(1));
    assert!(
        effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW)
            .is_none()
    );
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(uploads(&ops(&commit), 2), "{:?}", ops(&commit));
}

#[test]
fn a_parked_edit_forked_for_a_concurrent_edit_keeps_its_parked_entry() {
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "ss", 1)),
        Some(file(ROOT, "ss", 1)),
        Some(remote(file(ROOT, "ß", 3))),
    )]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "ss", 2), 1)
        },
    );
    trees.set_local("n".into(), local(dir(ROOT, "ß"), 2));
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(uploads(&ops(&commit), 2), "{:?}", ops(&commit));
    let (fork, node) = (trees.local().iter())
        .find(|(_, node)| node.identity.ino == 1)
        .unwrap();
    assert_ne!(fork, "h");
    assert!(node.parked);
    assert_eq!(trees.parked_name(fork), "h");
}

#[test]
fn an_edit_made_while_an_unsent_edit_is_out_stays_unsent_when_that_edit_is_acknowledged() {
    let (mut trees, mut commits, mut ids) = an_edit_to_the_content_a_lost_commit_replaced();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
    trees.set_local("n".into(), local(file(ROOT, "n", 5), 1));
    let other = RemoteNode {
        content_rev: 4,
        edit_rev: 4,
        ..remote(file(ROOT, "n", 5))
    };
    trees.set_remote("n".into(), other);
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert_eq!(trees.unsent_edits().iter().collect::<Vec<_>>(), ["n"]);
    let resent = RemoteNode {
        content_rev: 5,
        edit_rev: 5,
        ..remote(file(ROOT, "n", 6))
    };
    trees.set_remote("n".into(), resent);
    trees.settle();
    assert_eq!(trees.class("n"), Class::Concurrent);
}

#[test]
fn an_edit_of_a_file_whose_chunk_list_r_leaves_out_counts_for_loss_recovery() {
    let chunk = proto::ChunkRef {
        hash: vec![7; 32],
        size: 3,
        encoding: 0,
    };
    let listed = Shape {
        content: Some(Content {
            chunks: vec![chunk],
            chunk_count: 1,
            ..content(2)
        }),
        ..file(ROOT, "n", 2)
    };
    let unlisted = RemoteNode {
        content_rev: 4,
        edit_rev: 4,
        ..remote(file(ROOT, "n", 2))
    };
    let sent = listed.content.clone();
    let mut trees = trees(&[("n", Some(listed.clone()), Some(listed), Some(unlisted))]);
    trees.record_edited("n", sent.as_ref());
    assert_eq!(trees.edited_here(), [("n".to_owned(), 4)]);
}

#[test]
fn an_edit_of_a_file_in_the_trash_after_a_loss_is_kept_from_a_resend_after_a_restore_of_its_bytes()
{
    let mut trees = trees(&[synced("n", file(ROOT, "n", 3))]);
    let commits = Commits::default();
    let (trashed_here, edited) = (trees.trashed_here(), trees.edited_here());
    trees.set_remote("n".into(), trashed(file(ROOT, "n", 2)));
    trees.forget_lost(
        trashed_here,
        edited,
        Vec::new(),
        Vec::new(),
        commits.held_through(),
        commits.accepted(),
    );
    trees.settle();
    assert!(trees.unsent_edits().is_empty());
    trees.set_local("n".into(), local(file(ROOT, "n", 2), 1));
    trees.set_remote("n".into(), remote_at(file(ROOT, "n", 2), 3));
    trees.settle();
    let resent = RemoteNode {
        content_rev: 4,
        edit_rev: 4,
        ..remote_at(file(ROOT, "n", 3), 3)
    };
    trees.set_remote("n".into(), resent);
    trees.settle();
    assert_eq!(trees.class("n"), Class::Concurrent);
}

#[test]
fn a_parked_edit_whose_name_goes_to_a_folder_this_device_excludes_is_not_held_back() {
    let mut trees = trees(&[(
        "h",
        Some(file(ROOT, "x", 1)),
        Some(file(ROOT, "x", 1)),
        Some(remote(file(ROOT, "X", 1))),
    )]);
    trees.set_rules::<&str>(&[], Some(RuleFile::parse(b"x/\n")));
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let park = effects.plan(&trees, None, &mut ids, NOW).unwrap().clone();
    assert!(matches!(
        &park.effect,
        Effect::Move {
            to: Location::Parked(_),
            ..
        }
    ));
    effects
        .done(&mut trees, None, &park.effect_id, None)
        .unwrap();
    let mut edited = trees.local().get("h").unwrap().clone();
    edited.shape.content = Some(content(2));
    trees.set_local("h".into(), edited);
    trees.set_remote("h".into(), trashed(file(ROOT, "X", 1)));
    trees.set_remote("n".into(), remote(dir(ROOT, "x")));
    trees.settle();
    assert!(trees.excluded("n") && !trees.excluded("h"));
    let back = effects.plan(&trees, None, &mut ids, NOW).cloned();
    assert_eq!(
        back.map(|record| record.effect),
        Some(Effect::Move {
            from: Location::Parked("h".into()),
            identity: local(file(ROOT, "x", 1), 1).identity,
            to: Location::At {
                base: koidrive_engine::effects::Base::Root,
                dirs: vec![],
                name: "x".into(),
            },
        })
    );
}

#[test]
fn a_parked_edit_whose_name_a_synced_folder_holds_is_sent() {
    let mut trees = trees(&[
        (
            "h",
            Some(file(ROOT, "ss", 1)),
            Some(file(ROOT, "ss", 1)),
            Some(trashed(file(ROOT, "ß", 1))),
        ),
        (
            "n",
            Some(dir(ROOT, "ß")),
            Some(dir(ROOT, "ß")),
            Some(remote(dir(ROOT, "ß"))),
        ),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "ss", 2), 1)
        },
    );
    trees.settle();
    let (mut effects, mut commits, mut ids) =
        (Effects::default(), Commits::default(), IdGen::new(1));
    assert_eq!(
        effects
            .plan(&trees, commits.in_flight(), &mut ids, NOW)
            .map(|record| record.effect.clone()),
        None
    );
    let commit = build(&mut commits, &mut trees, &mut ids);
    assert!(
        commit
            .as_ref()
            .is_some_and(|commit| uploads(&ops(commit), 2)),
        "{:?}",
        commit.as_ref().map(ops)
    );
}

#[test]
fn a_parked_edit_is_sent_before_the_folder_taking_its_name_downloads() {
    let mut trees = trees(&[
        (
            "h",
            None,
            Some(file(ROOT, "x", 1)),
            Some(trashed(file(ROOT, "x", 1))),
        ),
        ("n", None, None, Some(remote(dir(ROOT, "x")))),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 2), 1)
        },
    );
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(ops(&sent).iter().any(|op| matches!(
        op,
        op::Op::CreateFile(create) if create.content == Some(content(2))
    )));
}

#[test]
fn a_parked_edit_goes_back_to_its_name_when_another_folder_gets_that_name() {
    let mut trees = trees(&[
        synced("d", dir(ROOT, "d")),
        (
            "h",
            None,
            Some(file(ROOT, "x", 1)),
            Some(trashed(file(ROOT, "x", 1))),
        ),
        ("n", None, None, Some(remote(dir("d", "x")))),
    ]);
    trees.set_local(
        "h".into(),
        LocalNode {
            parked: true,
            ..local(file(ROOT, "x", 2), 2)
        },
    );
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let mut moved_back = false;
    while let Some(record) = effects.plan(&trees, None, &mut ids, NOW).cloned() {
        moved_back |= matches!(
            &record.effect,
            Effect::Move { from: Location::Parked(_), to: Location::At { dirs, name, .. }, .. }
                if dirs.is_empty() && name == "x"
        );
        effects
            .done(
                &mut trees,
                None,
                &record.effect_id,
                Some(local(dir("d", "x"), 9).identity),
            )
            .unwrap();
        trees.settle();
    }
    assert!(moved_back);
}

#[test]
fn a_remote_move_waits_exactly_while_a_commit_in_flight_moves_the_node() {
    use error::Code;
    type Exit = Box<dyn Fn(&mut Commits, &mut Trees)>;
    let ok = || vec![outcome(op_result::Code::Applied)];
    let mut exits: Vec<(String, bool, Exit)> = (0..=i32::from(u8::MAX))
        .filter_map(|code| Code::try_from(code).ok())
        .map(|code| {
            let exit: Exit = Box::new(move |commits: &mut Commits, _: &mut Trees| {
                commits.failed(&refusal(code, 0)).unwrap();
            });
            (format!("{code:?}"), true, exit)
        })
        .collect();
    let others: [(&str, bool, Exit); 7] = [
        (
            "QuotaExceeded with a retry time",
            true,
            Box::new(|commits, _| {
                commits.failed(&refusal(Code::QuotaExceeded, 1)).unwrap();
            }),
        ),
        (
            "abandoned",
            true,
            Box::new(|commits, _| assert!(commits.abandon())),
        ),
        (
            "lifted to resend",
            true,
            Box::new(|commits, _| {
                commits.failed(&refusal(Code::Forbidden, 0)).unwrap();
                commits.lift(Lift::Resend).unwrap();
            }),
        ),
        (
            "lifted to replan",
            true,
            Box::new(|commits, _| {
                commits.failed(&refusal(Code::Forbidden, 0)).unwrap();
                commits.lift(Lift::Replan).unwrap();
            }),
        ),
        (
            "reconnected before its result",
            true,
            Box::new(|commits, _| {
                assert_eq!(commits.connected(&welcome(0)), Reconnect::Resend);
            }),
        ),
        (
            "reconnected to a server that lost it",
            true,
            Box::new(move |commits, _| {
                commits.received(result(1, ok())).unwrap();
                assert_eq!(commits.connected(&welcome(0)), Reconnect::AheadOfServer);
            }),
        ),
        (
            "acknowledged",
            false,
            Box::new(move |commits, trees| {
                commits.received(result(1, ok())).unwrap();
                assert!(commits.acknowledge(trees, 1));
            }),
        ),
    ];
    exits.extend(others.map(|(name, undone, exit)| (name.to_owned(), undone, exit)));
    let mut released = Vec::new();
    for (name, undone, exit) in exits {
        let mut trees = trees(&[synced("n", file(ROOT, "x", 1))]);
        let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
        trees.set_local("n".into(), local(file(ROOT, "sent", 1), 1));
        let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
        assert_eq!(ops(&sent), [mv("n", ROOT, "sent")]);
        if undone {
            trees.set_local("n".into(), local(file(ROOT, "x", 1), 1));
        }
        trees.set_remote("n".into(), remote(file(ROOT, "remote", 1)));
        trees.settle();
        let mut effects = Effects::default();
        let planned = effects.plan(&trees, commits.in_flight(), &mut ids, NOW);
        assert_eq!(planned, None, "{name}");
        exit(&mut commits, &mut trees);
        trees.settle();
        let planned = (effects.plan(&trees, commits.in_flight(), &mut ids, NOW)).cloned();
        if commits.in_flight().is_some() {
            assert_eq!(planned, None, "{name}");
            continue;
        }
        let next = planned.unwrap_or_else(|| panic!("{name}: R's move is not planned"));
        let to = next.after.as_ref().map(|after| after.name.as_str());
        assert_eq!(to, Some("remote"), "{name}");
        (effects.done(&mut trees, commits.in_flight(), &next.effect_id, None)).unwrap();
        trees.settle();
        assert_eq!(trees.class("n"), Class::InSync, "{name}");
        released.push(name);
    }
    assert_eq!(
        released,
        [
            "LimitExceeded",
            "ReadOnly",
            "UnsupportedVersion",
            "HeadMoved",
            "AheadOfServer",
            "abandoned",
            "lifted to replan",
            "reconnected to a server that lost it",
            "acknowledged",
        ]
    );
}

#[test]
fn a_rename_of_a_node_whose_update_waits_is_kept_from_an_effect_planned_before_it() {
    let (mut trees, commits, mut ids) = a_move_and_edit_whose_s_update_waits();
    let moved = RemoteNode {
        meta_rev: 3,
        ..remote(file(ROOT, "z", 1))
    };
    trees.set_remote("n".into(), moved);
    trees.settle();
    let mut effects = Effects::default();
    let record = loop {
        let record = (effects.plan(&trees, commits.in_flight(), &mut ids, NOW))
            .cloned()
            .unwrap();
        if record.node == "n" {
            break record;
        }
        (effects.done(&mut trees, commits.in_flight(), &record.effect_id, None)).unwrap();
        trees.settle();
    };
    assert_eq!(
        record.after.as_ref().map(|after| after.name.as_str()),
        Some("z")
    );
    trees.set_local("n".into(), local(file(ROOT, "c", 1), 2));
    trees.settle();
    assert!(trees.waiting().contains_key("n"));
    assert!(!record.applies(&trees, commits.in_flight()));
    let done = effects.done(&mut trees, commits.in_flight(), &record.effect_id, None);
    assert!(done.is_err(), "{done:?}");
    assert_eq!(trees.local().live("n"), Some(&file(ROOT, "c", 1)));
}

#[test]
fn a_rename_the_person_makes_while_the_owed_name_is_sent_is_kept() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [mv("a", ROOT, "a")]);
    trees.set_local("a".into(), local(dir(ROOT, "c"), 1));
    trees.set_remote("a".into(), remote(dir(ROOT, "a")));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    let next = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&next), [mv("a", ROOT, "c")]);
}

#[test]
fn a_move_the_person_makes_while_the_owed_name_is_sent_is_kept() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("d", dir(ROOT, "d")),
        synced("a", dir(ROOT, "a (pending)")),
    ]);
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [mv("a", ROOT, "a")]);
    trees.set_local("a".into(), local(dir("d", "a (pending)"), 2));
    trees.set_remote("a".into(), remote(dir(ROOT, "a")));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    let next = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&next), [mv("a", "d", "a (pending)")]);
}

#[test]
fn a_rename_back_to_the_pending_name_while_another_rename_is_sent_is_kept() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    trees.set_local("a".into(), local(dir(ROOT, "x"), 1));
    trees.settle();
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [mv("a", ROOT, "x")]);
    trees.set_local("a".into(), local(dir(ROOT, "a (pending)"), 1));
    trees.set_remote("a".into(), remote(dir(ROOT, "x")));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    let next = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&next), [mv("a", ROOT, "a (pending)")]);
}

#[test]
fn an_excluded_node_at_a_pending_name_gets_no_rename() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    trees.set_rules::<&str>(&[], Some(RuleFile::parse(b"a (pending)/\n")));
    trees.settle();
    assert!(trees.excluded("a"));
    assert!(!commits.due(&trees));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_held_node_at_a_pending_name_gets_no_rename() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("d", dir(ROOT, "d")),
        synced("a", dir("d", "a (pending)")),
    ]);
    trees.set_rules_unknown("d");
    trees.settle();
    assert!(trees.held("a"));
    assert!(!commits.due(&trees));
    assert_eq!(build(&mut commits, &mut trees, &mut ids), None);
}

#[test]
fn a_pending_name_is_kept_while_its_node_is_in_the_trash_and_dropped_once_it_is_purged() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        ("a", None, None, Some(trashed(dir(ROOT, "a (pending)")))),
        ("z", Some(file(ROOT, "z", 1)), None, None),
    ]);
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(uploads(&ops(&sent), 1), "{:?}", ops(&sent));
    assert!(commits.state().pending.contains_key("a"));

    trees.remove_remote("a");
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    trees.set_local("y".into(), local(file(ROOT, "y", 2), 3));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(!commits.state().pending.contains_key("a"));
}

#[test]
fn an_owed_rename_that_lands_is_not_sent_again_once_another_device_names_the_node_back() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [mv("a", ROOT, "a")]);
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a"), 1));
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a (pending)"), 2));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    assert!(commits.state().pending.is_empty());
    let next = build(&mut commits, &mut trees, &mut ids);
    assert!(
        next.as_ref()
            .is_none_or(|commit| !ops(commit).contains(&mv("a", ROOT, "a"))),
        "{:?}",
        next.as_ref().map(ops)
    );
}

#[test]
fn a_rename_this_device_makes_off_a_pending_name_ends_the_owed_rename() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    trees.set_local("a".into(), local(dir(ROOT, "c"), 1));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [mv("a", ROOT, "c")]);
    trees.set_remote("a".into(), remote_at(dir(ROOT, "c"), 1));
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a (pending)"), 2));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    trees.set_local("a".into(), local(dir(ROOT, "a (pending)"), 1));
    trees.settle();
    let next = build(&mut commits, &mut trees, &mut ids);
    assert!(
        next.as_ref()
            .is_none_or(|commit| !ops(commit).contains(&mv("a", ROOT, "a"))),
        "{:?}",
        next.as_ref().map(ops)
    );
}

#[test]
fn a_rename_another_device_makes_off_a_pending_name_ends_the_owed_rename_before_any_build() {
    let (mut commits, mut trees, mut ids) =
        owing_its_name(&[synced("a", dir(ROOT, "a (pending)"))]);
    trees.set_remote("a".into(), remote_at(dir(ROOT, "c"), 1));
    assert!(!commits.acknowledge(&mut trees, 1));
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a (pending)"), 2));
    assert!(!commits.acknowledge(&mut trees, 2));
    trees.settle();
    let next = build(&mut commits, &mut trees, &mut ids);
    assert!(
        next.as_ref()
            .is_none_or(|commit| !ops(commit).contains(&mv("a", ROOT, "a"))),
        "{:?}",
        next.as_ref().map(ops)
    );
}

#[test]
fn a_rename_another_device_makes_while_this_device_moves_the_node_ends_the_owed_rename() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("d", dir(ROOT, "d")),
        synced("a", dir(ROOT, "a (pending)")),
    ]);
    trees.set_local("a".into(), local(dir("d", "a (pending)"), 2));
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [mv("a", "d", "a (pending)")]);
    trees.set_remote("a".into(), remote_at(dir("d", "a (pending)"), 8));
    assert!(!commits.acknowledge(&mut trees, 8));
    trees.set_remote("a".into(), remote_at(dir("d", "c"), 9));
    assert!(!commits.acknowledge(&mut trees, 9));
    trees.set_remote("a".into(), remote_at(dir("d", "a (pending)"), 10));
    assert!(!commits.acknowledge(&mut trees, 10));
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied)]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 10));
    trees.settle();
    let next = build(&mut commits, &mut trees, &mut ids);
    assert!(
        next.as_ref()
            .is_none_or(|commit| !ops(commit).contains(&mv("a", "d", "a"))),
        "{:?}",
        next.as_ref().map(ops)
    );
}

#[test]
fn a_rename_another_device_makes_once_a_pending_create_lands_ends_the_owed_rename_before_the_result()
 {
    let mut trees = trees(&[
        (
            "h",
            Some(file("n", "child", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        ("n", Some(dir(ROOT, "a")), None, None),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(first.ops.len(), 2);
    assert!(!commits.state().pending["n"].landed);
    trees.remove_local("h");
    trees.remove_local("n");
    trees.set_remote("n".into(), remote_at(dir(ROOT, "a (pending)"), 8));
    trees.set_remote("h".into(), remote_at(file("n", "child", 1), 8));
    assert!(!commits.acknowledge(&mut trees, 8));
    assert!(commits.state().pending["n"].landed);
    trees.set_remote("n".into(), remote_at(dir(ROOT, "c"), 9));
    trees.set_remote("h".into(), remote_at(file("n", "child", 2), 9));
    assert!(!commits.acknowledge(&mut trees, 9));
    assert!(!commits.state().pending.contains_key("n"));
    trees.set_remote("n".into(), remote_at(dir(ROOT, "a (pending)"), 10));
    assert!(!commits.acknowledge(&mut trees, 10));
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied); 2]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 10));
    trees.settle();
    let _ = build(&mut commits, &mut trees, &mut ids);
    effects_for(&mut trees, &mut ids, "n");
    let next = build(&mut commits, &mut trees, &mut ids);
    assert!(
        next.as_ref()
            .is_none_or(|commit| !ops(commit).contains(&mv("n", ROOT, "a"))),
        "{:?}",
        next.as_ref().map(ops)
    );
}

#[test]
fn an_owed_rename_sent_through_a_name_cycle_still_brings_the_disk_to_its_name() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("a", dir(ROOT, "a (pending)")),
        (
            "b",
            Some(dir(ROOT, "A (pending)")),
            Some(dir(ROOT, "a")),
            Some(remote(dir(ROOT, "a"))),
        ),
    ]);
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(sent.ops.len(), 3);
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a"), 8));
    trees.set_remote("b".into(), remote_at(dir(ROOT, "A (pending)"), 8));
    commits
        .received(result(8, vec![outcome(op_result::Code::Applied); 3]))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    effects_for(&mut trees, &mut ids, "a");
    assert_eq!(
        trees.local().live("a").map(|shape| shape.name.as_str()),
        Some("a")
    );
}

#[test]
fn a_folder_another_device_moves_at_its_pending_name_keeps_that_name() {
    reparent_a_folder_at_its_pending_name(true);
}

#[test]
fn a_folder_another_device_moves_at_its_pending_name_before_the_answer_keeps_that_name() {
    reparent_a_folder_at_its_pending_name(false);
}

#[test]
fn a_folder_another_device_moves_while_its_owed_name_is_still_taken_keeps_that_place() {
    let mut trees = trees(&[
        (
            "h",
            Some(file("n", "a", 1)),
            Some(file(ROOT, "a", 1)),
            Some(remote(file(ROOT, "a", 1))),
        ),
        ("n", Some(dir(ROOT, "A")), None, None),
        (
            "x",
            Some(dir(ROOT, "x")),
            Some(dir(ROOT, "x")),
            Some(remote(dir(ROOT, "x"))),
        ),
    ]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let first = build(&mut commits, &mut trees, &mut ids).unwrap();
    let results = ops(&first)
        .iter()
        .map(|op| match op {
            op::Op::Move(m) if m.node_id == "h" => outcome(op_result::Code::Superseded),
            _ => outcome(op_result::Code::Applied),
        })
        .collect();
    trees.set_remote("n".into(), remote_at(dir(ROOT, "A (pending)"), 8));
    commits.received(result(8, results)).unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    trees.set_remote("n".into(), remote_at(dir("x", "A (pending)"), 9));
    commits.acknowledge(&mut trees, 9);
    trees.settle();
    let n_moves = |commit: &proto::Commit| {
        ops(commit)
            .into_iter()
            .filter(|op| matches!(op, op::Op::Move(m) if m.node_id == "n"))
            .collect::<Vec<_>>()
    };
    if let Some(commit) = build(&mut commits, &mut trees, &mut ids) {
        assert_eq!(n_moves(&commit), [], "{:?}", ops(&commit));
    }
    assert!(commits.in_flight().is_none());
    trees.set_remote("h".into(), remote_at(file("n", "a", 1), 10));
    commits.acknowledge(&mut trees, 10);
    trees.settle();
    let _ = build(&mut commits, &mut trees, &mut ids);
    effects_for(&mut trees, &mut ids, "n");
    if let Some(commit) = build(&mut commits, &mut trees, &mut ids) {
        assert_eq!(n_moves(&commit), [], "{:?}", ops(&commit));
    }
    let n = &trees.local().get("n").unwrap().shape;
    assert_eq!((n.parent.as_str(), n.name.as_str()), ("x", "A (pending)"));
}

#[test]
fn a_folder_the_user_moves_elsewhere_under_its_owed_name_is_still_moved() {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        synced("u", dir(ROOT, "u")),
        synced("x", dir(ROOT, "x")),
        (
            "a",
            Some(dir("u", "a")),
            Some(dir(ROOT, "a (pending)")),
            Some(remote(dir("x", "a (pending)"))),
        ),
    ]);
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&sent), [mv("a", "u", "a")]);
}

#[test]
fn a_file_another_device_deletes_while_this_device_edits_it_takes_its_owed_name_once_the_edit_brings_it_back()
 {
    let (mut commits, mut trees, mut ids) = owing_its_name(&[(
        "a",
        Some(file(ROOT, "a", 2)),
        Some(file(ROOT, "a (pending)", 1)),
        Some(trashed(file(ROOT, "a (pending)", 1))),
    )]);
    assert!(!commits.acknowledge(&mut trees, 1));
    trees.settle();
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(!ops(&sent).contains(&mv("a", ROOT, "a")));
    trees.set_remote("a".into(), remote_at(file(ROOT, "a (pending)", 2), 2));
    acknowledge(
        &mut commits,
        &mut trees,
        vec![outcome(op_result::Code::Applied)],
    );
    trees.settle();
    let next = build(&mut commits, &mut trees, &mut ids);
    assert_eq!(next.as_ref().map(ops), Some(vec![mv("a", ROOT, "a")]));
}

#[test]
fn a_folder_another_device_deletes_takes_its_owed_name_once_an_unsent_edit_below_it_brings_it_back()
{
    let (mut commits, mut trees, mut ids) = owing_its_name(&[
        (
            "a",
            Some(dir(ROOT, "a")),
            Some(dir(ROOT, "a (pending)")),
            Some(trashed(dir(ROOT, "a (pending)"))),
        ),
        (
            "f",
            Some(file("a", "c.txt", 1)),
            Some(file("a", "c.txt", 1)),
            Some(trashed(file("a", "c.txt", 1))),
        ),
    ]);
    let mut state = trees.state();
    state.unsent_edits.insert("f".into());
    state.dirty.insert("f".into());
    trees.restore(state).unwrap();
    trees.rules_scanned();
    trees.settle();
    assert!(!commits.acknowledge(&mut trees, 1));
    trees.settle();
    let sent = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(
        ops(&sent)
            .iter()
            .any(|op| matches!(op, op::Op::UpdateContent(update) if update.node_id == "f"))
    );
    assert!(
        !ops(&sent)
            .iter()
            .any(|op| matches!(op, op::Op::Move(moved) if moved.node_id == "a"))
    );
    trees.set_remote("a".into(), remote_at(dir(ROOT, "a (pending)"), 8));
    trees.set_remote("f".into(), remote_at(file("a", "c.txt", 1), 8));
    commits
        .received(result(
            8,
            vec![outcome(op_result::Code::Applied); sent.ops.len()],
        ))
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 8));
    trees.settle();
    let rename = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&rename), [mv("a", ROOT, "a")]);
}
