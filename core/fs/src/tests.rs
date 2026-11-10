// SPDX-License-Identifier: Apache-2.0

//! Tests that need to interrupt a scan at a fixed point or to take coverage
//! away from the platform watcher.

use std::fs;

use notify::Watcher as _;
use notify::event::Flag;

use super::*;

const WAIT: Duration = Duration::from_secs(10);

/// Whether the permission change a test just made blocks reads, given the read
/// `probe` that should now fail. Only root reads through it; the test is then
/// skipped, with a line written straight to stderr so that it shows up even
/// when test output is captured. Anyone else fails the test.
fn blocks_reads<T: std::fmt::Debug>(test: &str, probe: std::io::Result<T>) -> bool {
    use std::io::Write;

    match probe {
        Err(err) => {
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{test}: {err}"
            );
            true
        }
        Ok(found) => {
            assert!(
                rustix::process::geteuid().is_root(),
                "{test}: the permission change did not block reads ({found:?})"
            );
            let _ = writeln!(
                std::io::stderr(),
                "SKIPPED {test}: running as root, so permissions do not block reads"
            );
            false
        }
    }
}

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
        let result = snapshot.rescan_with(&root, paths, None, &mut |cut| {
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
        let result = snapshot.rescan_with(&root, paths, None, &mut |cut| {
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
    let result = snapshot.rescan_with(&root, [PathBuf::new()], None, &mut |cut| {
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
        let result = snapshot.rescan_with(&root, [PathBuf::new()], None, &mut |cut| {
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

/// The reviewer's schedule: after `d` was opened and checked, it moves out of
/// the root, an unreadable folder (or a symlink) takes its place with the
/// file still in it, and the file disappears from the detached original. The
/// scan read the detached copy, so it must publish nothing.
#[test]
fn ancestor_replaced_after_opening_publishes_nothing() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("z-victim"), "outside").unwrap();
    let scopes = [
        vec![PathBuf::new()],
        vec![PathBuf::from("d/a-trigger"), PathBuf::from("d/z-victim")],
    ];
    for replacement in ["unreadable", "symlink"] {
        for paths in scopes.clone() {
            fs::create_dir_all(root.join("d/a-trigger")).unwrap();
            fs::write(root.join("d/z-victim"), "user content").unwrap();
            let detached = dir.path().join("detached-d");
            let mut snapshot = Snapshot::scan(&root).unwrap();
            let before = snapshot.clone();

            let result = snapshot.rescan_with(&root, paths.clone(), None, &mut |cut| {
                if let Cut::OpeningDir(path) = cut
                    && path == Path::new("d/a-trigger")
                {
                    fs::rename(root.join("d"), &detached).unwrap();
                    match replacement {
                        "unreadable" => {
                            fs::create_dir(root.join("d")).unwrap();
                            fs::write(root.join("d/z-victim"), "new user content").unwrap();
                            fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o000))
                                .unwrap();
                        }
                        _ => std::os::unix::fs::symlink(&outside, root.join("d")).unwrap(),
                    }
                    fs::remove_file(detached.join("z-victim")).unwrap();
                }
            });

            assert!(
                matches!(result, Err(Error::Changed)),
                "{replacement} {paths:?}: {result:?}"
            );
            assert_eq!(snapshot, before, "{replacement} {paths:?}");
            if replacement == "unreadable" {
                fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o755)).unwrap();
            }
            fs::remove_dir_all(&root).unwrap();
            fs::remove_dir_all(&detached).unwrap();
        }
    }
}

/// The reviewer's ABA schedule: `d` is swapped for an unreadable folder while
/// the scan reads `m-victim` as missing from the detached original, then the
/// original gets `m-victim` back and returns to the root. Every folder identity
/// matches at publication, but the file exists, so its removal is not published
/// and it is handed back for another rescan.
#[test]
fn ancestor_swapped_out_and_back_publishes_no_removal() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let detached = dir.path().join("detached-d");
    let replacement = dir.path().join("replacement-d");
    let scopes = [
        vec![PathBuf::new()],
        vec![
            PathBuf::from("d/a-trigger"),
            PathBuf::from("d/m-victim"),
            PathBuf::from("d/z-trigger"),
        ],
    ];
    for paths in scopes {
        fs::create_dir_all(root.join("d/a-trigger")).unwrap();
        fs::create_dir_all(root.join("d/z-trigger")).unwrap();
        fs::write(root.join("d/m-victim"), "user content").unwrap();
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let before = snapshot.clone();

        let result = snapshot.rescan_with(&root, paths.clone(), None, &mut |cut| {
            let Cut::OpeningDir(path) = cut else { return };
            if path == Path::new("d/a-trigger") {
                fs::rename(root.join("d"), &detached).unwrap();
                fs::create_dir(root.join("d")).unwrap();
                fs::write(root.join("d/m-victim"), "user content").unwrap();
                fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o000)).unwrap();
                fs::remove_file(detached.join("m-victim")).unwrap();
            } else if path == Path::new("d/z-trigger") {
                fs::write(detached.join("m-victim"), "user content").unwrap();
                fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o755)).unwrap();
                fs::rename(root.join("d"), &replacement).unwrap();
                fs::rename(&detached, root.join("d")).unwrap();
            }
        });

        let rescan = result.unwrap_or_else(|err| panic!("{paths:?}: {err:?}"));
        assert_eq!(rescan.changes, [], "{paths:?}");
        assert_eq!(rescan.unsettled, [PathBuf::from("d/m-victim")], "{paths:?}");
        assert_eq!(snapshot, before, "{paths:?}");
        assert!(root.join("d/m-victim").exists());
        fs::remove_dir_all(&replacement).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }
}

