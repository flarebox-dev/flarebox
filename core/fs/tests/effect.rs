// SPDX-License-Identifier: Apache-2.0

//! The effect protocol of design §5.2 against a user acting at each hook
//! point, on the file system the tests run on (design §7.4).

mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use common::{
    Bytes, ID, REMOTE, SCENARIOS, SYNCED, assert_settled, at, contents, entries, marker, now,
    prepare, private_names, stage, trashed,
};
use flarebox_fs::{
    Action, Base, Displaced, Effect, Location, Outcome, PARKED_DIR, PRIVATE_DIR, Reason,
    STAGING_DIR, Snapshot, Space, Step, read_marker,
};

fn replace(root: &Path) -> Effect {
    prepare(&SCENARIOS[0], root)
}

fn apply(root: &Path, effect: &Effect, mut hook: impl FnMut(Step)) -> Outcome {
    Space::open(root)
        .unwrap()
        .apply(&marker(), effect, &mut Bytes, now(), &mut hook)
        .unwrap()
}

fn failed(reason: Reason) -> Outcome {
    Outcome::Failed {
        reason,
        recovered: None,
    }
}

fn listing(path: &Path) -> Vec<PathBuf> {
    let mut names: Vec<PathBuf> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    names.sort();
    names
}

#[test]
fn replacement_places_the_new_file_and_trashes_the_old_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let mut steps = Vec::new();
    assert_eq!(apply(root, &effect, |step| steps.push(step)), Outcome::Done);
    assert_eq!(
        steps,
        [
            Step::Marked,
            Step::Checked,
            Step::Quarantined,
            Step::Compared,
            Step::Placed,
            Step::Trashed
        ]
    );
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), REMOTE);
    assert_eq!(trashed(root), [SYNCED]);
    assert_settled(root);
}

#[test]
fn marker_holds_the_counter_before_the_disk_changes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let mut seen = None;
    apply(root, &effect, |step| {
        if step == Step::Marked {
            seen = Some((
                read_marker(root).unwrap(),
                fs::read(root.join("docs/report.txt")).unwrap(),
            ));
        }
    });
    assert_eq!(seen, Some((Some(marker()), SYNCED.to_vec())));
}

#[test]
fn write_before_the_check_fails_without_displacing_anything() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    fs::write(root.join("docs/report.txt"), "user edit").unwrap();
    assert_eq!(apply(root, &effect, |_| {}), failed(Reason::Changed));
    assert_eq!(
        fs::read(root.join("docs/report.txt")).unwrap(),
        b"user edit"
    );
    assert!(trashed(root).is_empty());
    assert_settled(root);
}

#[test]
fn write_between_the_check_and_the_quarantine_is_put_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let outcome = apply(root, &effect, |step| {
        if step == Step::Checked {
            fs::write(root.join("docs/report.txt"), "user edit").unwrap();
        }
    });
    assert_eq!(outcome, failed(Reason::Changed));
    assert_eq!(
        fs::read(root.join("docs/report.txt")).unwrap(),
        b"user edit"
    );
    assert!(trashed(root).is_empty());
    assert_settled(root);
}

#[test]
fn write_through_an_old_descriptor_before_the_comparison_is_put_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let held = OpenOptions::new()
        .write(true)
        .open(root.join("docs/report.txt"))
        .unwrap();
    let outcome = apply(root, &effect, |step| {
        if step == Step::Quarantined {
            held.write_all_at(b"SYNCED", 0).unwrap();
        }
    });
    assert_eq!(outcome, failed(Reason::Changed));
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), b"SYNCED");
    assert_settled(root);
}

#[test]
fn edit_whose_name_was_taken_again_becomes_a_recovered_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let outcome = apply(root, &effect, |step| match step {
        Step::Checked => fs::write(root.join("docs/report.txt"), "user edit").unwrap(),
        Step::Compared => fs::write(root.join("docs/report.txt"), "new file").unwrap(),
        _ => {}
    });
    let recovered = PathBuf::from("docs/report (recovered 2023-11-14).txt");
    assert_eq!(
        outcome,
        Outcome::Failed {
            reason: Reason::Changed,
            recovered: Some(recovered.clone()),
        }
    );
    assert_eq!(fs::read(root.join(recovered)).unwrap(), b"user edit");
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), b"new file");
    assert_settled(root);
}

#[test]
fn target_taken_during_placement_keeps_the_entry_there() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let outcome = apply(root, &effect, |step| {
        if step == Step::Compared {
            fs::write(root.join("docs/report.txt"), "someone else").unwrap();
        }
    });
    assert_eq!(outcome, failed(Reason::Taken));
    assert_eq!(
        fs::read(root.join("docs/report.txt")).unwrap(),
        b"someone else"
    );
    assert_eq!(trashed(root), [SYNCED]);
    assert_settled(root);
}

