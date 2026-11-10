// SPDX-License-Identifier: Apache-2.0

//! Fixtures shared by the effect tests: the effects of design §2.5 on small
//! trees, and what the disk must hold once each has been applied or
//! recovered.

#![allow(dead_code)]

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use flarebox_fs::{
    Action, Base, Contents, Dir, Displaced, Effect, Entry, EntryKind, FileId, Location, Marker,
    Outcome, PRIVATE_DIR, QUARANTINE_DIR, STAGING_DIR, Snapshot, Space, Step, TRASH_DIR,
    read_marker,
};

pub const SPACE: &str = "space-1";
pub const COUNTER: u64 = 5;
pub const ID: &str = "e1";
pub const SYNCED: &[u8] = b"synced";
pub const REMOTE: &[u8] = b"remote";
/// 2023-11-14T22:13:20Z.
pub const NOW: u64 = 1_700_000_000;

pub fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW)
}

pub fn marker() -> Marker {
    Marker {
        space_id: SPACE.into(),
        effects: COUNTER,
    }
}

/// Compares files with the synced content an effect displaces and the
/// remote content it writes.
pub struct Bytes;

fn read(mut file: &File) -> Vec<u8> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    bytes
}

impl Contents for Bytes {
    fn displaced(&mut self, file: &File) -> std::io::Result<bool> {
        Ok(read(file) == SYNCED)
    }

    fn written(&mut self, file: &File) -> std::io::Result<bool> {
        Ok(read(file) == REMOTE)
    }
}

pub fn entries(root: &Path, paths: &[&str]) -> Vec<Entry> {
    let snapshot = Snapshot::scan(root).unwrap();
    paths
        .iter()
        .map(|path| snapshot.entries[Path::new(path)])
        .collect()
}

pub fn dir(name: &str, entry: &Entry) -> Dir {
    Dir {
        name: name.into(),
        id: entry.id,
    }
}

pub fn at(dirs: Vec<Dir>, name: &str) -> Location {
    Location::At {
        base: Base::Root,
        dirs,
        name: name.into(),
    }
}

pub fn stage(space: &Space, bytes: &[u8]) {
    let mut file = space.stage(ID).unwrap();
    file.write_all(bytes).unwrap();
    drop(file);
    space.finish_staged(ID, false).unwrap();
}

/// The names in `.flarebox/<folder>`, or none when it does not exist.
pub fn private_names(root: &Path, folder: &str) -> Vec<String> {
    let Ok(listing) = fs::read_dir(root.join(PRIVATE_DIR).join(folder)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = listing
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

pub fn trashed(root: &Path) -> Vec<Vec<u8>> {
    private_names(root, TRASH_DIR)
        .iter()
        .map(|name| fs::read(root.join(PRIVATE_DIR).join(TRASH_DIR).join(name)).unwrap())
        .collect()
}

pub fn contents(path: PathBuf) -> Option<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => panic!("{err}"),
    }
}

/// What every effect leaves once it is over: nothing in quarantine or
/// staging, and the marker at the effect's counter.
pub fn assert_settled(root: &Path) {
    assert_eq!(private_names(root, QUARANTINE_DIR), Vec::<String>::new());
    assert_eq!(private_names(root, STAGING_DIR), Vec::<String>::new());
    assert_eq!(read_marker(root).unwrap(), Some(marker()));
}

/// One effect on a small tree, with the paths whose observed entries it
/// needs, and the step from which recovery after a crash completes it.
pub struct Scenario {
    pub name: &'static str,
    pub paths: &'static [&'static str],
    pub setup: fn(&Path, &Space),
    pub build: fn(&[Entry]) -> Effect,
    pub check: fn(&Path, &Outcome),
    pub done_from: Step,
}

