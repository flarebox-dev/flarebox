// SPDX-License-Identifier: Apache-2.0

//! Kills a real process with SIGKILL at every hook point of an effect (design
//! §5.2), and again at every hook point of the recovery that follows, then
//! recovers (design §5.3) and checks that the disk holds a state the outcome
//! explains, with nothing lost and nothing left in quarantine or staging
//! (design §7.4). The test binary re-runs itself as the child process.

mod common;

use std::env;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

use common::{
    Bytes, SCENARIOS, Scenario, assert_settled, decode, encode, entries, marker, now, prepare,
    scenario,
};
use flarebox_fs::{Effect, Outcome, Space, Step};

const CHILD: &str = "FLAREBOX_FS_CRASH_CHILD";
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

/// The child: applies or recovers the scenario's effect and kills itself
/// with SIGKILL when it reaches the step it was given. It exits normally if
/// it never does.
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
    let space = Space::open(root).unwrap();
    let mut hook = |reached: Step| {
        if format!("{reached:?}") == step {
            rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL)
                .unwrap();
            // The signal may land a moment later, so this thread must not
            // go on to the next step meanwhile.
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

/// The steps an uninterrupted run of the scenario's effect reaches.
fn steps(scenario: &Scenario) -> Vec<Step> {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let effect = prepare(scenario, &root);
    let mut steps = Vec::new();
    let outcome = Space::open(&root)
        .unwrap()
        .apply(&marker(), &effect, &mut common::Bytes, now(), &mut |step| {
            steps.push(step)
        })
        .unwrap();
    assert_eq!(outcome, Outcome::Done, "{}", scenario.name);
    steps
}

fn kill_at_every_step_then_recover(scenario: &Scenario) {
    let steps = steps(scenario);
    let done_from = steps
        .iter()
        .position(|step| *step == scenario.done_from)
        .unwrap();
    for (index, &kill) in steps.iter().enumerate() {
        for recovery_kill in std::iter::once(None).chain(RECOVERY_STEPS.map(Some)) {
            let context = format!(
                "{} killed at {kill:?}, recovery killed at {recovery_kill:?}",
                scenario.name
            );
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("space");
            let effect: Effect = prepare(scenario, &root);
            fs::write(effect_file(&root), encode(&entries(&root, scenario.paths))).unwrap();

            let status = run_child(scenario, &root, "apply", kill);
            assert_eq!(status.signal(), Some(9), "{context}: {status}");
            if let Some(step) = recovery_kill {
                let status = run_child(scenario, &root, "recover", step);
                assert!(
                    status.success() || status.signal() == Some(9),
                    "{context}: {status}"
                );
            }

            let space = Space::open(&root).unwrap();
            let outcome = space
                .recover(&marker(), &effect, &mut common::Bytes, now(), &mut |_| {})
                .unwrap();
            let expected_done = index >= done_from;
            assert_eq!(
                outcome == Outcome::Done,
                expected_done,
                "{context}: {outcome:?}"
            );
            (scenario.check)(&root, &outcome);
            assert_settled(&root);

            let again = space
                .recover(&marker(), &effect, &mut common::Bytes, now(), &mut |_| {})
                .unwrap();
            assert_eq!(
                again == Outcome::Done,
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
}

#[test]
fn kill_during_a_replacement_then_recover() {
    kill_at_every_step_then_recover(&SCENARIOS[0]);
}

#[test]
fn kill_during_a_new_file_then_recover() {
    kill_at_every_step_then_recover(&SCENARIOS[1]);
}

#[test]
fn kill_during_a_new_folder_then_recover() {
    kill_at_every_step_then_recover(&SCENARIOS[2]);
}

#[test]
fn kill_during_a_move_then_recover() {
    kill_at_every_step_then_recover(&SCENARIOS[3]);
}

#[test]
fn kill_during_a_file_removal_then_recover() {
    kill_at_every_step_then_recover(&SCENARIOS[4]);
}

#[test]
fn kill_during_a_folder_removal_then_recover() {
    kill_at_every_step_then_recover(&SCENARIOS[5]);
}
