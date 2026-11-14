// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_deleted_folder_comes_back_with_the_same_node_ids() {
    let seed = a_folder_with_a_file();
    let (space, _) = seeded(&seed);
    let nodes = nodes_with(&space, 1, None);
    let folder = nodes_with(&space, 1, Some(1))[0].clone();
    let file = nodes_with(&space, 1, Some(2))[0].clone();
    let folder_at = nodes.iter().position(|id| *id == folder).unwrap();
    for cut in CUTS {
        let delete = [act(4, folder_at, 0, 0, 0)];
        let restore = (true, vec![act(5, folder_at, 0, 0, 0)]);
        let ran = run_cut(&seed, &[], &delete, MAX_OPS, false, &[restore], cut).unwrap();
        assert_eq!(ran.resolved.len(), 1, "{cut:?}");
        assert_eq!(ran.resolved[0].intent.node(), folder, "{cut:?}");
        assert_eq!(ran.resolved[0].outcome, Outcome::Restored, "{cut:?}");
        assert_eq!(nodes_with(&ran.space, 1, None), nodes, "{cut:?}");
        assert_eq!(nodes_with(&ran.space, 2, None), Vec::<String>::new());
        assert_eq!(version_digests(&ran.space, &file).len(), 1, "{cut:?}");
    }
}

#[test]
fn a_node_moved_into_a_merged_folder_follows_it_on_disk() {
    let seed = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 2,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 6967822464045520996,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 3,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 9526621794622326438,
            name: 1,
            text: 0,
        },
    ];
    let local = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 10553703446551236695,
            name: 5,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 16106826873077682970,
            name: 2,
            text: 0,
        },
        Action {
            kind: 3,
            target: 880737998146561905,
            parent: 5552030480946143737,
            name: 1,
            text: 0,
        },
    ];
    let remote = vec![];
    let fits = 1;
    let fresh_dir_names = false;
    let during = vec![
        (
            false,
            vec![Action {
                kind: 3,
                target: 443641311708994000,
                parent: 3064515660927608345,
                name: 5,
                text: 0,
            }],
        ),
        (
            false,
            vec![Action {
                kind: 0,
                target: 0,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

#[test]
fn an_edited_renamed_file_deleted_remotely_is_uploaded_as_a_new_file() {
    let seed = vec![Action {
        kind: 1,
        target: 0,
        parent: 0,
        name: 0,
        text: 0,
    }];
    let local = vec![
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 0,
            parent: 0,
            name: 3,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 3,
            target: 2278650491807642941,
            parent: 9127562380887983576,
            name: 3,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 10772034385298179478,
            name: 3,
            text: 0,
        },
    ];
    let fits = 1000;
    let fresh_dir_names = false;
    let during = vec![
        (
            false,
            vec![
                Action {
                    kind: 4,
                    target: 12531602759342590661,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 3,
                    target: 283388978725368259,
                    parent: 0,
                    name: 4,
                    text: 0,
                },
                Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 1,
                },
            ],
        ),
        (
            false,
            vec![Action {
                kind: 2,
                target: 2943695200610841979,
                parent: 0,
                name: 0,
                text: 0,
            }],
        ),
        (
            true,
            vec![Action {
                kind: 1,
                target: 0,
                parent: 0,
                name: 3,
                text: 0,
            }],
        ),
        (
            false,
            vec![
                Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 4,
                    target: 6986862736185568223,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

#[test]
fn a_node_moved_into_a_merged_folder_follows_a_remote_move() {
    let seed = vec![Action {
        kind: 1,
        target: 0,
        parent: 0,
        name: 3,
        text: 0,
    }];
    let local = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 2,
            text: 0,
        },
        Action {
            kind: 4,
            target: 3390051005637832722,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 1,
            target: 0,
            parent: 3573038702653260728,
            name: 5,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 3,
            text: 0,
        },
    ];
    let remote = vec![
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 4,
            target: 761663922910750675,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 0,
            name: 5,
            text: 0,
        },
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 2,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        },
        Action {
            kind: 0,
            target: 0,
            parent: 3824710055192453609,
            name: 0,
            text: 0,
        },
    ];
    let fits = 1;
    let fresh_dir_names = false;
    let during = vec![
        (
            true,
            vec![
                Action {
                    kind: 0,
                    target: 0,
                    parent: 7106800540313915127,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 3,
                    target: 69233815001145249,
                    parent: 11870616208155072313,
                    name: 5,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![
                Action {
                    kind: 3,
                    target: 4672328705971433533,
                    parent: 9740151841705065456,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 4,
                    target: 1845576325211218462,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ],
        ),
        (
            true,
            vec![
                Action {
                    kind: 4,
                    target: 10069492413079588418,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
                Action {
                    kind: 3,
                    target: 17076322275721598278,
                    parent: 13906480724080066383,
                    name: 5,
                    text: 0,
                },
            ],
        ),
        (
            false,
            vec![Action {
                kind: 3,
                target: 10335766898245895506,
                parent: 4894905971208633107,
                name: 0,
                text: 0,
            }],
        ),
    ];
    run(&seed, &local, &remote, fits, fresh_dir_names, &during).unwrap();
}

#[test]
fn an_edit_the_other_device_deleted_gives_way_to_the_folder_it_made_at_that_name() {
    let seed = [
        act(1, 0, 0, 2, 0),
        act(0, 0, 0, 3, 0),
        act(0, 0, 95372568677471977, 0, 0),
    ];
    let local = [
        act(0, 0, 10639126546080908428, 0, 0),
        act(0, 0, 2083088007821781, 0, 0),
        act(5, 0, 0, 0, 3),
    ];
    let during = [
        (
            false,
            vec![act(4, 4134113597317913042, 0, 0, 0), act(3, 0, 0, 3, 0)],
        ),
        (true, vec![act(2, 0, 0, 0, 0)]),
        (false, vec![act(4, 0, 0, 0, 0), act(0, 0, 0, 3, 0)]),
    ];
    run_cut(&seed, &local, &[], 1, false, &during, Cut::None).unwrap();
}
