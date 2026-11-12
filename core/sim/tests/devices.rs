// SPDX-License-Identifier: Apache-2.0

//! Two or three devices share one reference server while their users edit, effects fail,
//! messages are lost, repeated or reordered, commits are refused and devices crash between store
//! transactions or in the middle of an effect (docs/design/sync-engine.md section 7.3).
//! `koidrive_sim::world` describes the harness and its two kinds of failure.
//!
//! `devices_lose_no_write` fails when a write is lost or when the engine, the store or the server
//! refuses a state. `devices_converge_once_the_faults_stop` also asks that every disk reaches the
//! server's tree and that no commit makes a change the user of its device never made.

use std::collections::{BTreeMap, BTreeSet};

use koidrive_proto::v1::error;
use koidrive_sim::world::disk::Place;
use koidrive_sim::world::{Action, Cut, Failure, NAMES, Outcome, Reply, Step, TEXTS, World};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;

fn action() -> impl Strategy<Value = Action> {
    (
        prop_oneof![1 => Just(0u8), 3 => Just(1u8), 2 => Just(2u8), 2 => Just(3u8), 1 => Just(4u8)],
        any::<usize>(),
        any::<usize>(),
        0..NAMES.len(),
        0..TEXTS,
    )
        .prop_map(|(kind, target, parent, name, text)| Action {
            kind,
            target,
            parent,
            name,
            text,
        })
}

#[derive(Clone, Copy)]
enum Losses {
    None,
    Restores,
    Rewinds,
}

fn step(losses: Losses) -> BoxedStrategy<Step> {
    let cut = prop_oneof![Just(Cut::Quarantined), Just(Cut::Placed), Just(Cut::Done)];
    let outcome = prop_oneof![
        5 => Just(Outcome::Run),
        1 => Just(Outcome::Fail),
        4 => cut.prop_map(Outcome::Crash),
    ];
    // The engine builds no commit that requires its base, so the server never gives it
    // `HEAD_MOVED`.
    let refused = prop_oneof![
        Just(error::Code::RetryLater),
        Just(error::Code::LimitExceeded),
    ];
    let reply = prop_oneof![
        8 => Just(Reply::Served),
        1 => refused.prop_map(Reply::Refused),
        1 => Just(Reply::NoChunks),
    ];
    let steps = prop_oneof![
        8 => action().prop_map(Step::User),
        12 => any::<usize>().prop_map(Step::Work),
        2 => Just(Step::Scan),
        4 => outcome.prop_map(Step::Effect),
        4 => reply.prop_map(Step::Commit),
        2 => Just(Step::Connect),
        3 => any::<usize>().prop_map(Step::Deliver),
        1 => any::<usize>().prop_map(Step::Duplicate),
        1 => Just(Step::Drop),
        1 => Just(Step::Crash),
    ]
    .boxed();
    match losses {
        Losses::None => steps,
        Losses::Restores => prop_oneof![
            19 => steps,
            1 => prop_oneof![Just(Step::Backup), Just(Step::Restore)],
        ]
        .boxed(),
        Losses::Rewinds => prop_oneof![
            60 => steps,
            2 => Just(Step::Backup),
            1 => Just(Step::Rewind),
            3 => Just(Step::CopyStore),
            2 => Just(Step::RollBack),
        ]
        .boxed(),
    }
}

fn phases(losses: Losses) -> impl Strategy<Value = Vec<Vec<(usize, Step)>>> {
    prop::collection::vec(
        prop::collection::vec((0..3usize, step(losses)), 0..48),
        1..=3,
    )
}

/// Devices that print every step with the state it leaves when `KOIDRIVE_SIM_TRACE` is set.
fn world(devices: usize) -> World {
    let mut world = World::new(devices);
    world.trace = std::env::var_os("KOIDRIVE_SIM_TRACE").is_some();
    world
}

/// Runs every phase and stops at the first failure that `stops` names. A phase that fails in
/// another way leaves the devices as they are for the next one.
fn run(
    devices: usize,
    phases: &[Vec<(usize, Step)>],
    stops: fn(&Failure) -> bool,
) -> Result<World, Failure> {
    let mut world = world(devices);
    for (phase, schedule) in phases.iter().enumerate() {
        match world.phase(phase, schedule) {
            Err(failure) if stops(&failure) => return Err(failure),
            _ => {}
        }
    }
    Ok(world)
}

/// Stops at the first failure of any kind.
fn converge(devices: usize, phases: &[Vec<(usize, Step)>]) -> Result<World, Failure> {
    run(devices, phases, |_| true)
}

