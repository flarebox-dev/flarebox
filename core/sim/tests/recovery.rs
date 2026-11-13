// SPDX-License-Identifier: Apache-2.0

use koidrive_proto::v1::{Commit, NodeStatus, error};
use koidrive_sim::world::disk::Disk;
use koidrive_sim::world::{Action, Cut, Failure, NAMES, Outcome, Reply, Step, World};

fn user(kind: u8, target: usize, parent: usize, name: &str) -> Step {
    let name = NAMES.iter().position(|other| *other == name).unwrap();
    Step::User(Action {
        kind,
        target,
        parent,
        name,
        text: 0,
    })
}

fn file(name: &str) -> Step {
    user(1, 0, 0, name)
}

fn overwrite(target: usize) -> Step {
    user(2, target, 0, "a")
}

fn rename(target: usize, name: &str) -> Step {
    user(3, target, 0, name)
}

fn delete(target: usize) -> Step {
    user(4, target, 0, "a")
}

const SCAN: Step = Step::Scan;
const EFFECT: Step = Step::Effect(Outcome::Run);
const COMMIT: Step = Step::Commit(Reply::Served);
const DELIVER: Step = Step::Deliver(0);

fn commit(device: usize) -> Vec<(usize, Step)> {
    vec![
        (device, SCAN),
        (device, COMMIT),
        (device, COMMIT),
        (device, DELIVER),
    ]
}

fn synced(devices: usize, phases: &[Vec<(usize, Step)>]) -> World {
    let mut world = World::new(devices);
    world.trace = std::env::var_os("KOIDRIVE_SIM_TRACE").is_some();
    for (phase, schedule) in phases.iter().enumerate() {
        world.phase(phase, schedule).unwrap();
    }
    world
}

fn live(world: &World) -> Vec<String> {
    let root = world.space().space_id().to_owned();
    let mut names: Vec<String> = (world.space().nodes().into_iter())
        .filter(|node| node.node_id != root && node.status == NodeStatus::Live as i32)
        .map(|node| node.name)
        .collect();
    names.sort();
    names
}

fn refusal(world: &World, d: usize, commit: &Commit) -> Option<error::Code> {
    let mut space = world.space().clone();
    let refused = space.commit(&world.devices[d].session, 0, commit).err()?;
    error::Code::try_from(refused.code).ok()
}

fn recovering(world: &World, d: usize) -> &str {
    (world.devices[d].recovering.as_deref())
        .unwrap_or_else(|| panic!("device {d} did not start recovery"))
}

#[test]
fn pitr_replay() {
    let mut world = synced(2, &[vec![(0, file("a"))]]);
    world
        .run(
            1,
            &[
                (0, Step::Backup),
                (0, delete(0)),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, Step::Drop),
                (0, Step::Rewind),
                (0, Step::Connect),
            ],
        )
        .unwrap();
    assert_eq!(recovering(&world, 0), "the server's history changed");
    let k = (world.devices[0].engine.commits.in_flight())
        .expect("K is kept as evidence")
        .commit
        .clone();
    assert_eq!(refusal(&world, 0, &k), Some(error::Code::HistoryMismatch));
    world.phase(2, &[]).unwrap();
    recovering(&world, 1);
    assert_eq!(live(&world), ["a"]);
    assert!(world.devices[0].disk.entries().is_empty());
}

#[test]
fn history_change_on_connect() {
    let mut world = synced(
        2,
        &[
            vec![(0, Step::Backup), (0, file("a"))],
            vec![(1, file("b"))],
        ],
    );
    world.run(2, &[(0, Step::Rewind)]).unwrap();
    let before = world.devices[1].engine.clone();
    world
        .run(2, &[(1, Step::Connect), (1, Step::Work(0))])
        .unwrap();
    recovering(&world, 1);
    assert_eq!(world.devices[1].engine, before);
    world.phase(3, &[]).unwrap();
    assert!(live(&world).is_empty());
    assert_eq!(world.devices[0].disk.entries().len(), 2);
}

