// SPDX-License-Identifier: Apache-2.0

//! Tests that need to interrupt a scan at a fixed point or to take coverage
//! away from the platform watcher.

use std::fs;

use notify::event::Flag;

use super::*;

const WAIT: Duration = Duration::from_secs(10);

fn tree(root: &Path, files: usize) {
    fs::create_dir_all(root.join("d")).unwrap();
    fs::write(root.join("d/x"), "x").unwrap();
    for i in 0..files {
        fs::write(root.join(format!("f{i}.txt")), "f").unwrap();
    }
}

fn scoped(files: usize) -> Vec<PathBuf> {
    (0..files)
        .map(|i| PathBuf::from(format!("f{i}.txt")))
        .collect()
}

#[test]
fn root_moved_after_opening_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    tree(&root, 200);
    for paths in [vec![PathBuf::new()], scoped(200)] {
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let before = snapshot.clone();
        let moved = dir.path().join("moved");
        let result = snapshot.rescan_with(&root, paths, &mut |cut| {
            if let Cut::RootOpened = cut {
                fs::rename(&root, &moved).unwrap();
            }
        });
        assert!(
            matches!(&result, Err(Error::Io(err)) if err.kind() == io::ErrorKind::NotFound),
            "{result:?}"
        );
        assert_eq!(snapshot, before);
        fs::rename(&moved, &root).unwrap();
    }
}

#[test]
fn root_removed_after_opening_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    for paths in [vec![PathBuf::new()], scoped(200)] {
        tree(&root, 200);
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let before = snapshot.clone();
        let result = snapshot.rescan_with(&root, paths, &mut |cut| {
            if let Cut::RootOpened = cut {
                fs::remove_dir_all(&root).unwrap();
            }
        });
        assert!(
            matches!(&result, Err(Error::Io(err)) if err.kind() == io::ErrorKind::NotFound),
            "{result:?}"
        );
        assert_eq!(snapshot, before);
    }
}

#[test]
fn root_replaced_after_opening_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    tree(&root, 20);
    let mut snapshot = Snapshot::scan(&root).unwrap();
    let before = snapshot.clone();
    let result = snapshot.rescan_with(&root, [PathBuf::new()], &mut |cut| {
        if let Cut::RootOpened = cut {
            fs::rename(&root, dir.path().join("moved")).unwrap();
            fs::create_dir(&root).unwrap();
        }
    });
    assert!(matches!(result, Err(Error::RootReplaced)), "{result:?}");
    assert_eq!(snapshot, before);
}

#[test]
fn folder_replaced_during_traversal_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("x"), "outside").unwrap();
    for replacement in ["symlink", "folder"] {
        tree(&root, 0);
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let before = snapshot.clone();
        let result = snapshot.rescan_with(&root, [PathBuf::new()], &mut |cut| {
            if let Cut::OpeningDir(path) = cut
                && path == Path::new("d")
            {
                fs::rename(root.join("d"), root.join("d-old")).unwrap();
                match replacement {
                    "symlink" => std::os::unix::fs::symlink(&outside, root.join("d")).unwrap(),
                    _ => fs::create_dir(root.join("d")).unwrap(),
                }
            }
        });
        assert!(
            matches!(result, Err(Error::Changed)),
            "{replacement}: {result:?}"
        );
        assert_eq!(snapshot, before, "{replacement}");
        fs::remove_dir_all(&root).unwrap();
    }
}

fn wait_for(watcher: &mut NativeWatcher, expected: &[Change]) -> Vec<Change> {
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    while !expected.iter().all(|change| seen.contains(change)) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected:?}; saw {seen:?}"
        );
        seen.extend(watcher.poll(Duration::from_millis(500)).unwrap());
    }
    seen
}

fn created(path: &str, kind: EntryKind) -> Change {
    Change::Created {
        path: path.into(),
        kind,
    }
}

/// Replaces the platform watcher with one that watches the root folder only,
/// as if the watches below it were lost.
fn lose_coverage(watcher: &mut NativeWatcher) {
    watcher.watcher = notify::recommended_watcher(watcher.sender.clone()).unwrap();
    watcher
        .watcher
        .watch(&watcher.root, RecursiveMode::NonRecursive)
        .unwrap();
}

#[test]
fn rescan_request_restores_lost_watch_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir(root.join("sub")).unwrap();
    let mut watcher = NativeWatcher::start(&root, Snapshot::default()).unwrap();
    wait_for(&mut watcher, &[created("sub", EntryKind::Dir)]);

    lose_coverage(&mut watcher);
    fs::write(root.join("sub/missed.txt"), "m").unwrap();
    #[cfg(target_os = "linux")]
    assert_eq!(watcher.poll(Duration::from_millis(500)).unwrap(), []);

    watcher
        .sender
        .send(Ok(
            notify::Event::new(EventKind::Other).set_flag(Flag::Rescan)
        ))
        .unwrap();
    wait_for(&mut watcher, &[created("sub/missed.txt", EntryKind::File)]);

    fs::write(root.join("sub/later.txt"), "l").unwrap();
    wait_for(&mut watcher, &[created("sub/later.txt", EntryKind::File)]);
}

#[cfg(target_os = "linux")]
#[test]
fn new_folders_are_registered_before_their_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut watcher = NativeWatcher::start(&root, Snapshot::default()).unwrap();
    assert_eq!(watcher.poll(Duration::from_millis(300)).unwrap(), []);

    lose_coverage(&mut watcher);
    let early = root.join("sub/early.txt");
    watcher.before_register = Some(Box::new(move || fs::write(&early, "e").unwrap()));
    fs::create_dir(root.join("sub")).unwrap();
    wait_for(
        &mut watcher,
        &[
            created("sub", EntryKind::Dir),
            created("sub/early.txt", EntryKind::File),
        ],
    );

    watcher.before_register = None;
    fs::write(root.join("sub/late.txt"), "l").unwrap();
    wait_for(&mut watcher, &[created("sub/late.txt", EntryKind::File)]);
}

#[cfg(target_os = "linux")]
#[test]
fn a_new_folder_that_vanishes_before_its_watch_is_not_a_watch_failure() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut watcher = NativeWatcher::start(&root, Snapshot::default()).unwrap();
    assert_eq!(watcher.poll(Duration::from_millis(300)).unwrap(), []);

    let sub = root.join("tmp-build");
    let gone = sub.clone();
    watcher.before_register = Some(Box::new(move || fs::remove_dir(&gone).unwrap()));
    fs::create_dir(&sub).unwrap();
    let seen = wait_for(
        &mut watcher,
        &[
            created("tmp-build", EntryKind::Dir),
            Change::Removed {
                path: "tmp-build".into(),
                kind: EntryKind::Dir,
            },
        ],
    );
    assert_eq!(seen.len(), 2, "{seen:?}");

    watcher.before_register = None;
    assert_eq!(watcher.poll(Duration::from_millis(300)).unwrap(), []);
}