pub const SCENARIOS: [Scenario; 6] = [
    Scenario {
        name: "replace",
        paths: &["docs", "docs/report.txt"],
        setup: |root, space| {
            fs::create_dir(root.join("docs")).unwrap();
            fs::write(root.join("docs/report.txt"), SYNCED).unwrap();
            stage(space, REMOTE);
        },
        build: |entries| {
            let at = at(vec![dir("docs", &entries[0])], "report.txt");
            Effect {
                id: ID.into(),
                action: Action::WriteFile {
                    at: at.clone(),
                    replaces: Some(Displaced {
                        at,
                        entry: entries[1],
                    }),
                },
            }
        },
        check: |root, outcome| {
            let target = contents(root.join("docs/report.txt"));
            if *outcome == Outcome::Done {
                assert_eq!(target.as_deref(), Some(REMOTE));
                assert_eq!(trashed(root), [SYNCED]);
            } else {
                assert_eq!(target.as_deref(), Some(SYNCED));
                assert!(trashed(root).is_empty());
            }
        },
        done_from: Step::Quarantined,
    },
    Scenario {
        name: "write_new",
        paths: &["docs"],
        setup: |root, space| {
            fs::create_dir(root.join("docs")).unwrap();
            stage(space, REMOTE);
        },
        build: |entries| Effect {
            id: ID.into(),
            action: Action::WriteFile {
                at: at(vec![dir("docs", &entries[0])], "new.txt"),
                replaces: None,
            },
        },
        check: |root, outcome| {
            let expected = (*outcome == Outcome::Done).then_some(REMOTE);
            assert_eq!(contents(root.join("docs/new.txt")).as_deref(), expected);
        },
        done_from: Step::Placed,
    },
    Scenario {
        name: "create_dir",
        paths: &["docs"],
        setup: |root, _| fs::create_dir(root.join("docs")).unwrap(),
        build: |entries| Effect {
            id: ID.into(),
            action: Action::CreateDir {
                at: at(vec![dir("docs", &entries[0])], "sub"),
            },
        },
        check: |root, outcome| {
            assert_eq!(root.join("docs/sub").is_dir(), *outcome == Outcome::Done);
        },
        done_from: Step::Placed,
    },
    Scenario {
        name: "move",
        paths: &["docs", "docs/a.txt", "other"],
        setup: |root, _| {
            fs::create_dir_all(root.join("docs")).unwrap();
            fs::create_dir_all(root.join("other")).unwrap();
            fs::write(root.join("docs/a.txt"), SYNCED).unwrap();
        },
        build: |entries| Effect {
            id: ID.into(),
            action: Action::Move {
                from: at(vec![dir("docs", &entries[0])], "a.txt"),
                id: entries[1].id,
                to: at(vec![dir("other", &entries[2])], "b.txt"),
            },
        },
        check: |root, outcome| {
            let (from, to) = (
                contents(root.join("docs/a.txt")),
                contents(root.join("other/b.txt")),
            );
            if *outcome == Outcome::Done {
                assert_eq!((from, to.as_deref()), (None, Some(SYNCED)));
            } else {
                assert_eq!((from.as_deref(), to), (Some(SYNCED), None));
            }
        },
        done_from: Step::Placed,
    },
    Scenario {
        name: "remove_file",
        paths: &["docs", "docs/report.txt"],
        setup: |root, _| {
            fs::create_dir(root.join("docs")).unwrap();
            fs::write(root.join("docs/report.txt"), SYNCED).unwrap();
        },
        build: |entries| Effect {
            id: ID.into(),
            action: Action::RemoveFile {
                displaced: Displaced {
                    at: at(vec![dir("docs", &entries[0])], "report.txt"),
                    entry: entries[1],
                },
            },
        },
        check: |root, outcome| {
            let target = contents(root.join("docs/report.txt"));
            if *outcome == Outcome::Done {
                assert_eq!(target, None);
                assert_eq!(trashed(root), [SYNCED]);
            } else {
                assert_eq!(target.as_deref(), Some(SYNCED));
                assert!(trashed(root).is_empty());
            }
        },
        done_from: Step::Quarantined,
    },
    Scenario {
        name: "remove_dir",
        paths: &["docs", "docs/empty"],
        setup: |root, _| fs::create_dir_all(root.join("docs/empty")).unwrap(),
        build: |entries| Effect {
            id: ID.into(),
            action: Action::RemoveDir {
                at: at(vec![dir("docs", &entries[0])], "empty"),
                id: entries[1].id,
            },
        },
        check: |root, outcome| {
            assert_eq!(root.join("docs/empty").is_dir(), *outcome != Outcome::Done);
            let trashed_dirs = private_names(root, TRASH_DIR).len();
            assert_eq!(trashed_dirs, usize::from(*outcome == Outcome::Done));
        },
        done_from: Step::Quarantined,
    },
];

pub fn scenario(name: &str) -> &'static Scenario {
    SCENARIOS
        .iter()
        .find(|scenario| scenario.name == name)
        .unwrap()
}

/// Sets `scenario` up at `root` and returns its effect.
pub fn prepare(scenario: &Scenario, root: &Path) -> Effect {
    fs::create_dir_all(root).unwrap();
    let space = Space::open(root).unwrap();
    (scenario.setup)(root, &space);
    (scenario.build)(&entries(root, scenario.paths))
}

/// Writes entries as text, one per line, for a child process to rebuild
/// the same effect from.
pub fn encode(entries: &[Entry]) -> String {
    entries
        .iter()
        .map(|entry| {
            format!(
                "{} {} {} {} {} {} {}\n",
                matches!(entry.kind, EntryKind::Dir),
                entry.id.dev,
                entry.id.ino,
                entry
                    .id
                    .birth_ns
                    .map_or("-".to_string(), |ns| ns.to_string()),
                entry.size,
                entry.mtime_ns,
                entry.executable
            )
        })
        .collect()
}

pub fn decode(text: &str) -> Vec<Entry> {
    text.lines()
        .map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            Entry {
                kind: if fields[0] == "true" {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                },
                id: FileId {
                    dev: fields[1].parse().unwrap(),
                    ino: fields[2].parse().unwrap(),
                    birth_ns: (fields[3] != "-").then(|| fields[3].parse().unwrap()),
                },
                size: fields[4].parse().unwrap(),
                mtime_ns: fields[5].parse().unwrap(),
                executable: fields[6] == "true",
            }
        })
        .collect()
}