/// Stops only when a write is lost or a state is refused.
fn keep(devices: usize, phases: &[Vec<(usize, Step)>]) -> Result<World, Failure> {
    run(devices, phases, |failure| {
        matches!(failure, Failure::Broken(_))
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|cases| cases.parse().ok()).unwrap_or(256),
    ))]

    #[test]
    fn devices_lose_no_write(devices in 2..=3usize, phases in phases(Losses::None)) {
        keep(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    fn devices_converge_once_the_faults_stop(devices in 2..=3usize, phases in phases(Losses::None)) {
        converge(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    fn devices_lose_no_write_across_server_restores(devices in 2..=3usize, phases in phases(Losses::Restores)) {
        keep(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    #[ignore = "after a server loss, a device can move a node back to a position it never set (GUN-531)"]
    fn devices_converge_across_server_restores(devices in 2..=3usize, phases in phases(Losses::Restores)) {
        converge(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    fn devices_lose_no_write_across_rewinds_and_rollbacks(devices in 2..=3usize, phases in phases(Losses::Rewinds)) {
        keep(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    fn devices_converge_across_rewinds_and_rollbacks(devices in 2..=3usize, phases in phases(Losses::Rewinds)) {
        converge(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }
}

/// A fixed sample of the schedules reaches every fault and every way back from one that the
/// harness counts, so a change to the strategies cannot drop one without this test failing.
#[test]
fn the_schedules_reach_every_fault() {
    reaches(Losses::None, &EVENTS);
}

#[test]
fn the_rewind_schedules_reach_every_recovery() {
    reaches(
        Losses::Rewinds,
        &[
            "server rewound",
            "rewind takes a write a disk holds",
            "rewind takes a write only one disk holds",
            "store rolled back",
            "recovery starts after a rewind",
            "recovery starts after a store rollback",
            "crash during a recovery",
            "store rolled back during a recovery",
        ],
    );
}

fn reaches(losses: Losses, events: &[&str]) {
    let mut runner = TestRunner::deterministic();
    let strategy = (2..=3usize, phases(losses));
    let mut seen: BTreeMap<&str, u64> = BTreeMap::new();
    for _ in 0..256 {
        let (devices, phases) = strategy.new_tree(&mut runner).unwrap().current();
        let world = keep(devices, &phases).unwrap();
        for (what, count) in world.seen {
            *seen.entry(what).or_default() += count;
        }
    }
    for (what, count) in &seen {
        println!("{count:>8} {what}");
    }
    for what in events {
        assert!(
            seen.contains_key(what),
            "no schedule of the sample reached: {what}"
        );
    }
}

/// The events of `World::seen` that the sample must reach.
const EVENTS: [&str; 28] = [
    "effect failed before the disk",
    "effect refused by the disk",
    "effect parked a node",
    "crash before an effect reached the disk",
    "crash with a download staged",
    "crash with a displaced entry in quarantine",
    "crash after the disk refused an effect",
    "crash after an effect reached the disk",
    "restart finds the effect in flight done",
    "restart finds the effect in flight not done",
    "restart ends an effect that left an entry in quarantine",
    "restart with a commit in flight",
    "reconnect resends a commit the server applied",
    "reconnect resends a commit the server never got",
    "own commit replayed before its result",
    "commit answered from the stored result",
    "result arrives after its records",
    "result arrives before earlier commits",
    "records arrive past a gap",
    "records arrive twice",
    "answer arrives twice",
    "commit refused, sent again",
    "commit of one op refused for its size, its node is a problem",
    "no commit built while a node is a problem",
    "rescan lets a problem node be planned again",
    "commit abandoned, its bytes are gone",
    "op renamed by the server",
    "op merged a folder",
];

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

/// Creates a folder in the root.
fn folder(name: &str) -> Step {
    user(0, 0, 0, name)
}

/// Creates a file in the root.
fn file(name: &str) -> Step {
    user(1, 0, 0, name)
}

/// Overwrites the `target`th entry of the disk, in the order the entries were made.
fn overwrite(target: usize) -> Step {
    user(2, target, 0, "a")
}

/// Moves the `target`th entry into the root, or into the `parent`th folder counted from 1.
fn rename(target: usize, parent: usize, name: &str) -> Step {
    user(3, target, parent, name)
}

fn delete(target: usize) -> Step {
    user(4, target, 0, "a")
}

const SCAN: Step = Step::Scan;
const EFFECT: Step = Step::Effect(Outcome::Run);
/// The effect in flight reaches the disk and the device crashes before the store hears of it.
const CRASH: Step = Step::Effect(Outcome::Crash(Cut::Done));
const COMMIT: Step = Step::Commit(Reply::Served);
const DELIVER: Step = Step::Deliver(0);

/// One device's steps from a rescan to the answer of its commit.
fn commit(device: usize) -> Vec<(usize, Step)> {
    vec![
        (device, SCAN),
        (device, COMMIT),
        (device, COMMIT),
        (device, DELIVER),
    ]
}

#[test]
fn two_devices_take_each_others_files() {
    let world = converge(2, &[vec![(0, file("a")), (1, file("b"))]]).unwrap();
    assert_eq!(world.devices[0].disk.entries().len(), 2);
    assert_eq!(world.devices[1].disk.entries().len(), 2);
}

/// The bytes of the two files are the same, and both files stay (the planted digest
/// deduplication of scripts/engine-mutations.sh removes one).
#[test]
fn two_new_files_with_the_same_bytes_are_both_kept() {
    let world = converge(2, &[vec![(0, file("a")), (0, file("b"))]]).unwrap();
    assert_eq!(world.devices[0].disk.entries().len(), 2);
    assert_eq!(world.devices[1].disk.entries().len(), 2);
}

#[test]
fn two_overwrites_of_one_file_end_as_two_files() {
    let synced = vec![(0, file("a"))];
    let racing = vec![(0, overwrite(0)), (1, overwrite(0))];
    let world = converge(2, &[synced, racing]).unwrap();
    assert_eq!(world.devices[0].disk.entries().len(), 2);
    assert_eq!(world.devices[1].disk.entries().len(), 2);
}

/// Device 1 overwrites a file, and device 0 then writes the first bytes back. Device 2 deletes
/// the file while its disk still has the first write. The engine tells contents apart by their
/// bytes, so the delete applies to the last write.
#[test]
fn a_delete_of_a_file_that_got_its_bytes_back_is_applied() {
    let synced = vec![(0, file("a"))];
    let mut racing = vec![(1, overwrite(0))];
    racing.extend(commit(1));
    racing.extend([(0, DELIVER), (0, EFFECT), (0, EFFECT), (0, overwrite(0))]);
    racing.extend(commit(0));
    racing.push((2, delete(0)));
    let world = converge(3, &[synced, racing]).unwrap();
    assert!(world.devices[0].disk.entries().is_empty());
}

/// The answer to a commit is lost, and the device reads its own commit in the backlog before the
/// stored result arrives.
#[test]
fn a_commit_replayed_before_its_result_is_applied_once() {
    let schedule = vec![
        (0, file("a")),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, Step::Drop),
        (0, Step::Connect),
        (0, DELIVER),
        (0, EFFECT),
    ];
    let world = converge(2, &[schedule]).unwrap();
    assert_eq!(world.seen["own commit replayed before its result"], 1);
    assert_eq!(world.seen["commit answered from the stored result"], 1);
    assert_eq!(world.devices[0].disk.entries().len(), 1);
}

/// Both devices create the folder "a" with a file in it. Device 1's folder is merged into device
/// 0's, the answer is lost, and device 1 reads its own commit in the backlog first.
#[test]
fn a_merged_folder_replayed_before_its_result_is_kept_once() {
    let mut schedule = vec![(0, folder("a")), (0, user(1, 0, 1, "b"))];
    schedule.extend(commit(0));
    schedule.extend([
        (1, folder("a")),
        (1, user(1, 0, 1, "c.txt")),
        (1, SCAN),
        (1, COMMIT),
        (1, COMMIT),
        (1, Step::Drop),
        (1, Step::Connect),
        (1, DELIVER),
        (1, EFFECT),
        (1, EFFECT),
    ]);
    let world = converge(2, &[schedule]).unwrap();
    assert_eq!(world.seen["op merged a folder"], 1);
    assert_eq!(world.seen["own commit replayed before its result"], 1);
    assert_eq!(world.devices[1].disk.entries().len(), 3);
}

/// GUN-378. Device 0 renames a to b, and the answer to its commit is lost. Device 1 then renames
/// the file to c.txt, and so does the user of device 0, who reads both commits in the backlog
/// before the stored result arrives and then renames the file back to b. Device 1 commits a
/// rename to ss after device 0 acknowledged its commit. Device 0's rename back to b is then
/// committed after ss, so it wins on every device instead of being undone.
#[test]
fn a_rename_made_while_a_replayed_commit_waits_for_its_result_is_kept() {
    let mut synced = vec![(0, file("a"))];
    synced.extend(commit(0));
    let mut racing = vec![
        (0, rename(0, 0, "b")),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, Step::Drop),
        (1, rename(0, 0, "c.txt")),
    ];
    racing.extend(commit(1));
    racing.extend([
        (1, DELIVER),
        (0, rename(0, 0, "c.txt")),
        (0, Step::Connect),
        (0, DELIVER),
        (0, SCAN),
        (0, rename(0, 0, "b")),
        (0, SCAN),
        (0, COMMIT),
        (0, DELIVER),
        (1, rename(0, 0, "ss")),
    ]);
    racing.extend(commit(1));
    let world = converge(2, &[synced, racing]).unwrap();
    assert_eq!(world.seen["own commit replayed before its result"], 1);
    assert_eq!(world.seen["result arrives after its records"], 1);
    for device in &world.devices {
        let names: Vec<&str> = (device.disk.entries().values())
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["b"]);
    }
}

/// A download reaches the disk and the device crashes before the store hears of it.
#[test]
fn a_download_cut_by_a_crash_is_not_done_twice() {
    let mut schedule = vec![(0, file("a"))];
    schedule.extend(commit(0));
    schedule.extend([(1, DELIVER), (1, EFFECT), (1, CRASH)]);
    let world = converge(2, &[schedule]).unwrap();
    assert_eq!(world.seen["restart finds the effect in flight done"], 1);
    assert_eq!(world.devices[1].disk.entries().len(), 1);
}

/// As above, and the user renames the downloaded file while the device is down. The restart finds
/// no file with the download's bytes at its name and reports the download as not done (section
/// 5.3). The engine takes the renamed file for a new one and downloads the file again.
#[test]
fn a_file_renamed_while_the_device_was_down_after_its_download_is_downloaded_again() {
    let mut schedule = vec![(0, file("a"))];
    schedule.extend(commit(0));
    schedule.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, CRASH),
        (1, rename(0, 0, "b")),
    ]);
    let world = converge(2, &[schedule]).unwrap();
    assert_eq!(world.seen["restart finds the effect in flight not done"], 1);
    let names = |d: usize| -> Vec<&str> {
        let entries = world.devices[d].disk.entries().values();
        entries.map(|entry| entry.name.as_str()).collect()
    };
    assert_eq!(names(0), ["a", "b"]);
    assert_eq!(names(1), ["b", "a"]);
}

/// As above, with an overwrite instead of a rename. The engine takes the overwritten file for a new
/// one, so the user's bytes are uploaded beside the file that the download brings again.
#[test]
fn a_file_overwritten_while_the_device_was_down_after_its_download_is_kept_beside_it() {
    let synced = vec![(0, file("a"))];
    let mut racing = vec![(0, overwrite(0))];
    racing.extend(commit(0));
    racing.extend([(1, DELIVER), (1, EFFECT), (1, CRASH), (1, overwrite(0))]);
    let world = converge(2, &[synced, racing]).unwrap();
    assert_eq!(world.seen["restart finds the effect in flight not done"], 1);
    assert_eq!(world.devices[0].disk.entries().len(), 2);
    assert_eq!(world.devices[1].disk.entries().len(), 2);
}

/// A download that replaces a file stops when the old file is in quarantine and the new one has
/// no name yet. The restart gives the download its name and sends the old file to the local
/// trash.
#[test]
fn a_replacement_cut_between_quarantine_and_placement_is_ended_by_the_restart() {
    let synced = vec![(0, file("a"))];
    let mut racing = vec![(0, overwrite(0))];
    racing.extend(commit(0));
    let cut = Step::Effect(Outcome::Crash(Cut::Quarantined));
    racing.extend([(1, DELIVER), (1, EFFECT), (1, cut)]);
    let world = converge(2, &[synced, racing]).unwrap();
    assert_eq!(world.seen["crash with a displaced entry in quarantine"], 1);
    assert_eq!(
        world.seen["restart ends an effect that left an entry in quarantine"],
        1
    );
    assert_eq!(world.devices[1].disk.entries().len(), 1);
}

/// As above, and the user makes a file at the free name while the device is down. The download
/// finds its name taken, and both the user's file and the download are kept.
#[test]
fn a_file_made_at_the_name_of_a_quarantined_file_is_kept() {
    let synced = vec![(0, file("a"))];
    let mut racing = vec![(0, overwrite(0))];
    racing.extend(commit(0));
    let cut = Step::Effect(Outcome::Crash(Cut::Quarantined));
    racing.extend([(1, DELIVER), (1, EFFECT), (1, cut), (1, file("a"))]);
    let world = converge(2, &[synced, racing]).unwrap();
    assert_eq!(
        world.seen["restart finds the effect in flight refused by the disk"],
        1
    );
    assert_eq!(world.devices[0].disk.entries().len(), 2);
    assert_eq!(world.devices[1].disk.entries().len(), 2);
}

/// The server refuses a commit of one op with `LIMIT_EXCEEDED`. The engine marks the node as a
/// problem and builds no commit for it until the next rescan.
#[test]
fn a_commit_of_one_op_refused_for_its_size_waits_for_a_rescan() {
    let schedule = vec![
        (0, file("a")),
        (0, SCAN),
        (0, Step::Commit(Reply::Refused(error::Code::LimitExceeded))),
        (0, DELIVER),
        (0, COMMIT),
        (0, COMMIT),
    ];
    let world = converge(2, &[schedule]).unwrap();
    assert_eq!(
        world.seen["commit of one op refused for its size, its node is a problem"],
        1
    );
    assert_eq!(world.seen["no commit built while a node is a problem"], 2);
    assert_eq!(world.seen["rescan lets a problem node be planned again"], 1);
    assert_eq!(world.seen["commit applied"], 1);
    assert_eq!(world.devices[1].disk.entries().len(), 1);
}

/// A download fails before it touches the disk, and a commit is refused before the server applies
/// it. Both are done again.
#[test]
fn a_failed_download_and_a_refused_commit_are_done_again() {
    let schedule = vec![
        (0, file("a")),
        (0, SCAN),
        (0, Step::Commit(Reply::Refused(error::Code::RetryLater))),
        (0, DELIVER),
        (0, COMMIT),
        (0, DELIVER),
        (1, DELIVER),
        (1, Step::Effect(Outcome::Fail)),
    ];
    let world = converge(2, &[schedule]).unwrap();
    assert_eq!(world.seen["commit refused, sent again"], 1);
    assert_eq!(world.seen["effect failed before the disk"], 1);
}

/// Both devices overwrite one file. Device 1's commit makes a conflict copy and its answer is lost.
/// After reconnecting, device 1 replays its own commit from the log and downloads the copy before
/// the stored result arrives.
///
/// Found as GUN-326.
#[test]
fn a_conflict_copy_replayed_before_its_result_is_kept_once() {
    let synced = vec![(0, file("a"))];
    let mut racing = vec![(0, overwrite(0)), (1, overwrite(0))];
    racing.extend(commit(0));
    racing.extend([
        (1, SCAN),
        (1, COMMIT),
        (1, COMMIT),
        (1, Step::Drop),
        (1, Step::Connect),
        (1, DELIVER),
        (1, EFFECT),
        (1, EFFECT),
    ]);
    converge(2, &[synced, racing]).unwrap();
}

/// Device 1 starts to download a new file. Before the download ends, device 0 renames the file
/// and device 1 hears of it.
///
/// Found as GUN-327.
#[test]
fn a_rename_that_arrives_during_the_download_is_not_undone() {
    let mut schedule = vec![(0, file("a"))];
    schedule.extend(commit(0));
    schedule.extend([(1, DELIVER), (1, EFFECT), (0, rename(0, 0, "b"))]);
    schedule.extend(commit(0));
    schedule.extend([(1, DELIVER), (1, EFFECT)]);
    converge(2, &[schedule]).unwrap();
}

/// On a disk that tells "A" from "a", device 0 edits the file "A" and creates the folder "a" while
/// device 1 edits and renames the file. Device 0's edit becomes a conflict copy.
///
/// Found as GUN-328.
#[test]
fn a_conflict_copy_whose_old_name_a_new_folder_took_leaves_waiting() {
    let synced = vec![(0, file("A"))];
    let racing = vec![
        (0, overwrite(0)),
        (0, folder("a")),
        (1, overwrite(0)),
        (1, rename(0, 0, "b")),
        (0, SCAN),
        (1, SCAN),
        (1, COMMIT),
        (0, COMMIT),
        (1, COMMIT),
    ];
    converge(2, &[synced, racing]).unwrap();
}

/// Device 0 creates the file "a" where device 1 has created a folder, so the server renames the
/// file. The user of device 0 deletes the file before the disk has the server's name for it.
///
/// Found as GUN-332.
#[test]
fn a_file_deleted_before_the_disk_took_the_servers_name_stays_deleted() {
    let mut schedule = vec![(1, folder("a")), (0, file("a"))];
    schedule.extend(commit(1));
    schedule.extend(commit(0));
    schedule.extend([(0, DELIVER), (0, delete(0)), (0, SCAN), (0, COMMIT)]);
    converge(2, &[schedule]).unwrap();
}

/// Device 1 edits a file and deletes an empty folder. Meanwhile device 0 has moved the file into
/// that folder and edited it, so device 1's edit becomes a conflict copy inside the folder that the
/// same commit deletes.
#[test]
fn an_edit_that_becomes_a_conflict_copy_in_a_folder_its_commit_deletes_is_kept() {
    let synced = vec![(0, file("b")), (0, folder("a"))];
    let mut racing = vec![
        (1, overwrite(0)),
        (1, delete(1)),
        (0, rename(0, 1, "c.txt")),
        (0, overwrite(0)),
    ];
    racing.extend(commit(0));
    racing.extend([(1, SCAN), (1, COMMIT), (1, COMMIT)]);
    converge(2, &[synced, racing]).unwrap();
}

/// Device 1 deletes the empty folder "b". Meanwhile device 0 has moved the folder "a", which holds
/// a file, into "b".
#[test]
fn a_file_in_a_folder_moved_into_a_folder_that_another_device_deletes_is_kept() {
    let synced = vec![
        (0, folder("a")),
        (0, user(1, 0, 1, "c.txt")),
        (0, folder("b")),
    ];
    let mut racing = vec![(1, delete(1)), (0, rename(0, 2, "a"))];
    racing.extend(commit(0));
    racing.extend([(1, SCAN), (1, COMMIT), (1, COMMIT)]);
    converge(2, &[synced, racing]).unwrap();
}

/// Device 2 deletes a folder that holds a file, and device 1 renames the folder, which brings both
/// back on the server. Device 0 has taken the delete and is removing the file from its disk when
/// the rename arrives.
///
/// Found as GUN-334.
#[test]
fn a_file_that_an_effect_removes_after_the_server_restored_it_is_not_deleted() {
    let synced = vec![(0, folder("b")), (0, user(1, 0, 1, "a"))];
    let mut racing = vec![(2, delete(0)), (1, rename(0, 0, "c.txt"))];
    racing.extend(commit(2));
    racing.extend([(0, DELIVER), (0, EFFECT)]);
    racing.extend(commit(1));
    racing.extend([(0, DELIVER), (0, EFFECT)]);
    converge(3, &[synced, racing]).unwrap();
}

/// Device 1 plans the download of the file "a". Its user then creates the folder "a", and moves it
/// away after the engine has seen it there. The download comes up before the next scan, while L
/// still holds the folder at that name.
///
/// Found as GUN-330.
#[test]
fn a_download_onto_a_name_that_a_moved_folder_held_leaves_a_loadable_store() {
    let mut schedule = vec![(0, file("a"))];
    schedule.extend(commit(0));
    schedule.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, folder("a")),
        (1, SCAN),
        (1, rename(0, 0, "b")),
        (1, EFFECT),
    ]);
    converge(2, &[schedule]).unwrap();
}

/// Another device deletes the folder "a". While its removal is in flight, the user moves a file
/// into the folder, a rescan sees it there, and the user moves it out again. The removal comes up
/// before the next scan, while L still holds the file in the folder.
///
/// Found as GUN-330.
#[test]
fn a_removal_of_a_folder_that_a_file_left_unseen_leaves_a_store_that_loads() {
    let mut schedule = vec![(0, folder("a")), (0, file("b"))];
    schedule.extend(commit(0));
    schedule.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, EFFECT),
        (1, EFFECT),
        (1, EFFECT),
    ]);
    schedule.push((0, delete(0)));
    schedule.extend(commit(0));
    schedule.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, rename(1, 1, "b")),
        (1, SCAN),
        (1, rename(1, 0, "b")),
        (1, EFFECT),
    ]);
    converge(2, &[schedule]).unwrap();
}