/// A removal's parent is swapped for a folder that holds the path while the
/// confirmation looks through the original, so the confirmation sees the path
/// gone. The final look from a fresh root finds another folder there, so the
/// removal is not published and the path is handed back for another rescan.
#[test]
fn parent_swapped_during_confirmation_publishes_no_removal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let detached = dir.path().join("detached-d");
    for paths in [vec![PathBuf::new()], vec![PathBuf::from("d/sub/victim")]] {
        fs::create_dir_all(root.join("d/sub")).unwrap();
        fs::write(root.join("d/sub/victim"), "user content").unwrap();
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let before = snapshot.clone();
        fs::remove_file(root.join("d/sub/victim")).unwrap();

        let mut swapped = false;
        let result = snapshot.rescan_with(&root, paths.clone(), None, &mut |cut| {
            let at_d = matches!(cut, Cut::Confirming(path) if path == Path::new("d"));
            if swapped || !at_d {
                return;
            }
            swapped = true;
            fs::rename(root.join("d"), &detached).unwrap();
            fs::create_dir_all(root.join("d/sub")).unwrap();
            fs::write(root.join("d/sub/victim"), "user content").unwrap();
        });

        assert!(swapped, "{paths:?}");
        let rescan = result.unwrap_or_else(|err| panic!("{paths:?}: {err:?}"));
        assert_eq!(rescan.changes, [], "{paths:?}");
        assert_eq!(
            rescan.unsettled,
            [PathBuf::from("d/sub/victim")],
            "{paths:?}"
        );
        assert_eq!(snapshot, before, "{paths:?}");
        assert!(root.join("d/sub/victim").exists());
        fs::remove_dir_all(&detached).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }
}

/// Siblings are confirmed against one listing of their parent. A sibling
/// recreated after that listing, here while another parent is looked at, is
/// not published as removed: the final look sees the parent changed and hands
/// back every path confirmed against it. A rescan of those paths settles them.
#[test]
fn sibling_recreated_after_its_parent_was_listed_publishes_no_removal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let files = ["d/a-first", "d/m/y", "d/z-victim"];
    let scopes = [
        vec![PathBuf::new()],
        files.iter().map(PathBuf::from).collect(),
    ];
    for paths in scopes {
        fs::create_dir_all(root.join("d/m")).unwrap();
        for file in files {
            fs::write(root.join(file), "user content").unwrap();
        }
        let mut snapshot = Snapshot::scan(&root).unwrap();
        let mut expected = snapshot.clone();
        expected.entries.remove(Path::new("d/m/y"));
        for file in files {
            fs::remove_file(root.join(file)).unwrap();
        }

        let mut recreated = false;
        let result = snapshot.rescan_with(&root, paths.clone(), None, &mut |cut| {
            let at_m = matches!(cut, Cut::Confirming(path) if path == Path::new("d/m"));
            if recreated || !at_m {
                return;
            }
            recreated = true;
            fs::write(root.join("d/z-victim"), "recreated by the user").unwrap();
        });

        assert!(recreated, "{paths:?}");
        let rescan = result.unwrap_or_else(|err| panic!("{paths:?}: {err:?}"));
        assert_eq!(
            rescan.changes,
            [Change::Removed {
                path: PathBuf::from("d/m/y"),
                kind: EntryKind::File
            }],
            "{paths:?}"
        );
        assert_eq!(
            rescan.unsettled,
            [PathBuf::from("d/a-first"), PathBuf::from("d/z-victim")],
            "{paths:?}"
        );
        assert_eq!(snapshot, expected, "{paths:?}");

        let settled = snapshot.rescan(&root, rescan.unsettled).unwrap();
        assert_eq!(
            settled,
            Rescan {
                changes: vec![
                    Change::Removed {
                        path: PathBuf::from("d/a-first"),
                        kind: EntryKind::File
                    },
                    Change::Modified {
                        path: PathBuf::from("d/z-victim")
                    },
                ],
                unsettled: vec![],
                unreadable: vec![],
                root: root_id(&root),
                rules: vec![],
            },
            "{paths:?}"
        );
        assert_eq!(snapshot, Snapshot::scan(&root).unwrap(), "{paths:?}");
        fs::remove_dir_all(&root).unwrap();
    }
}

