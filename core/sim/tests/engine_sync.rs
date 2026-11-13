// SPDX-License-Identifier: Apache-2.0

//! The engine's commits and effects against the reference server (docs/design/sync-engine.md
//! section 7.2, planner property). A device with offline local changes meets another device's
//! concurrent commits, then plans until nothing is left. It must end with L = S = R equal to the
//! server's tree, and every write either device made must still be on the server.
//!
//! Both devices also restore from the trash and restore earlier versions (section 6.3). The other
//! device commits its restores; this one asks its engine for them, as an app does, and every
//! request must end with the result the server gave its `Restore` op, sent once.

include!("engine_sync/common.rs");
include!("engine_sync/first_link.rs");
include!("engine_sync/moves_and_deletes.rs");
include!("engine_sync/parked_nodes.rs");
include!("engine_sync/restores.rs");
include!("engine_sync/rules_and_held_entries.rs");
include!("engine_sync/waiting_updates.rs");

proptest! {
    #![proptest_config(koidrive_test_support::regression_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/engine_sync.proptest-regressions"),
        1024,
    ))]

    #[test]
    fn a_device_with_local_changes_converges_with_the_server_and_keeps_every_write(
        seed in prop::collection::vec(action(2), 0..12),
        local in prop::collection::vec(action(7), 0..8),
        remote in prop::collection::vec(action(7), 0..8),
        fits in prop_oneof![Just(MAX_OPS), 1usize..4],
        fresh_dir_names in any::<bool>(),
        during in prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(7), 1..4)),
            0..6,
        ),
        cut in prop::sample::select(vec![
            Cut::None,
            Cut::Built,
            Cut::Applied,
            Cut::Answered,
            Cut::Effect,
        ]),
    ) {
        run_cut(&seed, &local, &remote, fits, fresh_dir_names, &during, cut)?;
    }

    /// Section 5.10: becoming ignored never deletes anything, on either side. Neither device
    /// deletes here, so whatever the rules and the moves into and out of ignored paths do, the
    /// device sends no `Delete`, removes nothing from its disk, and the server trashes nothing,
    /// across the restart the run makes at every step.
    #[test]
    fn a_device_whose_rules_change_deletes_nothing_on_either_side(
        seed in prop::collection::vec(action(2), 0..12),
        local in prop::collection::vec(action(4), 0..8),
        remote in prop::collection::vec(action(4), 0..8),
        fits in prop_oneof![Just(MAX_OPS), 1usize..4],
        during in prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(4), 1..4)),
            0..6,
        ),
        rulings in prop::collection::vec(ruling(), 0..7),
    ) {
        let ran = run_ignoring(
            &seed,
            &local,
            &remote,
            fits,
            true,
            &during,
            Cut::None,
            &rulings,
            Answer::Delete,
            false,
        )?;
        prop_assert_eq!((ran.deletes, ran.removals, ran.trashed), (0, 0, 0));
    }
}

proptest! {
    #![proptest_config(koidrive_test_support::regression_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/engine_sync.proptest-regressions"),
        128,
    ))]

    /// Section 6.1: removing a folder that holds more than half of a space of at least 20 files
    /// is held, among other local changes that still sync. No `Delete` of a held node goes out
    /// before the person answers, across the restart the run makes at every step. After
    /// `restore` the files come back on disk and the server trashes nothing; after `delete` the
    /// server trashes the folder and every file in it.
    #[test]
    fn removing_most_of_a_space_is_held_until_the_person_answers(
        inside in 11usize..20,
        loose in 9usize..11,
        local in prop::collection::vec(action(3), 0..4),
        fits in prop_oneof![Just(MAX_OPS), 1usize..4],
        answer in prop_oneof![Just(Answer::Delete), Just(Answer::Restore)],
        cut in prop::sample::select(CUTS.to_vec()),
    ) {
        let mut seed = vec![act(0, 0, 0, 0, 0)];
        seed.extend((0..inside).map(|i| act(1, 0, 1, i % NAMES.len(), 0)));
        seed.extend((0..loose).map(|i| act(1, 0, 0, i % NAMES.len(), 0)));
        let (space, _) = seeded(&seed);
        let root = space.space_id().to_owned();
        let mut live: Vec<_> = (space.nodes().into_iter())
            .filter(|node| node.status == 1 && node.node_id != root)
            .collect();
        live.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        prop_assert_eq!(live.len(), inside + loose + 1, "every seeded file has its own name");
        let folder = live.iter().position(|node| node.kind == 1).unwrap();
        let mut actions = vec![act(4, folder, 0, 0, 0)];
        actions.extend(local);

        let ran = run_ignoring(&seed, &actions, &[], fits, true, &[], cut, &[], answer, false)?;

        prop_assert!(ran.asked >= 1, "the removal was asked about");
        match answer {
            Answer::Restore => prop_assert_eq!((ran.deletes, ran.removals, ran.trashed), (0, 0, 0)),
            Answer::Delete => prop_assert_eq!(ran.trashed, inside + 1),
            other => unreachable!("the guard does not offer {other}"),
        }
    }
}

proptest! {
    #![proptest_config(koidrive_test_support::regression_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/engine_sync.proptest-regressions"),
        256,
    ))]

    /// Section 6.4: on the first link of a folder to a space that holds something, what both hold
    /// alike is adopted and the rest of the folder is held. No create of a held entry goes out
    /// before the person publishes it, across the restart the run makes at every step, while the
    /// user and the other device go on changing things. After `publish` every local write is on the
    /// server; after `discard` the discarded entries are in the local trash and every other write
    /// is on the server.
    #[test]
    fn a_first_link_sends_nothing_it_holds_until_the_person_publishes_it(
        seed in prop::collection::vec(action(2), 1..12),
        local in prop::collection::vec(action(2), 1..8),
        fits in prop_oneof![Just(MAX_OPS), 1usize..4],
        during in prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(5), 1..4)),
            0..6,
        ),
        answer in prop_oneof![Just(Answer::Publish), Just(Answer::Discard)],
        cut in prop::sample::select(CUTS.to_vec()),
    ) {
        run_ignoring(&seed, &local, &[], fits, false, &during, cut, &[], answer, true)?;
    }

    #[test]
    fn a_first_link_sends_nothing_the_rules_leave_out_for_a_while_until_the_person_publishes_it(
        seed in prop::collection::vec(action(2), 1..12),
        local in prop::collection::vec(action(2), 1..8),
        fits in prop_oneof![Just(MAX_OPS), 1usize..4],
        during in prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(5), 1..4)),
            0..6,
        ),
        rulings in prop::collection::vec(ruling(), 0..7),
        answer in prop_oneof![Just(Answer::Publish), Just(Answer::Discard)],
        cut in prop::sample::select(CUTS.to_vec()),
    ) {
        run_ignoring(&seed, &local, &[], fits, false, &during, cut, &rulings, answer, true)?;
    }
}