/// Another device deletes the folder "a". While its removal is in flight, the user renames the
/// folder and makes a folder in it, a rescan sees both, and the device stops. The restart finds
/// nothing at the name "a", which recovery reports as the removal done (section 5.3), while L
/// holds the folder under its new name.
///
/// Found as GUN-339.
#[test]
fn a_restart_during_the_removal_of_a_renamed_folder_leaves_a_store_that_loads() {
    let mut schedule = vec![(0, folder("a"))];
    schedule.extend(commit(0));
    schedule.extend([(1, DELIVER), (1, EFFECT), (1, EFFECT), (0, delete(0))]);
    schedule.extend(commit(0));
    schedule.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, rename(0, 0, "b")),
        (1, user(0, 0, 1, "a")),
        (1, SCAN),
        (1, Step::Crash),
    ]);
    converge(2, &[schedule]).unwrap();
}

/// Device 1 plans the download of the file "a" and stops. Its user renames the file "b", which
/// has the same bytes, to "a". The restart finds those bytes at the name "a", which recovery
/// reports as the download done (section 5.3), in the entry that L holds as "b".
///
/// Found as GUN-342.
#[test]
fn a_file_renamed_onto_the_name_of_a_download_in_flight_is_kept() {
    let mut schedule = vec![(0, file("b"))];
    schedule.extend(commit(0));
    schedule.extend([(1, DELIVER), (1, EFFECT), (1, EFFECT), (0, file("a"))]);
    schedule.extend(commit(0));
    schedule.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, Step::Crash),
        (1, rename(0, 0, "a")),
    ]);
    let world = converge(2, &[schedule]).unwrap();
    assert_eq!(world.devices[0].disk.entries().len(), 2);
}