/// The scans of a poll found an unreadable folder and nothing else happened,
/// so the poll ends on its timeout. It must still report the folder.
#[test]
fn idle_native_poll_reports_an_unreadable_folder() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let locked = root.join("d/locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(locked.join("x"), "x").unwrap();
    let snapshot = Snapshot::scan(root).unwrap();
    let mut watcher = NativeWatcher::start(root, snapshot).unwrap();
    let watched = watcher.root.clone();
    watcher.watcher.unwatch(&watched).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let probe = fs::symlink_metadata(locked.join("x"));
    if !blocks_reads("idle_native_poll_reports_an_unreadable_folder", probe) {
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let first = watcher.poll(Duration::from_millis(200));
    let second = watcher.poll(Duration::from_millis(200));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        first.unwrap(),
        Poll {
            unreadable: vec![PathBuf::from("d/locked")],
            root: Some(root_id(root)),
            ..Poll::default()
        }
    );
    assert_eq!(second.unwrap(), Poll::default());
}

/// A watcher that records each registration and fails the ones a test names,
/// the way notify's backends fail them.
/// Makes a registration of the folder it is given fail.
type Fail = fn(&Path) -> notify::Error;

/// A watcher that registers folders the way notify's inotify backend does: a
/// recursive registration walks the tree and registers one folder at a time,
/// stopping at the first folder that fails, and the folders before it stay
/// registered.
#[derive(Default)]
struct Registrations {
    fail: Vec<(PathBuf, Fail)>,
    watched: BTreeSet<PathBuf>,
}

impl Registrations {
    fn register(&mut self, path: &Path) -> notify::Result<()> {
        if let Some((_, error)) = self.fail.iter().find(|(at, _)| at == path) {
            return Err(error(path));
        }
        self.watched.insert(path.to_path_buf());
        Ok(())
    }

    fn register_tree(&mut self, path: &Path) -> notify::Result<()> {
        self.register(path)?;
        let mut folders: Vec<PathBuf> = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .collect();
        folders.sort();
        folders
            .iter()
            .try_for_each(|folder| self.register_tree(folder))
    }
}

impl notify::Watcher for Registrations {
    fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
        Ok(Self::default())
    }

    fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
        match mode {
            RecursiveMode::Recursive => self.register_tree(path),
            RecursiveMode::NonRecursive => self.register(path),
        }
    }

    fn unwatch(&mut self, _: &Path) -> notify::Result<()> {
        Ok(())
    }

    fn kind() -> notify::WatcherKind {
        notify::WatcherKind::NullWatcher
    }
}

