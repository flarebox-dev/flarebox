// SPDX-License-Identifier: Apache-2.0

//! The root marker, the root check and the rename that never replaces its
//! target, on the file system the tests run on.

use std::fs;
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use koidrive_fs::{
    Marker, PRIVATE_DIR, RootProblem, Space, Unresolved, check_root, probe_no_replace, read_marker,
    rename_no_replace, write_marker,
};

fn marker(space_id: &str, effects: u64) -> Marker {
    Marker {
        space_id: space_id.into(),
        effects,
    }
}

fn private_names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root.join(PRIVATE_DIR))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn marker_round_trips_and_replaces_a_stale_temporary_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert_eq!(read_marker(root).unwrap(), None);

    write_marker(root, &marker("space-1", 0)).unwrap();
    assert_eq!(read_marker(root).unwrap(), Some(marker("space-1", 0)));

    fs::write(
        root.join(PRIVATE_DIR).join("space.json.tmp"),
        "left by a crash",
    )
    .unwrap();
    write_marker(root, &marker("space-1", 7)).unwrap();
    assert_eq!(read_marker(root).unwrap(), Some(marker("space-1", 7)));
    assert_eq!(private_names(root), ["space.json"]);
}

#[test]
fn check_root_names_each_problem() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let dev = fs::metadata(dir.path()).unwrap().dev();

    assert!(matches!(
        check_root(&root, "space-1", dev),
        Err(RootProblem::Missing)
    ));
    fs::write(&root, "a file").unwrap();
    assert!(matches!(
        check_root(&root, "space-1", dev),
        Err(RootProblem::NotAFolder)
    ));
    fs::remove_file(&root).unwrap();
    fs::create_dir(&root).unwrap();
    assert!(matches!(
        check_root(&root, "space-1", dev),
        Err(RootProblem::NoMarker)
    ));
    fs::create_dir(root.join(PRIVATE_DIR)).unwrap();
    fs::write(root.join(PRIVATE_DIR).join("space.json"), "{").unwrap();
    assert!(matches!(
        check_root(&root, "space-1", dev),
        Err(RootProblem::BadMarker(_))
    ));
    write_marker(&root, &marker("space-2", 3)).unwrap();
    assert!(matches!(
        check_root(&root, "space-1", dev),
        Err(RootProblem::OtherSpace(id)) if id == "space-2"
    ));
    assert!(matches!(
        check_root(&root, "space-2", dev.wrapping_add(1)),
        Err(RootProblem::OtherDevice { expected, found }) if expected == dev.wrapping_add(1) && found == dev
    ));
    assert_eq!(
        check_root(&root, "space-2", dev).unwrap(),
        marker("space-2", 3)
    );

    fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
    let readable = fs::read_dir(&root).is_ok();
    let result = check_root(&root, "space-2", dev);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    if !readable {
        assert!(
            matches!(result, Err(RootProblem::Unreadable(_))),
            "{result:?}"
        );
    }
}

#[test]
fn marker_behind_a_symlink_is_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let outside = dir.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    write_marker(&outside, &marker("space-1", 0)).unwrap();
    std::os::unix::fs::symlink(outside.join(PRIVATE_DIR), root.join(PRIVATE_DIR)).unwrap();
    let dev = fs::metadata(&root).unwrap().dev();

    assert!(read_marker(&root).is_err());
    assert!(matches!(
        check_root(&root, "space-1", dev),
        Err(RootProblem::BadMarker(_))
    ));
}

#[test]
fn rename_never_replaces_its_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(probe_no_replace(root).unwrap());
    assert!(private_names(root).is_empty());

    fs::write(root.join("a"), "a").unwrap();
    fs::write(root.join("b"), "b").unwrap();
    let here = fs::File::open(root).unwrap();
    let err =
        rename_no_replace(here.as_fd(), "a".as_ref(), here.as_fd(), "b".as_ref()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read_to_string(root.join("a")).unwrap(), "a");
    assert_eq!(fs::read_to_string(root.join("b")).unwrap(), "b");

    rename_no_replace(here.as_fd(), "a".as_ref(), here.as_fd(), "c".as_ref()).unwrap();
    assert!(!root.join("a").exists());
    assert_eq!(fs::read_to_string(root.join("c")).unwrap(), "a");
}

/// A folder on a file system whose rename refuses the no-replace flag, such
/// as a FUSE file system without `RENAME_NOREPLACE`. Linux checks an existing
/// target before the file system sees the flag, so only a rename to a free
/// name shows the gap. CI has no such mount; the variable names one set up in
/// a container.
const NO_REPLACE_FREE_DIR: &str = "KOIDRIVE_TEST_NO_REPLACE_FREE_DIR";

#[test]
fn probe_refuses_a_file_system_whose_rename_lacks_no_replace() {
    use std::io::Write;

    let Some(dir) = std::env::var_os(NO_REPLACE_FREE_DIR) else {
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED probe_refuses_a_file_system_whose_rename_lacks_no_replace: \
             {NO_REPLACE_FREE_DIR} names no folder on such a file system"
        );
        return;
    };
    let root = Path::new(&dir).join(format!("probe-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    assert!(!probe_no_replace(&root).unwrap());
}
#[test]
fn marker_and_probe_go_through_the_held_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let moved = dir.path().join("moved");
    fs::create_dir(&root).unwrap();
    let space = Space::bind(fs::File::open(&root).unwrap().into(), &root).unwrap();
    assert_eq!(space.marker().unwrap(), None);
    assert!(!root.join(PRIVATE_DIR).exists());
    assert!(space.probe_no_replace().unwrap());
    assert!(private_names(&root).is_empty());

    write_marker(&root, &marker("space-1", 3)).unwrap();
    assert_eq!(space.marker().unwrap(), Some(marker("space-1", 3)));
    assert_eq!(private_names(&root), ["space.json"]);

    fs::rename(&root, &moved).unwrap();
    fs::create_dir(&root).unwrap();
    write_marker(&root, &marker("space-2", 9)).unwrap();
    assert_eq!(space.marker().unwrap(), Some(marker("space-1", 3)));
    let err = space.probe_no_replace().unwrap_err();
    assert!(
        matches!(
            err.get_ref()
                .and_then(|err| err.downcast_ref::<Unresolved>()),
            Some(Unresolved::RootWithdrawn(_))
        ),
        "{err}"
    );
    assert_eq!(private_names(&root), ["space.json"]);
    assert_eq!(private_names(&moved), ["space.json"]);
}
