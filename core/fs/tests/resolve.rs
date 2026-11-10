// SPDX-License-Identifier: Apache-2.0

//! Resolving effect locations from the root descriptor (design §5.2), on the
//! file system the tests run on.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use flarebox_fs::{
    Base, Dir, FileId, Location, PARKED_DIR, PRIVATE_DIR, Snapshot, Space, Unresolved,
};

fn id(root: &Path, path: &str) -> FileId {
    Snapshot::scan(root).unwrap().entries[Path::new(path)].id
}

fn dir(root: &Path, path: &str) -> Dir {
    Dir {
        name: Path::new(path).file_name().unwrap().into(),
        id: id(root, path),
    }
}

fn at(dirs: Vec<Dir>, name: &str) -> Location {
    Location::At {
        base: Base::Root,
        dirs,
        name: name.into(),
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

/// Asserts the precondition of a permission test. Returns false when the
/// permission change does not block reads because the test runs as root.
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

#[test]
fn location_resolves_through_folders_with_their_identities() {
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    let space = Space::open(root).unwrap();

    let location = at(vec![dir(root, "a"), dir(root, "a/b")], "f.txt");
    let (folder, name) = space.parent(&location).unwrap();
    assert_eq!(name, "f.txt");
    assert_eq!(folder.id(), id(root, "a/b"));
    assert_eq!(folder.path(), Path::new("a/b"));
    space.recheck(&folder).unwrap();

    let top_level = at(Vec::new(), "g");
    let (top, name) = space.parent(&top_level).unwrap();
    assert_eq!(name, "g");
    assert_eq!(top.path(), Path::new(""));
    space.recheck(&top).unwrap();
}

#[test]
fn parent_replaced_by_a_symlink_is_not_followed() {
    let outside = tempfile::tempdir().unwrap();
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    let location = at(vec![dir(root, "a"), dir(root, "a/b")], "f.txt");
    fs::create_dir(outside.path().join("b")).unwrap();
    let before = listing(outside.path());

    fs::rename(root.join("a/b"), root.join("old-b")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("b"), root.join("a/b")).unwrap();
    let space = Space::open(root).unwrap();
    assert!(matches!(
        space.parent(&location),
        Err(Unresolved::NotAFolder(path)) if path == Path::new("a/b")
    ));
    assert_eq!(listing(outside.path()), before);
}

#[test]
fn parent_replaced_by_another_folder_fails_its_identity_check() {
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    let location = at(vec![dir(root, "a"), dir(root, "a/b")], "f.txt");
    let expected = id(root, "a/b");

    fs::rename(root.join("a/b"), root.join("old-b")).unwrap();
    fs::create_dir(root.join("a/b")).unwrap();
    let space = Space::open(root).unwrap();
    match space.parent(&location) {
        Err(Unresolved::Replaced {
            path,
            expected: wanted,
            found,
        }) => {
            assert_eq!(path, Path::new("a/b"));
            assert_eq!(wanted, expected);
            assert_eq!(found, id(root, "a/b"));
        }
        other => panic!("expected a replaced folder, got {other:?}"),
    }
}

#[test]
fn parent_moved_out_of_the_root_fails_the_recheck() {
    let outside = tempfile::tempdir().unwrap();
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    let space = Space::open(root).unwrap();
    let location = at(vec![dir(root, "a"), dir(root, "a/b")], "f.txt");
    let (folder, _) = space.parent(&location).unwrap();

    fs::rename(root.join("a"), outside.path().join("a")).unwrap();
    assert!(matches!(
        space.recheck(&folder),
        Err(Unresolved::Missing(path)) if path == Path::new("a")
    ));

    // Moved back under another name, the route no longer leads to it either.
    fs::rename(outside.path().join("a"), root.join("c")).unwrap();
    assert!(matches!(
        space.recheck(&folder),
        Err(Unresolved::Missing(path)) if path == Path::new("a")
    ));
}

#[test]
fn ancestor_swapped_after_resolution_fails_the_recheck() {
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    let space = Space::open(root).unwrap();
    let location = at(vec![dir(root, "a"), dir(root, "a/b")], "f.txt");
    let (folder, _) = space.parent(&location).unwrap();

    fs::rename(root.join("a"), root.join("old-a")).unwrap();
    fs::create_dir_all(root.join("a/b")).unwrap();
    assert!(matches!(
        space.recheck(&folder),
        Err(Unresolved::Replaced { path, .. }) if path == Path::new("a")
    ));
}

#[test]
fn parked_base_checks_the_parked_folder_identity() {
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    let parked = root.join(PRIVATE_DIR).join(PARKED_DIR);
    fs::create_dir_all(parked.join("node-1/sub")).unwrap();
    fs::create_dir_all(root.join("x")).unwrap();
    let space = Space::open(root).unwrap();
    let node_id = |path: &Path| {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::symlink_metadata(path).unwrap();
        (meta.dev(), meta.ino())
    };

    let parked_scan = Snapshot::scan(&parked).unwrap();
    let node = parked_scan.entries[Path::new("node-1")].id;
    let sub = parked_scan.entries[Path::new("node-1/sub")].id;
    let location = Location::At {
        base: Base::Parked {
            name: "node-1".into(),
            id: node,
        },
        dirs: vec![Dir {
            name: "sub".into(),
            id: sub,
        }],
        name: "f.txt".into(),
    };
    let (folder, name) = space.parent(&location).unwrap();
    assert_eq!(name, "f.txt");
    assert_eq!(
        (folder.id().dev, folder.id().ino),
        node_id(&parked.join("node-1/sub"))
    );
    space.recheck(&folder).unwrap();

    let parked_entry = Location::Parked("node-2".into());
    let (holder, name) = space.parent(&parked_entry).unwrap();
    assert_eq!(name, "node-2");
    assert_eq!(holder.path(), Path::new(".flarebox/parked"));

    let wrong = Location::At {
        base: Base::Parked {
            name: "node-1".into(),
            id: id(root, "x"),
        },
        dirs: Vec::new(),
        name: "f.txt".into(),
    };
    assert!(matches!(
        space.parent(&wrong),
        Err(Unresolved::Replaced { path, .. }) if path == Path::new(".flarebox/parked/node-1")
    ));
}

#[test]
fn names_that_are_not_one_component_are_refused() {
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    fs::create_dir(root.join("a")).unwrap();
    let space = Space::open(root).unwrap();
    for name in ["", ".", "..", "a/b", "/etc", "nul\0"] {
        assert!(
            matches!(
                space.parent(&at(Vec::new(), name)),
                Err(Unresolved::BadName(_))
            ),
            "{name:?}"
        );
        let through = Dir {
            name: OsString::from(name),
            id: id(root, "a"),
        };
        assert!(
            matches!(
                space.parent(&at(vec![through], "f")),
                Err(Unresolved::BadName(_))
            ),
            "{name:?}"
        );
    }
}

#[test]
fn unreadable_parent_is_not_missing() {
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    let location = at(vec![dir(root, "a"), dir(root, "a/b")], "f.txt");
    fs::set_permissions(root.join("a"), fs::Permissions::from_mode(0o000)).unwrap();
    let probe = fs::read_dir(root.join("a/b"));
    if !blocks_reads("unreadable_parent_is_not_missing", probe) {
        fs::set_permissions(root.join("a"), fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let space = Space::open(root).unwrap();
    let resolved = space.parent(&location);
    fs::set_permissions(root.join("a"), fs::Permissions::from_mode(0o755)).unwrap();
    match resolved {
        Err(Unresolved::Io { path, err }) => {
            assert_eq!(path, Path::new("a"));
            assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        }
        other => panic!("expected a permission error, got {other:?}"),
    }
}
