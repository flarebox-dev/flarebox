// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use flarebox_fs::{Change, EntryKind, Error, NativeWatcher, RescanWatcher, Snapshot, Watcher};

const WAIT: Duration = Duration::from_secs(10);

fn created(path: &str, kind: EntryKind) -> Change {
    Change::Created {
        path: path.into(),
        kind,
    }
}

fn modified(path: &str) -> Change {
    Change::Modified { path: path.into() }
}

fn renamed(from: &str, to: &str, kind: EntryKind) -> Change {
    Change::Renamed {
        from: from.into(),
        to: to.into(),
        kind,
    }
}

fn removed(path: &str, kind: EntryKind) -> Change {
    Change::Removed {
        path: path.into(),
        kind,
    }
}

use EntryKind::{Dir, File};

/// Builds a tree, snapshots it, then changes it with no watcher running.
fn tree_changed_offline() -> (tempfile::TempDir, Snapshot, Vec<Change>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (path, body) in [
        ("keep.txt", "same"),
        ("edit.txt", "old"),
        ("move-me.txt", "m"),
        ("gone.txt", "g"),
        ("docs/a.txt", "a"),
        ("docs/deep/b.txt", "b"),
        ("trash/x.txt", "x"),
        ("trash/sub/y.txt", "y"),
    ] {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    let previous = Snapshot::scan(root).unwrap();

    fs::write(root.join("new.txt"), "n").unwrap();
    fs::create_dir(root.join("newdir")).unwrap();
    fs::write(root.join("newdir/inner.txt"), "i").unwrap();
    fs::write(root.join("edit.txt"), "changed content").unwrap();
    fs::rename(root.join("move-me.txt"), root.join("moved.txt")).unwrap();
    fs::rename(root.join("docs"), root.join("papers")).unwrap();
    fs::remove_file(root.join("gone.txt")).unwrap();
    fs::remove_dir_all(root.join("trash")).unwrap();

    let expected = vec![
        removed("gone.txt", File),
        removed("trash", Dir),
        renamed("docs", "papers", Dir),
        renamed("move-me.txt", "moved.txt", File),
        created("new.txt", File),
        created("newdir", Dir),
        created("newdir/inner.txt", File),
        modified("edit.txt"),
    ];
    (dir, previous, expected)
}

#[test]
fn rescan_watcher_catches_changes_made_while_nothing_watched() {
    let (dir, previous, expected) = tree_changed_offline();
    let mut watcher = RescanWatcher::new(dir.path(), previous, Duration::from_millis(20)).unwrap();
    assert_eq!(watcher.poll(WAIT).unwrap(), expected);
    assert_eq!(watcher.poll(Duration::from_millis(100)).unwrap(), []);
    assert_eq!(watcher.snapshot(), &Snapshot::scan(dir.path()).unwrap());
}

#[test]
fn native_watcher_catches_changes_made_while_nothing_watched() {
    let (dir, previous, expected) = tree_changed_offline();
    let mut watcher = NativeWatcher::start(dir.path(), previous).unwrap();
    assert_eq!(watcher.poll(WAIT).unwrap(), expected);
    assert_eq!(watcher.poll(Duration::from_millis(300)).unwrap(), []);
    assert_eq!(watcher.snapshot(), &Snapshot::scan(dir.path()).unwrap());
}

/// Polls until every expected change arrived, then a little longer for
/// stragglers. A created file may also show up as modified when its write
/// lands in a later batch than its creation; nothing else may appear.
fn expect(watcher: &mut dyn Watcher, expected: &[Change]) {
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    while !expected.iter().all(|change| seen.contains(change)) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected:?}; saw {seen:?}"
        );
        seen.extend(watcher.poll(Duration::from_millis(500)).unwrap());
    }
    seen.extend(watcher.poll(Duration::from_millis(300)).unwrap());
    let unexpected: Vec<_> = seen
        .iter()
        .filter(|change| !expected.contains(change))
        .filter(|change| match change {
            Change::Modified { path } => !expected.contains(&Change::Created {
                path: path.clone(),
                kind: File,
            }),
            _ => true,
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "unexpected {unexpected:?} while waiting for {expected:?}"
    );
}