#[test]
fn write_through_a_descriptor_held_across_the_effect_lands_in_the_trash() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let mut held = OpenOptions::new()
        .append(true)
        .open(root.join("docs/report.txt"))
        .unwrap();
    assert_eq!(apply(root, &effect, |_| {}), Outcome::Done);
    held.write_all(b" and more").unwrap();
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), REMOTE);
    assert_eq!(trashed(root), [b"synced and more".to_vec()]);
}

#[test]
fn removed_file_goes_to_the_trash_unless_it_changed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(&SCENARIOS[4], root);
    assert_eq!(apply(root, &effect, |_| {}), Outcome::Done);
    assert_eq!(contents(root.join("docs/report.txt")), None);
    assert_eq!(trashed(root), [SYNCED]);
    assert_settled(root);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(&SCENARIOS[4], root);
    let outcome = apply(root, &effect, |step| {
        if step == Step::Checked {
            fs::write(root.join("docs/report.txt"), "user edit").unwrap();
        }
    });
    assert_eq!(outcome, failed(Reason::Changed));
    assert_eq!(
        fs::read(root.join("docs/report.txt")).unwrap(),
        b"user edit"
    );
    assert!(trashed(root).is_empty());
    assert_settled(root);
}

#[test]
fn folder_that_gained_an_entry_is_put_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(&SCENARIOS[5], root);
    let outcome = apply(root, &effect, |step| {
        if step == Step::Checked {
            fs::write(root.join("docs/empty/late.txt"), "late").unwrap();
        }
    });
    assert_eq!(outcome, failed(Reason::Changed));
    assert_eq!(fs::read(root.join("docs/empty/late.txt")).unwrap(), b"late");
    assert!(private_names(root, "trash").is_empty());
    assert_settled(root);
}

#[test]
fn creations_never_take_a_name_that_is_in_use() {
    for scenario in [&SCENARIOS[1], &SCENARIOS[2]] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let effect = prepare(scenario, root);
        let name = match &effect.action {
            Action::CreateDir { .. } => "docs/sub",
            _ => "docs/new.txt",
        };
        fs::write(root.join(name), "already here").unwrap();
        assert_eq!(
            apply(root, &effect, |_| {}),
            failed(Reason::Taken),
            "{}",
            scenario.name
        );
        assert_eq!(fs::read(root.join(name)).unwrap(), b"already here");
        assert_settled(root);
    }
}

#[test]
fn move_needs_the_source_identity_and_a_free_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(&SCENARIOS[3], root);
    fs::write(root.join("other/b.txt"), "taken").unwrap();
    assert_eq!(apply(root, &effect, |_| {}), failed(Reason::Taken));
    assert_eq!(fs::read(root.join("docs/a.txt")).unwrap(), SYNCED);
    assert_eq!(fs::read(root.join("other/b.txt")).unwrap(), b"taken");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(&SCENARIOS[3], root);
    fs::write(root.join("docs/a2.txt"), SYNCED).unwrap();
    fs::rename(root.join("docs/a2.txt"), root.join("docs/a.txt")).unwrap();
    assert_eq!(apply(root, &effect, |_| {}), failed(Reason::Changed));
    assert_eq!(contents(root.join("other/b.txt")), None);
}

#[test]
fn move_into_and_out_of_the_parked_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("a")).unwrap();
    fs::write(root.join("a/f.txt"), SYNCED).unwrap();
    let found = entries(root, &["a", "a/f.txt"]);
    let parked = Location::Parked("node-1".into());
    let park = Effect {
        id: ID.into(),
        action: Action::Move {
            from: at(vec![common::dir("a", &found[0])], "f.txt"),
            id: found[1].id,
            to: parked.clone(),
        },
    };
    assert_eq!(apply(root, &park, |_| {}), Outcome::Done);
    let parked_path = root.join(PRIVATE_DIR).join(PARKED_DIR).join("node-1");
    assert_eq!(fs::read(&parked_path).unwrap(), SYNCED);

    let unpark = Effect {
        id: "e2".into(),
        action: Action::Move {
            from: parked,
            id: found[1].id,
            to: at(Vec::new(), "F.txt"),
        },
    };
    assert_eq!(apply(root, &unpark, |_| {}), Outcome::Done);
    assert_eq!(fs::read(root.join("F.txt")).unwrap(), SYNCED);
}

#[test]
fn parent_swapped_for_a_symlink_after_the_check_fails_the_effect() {
    let outside = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let outcome = apply(root, &effect, |step| {
        if step == Step::Checked {
            fs::rename(root.join("docs"), root.join("docs-old")).unwrap();
            std::os::unix::fs::symlink(outside.path(), root.join("docs")).unwrap();
        }
    });
    assert_eq!(outcome, failed(Reason::Moved));
    assert_eq!(fs::read(root.join("docs-old/report.txt")).unwrap(), SYNCED);
    assert!(listing(outside.path()).is_empty());
    assert_settled(root);
}