#[test]
fn stale_store_replay() {
    let mut world = synced(1, &[]);
    world
        .run(
            0,
            &[
                (0, file("a")),
                (0, SCAN),
                (0, COMMIT),
                (0, Step::CopyStore),
                (0, COMMIT),
                (0, DELIVER),
                (0, file("b")),
            ],
        )
        .unwrap();
    world.run(0, &commit(0)).unwrap();
    world.truncate_log();
    world
        .run(
            0,
            &[(0, Step::RollBack), (0, Step::Connect), (0, Step::Connect)],
        )
        .unwrap();
    assert!(recovering(&world, 0).contains("cannot account for"));
    let k = (world.devices[0].engine.commits.in_flight())
        .expect("K is in flight in the copied store")
        .commit
        .clone();
    assert_eq!(refusal(&world, 0, &k), Some(error::Code::StaleDeviceState));
    world.phase(1, &[]).unwrap();
    assert_eq!(live(&world), ["a", "b"]);
}

#[test]
fn store_rollback_below_the_log_floor() {
    let mut world = synced(2, &[vec![(0, file("a"))]]);
    world
        .run(
            0,
            &[
                (0, Step::CopyStore),
                (0, Step::Drop),
                (1, file("b")),
                (1, SCAN),
                (1, COMMIT),
                (1, COMMIT),
            ],
        )
        .unwrap();
    world.truncate_log();
    world
        .run(
            1,
            &[(0, Step::RollBack), (0, Step::Connect), (0, Step::Connect)],
        )
        .unwrap();
    assert!(world.seen.contains_key("resume finds the log truncated"));
    assert!(world.devices[0].recovering.is_none());
    world.phase(1, &[]).unwrap();
    assert_eq!(live(&world), ["a", "b"]);
}

#[test]
fn remote_rename_then_store_rollback() {
    let mut world = synced(2, &[vec![(0, file("a"))]]);
    world
        .run(1, &[(1, Step::CopyStore), (0, rename(0, "b"))])
        .unwrap();
    world.run(1, &commit(0)).unwrap();
    world
        .run(
            1,
            &[
                (1, DELIVER),
                (1, EFFECT),
                (1, EFFECT),
                (1, Step::RollBack),
                (1, Step::Connect),
            ],
        )
        .unwrap();
    assert!(recovering(&world, 1).contains("root marker"));
    world.phase(2, &[]).unwrap();
    assert_eq!(live(&world), ["b"]);
    let disk = world.devices[1].disk.entries();
    assert_eq!(disk.len(), 1);
    assert!(disk.values().all(|entry| entry.name == "b"));
}

#[test]
fn remote_rename_then_store_rollback_and_a_delete() {
    let mut world = synced(2, &[vec![(0, file("a"))]]);
    world
        .run(1, &[(1, Step::CopyStore), (0, rename(0, "b"))])
        .unwrap();
    world.run(1, &commit(0)).unwrap();
    world
        .run(
            1,
            &[(1, DELIVER), (1, EFFECT), (1, EFFECT), (1, Step::RollBack)],
        )
        .unwrap();
    world.run(1, &[(0, delete(0))]).unwrap();
    world.run(1, &commit(0)).unwrap();
    world.phase(2, &[]).unwrap();
    recovering(&world, 1);
    assert!(live(&world).is_empty());
    assert_eq!(world.devices[1].disk.entries().len(), 1);
}

fn four_commits() -> World {
    let world = synced(
        2,
        &[
            vec![(0, file("a"))],
            vec![(0, file("b"))],
            vec![(0, overwrite(0))],
            vec![(0, overwrite(1))],
        ],
    );
    assert_eq!(world.space().device_seq("device-0"), 4);
    world
}

#[test]
fn accepted_response_lost() {
    let mut world = four_commits();
    world
        .run(
            4,
            &[
                (0, overwrite(0)),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, Step::Drop),
            ],
        )
        .unwrap();
    assert_eq!(world.space().device_seq("device-0"), 5);
    world.phase(4, &[]).unwrap();
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.recovering.is_none())
    );
    assert!(
        world
            .seen
            .contains_key("reconnect resends a commit the server applied")
    );
    assert!(
        world
            .seen
            .contains_key("commit answered from the stored result")
    );
    assert_eq!(world.space().device_seq("device-0"), 5);
}