/// Device 0 swaps two files through a third name. Its commit moves one of them through a pending
/// name, which the user never gave, and both devices end with the two files at the swapped names.
#[test]
fn two_files_swapped_through_a_third_name_reach_the_other_device() {
    let synced = vec![(0, file("a")), (0, file("b"))];
    let swap = vec![
        (0, rename(0, 0, "c.txt")),
        (0, rename(1, 0, "a")),
        (0, rename(0, 0, "b")),
    ];
    let world = converge(2, &[synced, swap]).unwrap();
    assert_eq!(world.devices[1].disk.entries().len(), 2);
}

/// Device 2 sends its rename of a file to b, and while the commit is in flight it receives device
/// 0's rename to b and then device 1's rename to c.txt. The server applies device 2's commit after
/// device 1's, so the file ends as b. The driver plans no effect while a commit is in flight, so
/// device 2 never moves its file to c.txt and then commits that name as its own. The minimal input
/// proptest found for `devices_converge_once_the_faults_stop` (GUN-383).
#[test]
fn a_device_with_a_commit_in_flight_does_not_commit_a_remote_name_back() {
    let phases = [
        vec![(0, file("a")), (0, rename(0, 0, "ß"))],
        vec![
            (2, rename(0, 0, "b")),
            (0, rename(0, 0, "b")),
            (0, Step::Work(0)),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (0, Step::Work(913562)),
            (1, COMMIT),
            (1, Step::Work(6613876784364572377)),
            (
                1,
                rename(7269053276869567780, 12266595847907186486, "c.txt"),
            ),
            (1, Step::Work(2973521201126422277)),
            (1, Step::Work(10427516227233112141)),
        ],
    ];
    converge(3, &phases).unwrap();
}