/// The reviewer's registration probe: a recursive registration can fail on a
/// folder deep inside the one it was asked for, here `a/gone` inside `a`, which
/// disappears while it is registered. The rest of `a` still needs its watches,
/// whether the registration first fell back because of an unreadable folder or
/// because of that failure itself. Only folders that may not be watched or are
/// gone stay unwatched. A not-found error for a folder that is still there or
/// that cannot be looked up because its parent may no longer be searched, a
/// requested folder that is gone, and any other error, such as the watch limit,
/// are returned.
#[test]
fn registration_covers_the_tree_around_a_failed_folder() {
    use std::os::unix::fs::PermissionsExt;

    let denied: Fail = |_| notify::Error::io(io::Error::from(io::ErrorKind::PermissionDenied));
    let vanishing: Fail = |path| {
        fs::remove_dir_all(path).unwrap();
        notify::Error::new(notify::ErrorKind::PathNotFound)
    };
    let not_found: Fail = |_| notify::Error::new(notify::ErrorKind::PathNotFound);
    let full: Fail = |_| notify::Error::new(notify::ErrorKind::MaxFilesWatch);
    let unsearchable: Fail = |path| {
        let parent = path.parent().unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o000)).unwrap();
        notify::Error::new(notify::ErrorKind::PathNotFound)
    };
    let register = |fail: &[(&str, Fail)]| {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        for folder in ["0-locked", "a/gone", "a/still/deep", "b"] {
            fs::create_dir_all(root.join(folder)).unwrap();
        }
        let mut watcher = Registrations {
            fail: fail
                .iter()
                .map(|(name, error)| (root.join(name), *error))
                .collect(),
            ..Registrations::default()
        };
        let result = watch_tree(&mut watcher, &root);
        let _ = fs::set_permissions(root.join("a"), fs::Permissions::from_mode(0o755));
        let watched: BTreeSet<String> = watcher
            .watched
            .iter()
            .map(|path| path.strip_prefix(&root).unwrap().display().to_string())
            .collect();
        (result, watched)
    };
    let names = |names: &[&str]| -> BTreeSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    };

    let (result, watched) = register(&[("0-locked", denied), ("a/gone", vanishing)]);
    result.unwrap();
    assert_eq!(watched, names(&["", "a", "a/still", "a/still/deep", "b"]));

    let (result, watched) = register(&[("a/gone", vanishing)]);
    result.unwrap();
    assert_eq!(
        watched,
        names(&["", "0-locked", "a", "a/still", "a/still/deep", "b"])
    );

    for fail in [
        &[("0-locked", denied), ("a/gone", not_found)][..],
        &[("a/gone", not_found)][..],
        &[("0-locked", denied), ("a/still", unsearchable)][..],
    ] {
        let (result, _) = register(fail);
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, notify::ErrorKind::PathNotFound),
            "{err:?}"
        );
    }

    let (result, _) = register(&[("", vanishing)]);
    let err = result.unwrap_err();
    assert!(
        matches!(err.kind, notify::ErrorKind::PathNotFound),
        "{err:?}"
    );

    let (result, _) = register(&[("0-locked", denied), ("b", full)]);
    let err = result.unwrap_err();
    assert!(
        matches!(err.kind, notify::ErrorKind::MaxFilesWatch),
        "{err:?}"
    );
}

/// The reviewer's lookup probe: a folder behind a parent that may not be
/// searched cannot be looked up, which says nothing about whether it is gone.
/// Only a lookup that finds nothing, or finds something other than a folder,
/// does.
#[test]
fn folder_behind_an_unsearchable_parent_is_not_gone() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("a");
    let child = parent.join("child");
    fs::create_dir_all(&child).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).unwrap();
    let blocked = blocks_reads(
        "folder_behind_an_unsearchable_parent_is_not_gone",
        fs::symlink_metadata(&child),
    );
    let unknown = gone(&child);
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    if blocked {
        assert!(!unknown);
    }
    assert!(!gone(&child));
    fs::remove_dir(&child).unwrap();
    assert!(gone(&child));
    fs::write(&child, "f").unwrap();
    assert!(gone(&child));
    assert!(gone(&child.join("below")));
}

fn wait_for(watcher: &mut NativeWatcher, expected: &[Change]) -> Vec<Change> {
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    while !expected.iter().all(|change| seen.contains(change)) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected:?}; saw {seen:?}"
        );
        seen.extend(watcher.poll(Duration::from_millis(500)).unwrap().changes);
    }
    seen
}

fn created(path: &str, kind: EntryKind) -> Change {
    Change::Created {
        path: path.into(),
        kind,
    }
}

/// The identity of the folder at `root`.
fn root_id(root: &Path) -> FileId {
    stat_fd(open_root(root).unwrap().as_fd()).unwrap().id
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
    assert_eq!(
        Poll {
            root: None,
            ..watcher.poll(Duration::from_millis(500)).unwrap()
        },
        Poll::default()
    );

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
    assert_eq!(
        watcher.poll(Duration::from_millis(300)).unwrap(),
        Poll {
            root: Some(root_id(&root)),
            ..Poll::default()
        }
    );

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
    assert_eq!(
        watcher.poll(Duration::from_millis(300)).unwrap(),
        Poll {
            root: Some(root_id(&root)),
            ..Poll::default()
        }
    );

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
    assert_eq!(
        Poll {
            root: None,
            ..watcher.poll(Duration::from_millis(300)).unwrap()
        },
        Poll::default()
    );
}