#[test]
fn parent_swapped_for_another_folder_after_the_check_fails_the_effect() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let outcome = apply(root, &effect, |step| {
        if step == Step::Checked {
            fs::rename(root.join("docs"), root.join("docs-old")).unwrap();
            fs::create_dir(root.join("docs")).unwrap();
            fs::write(root.join("docs/report.txt"), SYNCED).unwrap();
        }
    });
    assert_eq!(outcome, failed(Reason::Moved));
    assert_eq!(fs::read(root.join("docs-old/report.txt")).unwrap(), SYNCED);
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), SYNCED);
    assert_settled(root);
}

#[test]
fn parent_moved_out_of_the_root_after_the_check_is_left_alone() {
    for scenario in &SCENARIOS {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let effect = prepare(scenario, root);
        let before = Snapshot::scan(root.join("docs")).unwrap();
        let outcome = apply(root, &effect, |step| {
            if step == Step::Checked {
                fs::rename(root.join("docs"), outside.path().join("docs")).unwrap();
            }
        });
        assert_eq!(outcome, failed(Reason::Moved), "{}", scenario.name);
        assert_eq!(
            listing(outside.path()),
            [outside.path().join("docs")],
            "{}",
            scenario.name
        );
        let after = Snapshot::scan(outside.path().join("docs")).unwrap();
        assert_eq!(after.entries, before.entries, "{}", scenario.name);
        assert_settled(root);
    }
}

#[test]
fn parked_effects_need_the_parked_folder_identity() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let parked = root.join(PRIVATE_DIR).join(PARKED_DIR);
    fs::create_dir_all(parked.join("node-1")).unwrap();
    fs::write(parked.join("node-1/f.txt"), SYNCED).unwrap();
    fs::create_dir(root.join("x")).unwrap();
    let wrong = entries(root, &["x"])[0].id;
    let file = Snapshot::scan(parked.join("node-1")).unwrap().entries[Path::new("f.txt")];
    let effect = Effect {
        id: ID.into(),
        action: Action::RemoveFile {
            displaced: Displaced {
                at: Location::At {
                    base: Base::Parked {
                        name: "node-1".into(),
                        id: wrong,
                    },
                    dirs: Vec::new(),
                    name: "f.txt".into(),
                },
                entry: file,
            },
        },
    };
    assert_eq!(apply(root, &effect, |_| {}), failed(Reason::Moved));
    assert_eq!(fs::read(parked.join("node-1/f.txt")).unwrap(), SYNCED);
}

#[test]
fn recovery_puts_back_a_file_written_through_an_old_descriptor_after_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let held = OpenOptions::new()
        .write(true)
        .open(root.join("docs/report.txt"))
        .unwrap();
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        apply(root, &effect, |step| {
            if step == Step::Quarantined {
                panic!("crash");
            }
        })
    }));
    assert!(crashed.is_err());
    held.write_all_at(b"SYNCED", 0).unwrap();

    let space = Space::open(root).unwrap();
    let outcome = space
        .recover(&marker(), &effect, &mut Bytes, now(), &mut |_| {})
        .unwrap();
    assert_eq!(outcome, failed(Reason::Changed));
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), b"SYNCED");
    assert_settled(root);
}

#[test]
fn staged_files_without_a_record_are_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let space = Space::open(root).unwrap();
    stage(&space, REMOTE);
    let mut other = space.stage("e0").unwrap();
    other.write_all(b"older").unwrap();
    assert_eq!(space.clear_staging(Some(ID)).unwrap(), ["e0"]);
    assert_eq!(private_names(root, STAGING_DIR), [ID]);
    assert_eq!(space.clear_staging(None).unwrap(), [ID]);
    assert!(private_names(root, STAGING_DIR).is_empty());
}

#[test]
fn staged_file_gets_the_executable_bit() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let space = Space::open(root).unwrap();
    stage(&space, REMOTE);
    let staged = root.join(PRIVATE_DIR).join(STAGING_DIR).join(ID);
    assert_eq!(
        fs::metadata(&staged).unwrap().permissions().mode() & 0o111,
        0
    );
    space.finish_staged(ID, true).unwrap();
    let mode = fs::metadata(&staged).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, (mode & 0o444) >> 2);
    assert_ne!(mode & 0o111, 0);
}

#[test]
fn effect_ids_must_be_single_names() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut effect = replace(root);
    let space = Space::open(root).unwrap();
    for id in ["", ".", "..", "../e1", "a/b"] {
        effect.id = id.into();
        let err = space
            .apply(&marker(), &effect, &mut Bytes, now(), &mut |_| {})
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{id:?}");
        assert_eq!(
            space.stage(id).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), SYNCED);
}