/// Device 2 makes folder A next to the synced folder a, commits it, and moves A into a while
/// device 0 deletes a. A's acknowledged S update waits, since a still holds the name key in S on
/// device 2, and a cannot leave the disk while it holds A. Device 2 sends the move against the
/// waiting update, so the server brings a back with A in it. The minimal input proptest found for
/// `devices_converge_once_the_faults_stop` (GUN-399).
#[test]
fn a_folder_moved_into_a_deleted_folder_while_its_create_waits_is_sent() {
    let phases = [
        vec![(0, folder("a"))],
        vec![
            (2, folder("A")),
            (2, Step::Work(0)),
            (2, COMMIT),
            (2, rename(1, 1, "a")),
            (0, delete(0)),
        ],
    ];
    let world = converge(3, &phases).unwrap();
    let entries = world.devices[0].disk.entries();
    assert_eq!(entries.len(), 2);
    let moved = (entries.values())
        .find(|entry| matches!(entry.place, Place::In(_)))
        .expect("A is inside a");
    let Place::In(folder) = moved.place else {
        unreachable!()
    };
    assert_eq!(moved.name, "a");
    assert_eq!(entries[&folder].place, Place::Root);
}

#[test]
fn a_folder_revived_at_its_pending_name_after_its_delete_landed_takes_its_name() {
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![
            (2, Step::Work(4007745936436893426)),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 13283880982325300966,
                    parent: 7530012053189467691,
                    name: 6,
                    text: 3,
                }),
            ),
            (2, Step::Work(18389991862780469496)),
            (2, Step::Work(9775417661590067865)),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 8851446434072450680,
                    parent: 2745539858988442782,
                    name: 0,
                    text: 2,
                }),
            ),
            (0, Step::Work(4450296097999021389)),
            (2, Step::Deliver(15501889018198306176)),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 4254539288740600970,
                    parent: 17259486623974888835,
                    name: 0,
                    text: 3,
                }),
            ),
            (0, Step::Deliver(7951671155622776781)),
            (0, Step::Duplicate(16868798682183253529)),
            (0, Step::Commit(Reply::Served)),
        ],
        vec![
            (1, Step::Commit(Reply::Served)),
            (
                2,
                Step::User(Action {
                    kind: 0,
                    target: 4696540523973067749,
                    parent: 17289433333287293583,
                    name: 1,
                    text: 0,
                }),
            ),
            (0, Step::Connect),
            (0, Step::Scan),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 17156652917260369542,
                    parent: 15791111473684421551,
                    name: 3,
                    text: 2,
                }),
            ),
            (1, Step::Work(7157279081864785743)),
            (1, Step::Duplicate(18363909320702985799)),
            (2, Step::Deliver(3717140028059626527)),
            (1, Step::Commit(Reply::Served)),
            (2, Step::Deliver(12029961515716932305)),
            (2, Step::Commit(Reply::Refused(error::Code::LimitExceeded))),
            (
                2,
                Step::User(Action {
                    kind: 3,
                    target: 13882752570192843626,
                    parent: 10028917631472640197,
                    name: 3,
                    text: 1,
                }),
            ),
            (1, Step::Work(18216690719357743194)),
            (
                0,
                Step::User(Action {
                    kind: 2,
                    target: 16789675239341262655,
                    parent: 995864001040360455,
                    name: 0,
                    text: 2,
                }),
            ),
            (0, Step::Work(14807180377262276184)),
            (0, Step::Duplicate(7397412081686271012)),
            (0, Step::Work(8057686803297367230)),
            (1, Step::Work(4512714835434908715)),
            (
                0,
                Step::User(Action {
                    kind: 4,
                    target: 17627914801873212641,
                    parent: 12616595891657092197,
                    name: 6,
                    text: 0,
                }),
            ),
            (2, Step::Work(7337790291513551950)),
            (2, Step::Work(1180466901371692876)),
            (0, Step::Deliver(3821039509024783184)),
            (
                2,
                Step::User(Action {
                    kind: 1,
                    target: 13107520285440010231,
                    parent: 10598960963687546088,
                    name: 4,
                    text: 3,
                }),
            ),
            (
                1,
                Step::User(Action {
                    kind: 2,
                    target: 2246543535808375351,
                    parent: 6319698275849929893,
                    name: 0,
                    text: 1,
                }),
            ),
            (0, Step::Work(7175920170899054987)),
            (1, Step::Effect(Outcome::Run)),
            (1, Step::Work(4159517033770470710)),
            (0, Step::Work(18404622977775708442)),
            (0, Step::Commit(Reply::Served)),
            (
                0,
                Step::User(Action {
                    kind: 4,
                    target: 6365491237751273237,
                    parent: 10782634279233329875,
                    name: 1,
                    text: 3,
                }),
            ),
            (0, Step::Commit(Reply::Served)),
            (2, Step::Work(8995257431548856822)),
            (1, Step::Connect),
            (0, Step::Deliver(10788655906123123742)),
            (1, Step::Deliver(16420210780264312280)),
            (0, Step::Work(479295481448839719)),
            (0, Step::Work(2208229052780071360)),
            (0, Step::Duplicate(3038716511811611936)),
            (0, Step::Effect(Outcome::Fail)),
            (2, Step::Work(15188616158263645844)),
            (2, Step::Work(4858157282421835116)),
        ],
        vec![
            (0, Step::Crash),
            (0, Step::Drop),
            (
                0,
                Step::User(Action {
                    kind: 3,
                    target: 17411163511274481715,
                    parent: 1312539373553298619,
                    name: 1,
                    text: 1,
                }),
            ),
            (2, Step::Work(13584574805752267923)),
            (
                1,
                Step::User(Action {
                    kind: 1,
                    target: 14844142914246983467,
                    parent: 13391967695461610201,
                    name: 5,
                    text: 3,
                }),
            ),
            (2, Step::Commit(Reply::Served)),
            (0, Step::Effect(Outcome::Run)),
            (1, Step::Deliver(7904161053102581264)),
            (2, Step::Deliver(11456114341529141909)),
            (0, Step::Work(16877029101800043325)),
            (1, Step::Work(3138824959598783305)),
            (0, Step::Effect(Outcome::Run)),
            (1, Step::Work(9649128248737177936)),
        ],
    ];
    converge(2, &phases).unwrap();
}

const BACKUP: Step = Step::Backup;
const RESTORE: Step = Step::Restore;

/// Runs the phases, with a restore of the server to a backup taken at the start of the second one
/// as the third, and checks that the devices handled the loss as the design says.
fn restored(devices: usize, synced: Vec<(usize, Step)>, lost: Vec<(usize, Step)>) -> World {
    let lost = std::iter::once((0, BACKUP)).chain(lost).collect();
    let world = converge(devices, &[synced, lost, vec![(0, RESTORE)]]).unwrap();
    for what in [
        "server restored, commits lost",
        "reconnect finds the server lost commits",
        "resume finds the server behind",
        "listing replaces R",
    ] {
        assert!(
            world.seen.contains_key(what),
            "the run did not reach: {what}"
        );
    }
    world
}

