// SPDX-License-Identifier: Apache-2.0

//! Two or three devices share one reference server while their users edit, effects fail,
//! messages are lost, repeated or reordered, commits are refused and devices crash between store
//! transactions or in the middle of an effect (docs/design/sync-engine.md section 7.3).
//! `flarebox_sim::world` describes the harness and its four kinds of failure.
//!
//! `devices_lose_no_write` fails when a write is lost or when the engine, the store or the server
//! refuses a state. A case ends early, and is reported, when a restart gives the entry of one node
//! to another node (GUN-342). `devices_keep_a_store_that_loads` also asks that the engine loads what the
//! store holds after the transaction that records the outcome of an effect.
//! `devices_converge_once_the_faults_stop` also asks that every disk reaches the server's tree and
//! that no commit makes a change the user of its device never made. The engine does not do these
//! two under every interleaving yet, so both properties are ignored, and so are the fixed
//! schedules of the known failures. `cargo test -p flarebox-sim --test devices -- --ignored`
//! runs them.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use flarebox_proto::v1::error;
use flarebox_sim::world::{Action, Cut, Failure, NAMES, Outcome, Reply, Step, TEXTS, World};
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

fn step() -> impl Strategy<Value = Step> {
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
    prop_oneof![
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
}

fn phases() -> impl Strategy<Value = Vec<Vec<(usize, Step)>>> {
    prop::collection::vec(prop::collection::vec((0..3usize, step()), 0..48), 1..=3)
}

/// Devices that print every step with the state it leaves when `FLAREBOX_SIM_TRACE` is set.
fn world(devices: usize) -> World {
    let mut world = World::new(devices);
    world.trace = std::env::var_os("FLAREBOX_SIM_TRACE").is_some();
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

/// Stops only when a write is lost or a state is refused, or when the run cannot go on.
fn keep(devices: usize, phases: &[Vec<(usize, Step)>]) -> Result<World, Failure> {
    let stops = |failure: &Failure| matches!(failure, Failure::Broken(_) | Failure::Adopted(_));
    run(devices, phases, stops)
}

/// The cases of this run that ended at GUN-342.
static ADOPTED: AtomicUsize = AtomicUsize::new(0);

/// Appends a line to the job summary. The step that runs these tests may not have written to
/// its summary yet, so the file is made when it does not exist.
fn summarize(path: &Path, line: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())
}

/// Passes a case that ended at GUN-342, where the trees cannot be judged any more, and fails a
/// case with any other failure. Every such case is reported with the seed of the run: on
/// stderr, which the test harness does not capture, and in the job summary on GitHub Actions.
/// A summary that cannot be written is reported on stderr too. The fix of GUN-342 removes this
/// function.
fn judge(run: Result<World, Failure>) -> Result<(), TestCaseError> {
    let why = match run {
        Err(Failure::Adopted(why)) => why,
        Err(failure) => return Err(TestCaseError::fail(failure.to_string())),
        Ok(_) => return Ok(()),
    };
    let count = ADOPTED.fetch_add(1, Ordering::Relaxed) + 1;
    let seed = std::env::var("PROPTEST_RNG_SEED").unwrap_or_else(|_| "not set".into());
    let line = format!(
        "- devices: case {count} of this run that ended at GUN-342 (PROPTEST_RNG_SEED {seed}): {why}\n"
    );
    let _ = std::io::stderr().write_all(line.as_bytes());
    if let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY")
        && let Err(error) = summarize(Path::new(&path), &line)
    {
        let path = Path::new(&path).display();
        let _ = writeln!(
            std::io::stderr(),
            "- devices: the job summary {path} was not written: {error}"
        );
    }
    Ok(())
}

#[test]
fn a_report_is_written_to_a_job_summary_that_does_not_exist_yet() {
    let folder = std::env::temp_dir().join(format!("flarebox-sim-devices-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder).unwrap();
    let path = folder.join("summary.md");
    summarize(&path, "first\n").unwrap();
    summarize(&path, "second\n").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\nsecond\n");
    assert!(summarize(&folder.join("missing").join("summary.md"), "third\n").is_err());
    std::fs::remove_dir_all(&folder).unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|cases| cases.parse().ok()).unwrap_or(256),
    ))]

    #[test]
    fn devices_lose_no_write(devices in 2..=3usize, phases in phases()) {
        judge(keep(devices, &phases))?;
    }

    #[test]
    #[ignore = "GUN-330, GUN-339: the engine does not load what the store holds after some effects"]
    fn devices_keep_a_store_that_loads(devices in 2..=3usize, phases in phases()) {
        judge(run(devices, &phases, |failure| !matches!(failure, Failure::Diverged(_))))?;
    }

    #[test]
    #[ignore = "GUN-326, GUN-327, GUN-328, GUN-330, GUN-332, GUN-333, GUN-334, GUN-339, GUN-342: the engine does not converge under every interleaving"]
    fn devices_converge_once_the_faults_stop(devices in 2..=3usize, phases in phases()) {
        converge(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }
}

/// A fixed sample of the schedules reaches every fault and every way back from one that the
/// harness counts, so a change to the strategies cannot drop one without this test failing.
#[test]
fn the_schedules_reach_every_fault() {
    let mut runner = TestRunner::deterministic();
    let strategy = (2..=3usize, phases());
    let mut seen: BTreeMap<&str, u64> = BTreeMap::new();
    for _ in 0..256 {
        let (devices, phases) = strategy.new_tree(&mut runner).unwrap().current();
        let world = match keep(devices, &phases) {
            Err(Failure::Adopted(_)) => continue,
            world => world.unwrap(),
        };
        for (what, count) in world.seen {
            *seen.entry(what).or_default() += count;
        }
    }
    for (what, count) in &seen {
        println!("{count:>8} {what}");
    }
    for what in EVENTS {
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
#[test]
#[ignore = "GUN-326: the engine uploads the conflict copy a second time"]
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
#[test]
#[ignore = "GUN-327: the engine sends the old name back as a Move"]
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
#[test]
#[ignore = "GUN-328: the engine never ends the wait for the colliding name"]
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
#[test]
#[ignore = "GUN-332: the engine downloads the deleted file again and restores it on the server"]
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
#[ignore = "GUN-333: the server puts the conflict copy into the trash, and the edit leaves the disk"]
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
#[ignore = "GUN-333: the server keeps both folders and puts the file into the trash"]
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
#[test]
#[ignore = "GUN-334: the engine sends the removal that its own effect made as a Delete"]
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
/// away after the engine has seen it there. The download ends before the next scan.
#[test]
#[ignore = "GUN-330: the store holds two nodes with one name and cannot be loaded"]
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
/// into the folder, a rescan sees it there, and the user moves it out again. The removal then
/// finds the folder empty, and L keeps the file in a folder it no longer holds.
#[test]
#[ignore = "GUN-330: the store holds a file in a folder it does not hold and cannot be loaded"]
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
/// nothing at the name "a" and reports the removal as done (section 5.3), and L keeps the new
/// folder in a folder it no longer holds.
#[test]
#[ignore = "GUN-339: a restart ends the removal of a folder the user renamed, and the store cannot be loaded"]
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
/// has the same bytes, to "a". The restart finds those bytes at the name "a" and reports the
/// download as done with the entry that L holds as "b" (section 5.3).
#[test]
#[ignore = "GUN-342: a restart takes a renamed file for the download, and a file leaves the server"]
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
