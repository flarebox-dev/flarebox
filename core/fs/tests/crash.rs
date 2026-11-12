// SPDX-License-Identifier: Apache-2.0

mod common;

use std::env;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

use common::{
    Bytes, RETURNS, SCENARIOS, Scenario, assert_settled, bind, decode, encode, entries, marker,
    now, prepare, scenario,
};
use koidrive_fs::{Effect, Outcome, Step};

const CHILD: &str = "KOIDRIVE_FS_CRASH_CHILD";
const RECOVERY_STEPS: [Step; 5] = [
    Step::Marked,
    Step::Compared,
    Step::Placed,
    Step::Returned,
    Step::Trashed,
];

fn effect_file(root: &Path) -> std::path::PathBuf {
    root.parent().unwrap().join("entries.txt")
}

#[test]
fn child_process_for_crash_tests() {
    let Ok(spec) = env::var(CHILD) else {
        return;
    };
    let [name, root, phase, step]: [&str; 4] =
        spec.split(';').collect::<Vec<_>>().try_into().unwrap();
    let root = Path::new(root);
    let scenario = scenario(name);
    let effect = (scenario.build)(&decode(&fs::read_to_string(effect_file(root)).unwrap()));
    let space = bind(root);
    let mut hook = |reached: Step| {
        if format!("{reached:?}") == step {
            rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL)
                .unwrap();
            loop {
                std::thread::park();
            }
        }
    };
    let result = match phase {
        "apply" => space.apply(&marker(), &effect, &mut Bytes, now(), &mut hook),
        _ => space.recover(&marker(), &effect, &mut Bytes, now(), &mut hook),
    };
    result.unwrap();
}

fn run_child(scenario: &Scenario, root: &Path, phase: &str, step: Step) -> ExitStatus {
    Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "child_process_for_crash_tests",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(
            CHILD,
            format!("{};{};{phase};{step:?}", scenario.name, root.display()),
        )
        .stdout(Stdio::null())
        .status()
        .unwrap()
}

fn steps(scenario: &Scenario) -> Vec<Step> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let effect = prepare(scenario, &root);
    let mut steps = Vec::new();
    let outcome = bind(&root)
        .apply(&marker(), &effect, &mut common::Bytes, now(), &mut |step| {
            steps.push(step)
        })
        .unwrap();
    assert_eq!(
        matches!(outcome, Outcome::Done { .. }),
        scenario.done_from.is_some(),
        "{}: {outcome:?}",
        scenario.name
    );
    steps
}

fn crash(scenario: &Scenario, dir: &Path, kill: Step) -> (std::path::PathBuf, Effect) {
    let root = dir.join("space");
    let effect = prepare(scenario, &root);
    fs::write(effect_file(&root), encode(&entries(&root, scenario.paths))).unwrap();
    let status = run_child(scenario, &root, "apply", kill);
    assert_eq!(
        status.signal(),
        Some(9),
        "{} killed at {kill:?}: {status}",
        scenario.name
    );
    (scenario.disturb)(&root);
    (root, effect)
}

fn recovery_steps(scenario: &Scenario, kill: Step) -> Vec<Step> {
    let dir = tempfile::tempdir().unwrap();
    let (root, effect) = crash(scenario, dir.path(), kill);
    let mut steps = Vec::new();
    bind(&root)
        .recover(&marker(), &effect, &mut Bytes, now(), &mut |step| {
            steps.push(step)
        })
        .unwrap();
    steps
}