/// The server loses the commit that created a file. Its author sends the create again.
#[test]
fn a_file_whose_create_the_server_lost_is_created_again() {
    let world = restored(2, Vec::new(), vec![(0, file("a"))]);
    assert_eq!(world.devices[1].disk.entries().len(), 1);
}

/// The server loses a delete. Its author sends the delete again, and the file comes back on no
/// device.
#[test]
fn a_file_whose_delete_the_server_lost_stays_deleted() {
    let world = restored(2, vec![(0, file("a"))], vec![(0, delete(0))]);
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().is_empty())
    );
}

/// The server loses a rename. Both devices hold the new name, so both send it again.
#[test]
fn a_file_whose_rename_the_server_lost_keeps_its_new_name() {
    let world = restored(2, vec![(0, file("a"))], vec![(0, rename(0, 0, "b"))]);
    for device in &world.devices {
        let names: Vec<&str> = (device.disk.entries().values())
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["b"]);
    }
}
/// The server loses the create of a file, and the devices create it again. A copy of its bytes
/// that the engine makes later, with no user asking for one, is still a divergence.
#[test]
fn a_later_copy_of_a_write_the_server_lost_is_still_refused() {
    let mut world = restored(2, Vec::new(), vec![(0, file("a"))]);
    let bytes = (world.devices[0]
        .disk
        .entries()
        .values()
        .next()
        .unwrap()
        .content)
        .clone();
    for name in ["b", "c", "d"] {
        world.devices[0]
            .disk
            .create(Place::Root, name, bytes.clone())
            .unwrap();
    }
    assert!(world.phase(3, &[]).is_err());
}

/// A restore that loses no rename allows no later rename to a name the user never gave.
#[test]
fn a_restore_that_loses_no_rename_allows_no_unasked_rename() {
    let mut world = converge(2, &[vec![(0, file("a")), (1, file("b"))]]).unwrap();
    world.phase(1, &[(0, BACKUP), (0, RESTORE)]).unwrap();
    let disk = &world.devices[1].disk;
    let b = (disk.visible().iter()).position(|ino| disk.entries()[ino].name == "b");
    world
        .phase(2, &[(1, rename(b.unwrap(), 0, "c.txt"))])
        .unwrap();
    let a = world.devices[0].disk.child(&Place::Root, "a").unwrap();
    world.devices[0].disk.rename(a, Place::Root, "b").unwrap();
    assert!(world.phase(3, &[]).is_err());
}
/// A restore that loses an edit lets a device send the position L holds for the file again, here
/// when it revives the file that another device's delete trashed after the restore.
#[test]
fn a_restore_that_loses_an_edit_lets_its_file_be_revived() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases = vec![
        vec![
            (0, u(1, 0, 0, 6, 0)),
            (0, Step::Work(0)),
            (2, u(0, 0, 0, 0, 0)),
        ],
        vec![
            (0, u(1, 0, 0, 2, 0)),
            (2, u(0, 0, 0, 2, 0)),
            (2, u(1, 0, 0, 3, 0)),
            (0, Step::Work(0)),
            (0, u(2, 925163541728872510, 0, 0, 0)),
            (0, u(0, 0, 11523797806819799120, 0, 0)),
        ],
        vec![
            (0, u(0, 0, 0, 0, 0)),
            (0, BACKUP),
            (1, u(2, 10499357163619158257, 0, 0, 0)),
            (0, u(0, 0, 20731787025803467, 0, 0)),
            (1, Step::Work(0)),
            (1, Step::Work(0)),
            (1, Step::Work(0)),
            (0, u(4, 4907598952368571970, 0, 0, 0)),
            (1, Step::Work(0)),
            (0, RESTORE),
        ],
    ];
    converge(3, &phases).unwrap();
}

/// Once the devices are quiet after a restore, a copy of a write the restore took from the server
/// is refused like any other copy.
#[test]
fn a_copy_of_a_lost_write_after_the_recovery_is_refused() {
    let mut world = restored(2, Vec::new(), vec![(0, file("a"))]);
    let bytes = (world.devices[0]
        .disk
        .entries()
        .values()
        .next()
        .unwrap()
        .content)
        .clone();
    world.devices[0]
        .disk
        .create(Place::Root, "b", bytes)
        .unwrap();
    assert!(world.phase(3, &[]).is_err());
}

/// A copy of a lost write that the engine makes while the devices still recover from the
/// restore, past the files the restore lets the write end in, is a divergence.
#[test]
fn a_copy_of_a_lost_write_during_the_recovery_is_refused() {
    let mut world = converge(2, &[Vec::new(), vec![(0, BACKUP), (0, file("a"))]]).unwrap();
    world.run(2, &[(0, RESTORE)]).unwrap();
    let bytes = (world.devices[0]
        .disk
        .entries()
        .values()
        .next()
        .unwrap()
        .content)
        .clone();
    world.devices[0]
        .disk
        .create(Place::Root, "b", bytes)
        .unwrap();
    let failure = world.phase(2, &[]).unwrap_err();
    assert!(
        failure.to_string().contains("is in more than one file"),
        "{failure}"
    );
}

/// Once the devices are quiet after a restore that rolled a file back, the write it took stays in
/// the files the server holds it in, not in one file for each device that held it.
#[test]
fn a_copy_of_a_rolled_back_write_after_the_recovery_is_refused() {
    let edit = Step::User(Action {
        kind: 2,
        target: 0,
        parent: 0,
        name: 0,
        text: 0,
    });
    let mut world = converge(2, &[vec![(0, file("a"))], vec![(0, BACKUP), (0, edit)]]).unwrap();
    world.phase(2, &[(0, RESTORE)]).unwrap();
    let lost = (world.devices[0].disk.entries().values()).find_map(|entry| {
        entry
            .content
            .clone()
            .filter(|content| content.mtime_ns == 2)
    });
    world.devices[0]
        .disk
        .create(Place::Root, "b", lost)
        .unwrap();
    let failure = world.phase(3, &[]).unwrap_err();
    assert!(
        failure.to_string().contains("is in more than one file"),
        "{failure}"
    );
}

#[test]
fn an_edit_back_to_the_bytes_a_restore_left_keeps_its_file_and_the_lost_edit_beside_it() {
    let world = converge(
        2,
        &[
            vec![(0, file("a"))],
            vec![(0, BACKUP), (1, overwrite(0))],
            vec![(0, RESTORE), (0, overwrite(0))],
        ],
    )
    .unwrap();
    for device in &world.devices {
        let disk = &device.disk;
        let write = |ino: u128| disk.entries()[&ino].content.as_ref().map(|c| c.mtime_ns);
        let a = disk.child(&Place::Root, "a").unwrap();
        assert_eq!(write(a), Some(3));
        let writes: BTreeSet<Option<i64>> = disk.entries().keys().map(|ino| write(*ino)).collect();
        assert_eq!(writes, BTreeSet::from([Some(2), Some(3)]));
    }
}