#[test]
fn refused_then_replan() {
    let mut world = four_commits();
    world
        .run(4, &[(0, overwrite(0)), (0, SCAN), (0, COMMIT)])
        .unwrap();
    let k = (world.devices[0].engine.commits.in_flight())
        .expect("K is in flight")
        .commit
        .clone();
    assert_eq!(k.device_seq, 5);
    world
        .run(
            4,
            &[
                (0, overwrite(0)),
                (0, Step::Commit(Reply::NoChunks)),
                (0, DELIVER),
                (0, SCAN),
                (0, COMMIT),
            ],
        )
        .unwrap();
    let replanned = (world.devices[0].engine.commits.in_flight())
        .expect("the changes are planned again")
        .commit
        .clone();
    assert_eq!(replanned.device_seq, 5);
    assert_ne!(replanned.commit_id, k.commit_id);
    world.phase(4, &[]).unwrap();
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.recovering.is_none())
    );
    assert_eq!(world.space().device_seq("device-0"), 5);
}

#[test]
fn rewound_write_only_one_disk_holds() {
    let mut world = synced(2, &[vec![(0, Step::Backup), (0, file("a"))]]);
    assert!(world.devices[0].disk.holds(1) && world.devices[1].disk.holds(1));
    world
        .run(
            1,
            &[
                (0, Step::Drop),
                (1, delete(0)),
                (1, SCAN),
                (1, COMMIT),
                (1, COMMIT),
                (1, DELIVER),
                (0, Step::Rewind),
            ],
        )
        .unwrap();
    assert!(
        world
            .seen
            .contains_key("rewind takes a write only one disk holds")
    );
    world.phase(2, &[]).unwrap();
    assert!(world.devices[0].disk.holds(1));
}

#[test]
fn rewound_write_whose_delete_this_device_has_not_applied() {
    let mut world = synced(2, &[vec![(0, Step::Backup), (0, file("a"))]]);
    world
        .run(
            1,
            &[
                (1, delete(0)),
                (1, SCAN),
                (1, COMMIT),
                (1, COMMIT),
                (1, DELIVER),
                (0, DELIVER),
                (0, DELIVER),
                (0, DELIVER),
                (0, DELIVER),
            ],
        )
        .unwrap();
    assert!(world.devices[0].engine.effects.in_flight().is_empty());
    assert!(world.devices[0].disk.holds(1));
    world.run(1, &[(0, Step::Rewind)]).unwrap();
    world.phase(2, &[]).unwrap();
    assert!(world.devices[0].disk.holds(1));
}

#[test]
fn rewound_write_another_device_overwrote() {
    let mut world = synced(2, &[vec![(0, Step::Backup), (0, file("a"))]]);
    world
        .run(
            1,
            &[
                (0, Step::Drop),
                (1, overwrite(0)),
                (1, SCAN),
                (1, COMMIT),
                (1, COMMIT),
                (1, DELIVER),
                (1, delete(0)),
                (0, Step::Rewind),
            ],
        )
        .unwrap();
    world.phase(2, &[]).unwrap();
    assert!(world.devices[0].disk.holds(1));
}

#[test]
fn rewound_write_is_held_and_never_uploaded_unanswered() {
    let mut world = synced(2, &[vec![(0, Step::Backup), (0, file("a"))]]);
    world.run(1, &[(0, Step::Rewind)]).unwrap();
    world.phase(2, &[]).unwrap();
    assert_eq!(live(&world), Vec::<String>::new());
    assert!(world.devices[0].disk.holds(1) && world.devices[1].disk.holds(1));
}

#[test]
fn crash_and_rollback_during_recovery() {
    for fault in [Step::Crash, Step::RollBack] {
        let mut world = synced(1, &[vec![(0, Step::Backup), (0, file("a"))]]);
        world
            .run(
                1,
                &[(0, Step::CopyStore), (0, Step::Rewind), (0, Step::Connect)],
            )
            .unwrap();
        recovering(&world, 0);
        world
            .run(1, &[(0, fault), (0, Step::Connect), (0, Step::Connect)])
            .unwrap();
        assert!(
            world.seen["recovery starts after a rewind"] >= 2,
            "{fault:?}"
        );
        recovering(&world, 0);
        world.phase(2, &[]).unwrap();
        assert!(world.devices[0].disk.holds(1), "{fault:?}");
    }
}