/// The rescan that registers a new folder reaches another root than the poll
/// scanned first. The poll reports only the changes from its first root, and
/// the watcher fails instead of reporting the other root's.
#[cfg(target_os = "linux")]
#[test]
fn a_new_folder_rescan_that_reaches_another_root_fails_the_watcher() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("space");
    fs::create_dir(&root).unwrap();
    let first = root_id(&root);
    let mut watcher = NativeWatcher::start(&root, Snapshot::default()).unwrap();
    assert_eq!(
        watcher.poll(Duration::from_millis(300)).unwrap(),
        Poll {
            root: Some(first),
            ..Poll::default()
        }
    );

    let (path, moved) = (root.clone(), dir.path().join("moved"));
    watcher.before_register = Some(Box::new(move || {
        fs::rename(&path, &moved).unwrap();
        fs::create_dir_all(path.join("sub")).unwrap();
        fs::write(path.join("sub").join("theirs"), b"replacement").unwrap();
    }));
    fs::create_dir(root.join("sub")).unwrap();
    let poll = watcher.poll(WAIT).unwrap();
    assert_eq!(poll.changes, [created("sub", EntryKind::Dir)]);
    assert_eq!(poll.root, Some(first));
    assert_ne!(root_id(&root), first);
    assert!(matches!(
        watcher.poll(Duration::from_millis(300)),
        Err(Error::RootReplaced)
    ));
    assert!(
        !watcher
            .snapshot()
            .entries
            .contains_key(Path::new("sub/theirs")),
        "the snapshot took entries from the replacement root"
    );
}

#[test]
fn a_rescan_expecting_a_root_born_at_another_time_changes_nothing() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("theirs"), b"replacement").unwrap();
    let found = root_id(&root);
    let Some(born) = found.birth_ns else {
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED a_rescan_expecting_a_root_born_at_another_time_changes_nothing: no birth time"
        );
        return;
    };
    let reused = FileId {
        birth_ns: Some(born - 1),
        ..found
    };
    let mut snapshot = Snapshot::default();
    let result = snapshot.rescan_with(&root, [PathBuf::new()], Some(reused), &mut |_| {});
    assert!(matches!(result, Err(Error::RootReplaced)), "{result:?}");
    assert!(snapshot.entries.is_empty());
}

#[test]
fn utc_dates_follow_the_calendar() {
    let date = |secs: i64| {
        let time = if secs < 0 {
            std::time::UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
        } else {
            std::time::UNIX_EPOCH + Duration::from_secs(secs as u64)
        };
        recovered::utc_date(time)
    };
    assert_eq!(date(0), "1970-01-01");
    assert_eq!(date(-1), "1969-12-31");
    assert_eq!(date(951_782_400), "2000-02-29");
    assert_eq!(date(951_868_800), "2000-03-01");
    assert_eq!(date(4_107_456_000), "2100-02-28");
    assert_eq!(date(4_107_542_400), "2100-03-01");
}

#[test]
fn a_recovered_name_is_not_given_in_a_folder_moved_out_of_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir(&outside).unwrap();
    let space = Space::bind(open_root(&root).unwrap(), &root).unwrap();
    let held = space.private(&["held"]).unwrap();
    fs::write(root.join(PRIVATE_DIR).join("held/e1"), "kept").unwrap();
    let id = Snapshot::scan(&root).unwrap().entries[Path::new("docs")].id;
    let docs = space
        .folder(
            &Base::Root,
            &[Dir {
                name: "docs".into(),
                id,
            }],
        )
        .unwrap();
    fs::rename(root.join("docs"), outside.join("docs")).unwrap();

    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let original = std::ffi::OsStr::new("report.txt");
    let path = space
        .move_to_recovered_name(
            &held,
            std::ffi::OsStr::new("e1"),
            &docs,
            original,
            false,
            now,
        )
        .unwrap();

    let name = recovered::recovered_name(original, false, "2023-11-14", 1);
    assert_eq!(path, PathBuf::from(&name));
    assert_eq!(fs::read(root.join(&name)).unwrap(), b"kept");
    assert_eq!(fs::read_dir(outside.join("docs")).unwrap().count(), 0);
}