#[test]
fn a_write_two_downloads_place_after_a_restore_took_it_is_kept_by_both_beside_the_edit_over_it() {
    let mut lost = vec![(0, file("a"))];
    lost.extend(commit(0));
    lost.extend([
        (1, DELIVER),
        (1, EFFECT),
        (2, DELIVER),
        (2, EFFECT),
        (0, overwrite(0)),
        (0, RESTORE),
        (1, EFFECT),
        (2, EFFECT),
    ]);
    let world = converge(3, &[vec![(0, BACKUP)], lost]).unwrap();
    for device in &world.devices {
        let disk = &device.disk;
        let write = |ino: &u128| disk.entries()[ino].content.as_ref().map(|c| c.mtime_ns);
        let a = disk.child(&Place::Root, "a").unwrap();
        assert_eq!(write(&a), Some(2));
        let mut writes: Vec<Option<i64>> = disk.entries().keys().map(write).collect();
        writes.sort();
        assert_eq!(writes, [Some(1), Some(1), Some(2)]);
    }
}

fn created_again_by_both_and_deleted_by_one(made: Step, edit: bool) -> World {
    let mut lost = vec![
        (0, RESTORE),
        (0, Step::Connect),
        (1, Step::Connect),
        (0, DELIVER),
        (1, DELIVER),
    ];
    lost.extend(edit.then_some((1, overwrite(0))));
    lost.extend([
        (1, SCAN),
        (1, COMMIT),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (0, delete(0)),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (1, COMMIT),
        (1, DELIVER),
        (1, DELIVER),
        (1, DELIVER),
    ]);
    converge(2, &[Vec::new(), vec![(0, BACKUP), (0, made)], lost]).unwrap()
}

#[test]
fn a_lost_folder_another_device_creates_again_and_deletes_first_is_deleted_on_both() {
    let world = created_again_by_both_and_deleted_by_one(folder("a"), false);
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().is_empty())
    );
}

#[test]
fn a_lost_file_another_device_creates_again_and_deletes_first_is_deleted_on_both() {
    let world = created_again_by_both_and_deleted_by_one(file("a"), false);
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().is_empty())
    );
}

#[test]
fn an_edit_of_a_lost_file_another_device_creates_again_and_deletes_first_revives_it() {
    let world = created_again_by_both_and_deleted_by_one(file("a"), true);
    for device in &world.devices {
        let disk = &device.disk;
        let a = disk.child(&Place::Root, "a").unwrap();
        assert_eq!(disk.entries().len(), 1);
        assert_eq!(
            disk.entries()[&a].content.as_ref().map(|c| c.mtime_ns),
            Some(2)
        );
    }
}

#[test]
fn a_swap_after_a_restore_passes_through_the_pending_name_of_a_name_a_lost_commit_gave() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases = vec![
        vec![
            (1, u(0, 0, 0, 2, 0)),
            (1, u(1, 0, 6892790594048859264, 0, 0)),
        ],
        vec![
            (0, u(2, 0, 0, 0, 0)),
            (0, BACKUP),
            (0, u(1, 0, 1923514285381434823, 0, 0)),
            (2, u(1, 0, 0, 1, 0)),
            (0, u(1, 0, 13397848212569861839, 2, 0)),
            (0, u(1, 0, 0, 3, 0)),
        ],
        vec![
            (2, u(0, 0, 3443233485426864259, 1, 0)),
            (0, Step::Work(0)),
            (0, Step::Work(0)),
            (0, Step::Work(0)),
            (1, DELIVER),
            (0, RESTORE),
            (1, Step::Effect(Outcome::Crash(Cut::Placed))),
        ],
    ];
    converge(2, &phases).unwrap();
}

/// Once the devices are quiet after a restore that lost a rename, the old name is refused like
/// any other name the user did not give.
#[test]
fn a_name_a_restore_lost_is_refused_after_the_recovery() {
    let mut world = converge(2, &[vec![(0, file("a"))], vec![(0, BACKUP)]]).unwrap();
    world.phase(2, &[(1, rename(0, 0, "b"))]).unwrap();
    world.phase(3, &[(0, RESTORE)]).unwrap();
    world.phase(4, &[(1, rename(0, 0, "c.txt"))]).unwrap();
    let ino = world.devices[0].disk.child(&Place::Root, "c.txt").unwrap();
    world.devices[0].disk.rename(ino, Place::Root, "b").unwrap();
    assert!(world.phase(5, &[]).is_err());
}

/// A file whose create the server renamed is reverted until the effects move it to the server's
/// name, and the server loses the create before they do. Device 1 already took the file, so both
/// devices create it again. Device 0 keeps the node's ID instead of giving the file a new one, so
/// the server ends with the file once.
#[test]
fn a_lost_file_the_server_renamed_is_created_again_once() {
    let mut lost = vec![(0, BACKUP), (0, file("C.TXT"))];
    lost.extend(commit(0));
    lost.extend([
        (1, DELIVER),
        (1, DELIVER),
        (1, EFFECT),
        (1, EFFECT),
        (0, RESTORE),
    ]);
    converge(2, &[vec![(0, file("c.txt"))], lost]).unwrap();
}

/// Both devices create a file the server lost again under its ID, and device 0's create comes
/// first. The server refuses device 1's create with ID_EXISTS, and device 1 takes the file from R
/// instead of leaving it a problem.
#[test]
fn a_lost_file_both_devices_create_again_is_kept_once() {
    let lost = vec![(0, BACKUP), (0, file("a"))];
    let again = vec![
        (0, RESTORE),
        (1, Step::Connect),
        (1, DELIVER),
        (1, COMMIT),
        (0, Step::Connect),
        (0, DELIVER),
        (0, COMMIT),
        (0, COMMIT),
        (1, COMMIT),
    ];
    let world = converge(2, &[lost, again]).unwrap();
    assert_eq!(world.seen["op rejected"], 1);
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().len() == 1)
    );
}

/// Devices 1 and 0 rename a file that device 2 deletes, and device 1 crashes once it recorded the
/// removal and before the removal reached the file. Recovery finds the old name empty and nothing
/// in the local trash, so the removal failed, and device 1 moves the node instead of creating the
/// file again beside the one device 0 revived (GUN-443).
#[test]
fn a_removal_recovered_after_the_user_moved_its_file_keeps_one_file() {
    let u = |kind, name| {
        Step::User(Action {
            kind,
            target: 0,
            parent: 0,
            name,
            text: 0,
        })
    };
    let phases = vec![
        vec![(0, u(1, 0))],
        vec![
            (2, u(4, 0)),
            (1, u(3, 1)),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (1, DELIVER),
            (0, u(3, 1)),
            (1, Step::Effect(Outcome::Crash(Cut::Quarantined))),
        ],
    ];
    converge(3, &phases).unwrap();
}