fn kill_at_every_step_then_recover(scenario: &Scenario) -> Vec<Step> {
    let steps = steps(scenario);
    let done_from = scenario
        .done_from
        .map(|from| steps.iter().position(|step| *step == from).unwrap());
    let mut killed = Vec::new();
    for (index, &kill) in steps.iter().enumerate() {
        let reached = recovery_steps(scenario, kill);
        for recovery_kill in std::iter::once(None).chain(RECOVERY_STEPS.map(Some)) {
            let context = format!(
                "{} killed at {kill:?}, recovery killed at {recovery_kill:?}",
                scenario.name
            );
            let dir = tempfile::tempdir().unwrap();
            let (root, effect) = crash(scenario, dir.path(), kill);
            if let Some(step) = recovery_kill {
                let status = run_child(scenario, &root, "recover", step);
                if reached.contains(&step) {
                    assert_eq!(status.signal(), Some(9), "{context}: {status}");
                    if !killed.contains(&step) {
                        killed.push(step);
                    }
                } else {
                    assert!(status.success(), "{context}: {status}");
                }
            }

            let space = bind(&root);
            let outcome = space
                .recover(&marker(), &effect, &mut Bytes, now(), &mut |_| {})
                .unwrap();
            let expected_done = done_from.is_some_and(|from| index >= from);
            assert_eq!(
                matches!(outcome, Outcome::Done { .. }),
                expected_done,
                "{context}: {outcome:?}"
            );
            (scenario.check)(&root, &outcome);
            assert_settled(&root);

            let again = space
                .recover(&marker(), &effect, &mut Bytes, now(), &mut |_| {})
                .unwrap();
            assert_eq!(
                matches!(again, Outcome::Done { .. }),
                expected_done,
                "{context}: again {again:?}"
            );
            (scenario.check)(&root, &outcome);
            assert_settled(&root);

            let mut outside: Vec<_> = fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            outside.sort();
            assert_eq!(outside, ["entries.txt", "space"], "{context}");
        }
    }
    killed.sort_by_key(|step| RECOVERY_STEPS.iter().position(|known| known == step));
    killed
}

#[test]
fn kill_during_a_replacement_then_recover() {
    assert_eq!(
        kill_at_every_step_then_recover(&SCENARIOS[0]),
        [Step::Marked, Step::Compared, Step::Placed, Step::Trashed]
    );
}

#[test]
fn kill_during_a_new_file_then_recover() {
    assert_eq!(
        kill_at_every_step_then_recover(&SCENARIOS[1]),
        [Step::Marked]
    );
}

#[test]
fn kill_during_a_new_folder_then_recover() {
    assert_eq!(
        kill_at_every_step_then_recover(&SCENARIOS[2]),
        [Step::Marked]
    );
}

#[test]
fn kill_during_a_move_then_recover() {
    assert_eq!(
        kill_at_every_step_then_recover(&SCENARIOS[3]),
        [Step::Marked]
    );
}

#[test]
fn kill_during_a_file_removal_then_recover() {
    assert_eq!(
        kill_at_every_step_then_recover(&SCENARIOS[4]),
        [Step::Marked, Step::Compared, Step::Trashed]
    );
}

#[test]
fn kill_during_a_folder_removal_then_recover() {
    assert_eq!(
        kill_at_every_step_then_recover(&SCENARIOS[5]),
        [Step::Marked, Step::Compared, Step::Trashed]
    );
}

#[test]
fn kill_during_the_removal_of_a_folder_that_holds_platform_files_then_recover() {
    assert_eq!(
        kill_at_every_step_then_recover(&SCENARIOS[6]),
        [Step::Marked, Step::Compared, Step::Trashed]
    );
}

#[test]
fn kill_during_an_edited_replacement_then_recover() {
    assert_eq!(
        kill_at_every_step_then_recover(&RETURNS[0]),
        [Step::Marked, Step::Compared, Step::Returned]
    );
}

#[test]
fn kill_during_an_edited_replacement_whose_name_is_taken_then_recover() {
    assert_eq!(
        kill_at_every_step_then_recover(&RETURNS[1]),
        [Step::Marked, Step::Compared, Step::Returned]
    );
}

#[test]
fn kill_during_an_edited_replacement_whose_folder_moves_then_recover() {
    assert_eq!(
        kill_at_every_step_then_recover(&RETURNS[2]),
        [Step::Marked, Step::Compared, Step::Returned]
    );
}

#[test]
fn kill_during_a_replacement_whose_name_is_taken_then_recover() {
    assert_eq!(
        kill_at_every_step_then_recover(&RETURNS[3]),
        [Step::Marked, Step::Compared, Step::Trashed]
    );
}