fn live_changes(watcher: &mut dyn Watcher, root: &Path) {
    assert_eq!(watcher.poll(Duration::from_millis(300)).unwrap(), []);

    fs::write(root.join("a.txt"), "one").unwrap();
    expect(watcher, &[created("a.txt", File)]);

    fs::write(root.join("a.txt"), "one two").unwrap();
    expect(watcher, &[modified("a.txt")]);

    fs::set_permissions(root.join("a.txt"), fs::Permissions::from_mode(0o755)).unwrap();
    expect(watcher, &[modified("a.txt")]);

    fs::rename(root.join("a.txt"), root.join("b.txt")).unwrap();
    expect(watcher, &[renamed("a.txt", "b.txt", File)]);

    fs::remove_file(root.join("b.txt")).unwrap();
    expect(watcher, &[removed("b.txt", File)]);

    fs::create_dir(root.join("dir")).unwrap();
    expect(watcher, &[created("dir", Dir)]);

    fs::write(root.join("dir/inner.txt"), "inner").unwrap();
    expect(watcher, &[created("dir/inner.txt", File)]);

    fs::create_dir_all(root.join("dir/sub/leaf")).unwrap();
    expect(
        watcher,
        &[created("dir/sub", Dir), created("dir/sub/leaf", Dir)],
    );

    fs::rename(root.join("dir"), root.join("dir2")).unwrap();
    expect(watcher, &[renamed("dir", "dir2", Dir)]);

    fs::rename(root.join("dir2/inner.txt"), root.join("dir2/sub/inner.txt")).unwrap();
    expect(
        watcher,
        &[renamed("dir2/inner.txt", "dir2/sub/inner.txt", File)],
    );

    fs::remove_dir_all(root.join("dir2")).unwrap();
    expect(watcher, &[removed("dir2", Dir)]);

    assert_eq!(watcher.snapshot(), &Snapshot::scan(root).unwrap());
}

#[test]
fn native_watcher_reports_live_changes_to_files_and_folders() {
    let dir = tempfile::tempdir().unwrap();
    let mut watcher = NativeWatcher::start(dir.path(), Snapshot::default()).unwrap();
    live_changes(&mut watcher, dir.path());
}

#[test]
fn rescan_watcher_reports_live_changes_to_files_and_folders() {
    let dir = tempfile::tempdir().unwrap();
    let mut watcher =
        RescanWatcher::new(dir.path(), Snapshot::default(), Duration::from_millis(20)).unwrap();
    live_changes(&mut watcher, dir.path());
}

#[test]
fn replacing_a_file_reports_a_modification() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("doc.txt"), "draft").unwrap();
    fs::write(root.join("thing"), "file").unwrap();
    let mut snapshot = Snapshot::scan(root).unwrap();

    fs::write(root.join(".doc.txt.tmp"), "final").unwrap();
    fs::rename(root.join(".doc.txt.tmp"), root.join("doc.txt")).unwrap();
    fs::remove_file(root.join("thing")).unwrap();
    fs::create_dir(root.join("thing")).unwrap();

    let changes = snapshot.rescan(root, [PathBuf::new()]).unwrap();
    assert_eq!(
        changes,
        [
            removed("thing", File),
            created("thing", Dir),
            modified("doc.txt"),
        ]
    );
}

#[test]
fn rescan_refuses_a_missing_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.txt"), "a").unwrap();
    let mut snapshot = Snapshot::scan(&root).unwrap();
    fs::remove_dir_all(&root).unwrap();

    let before = snapshot.clone();
    assert!(matches!(
        snapshot.rescan(&root, [PathBuf::new()]),
        Err(Error::Io(_))
    ));
    assert_eq!(snapshot, before);
}