/// GUN-444: device 1's create is applied at seq 3 and its result arrives, but the server is
/// restored to seq 2 before device 1's stream reaches seq 3. The server lost a commit the device
/// saw accepted, so the device lists R again instead of starting recovery.
#[test]
fn a_device_holding_the_result_of_a_commit_the_server_lost_lists_r_again() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases = vec![
        vec![(0, u(1, 0, 0, 0, 0))],
        vec![
            (0, u(2, 0, 0, 0, 0)),
            (1, u(0, 0, 0, 1, 0)),
            (0, Step::Work(0)),
            (1, Step::Work(0)),
            (1, Step::Work(0)),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (0, BACKUP),
            (1, Step::Work(3407374106458668457)),
            (1, Step::Deliver(4808377001051081459)),
            (0, RESTORE),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn an_edit_made_after_its_commit_was_built_follows_the_conflict_copy_to_its_name() {
    let synced = vec![(0, file("c.txt"))];
    let mut racing = vec![(0, rename(0, 0, "b")), (0, overwrite(0))];
    racing.extend(commit(0));
    racing.extend([
        (1, overwrite(0)),
        (1, file("C.TXT")),
        (1, SCAN),
        (1, COMMIT),
        (1, overwrite(0)),
        (1, COMMIT),
        (1, DELIVER),
    ]);
    let world = converge(2, &[synced, racing]).unwrap();
    for device in &world.devices {
        assert_eq!(device.disk.entries().len(), 3);
    }
}

#[test]
fn a_conflict_copy_whose_name_a_file_of_the_same_commit_took_reaches_its_name() {
    let u = |kind, target, parent, name, text| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases = [
        vec![(0, u(1, 0, 0, 3, 0)), (0, Step::Work(0))],
        vec![
            (1, u(2, 0, 0, 0, 0)),
            (1, Step::Work(0)),
            (2, u(0, 0, 0, 0, 0)),
            (1, u(1, 0, 0, 0, 3)),
            (0, u(0, 8058298412, 13820684896388115470, 3, 2)),
            (1, u(2, 5595505104648703704, 14520073170452222456, 0, 3)),
            (2, Step::Work(1546513718156240915)),
            (2, COMMIT),
            (1, Step::Work(4166004558488083538)),
            (0, u(2, 6580354071670050619, 8205634420763423225, 3, 3)),
            (1, u(3, 2072403800777770851, 14264798506198798549, 4, 0)),
            (0, u(3, 3355719809822832468, 4572565801030521563, 0, 1)),
            (1, u(1, 11181736274241258047, 5920353529640080243, 0, 0)),
            (1, Step::Work(1407293088156174997)),
            (1, u(1, 9500849518790139864, 10437075667485134398, 5, 2)),
            (1, Step::Work(4005447178108011140)),
            (1, u(2, 7452752783842175740, 3965247538109782643, 4, 3)),
            (1, COMMIT),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn an_edited_file_parked_while_a_new_folder_takes_its_name_reaches_the_server() {
    converge(
        2,
        &[
            vec![(0, file("a")), (0, rename(0, 0, "ss"))],
            vec![
                (0, rename(0, 0, "ß")),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (0, Step::Work(0)),
                (1, Step::Work(0)),
                (1, EFFECT),
                (1, folder("ß")),
                (1, overwrite(0)),
                (0, delete(0)),
                (0, Step::Work(0)),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn an_edit_made_while_its_file_is_parked_for_a_rename_stays_on_the_server() {
    let synced = vec![(0, file("c.txt"))];
    let mut racing = vec![(0, rename(0, 0, "C.TXT")), (0, overwrite(0))];
    racing.extend(commit(0));
    racing.extend([
        (1, DELIVER),
        (1, EFFECT),
        (1, overwrite(0)),
        (1, SCAN),
        (1, COMMIT),
        (1, COMMIT),
        (1, DELIVER),
        (1, EFFECT),
        (1, COMMIT),
    ]);
    let world = converge(2, &[synced, racing]).unwrap();
    for device in &world.devices {
        assert_eq!(device.disk.entries().len(), 2);
    }
}

#[test]
fn a_write_a_restore_exposed_stays_on_the_disks_through_a_second_restore() {
    let edit = Step::User(Action {
        kind: 2,
        target: 0,
        parent: 0,
        name: 0,
        text: 7,
    });
    let mut lost = vec![(0, RESTORE), (0, Step::Connect), (1, Step::Connect)];
    for _ in 0..30 {
        lost.extend([(0, Step::Work(0)), (1, Step::Work(0))]);
    }
    lost.push((0, RESTORE));
    let world = keep(
        2,
        &[vec![(0, file("a"))], vec![(0, BACKUP), (0, edit)], lost],
    )
    .unwrap();
    assert!(world.devices.iter().all(|device| device.disk.holds(2)));
}

#[test]
fn a_lost_file_another_device_creates_again_renames_and_deletes_stays_deleted() {
    let lost = vec![
        (0, RESTORE),
        (0, Step::Connect),
        (1, Step::Connect),
        (0, DELIVER),
        (1, DELIVER),
        (1, SCAN),
        (1, COMMIT),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (0, rename(0, 0, "b")),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (0, delete(0)),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (1, COMMIT),
        (1, DELIVER),
        (1, DELIVER),
        (1, DELIVER),
    ];
    let world = converge(2, &[Vec::new(), vec![(0, BACKUP), (0, file("a"))], lost]).unwrap();
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().is_empty())
    );
}

#[test]
fn a_lost_file_created_again_edited_and_deleted_stays_deleted() {
    let lost = vec![
        (0, RESTORE),
        (0, Step::Connect),
        (1, Step::Connect),
        (0, DELIVER),
        (1, DELIVER),
        (1, SCAN),
        (1, COMMIT),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (0, overwrite(0)),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (0, delete(0)),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
        (0, DELIVER),
        (1, COMMIT),
        (1, DELIVER),
        (1, DELIVER),
        (1, DELIVER),
    ];
    let world = converge(2, &[Vec::new(), vec![(0, BACKUP), (0, file("a"))], lost]).unwrap();
    assert!(
        world
            .devices
            .iter()
            .all(|device| device.disk.entries().is_empty())
    );
}

#[test]
fn a_folder_sent_at_a_pending_name_that_an_edit_brings_back_takes_its_name() {
    let phases = vec![
        vec![(0, file("a"))],
        vec![
            (1, overwrite(0)),
            (0, Step::Work(0)),
            (0, Step::Work(0)),
            (0, file("b")),
            (0, Step::Work(0)),
            (0, Step::Work(0)),
            (0, folder("A")),
            (0, folder("c.txt")),
            (1, folder("a")),
            (0, Step::Work(191953005214663284)),
            (0, overwrite(9057257590032405897)),
            (0, Step::Work(1656413589262271176)),
            (0, rename(5708514389238217460, 9546544883134816759, "a")),
            (0, Step::Work(0)),
            (0, COMMIT),
            (0, delete(4880399959711454722)),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_folder_sent_at_a_pending_name_for_a_case_folded_name_that_an_edit_brings_back_takes_its_name()
{
    let act = |kind, target, parent, name: &str, text| {
        let name = NAMES.iter().position(|other| *other == name).unwrap();
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text,
        })
    };
    let phases = vec![
        vec![(1, file("ß")), (0, file("a")), (0, file("c.txt"))],
        vec![
            (0, overwrite(2828337353004866769)),
            (0, folder("a")),
            (0, Step::Work(0)),
            (0, Step::Work(15079979194851)),
            (
                0,
                act(0, 17522603601231760989, 2149907348572024772, "ss", 1),
            ),
            (1, act(2, 4682896762454486991, 9427922460036998013, "ß", 1)),
            (0, rename(14663728549993967226, 16228656401449306453, "A")),
            (0, Step::Connect),
            (0, SCAN),
            (0, Step::Work(13397615803372223702)),
            (0, Step::Work(8052162068318610133)),
            (
                0,
                act(1, 14296328599741790799, 10114328226956053520, "ß", 3),
            ),
            (0, act(4, 11544038771373029048, 9096678758980585778, "A", 2)),
            (0, COMMIT),
        ],
    ];
    converge(2, &phases).unwrap();
}