#[test]
fn commit_refused_after_store_rollback() {
    let act = |kind, name, text| {
        Step::User(Action {
            kind,
            target: 0,
            parent: 0,
            name,
            text,
        })
    };
    let mut world = World::new(2);
    world.trace = std::env::var_os("KOIDRIVE_SIM_TRACE").is_some();
    world
        .run(
            0,
            &[
                (0, act(1, 0, 0)),
                (0, act(1, 1, 1)),
                (0, act(0, 0, 0)),
                (0, Step::Work(0)),
                (0, act(2, 0, 0)),
                (0, EFFECT),
                (0, act(0, 0, 0)),
                (0, COMMIT),
                (0, act(0, 0, 0)),
                (0, Step::CopyStore),
                (0, Step::Work(2445374581055045480)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, act(0, 0, 0)),
                (0, Step::RollBack),
            ],
        )
        .unwrap();
    world.phase(1, &[]).unwrap();
    assert!(
        world
            .seen
            .contains_key("recovery starts after a refused commit"),
        "{:?}",
        world.seen
    );
}

fn erase_disk(world: &mut World, d: usize) {
    let inos: Vec<u128> = world.devices[d].disk.entries().keys().copied().collect();
    for ino in inos {
        world.devices[d].disk.delete(ino);
    }
}

#[test]
fn repeated_rewind_keeps_the_write_obligation() {
    let mut world = synced(2, &[vec![(0, Step::Backup), (0, file("a"))]]);
    world.phase(1, &[(0, Step::Rewind)]).unwrap();
    world
        .run(2, &[(0, Step::Backup), (0, Step::Rewind)])
        .unwrap();
    erase_disk(&mut world, 0);
    erase_disk(&mut world, 1);
    assert!(matches!(world.phase(2, &[]), Err(Failure::Broken(_))));
}

#[test]
fn accepted_write_not_yet_in_the_remote_tree_is_required() {
    let mut world = synced(1, &[]);
    world
        .run(
            0,
            &[
                (0, Step::Backup),
                (0, file("a")),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
            ],
        )
        .unwrap();
    assert_eq!(world.space().head_seq(), 1);
    assert_eq!(world.devices[0].engine.cursor.seq, 0);
    world
        .run(1, &[(0, Step::Rewind), (0, Step::Connect)])
        .unwrap();
    recovering(&world, 0);
    erase_disk(&mut world, 0);
    assert!(matches!(world.phase(1, &[]), Err(Failure::Broken(_))));
}

#[test]
fn rewound_write_only_a_quarantine_holds() {
    for swept in [false, true] {
        let mut world = synced(2, &[vec![(0, Step::Backup), (0, file("a"))]]);
        world
            .run(
                1,
                &[
                    (0, Step::Drop),
                    (1, user(2, 0, 0, "a")),
                    (1, SCAN),
                    (1, COMMIT),
                    (1, COMMIT),
                    (1, DELIVER),
                    (0, Step::Connect),
                    (0, DELIVER),
                    (0, Step::Effect(Outcome::Crash(Cut::Quarantined))),
                    (1, delete(0)),
                    (0, Step::Rewind),
                ],
            )
            .unwrap();
        assert!(world.devices[0].disk.holds(1) && !world.devices[0].disk.shows(1));
        assert!(!world.devices[1].disk.holds(1));
        if swept {
            let record = (world.devices[0].engine.effects.in_flight().first())
                .unwrap()
                .clone();
            world.devices[0].disk = Disk::default();
            world.devices[0].disk.apply(&record, None);
            assert!(matches!(world.phase(2, &[]), Err(Failure::Broken(_))));
        } else {
            world.phase(2, &[]).unwrap();
            assert!(world.devices[0].disk.binned(1));
        }
    }
}