#[test]
fn nothing_is_recovered_from_a_folder_moved_out_of_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    let space = Space::bind(open_root(&root).unwrap(), &root).unwrap();
    let held = space.private(&["held"]).unwrap();
    fs::write(root.join(PRIVATE_DIR).join("held/e1"), "kept").unwrap();
    fs::rename(root.join(PRIVATE_DIR).join("held"), outside.join("held")).unwrap();

    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let to = space.root_folder().unwrap();
    let original = std::ffi::OsStr::new("report.txt");
    space
        .move_to_recovered_name(&held, std::ffi::OsStr::new("e1"), &to, original, false, now)
        .unwrap_err();

    assert_eq!(fs::read(outside.join("held/e1")).unwrap(), b"kept");
    let names: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, [PRIVATE_DIR]);
}

/// A matcher whose rules files name, one per line, the entries their folder
/// leaves out. It shares the rules it was given with the test.
#[derive(Clone, Default)]
struct Lines(std::sync::Arc<std::sync::Mutex<BTreeMap<PathBuf, Vec<u8>>>>);

impl Ignore for Lines {
    fn set_rules(&mut self, dir: &Path, rules: Option<&[u8]>) {
        let mut given = self.0.lock().unwrap();
        match rules {
            Some(rules) => given.insert(dir.to_path_buf(), rules.to_vec()),
            None => given.remove(dir),
        };
    }

    fn status(&self, path: &Path, _: EntryKind) -> Ignored {
        let given = self.0.lock().unwrap();
        let mut dir = PathBuf::new();
        for name in path {
            let left_out = given.get(&dir).is_some_and(|rules| {
                rules
                    .split(|byte| *byte == b'\n')
                    .any(|line| line == name.as_bytes())
            });
            if left_out {
                return Ignored::Rule;
            }
            dir.push(name);
        }
        Ignored::No
    }
}

/// The scan reads the changed rules of the root and of `a` and then fails in
/// `m`. Both rules files must count as unread, for the matcher and for the
/// next scan, which reports them.
#[test]
fn a_failed_scan_leaves_the_rules_as_they_were() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    for (path, body) in [
        (".fbignore", "ig\n"),
        ("a/.fbignore", "long-name\n"),
        ("a/long-name", "x"),
        ("a/y", "y"),
        ("ig/f", "f"),
        ("m/f", "f"),
        ("z/f", "f"),
    ] {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    let matcher = Lines::default();
    let mut rules = Rules::new(Box::new(matcher.clone()));
    let mut snapshot = Snapshot::scan_with_rules(&root, &mut rules).unwrap();
    let before = snapshot.clone();
    let given = matcher.0.lock().unwrap().clone();
    assert_eq!(given.len(), 2);

    fs::write(root.join(".fbignore"), "z\n").unwrap();
    fs::write(root.join("a/.fbignore"), "y\n").unwrap();
    let result = snapshot.rescan_under(
        &root,
        [PathBuf::new()],
        None,
        Some(&mut rules),
        &mut |cut| {
            if let Cut::OpeningDir(path) = cut
                && path == Path::new("m")
            {
                assert_ne!(*matcher.0.lock().unwrap(), given);
                fs::rename(root.join("m"), root.join("m-old")).unwrap();
                fs::create_dir(root.join("m")).unwrap();
            }
        },
    );
    assert!(matches!(result, Err(Error::Changed)), "{result:?}");
    assert_eq!(snapshot, before);
    assert_eq!(*matcher.0.lock().unwrap(), given);
    assert_eq!(rules.status(Path::new("ig"), EntryKind::Dir), Ignored::Rule);
    assert_eq!(rules.status(Path::new("z"), EntryKind::Dir), Ignored::No);

    fs::remove_dir(root.join("m")).unwrap();
    fs::rename(root.join("m-old"), root.join("m")).unwrap();
    let rescan = snapshot
        .rescan_with_rules(&root, [PathBuf::new()], &mut rules)
        .unwrap();
    assert_eq!(
        rescan.rules,
        [
            (PathBuf::new(), Some(b"z\n".to_vec())),
            (PathBuf::from("a"), Some(b"y\n".to_vec())),
        ]
    );
    assert_eq!(
        rescan.changes,
        [
            created("a/long-name", EntryKind::File),
            created("ig", EntryKind::Dir),
            created("ig/f", EntryKind::File),
            Change::Modified {
                path: ".fbignore".into()
            },
            Change::Modified {
                path: "a/.fbignore".into()
            },
        ]
    );
}
