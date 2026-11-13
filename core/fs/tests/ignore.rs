// SPDX-License-Identifier: Apache-2.0

//! Scans and watchers that apply the rules of design §5.10, with the matcher
//! of `koidrive-ignore` over the engine's name key.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use koidrive_fs::{
    Change, EntryKind, FileId, Ignore, Ignored, Known, NativeWatcher, Poll, RULES_FILE,
    RescanWatcher, Rules, Snapshot, Watcher,
};
use koidrive_ignore::{Kind, LocalRules, Matcher, NameKey, Reason, RuleFile, Status};

use EntryKind::{Dir, File};

const WAIT: Duration = Duration::from_secs(10);

struct Folded;

impl NameKey for Folded {
    fn key(name: &str) -> String {
        koidrive_engine::names::name_key(name)
    }
}

struct Matched(Matcher<Folded>);

fn names(path: &Path) -> Vec<String> {
    path.iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

impl Ignore for Matched {
    fn set_rules(&mut self, dir: &Path, rules: Option<&[u8]>) {
        self.0
            .set_rules(&names(dir), rules.map(RuleFile::parse), []);
    }

    fn status(&self, path: &Path, kind: EntryKind) -> Ignored {
        let kind = match kind {
            File => Kind::File,
            Dir => Kind::Dir,
        };
        match self.0.status(&names(path), kind) {
            Status::Included => Ignored::No,
            Status::Excluded(Reason::BuiltIn) => Ignored::BuiltIn,
            Status::Excluded(_) => Ignored::Rule,
        }
    }

    fn key(&self, name: &OsStr) -> OsString {
        Folded::key(&name.to_string_lossy()).into()
    }
}

/// Rules that know no rules file yet, as after a start.
fn rules() -> Rules {
    Rules::new(Box::new(Matched(Matcher::new(LocalRules::default()))))
}

fn write(root: &Path, path: &str, body: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn known(snapshot: &Snapshot) -> Vec<&str> {
    snapshot
        .entries
        .keys()
        .map(|path| path.to_str().unwrap())
        .collect()
}

fn created(path: &str, kind: EntryKind) -> Change {
    Change::Created {
        path: path.into(),
        kind,
    }
}

fn removed(path: &str, kind: EntryKind) -> Change {
    Change::Removed {
        path: path.into(),
        kind,
    }
}

/// The change of rules a scan reports for the folder `dir`.
fn rule(dir: &str, rules: Option<&str>) -> (PathBuf, Option<Vec<u8>>) {
    (dir.into(), rules.map(|rules| rules.as_bytes().to_vec()))
}

fn whole() -> [PathBuf; 1] {
    [PathBuf::new()]
}

#[test]
fn a_root_rule_leaves_out_a_file_and_a_folder_with_everything_below_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, RULES_FILE, "*.log\nbuild/\n");
    write(root, "a.log", "a");
    write(root, "keep.txt", "k");
    write(root, "build/out/x.txt", "x");
    write(root, "src/main.rs", "m");
    write(root, "src/b.log", "b");
    write(root, "src/build/y.txt", "y");

    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        known(&snapshot),
        [RULES_FILE, "keep.txt", "src", "src/main.rs"]
    );
    assert_eq!(
        rescan.changes,
        [
            created(RULES_FILE, File),
            created("keep.txt", File),
            created("src", Dir),
            created("src/main.rs", File),
        ]
    );
    assert_eq!(rescan.rules, [rule("", Some("*.log\nbuild/\n"))]);
    assert_eq!(
        snapshot,
        Snapshot::scan_with_rules(root, &mut self::rules()).unwrap()
    );
    assert_eq!(
        rules.status(Path::new("build/out/x.txt"), File),
        Ignored::Rule
    );
}

#[test]
fn a_nested_rules_file_applies_to_its_folder_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "docs/.koiignore", "draft\n");
    write(root, "docs/draft", "d");
    write(root, "docs/deep/draft/x", "x");
    write(root, "docs/final", "f");
    write(root, "other/draft", "o");

    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        known(&snapshot),
        [
            "docs",
            "docs/.koiignore",
            "docs/deep",
            "docs/final",
            "other",
            "other/draft"
        ]
    );
    assert_eq!(rescan.rules, [rule("docs", Some("draft\n"))]);
}

/// A tree with entries the rule `*.tmp` or `cache/` matches at the root and
/// two folders below it, scanned before any rules file exists.
fn tree_without_rules() -> (tempfile::TempDir, Snapshot, Rules) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "c.tmp", "c");
    write(root, "sub/a.tmp", "a");
    write(root, "sub/b.txt", "b");
    write(root, "sub/cache/x/y.txt", "y");
    let mut rules = rules();
    let snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    assert_eq!(
        known(&snapshot),
        [
            "c.tmp",
            "sub",
            "sub/a.tmp",
            "sub/b.txt",
            "sub/cache",
            "sub/cache/x",
            "sub/cache/x/y.txt"
        ]
    );
    (dir, snapshot, rules)
}

#[test]
fn adding_a_rule_drops_the_matching_entries_without_a_removal_and_reports_the_rules() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "*.tmp\ncache/\n");

    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from(RULES_FILE)], &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, [created(RULES_FILE, File)]);
    assert_eq!(rescan.rules, [rule("", Some("*.tmp\ncache/\n"))]);
    assert_eq!(rescan.unsettled, Vec::<PathBuf>::new());
    assert_eq!(known(&snapshot), [RULES_FILE, "sub", "sub/b.txt"]);
    assert!(root.join("sub/cache/x/y.txt").exists());
}

#[test]
fn removing_a_rule_reports_the_entries_as_created_also_deep_below_the_rules_file() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "*.tmp\ncache/\n");
    snapshot
        .rescan_with_rules(root, [PathBuf::from(RULES_FILE)], &mut rules)
        .unwrap();

    write(root, RULES_FILE, "cache/\n");
    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from(RULES_FILE)], &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            created("c.tmp", File),
            created("sub/a.tmp", File),
            Change::Modified {
                path: RULES_FILE.into()
            },
        ]
    );
    assert_eq!(rescan.rules, [rule("", Some("cache/\n"))]);

    fs::remove_file(root.join(RULES_FILE)).unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from(RULES_FILE)], &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            removed(RULES_FILE, File),
            created("sub/cache", Dir),
            created("sub/cache/x", Dir),
            created("sub/cache/x/y.txt", File),
        ]
    );
    assert_eq!(rescan.rules, [rule("", None)]);
    assert_eq!(snapshot, Snapshot::scan(root).unwrap());
}

#[test]
fn rules_inside_a_folder_that_becomes_left_out_are_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "ig/.koiignore", "x\n");
    write(root, "ig/x", "x");
    write(root, "ig/y", "y");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.rules, [rule("ig", Some("x\n"))]);
    assert_eq!(known(&snapshot), ["ig", "ig/.koiignore", "ig/y"]);

    write(root, RULES_FILE, "ig/\n");
    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from(RULES_FILE)], &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, [created(RULES_FILE, File)]);
    assert_eq!(rescan.rules, [rule("", Some("ig/\n")), rule("ig", None)]);
    assert_eq!(known(&snapshot), [RULES_FILE]);

    fs::remove_file(root.join(RULES_FILE)).unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from(RULES_FILE)], &mut rules)
        .unwrap();
    assert_eq!(rescan.rules, [rule("", None), rule("ig", Some("x\n"))]);
    assert_eq!(known(&snapshot), ["ig", "ig/.koiignore", "ig/y"]);
}

#[test]
fn a_symbolic_link_or_a_folder_named_like_a_rules_file_is_not_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    write(dir.path(), "outside-rules", "x\n");
    write(&root, "linked/x", "x");
    write(&root, "folder/.koiignore/x", "x");
    write(&root, "folder/x", "x");
    std::os::unix::fs::symlink(
        dir.path().join("outside-rules"),
        root.join("linked/.koiignore"),
    )
    .unwrap();

    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(&root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.rules, []);
    assert_eq!(
        known(&snapshot),
        [
            "folder",
            "folder/.koiignore",
            "folder/.koiignore/x",
            "folder/x",
            "linked",
            "linked/x"
        ]
    );
}

#[test]
fn a_rules_file_over_the_size_limit_keeps_the_rules_the_folder_had() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, RULES_FILE, "x\n");
    write(root, "x", "x");
    write(root, "y", "y");
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    assert_eq!(known(&snapshot), [RULES_FILE, "y"]);

    write(root, RULES_FILE, &"y\n".repeat((1 << 19) + 1));
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [Change::Modified {
            path: RULES_FILE.into()
        }]
    );
    assert_eq!(rescan.rules, []);
    assert_eq!(known(&snapshot), [RULES_FILE, "y"]);

    write(root, RULES_FILE, &"y\n".repeat(1 << 19));
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.rules.len(), 1);
    assert_eq!(known(&snapshot), [RULES_FILE, "x"]);
}

#[test]
fn built_in_platform_files_are_left_out_and_are_not_ignored_entries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, RULES_FILE, "!.DS_Store\n");
    write(root, ".DS_Store", "finder");
    write(root, "d/Thumbs.db/inside", "t");
    write(root, "d/x.txt", "x");
    write(root, "gone.txt", "g");
    write(root, "moved.txt", "m");
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    assert_eq!(
        known(&snapshot),
        [RULES_FILE, "d", "d/x.txt", "gone.txt", "moved.txt"]
    );
    assert_eq!(rules.status(Path::new(".DS_Store"), File), Ignored::BuiltIn);

    fs::remove_file(root.join("gone.txt")).unwrap();
    fs::rename(root.join("moved.txt"), root.join("d/Thumbs.db/moved.txt")).unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [removed("gone.txt", File), removed("moved.txt", File)]
    );
}

/// A tree whose root rules leave out the folder `ig`, scanned.
fn tree_with_an_ignored_folder() -> (tempfile::TempDir, Snapshot, Rules) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, RULES_FILE, "ig/\n");
    write(root, "ig/deep/old.txt", "o");
    write(root, "f.txt", "f");
    write(root, "d/g.txt", "g");
    let mut rules = rules();
    let snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    assert_eq!(known(&snapshot), [RULES_FILE, "d", "d/g.txt", "f.txt"]);
    (dir, snapshot, rules)
}

#[test]
fn a_file_moved_into_an_ignored_folder_is_ignored_not_removed() {
    for (to, paths) in [
        ("ig/f.txt", whole().to_vec()),
        ("ig/deep/renamed.txt", whole().to_vec()),
        ("ig/f.txt", vec![PathBuf::from("f.txt")]),
    ] {
        let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
        let root = dir.path();
        let id = snapshot.entries[Path::new("f.txt")].id;
        fs::rename(root.join("f.txt"), root.join(to)).unwrap();

        let rescan = snapshot
            .rescan_with_rules(root, paths.clone(), &mut rules)
            .unwrap();
        assert_eq!(
            rescan.changes,
            [Change::Ignored {
                from: "f.txt".into(),
                to: to.into(),
                kind: File,
                id,
            }],
            "{to} {paths:?}"
        );
        assert_eq!(known(&snapshot), [RULES_FILE, "d", "d/g.txt"]);
    }
}

fn large_ignored_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, RULES_FILE, "ig/\n");
    for n in 0..30 {
        for m in 0..10 {
            write(root, &format!("ig/{n:02}/{m}.txt"), "x");
        }
    }
    fs::create_dir_all(root.join("ig/zz/deep")).unwrap();
    dir
}

#[test]
fn a_search_through_a_large_ignored_folder_goes_on_across_rescans() {
    for (gone, to) in [
        ("f.txt", Some("ig/zz/deep/f.txt")),
        ("d", Some("ig/zz/deep/d")),
        ("d/g.txt", None),
    ] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "f.txt", "f");
        write(root, "d/g.txt", "g");
        let mut rules = rules().with_search_budget(100);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let before = &snapshot.entries[Path::new(gone)];
        let (id, kind) = (before.id, before.kind);
        match to {
            Some(to) => fs::rename(root.join(gone), root.join(to)).unwrap(),
            None => fs::remove_file(root.join(gone)).unwrap(),
        }
        let mut paths = whole().to_vec();
        let mut rescans = 0;
        let changes = loop {
            rescans += 1;
            assert!(rescans < 20, "{gone}: the search never ended");
            let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
            if !rescan.changes.is_empty() {
                break rescan.changes;
            }
            assert_eq!(rescan.unsettled, [PathBuf::from(gone)], "{gone}");
            assert!(!snapshot.entries.contains_key(Path::new(gone)), "{gone}");
            paths = rescan.unsettled;
        };
        assert!(rescans > 2, "{gone}: {rescans} rescans");
        let expected = match to {
            Some(to) => Change::Ignored {
                from: gone.into(),
                to: to.into(),
                kind,
                id,
            },
            None => removed(gone, kind),
        };
        assert_eq!(changes, [expected], "{gone}");
        assert!(!snapshot.entries.contains_key(Path::new(gone)), "{gone}");
    }
}

#[test]
fn a_file_removed_from_a_folder_renamed_while_the_search_runs_waits_at_its_new_path() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "d/x.txt", "x");
    write(root, "d/y.txt", "y");
    let mut rules = rules().with_search_budget(100);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    fs::remove_file(root.join("d/x.txt")).unwrap();
    fs::rename(root.join("d"), root.join("e")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [Change::Renamed {
            from: "d".into(),
            to: "e".into(),
            kind: Dir,
        }]
    );
    assert_eq!(rescan.unsettled, [PathBuf::from("e/x.txt")]);
    let outside = |snapshot: &Snapshot| -> Vec<String> {
        (known(snapshot).into_iter())
            .filter(|path| path.starts_with('d') || path.starts_with('e'))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(outside(&snapshot), ["e", "e/y.txt"]);

    let mut paths = rescan.unsettled;
    let changes = loop {
        let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
        if !rescan.changes.is_empty() {
            break rescan.changes;
        }
        assert_eq!(rescan.unsettled, [PathBuf::from("e/x.txt")]);
        paths = rescan.unsettled;
    };
    assert_eq!(changes, [removed("e/x.txt", File)]);
    assert_eq!(outside(&snapshot), ["e", "e/y.txt"]);
}

#[test]
fn removals_rescanned_in_turn_are_each_decided() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "f.txt", "f");
    write(root, "other/a.txt", "a");
    let mut rules = rules().with_search_budget(100);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    fs::remove_file(root.join("f.txt")).unwrap();
    fs::remove_file(root.join("other/a.txt")).unwrap();

    let mut changes = Vec::new();
    for _ in 0..10 {
        for scope in ["f.txt", "other/a.txt"] {
            let rescan = snapshot
                .rescan_with_rules(root, [PathBuf::from(scope)], &mut rules)
                .unwrap();
            changes.extend(rescan.changes);
        }
    }
    assert_eq!(
        changes,
        [removed("f.txt", File), removed("other/a.txt", File)]
    );
}

#[test]
fn rescan_watcher_goes_on_with_a_search_without_waiting_for_its_interval() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "f.txt", "f");
    let mut rules = rules().with_search_budget(100);
    let snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    let mut watcher =
        RescanWatcher::with_rules(root, snapshot, Duration::from_secs(3600), rules).unwrap();
    fs::remove_file(root.join("f.txt")).unwrap();

    let poll = watcher.poll(Duration::from_secs(5)).unwrap();
    assert_eq!(poll.changes, [removed("f.txt", File)]);
}

#[test]
fn a_watcher_started_from_the_snapshot_of_another_reports_the_removal_it_held() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "f.txt", "f");
    let mut held = rules().with_search_budget(100);
    let snapshot = Snapshot::scan_with_rules(root, &mut held).unwrap();
    let hour = Duration::from_secs(3600);
    let mut first = RescanWatcher::with_rules(root, snapshot, hour, held).unwrap();
    fs::remove_file(root.join("f.txt")).unwrap();
    let poll = first.poll(Duration::ZERO).unwrap();
    assert_eq!(poll.changes, []);
    assert_eq!(poll.pending, [PathBuf::from("f.txt")]);

    let fresh = rules().with_search_budget(100);
    let mut second =
        RescanWatcher::with_rules(root, first.snapshot().clone(), hour, fresh).unwrap();
    let mut changes = Vec::new();
    for _ in 0..5 {
        changes.extend(second.poll(Duration::from_secs(2)).unwrap().changes);
    }
    assert_eq!(changes, [removed("f.txt", File)]);
}

#[test]
fn a_new_file_at_a_held_path_after_a_watcher_handover_keeps_the_move() {
    for (native, before) in [(false, true), (false, false), (true, true), (true, false)] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "d/f.txt", "old");
        let mut held = rules().with_search_budget(100);
        let start = Snapshot::scan_with_rules(root, &mut held).unwrap();
        let id = start.entries[Path::new("d/f.txt")].id;
        let mut a = RescanWatcher::with_rules(root, start, Duration::ZERO, held).unwrap();
        fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
        let poll = a.poll(Duration::ZERO).unwrap();
        assert_eq!(poll.pending, [PathBuf::from("d/f.txt")]);
        if before {
            write(root, "d/f.txt", "new");
        }
        let seed = a.snapshot().clone();
        let fresh = rules().with_search_budget(100);
        let mut b: Box<dyn Watcher> = match native {
            true => Box::new(NativeWatcher::start_with_rules(root, seed, fresh).unwrap()),
            false => {
                Box::new(RescanWatcher::with_rules(root, seed, Duration::ZERO, fresh).unwrap())
            }
        };
        if !before {
            write(root, "d/f.txt", "new");
        }
        let mut changes = Vec::new();
        for _ in 0..50 {
            let poll = b.poll(Duration::from_millis(200)).unwrap();
            changes.extend(poll.changes);
            if poll.pending.is_empty() && changes.len() >= 2 {
                break;
            }
        }
        let expected = [
            created("d/f.txt", File),
            Change::Ignored {
                from: "d/f.txt".into(),
                to: "ig/zz/deep/f.txt".into(),
                kind: File,
                id,
            },
        ];
        changes.sort_by_key(|change| matches!(change, Change::Ignored { .. }));
        assert_eq!(
            changes, expected,
            "native {native}, new file before {before}"
        );
    }
}

#[test]
fn a_search_budget_of_zero_still_settles() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "f.txt", "f");
    let mut rules = rules().with_search_budget(0);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    fs::remove_file(root.join("f.txt")).unwrap();
    let mut changes = Vec::new();
    for _ in 0..1000 {
        let rescan = snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        changes.extend(rescan.changes);
        if rescan.unsettled.is_empty() {
            break;
        }
    }
    assert_eq!(changes, [removed("f.txt", File)]);
}

#[test]
fn a_scoped_rescan_finds_a_hard_link_outside_its_scope_as_a_full_one_does() {
    for paths in [whole().to_vec(), vec![PathBuf::from("a")]] {
        let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
        let root = dir.path();
        write(root, "a", "a");
        fs::create_dir(root.join("other")).unwrap();
        fs::hard_link(root.join("a"), root.join("other/b")).unwrap();
        fs::hard_link(root.join("a"), root.join("ig/link")).unwrap();
        snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        let id = snapshot.entries[Path::new("a")].id;
        fs::remove_file(root.join("a")).unwrap();

        let rescan = snapshot
            .rescan_with_rules(root, paths.clone(), &mut rules)
            .unwrap();
        assert_eq!(
            rescan.changes,
            [Change::Ignored {
                from: "a".into(),
                to: "ig/link".into(),
                kind: File,
                id,
            }],
            "{paths:?}"
        );
    }
}

#[test]
fn a_link_moved_into_an_ignored_folder_while_another_link_stays_included_is_ignored() {
    for (scope, budget) in [
        (whole().to_vec(), usize::MAX),
        (vec![PathBuf::from("f.txt")], usize::MAX),
        (whole().to_vec(), 100),
    ] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "f.txt", "f");
        fs::hard_link(root.join("f.txt"), root.join("also.txt")).unwrap();
        let mut rules = rules().with_search_budget(budget);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let id = snapshot.entries[Path::new("f.txt")].id;
        fs::rename(root.join("f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();

        let mut paths = scope.clone();
        let mut changes = Vec::new();
        for _ in 0..20 {
            let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
            changes.extend(rescan.changes);
            if rescan.unsettled.is_empty() {
                break;
            }
            paths = rescan.unsettled;
        }
        assert_eq!(
            changes,
            [Change::Ignored {
                from: "f.txt".into(),
                to: "ig/zz/deep/f.txt".into(),
                kind: File,
                id,
            }],
            "{scope:?} {budget}"
        );
        assert!(snapshot.entries.contains_key(Path::new("also.txt")));
    }
}

#[test]
fn a_link_moved_into_an_ignored_folder_while_another_link_is_renamed_is_ignored() {
    for (renamed, ignored, over) in [
        ("also.txt", "f.txt", false),
        ("f.txt", "also.txt", false),
        ("also.txt", "f.txt", true),
    ] {
        for budget in [usize::MAX, 100] {
            let dir = large_ignored_tree();
            let root = dir.path();
            write(root, "f.txt", "f");
            if over {
                write(root, "other.txt", "o");
            }
            fs::hard_link(root.join("f.txt"), root.join("also.txt")).unwrap();
            let mut rules = rules().with_search_budget(budget);
            let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
            fs::rename(root.join(renamed), root.join("other.txt")).unwrap();
            fs::rename(root.join(ignored), root.join("ig/zz/deep/moved.txt")).unwrap();

            let mut paths = whole().to_vec();
            let mut changes = Vec::new();
            for _ in 0..20 {
                let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
                changes.extend(rescan.changes);
                if rescan.unsettled.is_empty() {
                    break;
                }
                paths = rescan.unsettled;
            }
            let case = format!("{renamed} {ignored} over={over} budget={budget}: {changes:?}");
            assert!(
                (changes.iter()).any(|change| matches!(change, Change::Ignored { to, .. }
                    if to == Path::new("ig/zz/deep/moved.txt"))),
                "{case}"
            );
            let removals = (changes.iter())
                .filter(|change| matches!(change, Change::Removed { .. }))
                .count();
            assert_eq!(removals, usize::from(over), "{case}");
        }
    }
}

#[test]
fn two_links_moved_into_an_ignored_folder_beside_a_new_included_link_are_not_removed() {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    write(root, "a", "a");
    fs::hard_link(root.join("a"), root.join("b")).unwrap();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("a"), root.join("ig/a")).unwrap();
    fs::rename(root.join("b"), root.join("ig/b")).unwrap();
    fs::hard_link(root.join("ig/a"), root.join("c")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(
        (rescan.changes.iter()).any(|change| matches!(change, Change::Ignored { .. })),
        "{:?}",
        rescan.changes
    );
    assert!(
        !(rescan.changes.iter()).any(|change| matches!(change, Change::Removed { .. })),
        "{:?}",
        rescan.changes
    );
}

#[test]
fn a_file_moved_over_another_with_an_older_link_in_an_ignored_folder_is_removed() {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    write(root, "a", "a");
    write(root, "x", "x");
    fs::hard_link(root.join("a"), root.join("ig/l")).unwrap();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("a"), root.join("x")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [removed("a", File), Change::Modified { path: "x".into() }]
    );
}

#[test]
fn a_file_moved_over_another_out_of_a_removed_folder_with_an_older_link_in_an_ignored_folder_is_removed()
 {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    write(root, "x", "x");
    fs::hard_link(root.join("d/g.txt"), root.join("ig/l")).unwrap();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("d/g.txt"), root.join("x")).unwrap();
    fs::remove_dir(root.join("d")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [removed("d", Dir), Change::Modified { path: "x".into() }]
    );
}

#[test]
fn a_link_removed_while_another_link_stays_included_is_removed() {
    for budget in [usize::MAX, 100] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "f.txt", "f");
        fs::hard_link(root.join("f.txt"), root.join("also.txt")).unwrap();
        let mut rules = rules().with_search_budget(budget);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        fs::remove_file(root.join("f.txt")).unwrap();

        let mut paths = whole().to_vec();
        let mut changes = Vec::new();
        for _ in 0..20 {
            let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
            changes.extend(rescan.changes);
            if rescan.unsettled.is_empty() {
                break;
            }
            paths = rescan.unsettled;
        }
        assert_eq!(changes, [removed("f.txt", File)], "{budget}");
        assert!(snapshot.entries.contains_key(Path::new("also.txt")));
    }
}

#[test]
fn a_held_entry_under_a_folder_that_cannot_be_read_is_not_removed() {
    use std::os::unix::fs::PermissionsExt;
    for left_out in [false, true] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "d/x", "x");
        write(root, "d/y", "y");
        let mut rules = rules().with_search_budget(100);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        fs::rename(root.join("d/x"), root.join("ig/zz/deep/x")).unwrap();
        let first = snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        assert_eq!(first.unsettled, [PathBuf::from("d/x")]);
        fs::rename(root.join("ig/zz/deep/x"), root.join("d/x")).unwrap();
        fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o000)).unwrap();
        if left_out {
            write(root, RULES_FILE, "ig/\nd/\n");
        }

        let mut changes = Vec::new();
        for _ in 0..10 {
            let rescan = snapshot.rescan_with_rules(root, whole(), &mut rules);
            changes.extend(rescan.unwrap().changes);
        }
        fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            !changes
                .iter()
                .any(|change| matches!(change, Change::Removed { .. })),
            "{left_out} {changes:?}"
        );
    }
}

#[test]
fn an_unrelated_first_rescan_keeps_the_search_for_a_path_another_watcher_held() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "d/f.txt", "old");
    write(root, "q/r/e.txt", "e");
    let mut held = rules().with_search_budget(100);
    let start = Snapshot::scan_with_rules(root, &mut held).unwrap();
    let id = start.entries[Path::new("d/f.txt")].id;
    let mut a = RescanWatcher::with_rules(root, start, Duration::ZERO, held).unwrap();
    fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
    let poll = a.poll(Duration::ZERO).unwrap();
    assert_eq!(poll.pending, [PathBuf::from("d/f.txt")]);
    write(root, "d/f.txt", "new");

    let mut snapshot = a.snapshot().clone();
    let mut fresh = rules().with_search_budget(100);
    let mut changes =
        (snapshot.rescan_with_rules(root, vec![PathBuf::from("q/r/e.txt")], &mut fresh))
            .unwrap()
            .changes;
    for _ in 0..10 {
        let rescan = snapshot.rescan_with_rules(root, whole(), &mut fresh);
        changes.extend(rescan.unwrap().changes);
    }
    changes.sort_by_key(|change| matches!(change, Change::Ignored { .. }));
    assert_eq!(
        changes,
        [
            created("d/f.txt", File),
            Change::Ignored {
                from: "d/f.txt".into(),
                to: "ig/zz/deep/f.txt".into(),
                kind: File,
                id,
            },
        ]
    );
}

#[test]
fn a_reset_over_a_held_path_then_a_new_file_there_keeps_the_move() {
    for native in [false, true] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "d/f.txt", "old");
        let mut rules = rules().with_search_budget(100);
        let start = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let id = start.entries[Path::new("d/f.txt")].id;
        let known: BTreeMap<PathBuf, Known> = (start.entries.iter())
            .filter(|(path, _)| path.starts_with("d"))
            .map(|(path, entry)| {
                let known = Known {
                    kind: entry.kind,
                    id: entry.id,
                };
                (path.clone(), known)
            })
            .collect();
        let mut watcher: Box<dyn Watcher> = match native {
            true => Box::new(NativeWatcher::start_with_rules(root, start, rules).unwrap()),
            false => {
                Box::new(RescanWatcher::with_rules(root, start, Duration::ZERO, rules).unwrap())
            }
        };
        fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
        let mut pending = Vec::new();
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(50));
            let poll = watcher.poll(Duration::ZERO).unwrap();
            assert_eq!(poll.changes, [], "native {native}");
            pending = poll.pending;
            if !pending.is_empty() {
                break;
            }
        }
        assert_eq!(pending, [PathBuf::from("d/f.txt")], "native {native}");
        watcher.reset(Path::new("d"), known);
        write(root, "d/f.txt", "new");

        let mut changes = Vec::new();
        for _ in 0..50 {
            let poll = watcher.poll(Duration::from_millis(200)).unwrap();
            changes.extend(poll.changes);
            if poll.pending.is_empty() && changes.len() >= 2 {
                break;
            }
        }
        changes.sort_by_key(|change| matches!(change, Change::Ignored { .. }));
        assert_eq!(
            changes,
            [
                created("d/f.txt", File),
                Change::Ignored {
                    from: "d/f.txt".into(),
                    to: "ig/zz/deep/f.txt".into(),
                    kind: File,
                    id,
                },
            ],
            "native {native}"
        );
    }
}

#[test]
fn a_folder_replaced_while_held_keeps_the_file_returned_to_it() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "d/x", "x");
    let mut rules = rules().with_search_budget(100);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    fs::rename(root.join("d"), root.join("ig/zz/deep/d")).unwrap();
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(first.unsettled, [PathBuf::from("d")]);
    fs::create_dir(root.join("d")).unwrap();
    fs::rename(root.join("ig/zz/deep/d/x"), root.join("d/x")).unwrap();
    fs::remove_dir(root.join("ig/zz/deep/d")).unwrap();

    let mut changes = Vec::new();
    for _ in 0..10 {
        let rescan = snapshot.rescan_with_rules(root, whole(), &mut rules);
        changes.extend(rescan.unwrap().changes);
    }
    assert_eq!(
        changes,
        [
            removed("d", Dir),
            Change::Renamed {
                from: "d/x".into(),
                to: "d/x".into(),
                kind: File,
            },
            created("d", Dir),
        ]
    );
}

#[test]
fn a_file_replaced_by_a_folder_renamed_onto_it_keeps_the_folder() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "a", "a");
    write(root, "b/c", "c");
    let mut rules = rules().with_search_budget(100);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    fs::remove_file(root.join("a")).unwrap();
    fs::rename(root.join("b"), root.join("a")).unwrap();

    let mut changes = Vec::new();
    for _ in 0..10 {
        let rescan = snapshot.rescan_with_rules(root, whole(), &mut rules);
        changes.extend(rescan.unwrap().changes);
    }
    assert_eq!(
        changes,
        [
            removed("a", File),
            Change::Renamed {
                from: "b".into(),
                to: "a".into(),
                kind: Dir,
            },
        ]
    );
}

#[test]
fn a_walk_under_way_looks_in_a_folder_the_rules_leave_out_since_it_began() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "f.txt", "f");
    write(root, "ig2/keep.txt", "k");
    let mut rules = rules().with_search_budget(100);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    let id = snapshot.entries[Path::new("f.txt")].id;
    fs::rename(root.join("f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(first.unsettled, [PathBuf::from("f.txt")]);
    write(root, RULES_FILE, "ig/\nig2/\n");
    fs::rename(root.join("ig/zz/deep/f.txt"), root.join("ig2/f.txt")).unwrap();

    let mut changes = Vec::new();
    for _ in 0..10 {
        let rescan = snapshot.rescan_with_rules(root, whole(), &mut rules);
        changes.extend(rescan.unwrap().changes);
    }
    assert!(
        changes.contains(&Change::Ignored {
            from: "f.txt".into(),
            to: "ig2/f.txt".into(),
            kind: File,
            id,
        }),
        "{changes:?}"
    );
}

#[test]
fn a_held_path_in_a_folder_the_scan_cannot_read_stays_unscanned() {
    use std::os::unix::fs::PermissionsExt;
    for reset in [true, false] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "d/f.txt", "old");
        let mut held = rules().with_search_budget(100);
        let start = Snapshot::scan_with_rules(root, &mut held).unwrap();
        let id = start.entries[Path::new("d/f.txt")].id;
        let known: BTreeMap<PathBuf, Known> = (start.entries.iter())
            .filter(|(path, _)| path.starts_with("d"))
            .map(|(path, entry)| {
                let known = Known {
                    kind: entry.kind,
                    id: entry.id,
                };
                (path.clone(), known)
            })
            .collect();
        let mut a = RescanWatcher::with_rules(root, start, Duration::ZERO, held).unwrap();
        fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
        let poll = a.poll(Duration::ZERO).unwrap();
        assert_eq!(poll.pending, [PathBuf::from("d/f.txt")]);
        let mut watcher = match reset {
            true => {
                a.reset(Path::new("d"), known);
                a
            }
            false => {
                let fresh = rules().with_search_budget(100);
                RescanWatcher::with_rules(root, a.snapshot().clone(), Duration::ZERO, fresh)
                    .unwrap()
            }
        };
        write(root, "d/f.txt", "new");
        fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o000)).unwrap();
        let mut changes = Vec::new();
        for _ in 0..3 {
            changes.extend(watcher.poll(Duration::ZERO).unwrap().changes);
        }
        fs::set_permissions(root.join("d"), fs::Permissions::from_mode(0o755)).unwrap();
        for _ in 0..10 {
            let poll = watcher.poll(Duration::ZERO).unwrap();
            changes.extend(poll.changes);
            if poll.pending.is_empty() && changes.len() >= 2 {
                break;
            }
        }
        changes.sort_by_key(|change| matches!(change, Change::Ignored { .. }));
        assert_eq!(
            changes,
            [
                created("d/f.txt", File),
                Change::Ignored {
                    from: "d/f.txt".into(),
                    to: "ig/zz/deep/f.txt".into(),
                    kind: File,
                    id,
                },
            ],
            "reset {reset}"
        );
    }
}

#[test]
fn a_folder_removed_while_the_search_for_its_file_runs_is_removed() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "d/f.txt", "f");
    let mut rules = rules().with_search_budget(100);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    fs::remove_file(root.join("d/f.txt")).unwrap();
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(first.unsettled, [PathBuf::from("d/f.txt")]);
    fs::remove_dir(root.join("d")).unwrap();

    let mut paths = whole().to_vec();
    for _ in 0..20 {
        let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
        if rescan.unsettled.is_empty() {
            assert_eq!(rescan.changes, [removed("d", Dir)]);
            return;
        }
        assert_eq!(rescan.changes, []);
        paths = rescan.unsettled;
    }
    panic!("d is still unsettled");
}

#[test]
fn a_file_moved_out_of_a_folder_moved_into_an_ignored_folder_is_renamed() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "d/x.txt", "x");
    fs::create_dir_all(root.join("keep")).unwrap();
    let mut rules = rules().with_search_budget(100);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    let id = snapshot.entries[Path::new("d")].id;
    fs::rename(root.join("d/x.txt"), root.join("keep/x.txt")).unwrap();
    fs::rename(root.join("d"), root.join("ig/zz/d")).unwrap();

    let mut paths = whole().to_vec();
    let mut changes = Vec::new();
    for _ in 0..20 {
        let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
        changes.extend(rescan.changes);
        let ids: Vec<_> = snapshot.entries.values().map(|entry| entry.id).collect();
        assert!(
            (ids.iter().enumerate()).all(|(n, id)| !ids[..n].contains(id)),
            "{changes:?}"
        );
        if rescan.unsettled.is_empty() {
            break;
        }
        paths = rescan.unsettled;
    }
    assert_eq!(
        changes,
        [
            Change::Renamed {
                from: "d/x.txt".into(),
                to: "keep/x.txt".into(),
                kind: File,
            },
            Change::Ignored {
                from: "d".into(),
                to: "ig/zz/d".into(),
                kind: Dir,
                id,
            },
        ]
    );
}

#[test]
fn a_new_entry_at_the_path_of_a_deferred_move_into_an_ignored_folder_keeps_the_move() {
    for (deferred, renamed) in [(true, false), (true, true), (false, true)] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "d/f.txt", "old");
        let mut rules = rules().with_search_budget(100);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let id = snapshot.entries[Path::new("d/f.txt")].id;
        fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
        if deferred {
            let rescan = snapshot
                .rescan_with_rules(root, whole(), &mut rules)
                .unwrap();
            assert_eq!(rescan.unsettled, [PathBuf::from("d/f.txt")]);
        }
        let at = if renamed {
            fs::rename(root.join("d"), root.join("e")).unwrap();
            "e/f.txt"
        } else {
            "d/f.txt"
        };
        write(root, at, "new");

        let rescan = snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        assert!(
            !(rescan.changes.iter()).any(|change| matches!(change, Change::Created { .. })),
            "{deferred} {renamed}: {:?}",
            rescan.changes
        );
        let mut changes = rescan.changes;
        let mut unsettled = rescan.unsettled;
        while !unsettled.is_empty() {
            let rescan = snapshot
                .rescan_with_rules(root, whole(), &mut rules)
                .unwrap();
            changes.extend(rescan.changes);
            unsettled = rescan.unsettled;
        }
        let mut expected = Vec::new();
        if renamed {
            expected.push(Change::Renamed {
                from: "d".into(),
                to: "e".into(),
                kind: Dir,
            });
        }
        expected.push(Change::Ignored {
            from: at.into(),
            to: "ig/zz/deep/f.txt".into(),
            kind: File,
            id,
        });
        expected.push(created(at, File));
        assert_eq!(changes, expected, "{deferred} {renamed}");
    }
}

#[test]
fn a_rename_onto_the_new_path_of_a_removal_keeps_the_renamed_file() {
    for into_ignored in [false, true] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "d/x.txt", "x");
        write(root, "d/y.txt", "yy");
        let mut rules = rules().with_search_budget(100);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let id = snapshot.entries[Path::new("d/x.txt")].id;
        if into_ignored {
            fs::rename(root.join("d/x.txt"), root.join("ig/zz/deep/x.txt")).unwrap();
        } else {
            fs::remove_file(root.join("d/x.txt")).unwrap();
        }
        fs::rename(root.join("d"), root.join("e")).unwrap();
        fs::rename(root.join("e/y.txt"), root.join("e/x.txt")).unwrap();

        let rescan = snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        let gone = match into_ignored {
            true => Change::Ignored {
                from: "d/x.txt".into(),
                to: "ig/zz/deep/x.txt".into(),
                kind: File,
                id,
            },
            false => removed("d/x.txt", File),
        };
        assert_eq!(
            rescan.changes,
            [
                gone,
                Change::Renamed {
                    from: "d".into(),
                    to: "e".into(),
                    kind: Dir,
                },
                Change::Renamed {
                    from: "d/y.txt".into(),
                    to: "e/x.txt".into(),
                    kind: File,
                },
            ],
            "{into_ignored}"
        );
        assert_eq!(rescan.unsettled, [] as [PathBuf; 0]);
    }
}

#[test]
fn a_file_moved_over_another_out_of_a_deferred_folder_leaves_with_the_folder() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "b/c/2.txt", "2");
    write(root, "top.txt", "top");
    let mut rules = rules().with_search_budget(100);
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    let id = snapshot.entries[Path::new("b/c")].id;
    fs::rename(root.join("top.txt"), root.join("ig/zz/deep/top.txt")).unwrap();
    fs::rename(root.join("b/c/2.txt"), root.join("top.txt")).unwrap();
    fs::rename(root.join("b/c"), root.join("ig/zz/deep/c")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [Change::Modified {
            path: "top.txt".into()
        }]
    );
    assert_eq!(rescan.unsettled, [PathBuf::from("b/c")]);
    assert_eq!(known(&snapshot), [RULES_FILE, "b", "top.txt"]);

    let mut paths = rescan.unsettled;
    let changes = loop {
        let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
        if !rescan.changes.is_empty() {
            assert_eq!(rescan.unsettled, [] as [PathBuf; 0]);
            break rescan.changes;
        }
        paths = rescan.unsettled;
    };
    assert_eq!(
        changes,
        [Change::Ignored {
            from: "b/c".into(),
            to: "ig/zz/deep/c".into(),
            kind: Dir,
            id,
        }]
    );
}

#[derive(Default)]
struct Random(u64);

impl Random {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }

    fn pick<T: Clone>(&mut self, items: &[T]) -> Option<T> {
        (!items.is_empty()).then(|| items[self.below(items.len())].clone())
    }
}

fn listed(root: &Path, under: &Path, into: &mut Vec<(PathBuf, bool)>) {
    for entry in fs::read_dir(root.join(under)).unwrap() {
        let entry = entry.unwrap();
        let path = under.join(entry.file_name());
        let dir = entry.file_type().unwrap().is_dir();
        into.push((path.clone(), dir));
        if dir {
            listed(root, &path, into);
        }
    }
}

fn on_disk(root: &Path) -> Vec<FileId> {
    let mut all = Vec::new();
    listed(root, Path::new(""), &mut all);
    (all.iter())
        .map(|(path, _)| {
            let meta = fs::symlink_metadata(root.join(path)).unwrap();
            let born = meta.created().ok().map(|time| {
                let since = time.duration_since(std::time::UNIX_EPOCH).unwrap();
                i64::try_from(since.as_nanos()).unwrap()
            });
            FileId {
                dev: meta.dev(),
                ino: u128::from(meta.ino()),
                birth_ns: born,
            }
        })
        .collect()
}

#[derive(Default)]
struct Changer {
    random: Random,
    fresh: usize,
    vacated: Vec<PathBuf>,
    out: BTreeSet<(u64, u128)>,
    bounced: BTreeSet<(u64, u128)>,
}

impl Changer {
    fn change(&mut self, root: &Path) {
        let mut all = Vec::new();
        listed(root, Path::new(""), &mut all);
        let included: Vec<(PathBuf, bool)> = (all.iter())
            .filter(|(path, _)| !path.starts_with("ig") && path != Path::new(RULES_FILE))
            .cloned()
            .collect();
        let folders = |ignored: bool| -> Vec<PathBuf> {
            (all.iter())
                .filter(|(path, dir)| *dir && path.starts_with("ig") == ignored)
                .map(|(path, _)| path.clone())
                .chain((!ignored).then(PathBuf::new))
                .collect()
        };
        self.fresh += 1;
        let name = format!("n{}", self.fresh);
        let moved = |from: &Path, to: &Path| {
            if !to.starts_with(from) && !root.join(to).exists() {
                fs::rename(root.join(from), root.join(to)).ok();
            }
        };
        let ids = |path: &Path| -> Vec<(u64, u128)> {
            let mut all = vec![(path.to_path_buf(), root.join(path).is_dir())];
            if all[0].1 {
                listed(root, path, &mut all);
            }
            (all.iter())
                .filter_map(|(path, _)| fs::symlink_metadata(root.join(path)).ok())
                .map(|meta| (meta.dev(), u128::from(meta.ino())))
                .collect()
        };
        let free = (self.vacated.iter())
            .filter(|path| {
                !root.join(path).exists()
                    && (path.parent()).is_some_and(|parent| root.join(parent).is_dir())
            })
            .cloned()
            .collect::<Vec<_>>();
        match self.random.below(8) {
            0 => {
                if let Some((path, false)) = self.random.pick(&included) {
                    fs::remove_file(root.join(path)).unwrap();
                }
            }
            1 => {
                if let Some((path, true)) = self.random.pick(&included) {
                    fs::remove_dir_all(root.join(path)).unwrap();
                }
            }
            2 => {
                if let (Some((from, _)), Some(into)) = (
                    self.random.pick(&included),
                    self.random.pick(&folders(false)),
                ) {
                    moved(&from, &into.join(&name));
                }
            }
            3 => {
                if let (Some((from, _)), Some(into)) = (
                    self.random.pick(&included),
                    self.random.pick(&folders(true)),
                ) {
                    let inside = ids(&from);
                    moved(&from, &into.join(&name));
                    self.vacated.push(from);
                    (self.bounced).extend(inside.into_iter().filter(|id| self.out.contains(id)));
                }
            }
            4 => {
                if let Some(into) = self.random.pick(&folders(false)) {
                    write(root, &into.join(&name).to_string_lossy(), "n");
                }
            }
            5 => {
                if let Some(path) = self.random.pick(&free) {
                    write(root, &path.to_string_lossy(), "new");
                }
            }
            6 => {
                if let (Some(to), Some((from, false))) =
                    (self.random.pick(&free), self.random.pick(&included))
                {
                    moved(&from, &to);
                }
            }
            _ => {
                let ignored: Vec<PathBuf> = (all.iter())
                    .filter(|(path, _)| path.starts_with("ig") && path.components().count() > 2)
                    .map(|(path, _)| path.clone())
                    .collect();
                if let (Some(from), Some(into)) = (
                    self.random.pick(&ignored),
                    self.random.pick(&folders(false)),
                ) {
                    self.out.extend(ids(&from));
                    moved(&from, &into.join(&name));
                }
            }
        }
    }
}

fn identities(snapshot: &Snapshot) -> BTreeMap<PathBuf, FileId> {
    (snapshot.entries.iter())
        .map(|(path, entry)| (path.clone(), entry.id))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    id: FileId,
    born: (usize, PathBuf),
}

type Gone = ((usize, PathBuf), Option<PathBuf>);

#[derive(Default)]
struct Synced {
    run: String,
    step: usize,
    nodes: BTreeMap<PathBuf, Node>,
    gone: Vec<Gone>,
    racy: BTreeSet<(u64, u128)>,
}

impl Synced {
    fn apply(&mut self, changes: &[Change], snapshot: &Snapshot, root: &Path, held: &[PathBuf]) {
        let run = &self.run;
        let sources: Vec<(&PathBuf, Option<&PathBuf>, Option<&PathBuf>)> = (changes.iter())
            .filter_map(|change| match change {
                Change::Removed { path, .. } => Some((path, None, None)),
                Change::Renamed { from, to, .. } => Some((from, Some(to), None)),
                Change::Ignored { from, to, .. } => Some((from, None, Some(to))),
                _ => None,
            })
            .collect();
        for (from, ..) in &sources {
            assert!(
                self.nodes.contains_key(*from),
                "{from:?} matches nothing in {run}: {changes:?}"
            );
        }
        let disk = on_disk(root);
        let there = |id: &FileId| disk.contains(id);
        for change in changes {
            if let Change::Ignored { to, id, .. } = change {
                let meta = fs::symlink_metadata(root.join(to)).unwrap();
                assert_eq!(
                    (meta.dev(), u128::from(meta.ino())),
                    (id.dev, id.ino),
                    "{run}: {change:?}"
                );
            }
        }
        let mut lost = Vec::new();
        for (path, node) in std::mem::take(&mut self.nodes) {
            let deepest = (sources.iter())
                .filter(|(from, ..)| path.starts_with(from))
                .max_by_key(|(from, ..)| from.components().count());
            match deepest {
                None => {
                    self.nodes.insert(path, node);
                }
                Some((from, Some(to), _)) => {
                    self.nodes
                        .insert(to.join(path.strip_prefix(from).unwrap()), node);
                }
                Some((_, None, ignored)) => {
                    if ignored.is_none()
                        && there(&node.id)
                        && !self.racy.contains(&(node.id.dev, node.id.ino))
                    {
                        lost.push((path, node.id));
                    }
                    self.gone.push((node.born, ignored.cloned()));
                }
            }
        }
        for change in changes {
            match change {
                Change::Created { path, .. } => {
                    let node = Node {
                        id: snapshot.entries[path].id,
                        born: (self.step, path.clone()),
                    };
                    let before = self.nodes.insert(path.clone(), node);
                    assert!(
                        before.is_none(),
                        "{path:?} created over {before:?} in {run}"
                    );
                }
                Change::Modified { path } => {
                    let node = self.nodes.get_mut(path);
                    let node = node.unwrap_or_else(|| panic!("{path:?} modified unknown in {run}"));
                    node.id = snapshot.entries[path].id;
                }
                _ => {}
            }
        }
        for (path, id) in lost {
            let at = (snapshot.entries.iter()).find(|(_, entry)| entry.id == id);
            assert!(
                at.is_some_and(|(at, _)| self.nodes.get(at).is_some_and(|node| node.id == id)),
                "{path:?} removed but on disk at {at:?} in {run}: {changes:?}"
            );
        }
        let outside = |path: &Path| !held.iter().any(|root| path.starts_with(root));
        let synced: BTreeMap<PathBuf, FileId> = (self.nodes.iter())
            .filter(|(path, _)| outside(path))
            .map(|(path, node)| (path.clone(), node.id))
            .collect();
        let scanned: BTreeMap<PathBuf, FileId> = (identities(snapshot).into_iter())
            .filter(|(path, _)| outside(path))
            .collect();
        assert_eq!(synced, scanned, "{run}: {changes:?} held {held:?}");
        let ids: Vec<FileId> = snapshot.entries.values().map(|entry| entry.id).collect();
        let unique: BTreeSet<(u64, u128)> = ids.iter().map(|id| (id.dev, id.ino)).collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "an identity at two paths in {run}: {changes:?}"
        );
    }
}

struct Run {
    snapshot: Snapshot,
    rules: Rules,
    synced: Synced,
}

impl Run {
    fn new(root: &Path, budget: usize, run: String) -> Self {
        let mut rules = rules().with_search_budget(budget);
        let snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let nodes = (snapshot.entries.iter())
            .map(|(path, entry)| {
                let born = (0, path.clone());
                (path.clone(), Node { id: entry.id, born })
            })
            .collect();
        let synced = Synced {
            run,
            nodes,
            ..Synced::default()
        };
        Self {
            snapshot,
            rules,
            synced,
        }
    }

    fn rescan(&mut self, root: &Path, paths: Vec<PathBuf>) -> Vec<PathBuf> {
        let rescan = (self.snapshot)
            .rescan_with_rules(root, paths, &mut self.rules)
            .unwrap();
        (self.synced).apply(&rescan.changes, &self.snapshot, root, &rescan.unsettled);
        rescan.unsettled
    }

    fn settle(&mut self, root: &Path) {
        let mut unsettled = self.rescan(root, whole().to_vec());
        for turn in 0..300 {
            if unsettled.is_empty() {
                return;
            }
            let scope = unsettled[turn % unsettled.len()].clone();
            unsettled = self.rescan(root, vec![scope]);
        }
        panic!("the search never ended in {}", self.synced.run);
    }
}

fn small_tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, RULES_FILE, "ig/\n");
    for n in 0..4 {
        for m in 0..4 {
            write(root, &format!("ig/{n}/{m}.txt"), "x");
        }
    }
    for path in [
        "a/1.txt",
        "a/2.txt",
        "b/1.txt",
        "b/c/1.txt",
        "b/c/2.txt",
        "top.txt",
    ] {
        write(root, path, "y");
    }
    dir
}

#[test]
fn a_bounded_search_ends_where_an_unbounded_one_does() {
    for seed in 1..=200u64 {
        let dir = small_tree();
        let root = dir.path();
        let mut changer = Changer {
            random: Random(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
            ..Changer::default()
        };
        let mut bounded = Run::new(root, 3, format!("bounded {seed}"));
        let mut unbounded = Run::new(root, usize::MAX, format!("unbounded {seed}"));
        let gone = |run: &mut Run| {
            let mut gone = std::mem::take(&mut run.synced.gone);
            gone.sort();
            gone
        };
        for step in 1..=10 {
            changer.out.clear();
            for _ in 0..=changer.random.below(3) {
                changer.change(root);
            }
            unbounded.synced.step = step;
            bounded.synced.step = step;
            let unsettled = unbounded.rescan(root, whole().to_vec());
            assert_eq!(unsettled, [] as [PathBuf; 0], "seed {seed}");
            if seed % 2 == 0 {
                bounded.settle(root);
                assert_eq!(bounded.synced.nodes, unbounded.synced.nodes, "seed {seed}");
                assert_eq!(gone(&mut bounded), gone(&mut unbounded), "seed {seed}");
            } else {
                bounded.synced.racy = changer.bounced.clone();
                bounded.rescan(root, whole().to_vec());
            }
        }
        bounded.settle(root);
        let ids = |run: &Run| -> BTreeMap<PathBuf, FileId> {
            (run.synced.nodes.iter())
                .map(|(path, node)| (path.clone(), node.id))
                .collect()
        };
        assert_eq!(ids(&bounded), ids(&unbounded), "seed {seed}");
        assert_eq!(ids(&bounded), identities(&bounded.snapshot), "seed {seed}");
    }
}

#[test]
fn a_folder_moved_into_an_ignored_folder_is_ignored_as_a_whole() {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    let id = snapshot.entries[Path::new("d")].id;
    fs::rename(root.join("d"), root.join("ig/deep/d")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [Change::Ignored {
            from: "d".into(),
            to: "ig/deep/d".into(),
            kind: Dir,
            id,
        }]
    );
    assert_eq!(known(&snapshot), [RULES_FILE, "f.txt"]);
}

#[test]
fn a_file_moved_into_an_ignored_folder_before_its_folder_was_removed_is_ignored() {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    let id = snapshot.entries[Path::new("d/g.txt")].id;
    fs::rename(root.join("d/g.txt"), root.join("ig/g.txt")).unwrap();
    fs::remove_dir(root.join("d")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            Change::Removed {
                path: "d".into(),
                kind: Dir,
            },
            Change::Ignored {
                from: "d/g.txt".into(),
                to: "ig/g.txt".into(),
                kind: File,
                id,
            },
        ]
    );
    assert_eq!(known(&snapshot), [RULES_FILE, "f.txt"]);
}

#[test]
fn a_file_moved_out_of_a_removed_folder_with_a_link_in_an_ignored_folder_is_renamed() {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    fs::hard_link(root.join("d/g.txt"), root.join("ig/alias.txt")).unwrap();
    fs::rename(root.join("d/g.txt"), root.join("moved.txt")).unwrap();
    fs::remove_dir(root.join("d")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let renamed = |change: &Change| {
        matches!(change, Change::Renamed { from, to, .. }
            if from == Path::new("d/g.txt") && to == Path::new("moved.txt"))
    };
    let ignored = |change: &Change| matches!(change, Change::Ignored { from, .. } if from == Path::new("d/g.txt"));
    assert!(rescan.changes.iter().any(renamed), "{:?}", rescan.changes);
    assert!(!rescan.changes.iter().any(ignored), "{:?}", rescan.changes);
}

/// `d/g.txt` and `d/h.txt` are links of one file. One goes to an included path and the other into the ignored
/// folder, so the scan reports one rename and one ignore.
#[test]
fn a_file_moved_out_of_a_removed_folder_is_renamed_and_its_link_moved_into_an_ignored_folder_is_ignored()
 {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    fs::hard_link(root.join("d/g.txt"), root.join("d/h.txt")).unwrap();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let id = snapshot.entries[Path::new("d/g.txt")].id;
    fs::rename(root.join("d/g.txt"), root.join("moved.txt")).unwrap();
    fs::rename(root.join("d/h.txt"), root.join("ig/h.txt")).unwrap();
    fs::remove_dir(root.join("d")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            Change::Removed {
                path: "d".into(),
                kind: Dir,
            },
            Change::Renamed {
                from: "d/g.txt".into(),
                to: "moved.txt".into(),
                kind: File,
            },
            Change::Ignored {
                from: "d/h.txt".into(),
                to: "ig/h.txt".into(),
                kind: File,
                id,
            },
        ]
    );
}

/// The folder `d/a` is renamed to `dest/a`, and the folder `x` inside it to `y`. The second rename carries
/// the file, so its link in the ignored folder does not count as a move.
#[test]
fn a_file_in_a_folder_renamed_inside_a_renamed_folder_with_a_link_in_an_ignored_folder_is_not_ignored()
 {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, RULES_FILE, "ig/\n");
    write(root, "d/a/x/file", "kept");
    fs::create_dir(root.join("ig")).unwrap();
    fs::create_dir(root.join("dest")).unwrap();
    let mut snapshot = Snapshot::default();
    let mut rules = rules();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::hard_link(root.join("d/a/x/file"), root.join("ig/alias")).unwrap();
    fs::rename(root.join("d/a"), root.join("dest/a")).unwrap();
    fs::rename(root.join("dest/a/x"), root.join("dest/a/y")).unwrap();
    fs::remove_dir(root.join("d")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            Change::Removed {
                path: "d".into(),
                kind: Dir,
            },
            Change::Renamed {
                from: "d/a".into(),
                to: "dest/a".into(),
                kind: Dir,
            },
            Change::Renamed {
                from: "d/a/x".into(),
                to: "dest/a/y".into(),
                kind: Dir,
            },
        ]
    );
    assert!(snapshot.entries.contains_key(Path::new("dest/a/y/file")));
}

#[test]
fn a_file_moved_into_an_ignored_folder_out_of_a_removed_folder_with_an_included_link_that_stays_is_ignored()
 {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    fs::hard_link(root.join("d/g.txt"), root.join("also.txt")).unwrap();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("d/g.txt"), root.join("ig/g.txt")).unwrap();
    fs::remove_dir(root.join("d")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let ignored = |change: &Change| {
        matches!(change, Change::Ignored { from, to, .. }
            if from == Path::new("d/g.txt") && to == Path::new("ig/g.txt"))
    };
    assert!(rescan.changes.iter().any(ignored), "{:?}", rescan.changes);
    assert!(snapshot.entries.contains_key(Path::new("also.txt")));
}

#[test]
fn a_file_moved_into_an_ignored_folder_before_a_folder_above_it_was_renamed_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, RULES_FILE, "ig/\n");
    write(root, "d/a/b/file", "kept");
    fs::create_dir(root.join("ig")).unwrap();
    fs::create_dir(root.join("dest")).unwrap();
    let mut snapshot = Snapshot::default();
    let mut rules = rules();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("d/a/b/file"), root.join("ig/file")).unwrap();
    fs::remove_dir(root.join("d/a/b")).unwrap();
    fs::rename(root.join("d/a"), root.join("dest/a")).unwrap();
    fs::remove_dir(root.join("d")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let ignored = |change: &&Change| {
        matches!(change, Change::Ignored { from, to, .. }
            if from == Path::new("d/a/b/file") && to == Path::new("ig/file"))
    };
    let found = rescan.changes.iter().filter(ignored).count();
    assert_eq!(found, 1, "{:?}", rescan.changes);
}

/// As above, with the folder that held the file moved into the ignored folder: it is ignored once, and the file
/// it holds goes with it.
#[test]
fn a_folder_moved_into_an_ignored_folder_before_a_folder_above_it_was_renamed_is_ignored_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, RULES_FILE, "ig/\n");
    write(root, "d/a/b/file", "kept");
    fs::create_dir(root.join("ig")).unwrap();
    fs::create_dir(root.join("dest")).unwrap();
    let mut snapshot = Snapshot::default();
    let mut rules = rules();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("d/a/b"), root.join("ig/b")).unwrap();
    fs::rename(root.join("d/a"), root.join("dest/a")).unwrap();
    fs::remove_dir(root.join("d")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let ignored: Vec<(&Path, &Path)> = (rescan.changes.iter())
        .filter_map(|change| match change {
            Change::Ignored { from, to, .. } => Some((from.as_path(), to.as_path())),
            _ => None,
        })
        .collect();
    assert_eq!(
        ignored,
        [(Path::new("d/a/b"), Path::new("ig/b"))],
        "{:?}",
        rescan.changes
    );
}

#[test]
fn a_file_renamed_to_a_name_a_rule_leaves_out_is_ignored_not_removed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, RULES_FILE, "*.bak\n");
    write(root, "d/notes.txt", "n");
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    let id = snapshot.entries[Path::new("d/notes.txt")].id;
    fs::rename(root.join("d/notes.txt"), root.join("d/notes.bak")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from("d/notes.txt")], &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [Change::Ignored {
            from: "d/notes.txt".into(),
            to: "d/notes.bak".into(),
            kind: File,
            id,
        }]
    );
}

/// The search misses a file whose identity changed after the move: an editor
/// saved it by writing a new file and renaming it over the moved one.
#[test]
fn a_file_moved_into_an_ignored_folder_and_saved_under_a_new_identity_is_removed() {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    fs::rename(root.join("f.txt"), root.join("ig/f.txt")).unwrap();
    write(root, "ig/f.txt.tmp", "saved");
    fs::rename(root.join("ig/f.txt.tmp"), root.join("ig/f.txt")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, [removed("f.txt", File)]);
    assert_eq!(known(&snapshot), [RULES_FILE, "d", "d/g.txt"]);
}

#[test]
fn a_removal_with_no_ignored_entry_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, RULES_FILE, "ig/\n*.tmp\n");
    write(root, "f.txt", "f");
    write(root, "d/g.txt", "g");
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    fs::remove_file(root.join("f.txt")).unwrap();
    fs::remove_dir_all(root.join("d")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, [removed("d", Dir), removed("f.txt", File)]);
    assert_eq!(known(&snapshot), [RULES_FILE]);
}

#[test]
fn a_removal_beside_an_ignored_folder_that_does_not_hold_the_entry_is_removed() {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    fs::remove_file(root.join("f.txt")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, [removed("f.txt", File)]);
}

#[test]
fn renaming_a_folder_that_holds_a_rules_file_moves_its_rules() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "a/.koiignore", "x\n");
    write(root, "a/x", "x");
    write(root, "a/y", "y");
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    assert_eq!(known(&snapshot), ["a", "a/.koiignore", "a/y"]);

    fs::rename(root.join("a"), root.join("b")).unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [Change::Renamed {
            from: "a".into(),
            to: "b".into(),
            kind: Dir,
        }]
    );
    assert_eq!(rescan.rules, [rule("a", None), rule("b", Some("x\n"))]);
    assert_eq!(known(&snapshot), ["b", "b/.koiignore", "b/y"]);

    write(root, "a/x", "x");
    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from("a")], &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, [created("a", Dir), created("a/x", File)]);
    assert_eq!(rescan.rules, []);
}

#[test]
fn a_folder_renamed_only_in_case_keeps_its_rules() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "a/.koiignore", "x\n");
    write(root, "a/x", "left out");
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    assert_eq!(known(&snapshot), ["a", "a/.koiignore"]);

    fs::rename(root.join("a"), root.join("A")).unwrap();
    for _ in 0..2 {
        let rescan = snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        assert_eq!(
            rescan.rules,
            [],
            "the matcher holds one folder for both names"
        );
        assert_eq!(known(&snapshot), ["A", "A/.koiignore"]);
    }
    assert_eq!(rules.status(Path::new("A/x"), File), Ignored::Rule);
}

#[test]
fn a_rules_file_renamed_only_in_case_keeps_its_rules() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, RULES_FILE, "x\n");
    write(root, "x", "left out");
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    assert_eq!(known(&snapshot), [RULES_FILE]);

    fs::rename(root.join(RULES_FILE), root.join(".KOIIGNORE")).unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.rules, []);
    assert_eq!(known(&snapshot), [".KOIIGNORE"]);

    write(root, ".KOIIGNORE", "y\n");
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.rules, [rule("", Some("y\n"))]);
    assert_eq!(known(&snapshot), [".KOIIGNORE", "x"]);
}

#[test]
fn a_rules_file_that_cannot_be_read_at_the_first_scan_leaves_its_folder_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "keep.txt", "k");
    write(root, "sub/.koiignore", &"x\n".repeat((1 << 19) + 1));
    write(root, "sub/x", "left out");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.unreadable, [PathBuf::from("sub")]);
    assert_eq!(rescan.rules, []);
    assert_eq!(known(&snapshot), ["keep.txt", "sub"]);

    write(root, "sub/.koiignore", "x\n");
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.unreadable, Vec::<PathBuf>::new());
    assert_eq!(rescan.rules, [rule("sub", Some("x\n"))]);
    assert_eq!(rescan.changes, [created("sub/.koiignore", File)]);
    assert_eq!(known(&snapshot), ["keep.txt", "sub", "sub/.koiignore"]);
}

/// A folder whose rules were unknown is reported read once a scan lists it, also with no rules file left to read and
/// no rules changed. A folder that is gone is not reported: the caller forgets it with the folder.
#[test]
fn a_folder_whose_rules_were_unknown_is_reported_once_they_are_known() {
    for how in ["the rules file is deleted", "the folder is deleted"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "sub/.koiignore", &"x\n".repeat((1 << 19) + 1));
        write(root, "sub/x", "kept");
        let mut rules = rules();
        let mut snapshot = Snapshot::default();
        let rescan = snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        assert_eq!(rescan.unreadable, [PathBuf::from("sub")], "{how}");
        assert_eq!(rescan.read, [PathBuf::new()], "{how}");

        let sub = root.join("sub");
        match how {
            "the folder is deleted" => fs::remove_dir_all(sub).unwrap(),
            _ => fs::remove_file(sub.join(".koiignore")).unwrap(),
        }
        let rescan = snapshot
            .rescan_with_rules(root, whole(), &mut rules)
            .unwrap();
        assert_eq!(rescan.unreadable, Vec::<PathBuf>::new(), "{how}");
        assert_eq!(rescan.rules, [], "{how}");
        let read: &[&str] = match how {
            "the folder is deleted" => &[""],
            _ => &["", "sub"],
        };
        assert_eq!(
            rescan.read,
            read.iter().map(PathBuf::from).collect::<Vec<_>>(),
            "{how}"
        );
    }
}

#[test]
fn scans_without_rules_leave_out_nothing_but_the_private_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, RULES_FILE, "ig/\n");
    write(root, "ig/x", "x");
    write(root, ".DS_Store", "finder");
    write(root, ".koidrive/space.json", "{}");
    let mut snapshot = Snapshot::default();
    let rescan = snapshot.rescan(root, whole()).unwrap();
    assert_eq!(known(&snapshot), [".DS_Store", RULES_FILE, "ig", "ig/x"]);
    assert_eq!(rescan.rules, []);
}

/// On a case-sensitive volume `proj` and `PROJ` are two folders. A scan that reads only `proj` says nothing about
/// `PROJ`, whose rules stay unknown.
#[cfg(target_os = "linux")]
#[test]
fn reading_a_folder_leaves_its_unread_namesake_in_another_case_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "proj/ok", "ok");
    write(root, "PROJ/.koiignore", &"x\n".repeat((1 << 19) + 1));
    write(root, "PROJ/x", "unknown rules apply here");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(first.unreadable, [PathBuf::from("PROJ")]);
    let partial = (snapshot.rescan_with_rules(root, [PathBuf::from("proj")], &mut rules)).unwrap();
    assert!(partial.unreadable.is_empty());
    assert_eq!(partial.rules, [], "only proj was read");
}

/// On a case-sensitive volume a folder can hold `.koiignore` and `.KOIIGNORE`, two rules files. Which of them other
/// devices would see is not known, so the folder's rules are unknown until one of them is gone.
#[cfg(target_os = "linux")]
#[test]
fn a_folder_with_two_rules_files_that_differ_only_in_case_is_unknown_until_one_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "sub/.koiignore", "a\n");
    write(root, "sub/.KOIIGNORE", "b\n");
    write(root, "sub/b", "left out by .KOIIGNORE only");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(first.unreadable, [PathBuf::from("sub")]);
    assert_eq!(first.read, [PathBuf::new()]);
    assert_eq!(known(&snapshot), ["sub"]);

    fs::remove_file(root.join("sub/.KOIIGNORE")).unwrap();
    let resolved = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(resolved.unreadable.is_empty());
    assert_eq!(resolved.read, [PathBuf::new(), PathBuf::from("sub")]);
    assert_eq!(resolved.rules, [rule("sub", Some("a\n"))]);
    assert_eq!(known(&snapshot), ["sub", "sub/.koiignore", "sub/b"]);
}

/// On a case-sensitive volume a folder can hold a folder named `.koiignore` beside the regular file `.KOIIGNORE`. Only
/// the regular file is a rules file, so its rules are in force.
#[cfg(target_os = "linux")]
#[test]
fn a_folder_named_like_the_rules_file_does_not_hide_a_rules_file_in_another_case() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "sub/.koiignore/x", "x");
    write(root, "sub/.KOIIGNORE", "b\n");
    write(root, "sub/b", "left out by .KOIIGNORE");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(rescan.unreadable.is_empty());
    assert_eq!(rescan.rules, [rule("sub", Some("b\n"))]);
    assert_eq!(
        known(&snapshot),
        [
            "sub",
            "sub/.KOIIGNORE",
            "sub/.koiignore",
            "sub/.koiignore/x"
        ]
    );
}

#[test]
fn review_renaming_an_unread_folder_and_removing_its_rules_reports_the_current_path_known() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "proj/.koiignore", &"x\n".repeat((1 << 19) + 1));
    write(root, "proj/ok", "ok");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(first.unreadable, [PathBuf::from("proj")]);
    fs::rename(root.join("proj"), root.join("PROJ")).unwrap();
    fs::remove_file(root.join("PROJ/.koiignore")).unwrap();
    let recovered = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(recovered.unreadable.is_empty());
    assert!(
        recovered.read.iter().any(|dir| dir == Path::new("PROJ")),
        "current folder never reported known: {:?}",
        recovered.read
    );
}

/// Polls until every change of `expected` and every rules change of `rules`
/// was seen, and returns the polls.
fn expect(
    watcher: &mut dyn Watcher,
    expected: &[Change],
    rules: &[(PathBuf, Option<Vec<u8>>)],
) -> Vec<Poll> {
    let deadline = Instant::now() + WAIT;
    let mut polls: Vec<Poll> = Vec::new();
    loop {
        let changes = || polls.iter().flat_map(|poll| &poll.changes);
        let reported = || polls.iter().flat_map(|poll| &poll.rules);
        if expected.iter().all(|change| changes().any(|c| c == change))
            && rules.iter().all(|rule| reported().any(|r| r == rule))
        {
            return polls;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected:?} and {rules:?}; saw {polls:?}"
        );
        polls.push(watcher.poll(Duration::from_millis(500)).unwrap());
    }
}

#[test]
fn native_watcher_reports_a_rules_change_and_does_not_rescan_below_an_ignored_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(&root, "ig/old.txt", "o");
    write(&root, "keep.txt", "k");
    let mut watcher = NativeWatcher::start_with_rules(&root, Snapshot::default(), rules()).unwrap();
    expect(
        &mut watcher,
        &[
            created("ig", Dir),
            created("ig/old.txt", File),
            created("keep.txt", File),
        ],
        &[],
    );

    write(&root, RULES_FILE, "ig/\n");
    let polls = expect(
        &mut watcher,
        &[created(RULES_FILE, File)],
        &[rule("", Some("ig/\n"))],
    );
    let changes: Vec<&Change> = polls.iter().flat_map(|poll| &poll.changes).collect();
    assert_eq!(changes, [&created(RULES_FILE, File)]);
    assert_eq!(known(watcher.snapshot()), [RULES_FILE, "keep.txt"]);

    // Events from before the rule may still arrive, so let them pass.
    while watcher
        .poll(Duration::from_millis(300))
        .unwrap()
        .root
        .is_some()
    {}
    write(&root, "ig/new.txt", "n");
    write(&root, "ig/sub/deep.txt", "d");
    for _ in 0..3 {
        let poll = watcher.poll(Duration::from_millis(300)).unwrap();
        assert_eq!(poll, Poll::default(), "an event below `ig` caused a scan");
    }

    write(&root, "later.txt", "l");
    expect(&mut watcher, &[created("later.txt", File)], &[]);
    assert_eq!(
        known(watcher.snapshot()),
        [RULES_FILE, "keep.txt", "later.txt"]
    );
}

#[test]
fn native_watcher_reports_a_file_moved_into_an_ignored_folder_as_ignored() {
    let (dir, snapshot, _) = tree_with_an_ignored_folder();
    let root = dir.path().canonicalize().unwrap();
    let id = snapshot.entries[Path::new("f.txt")].id;
    let mut watcher = NativeWatcher::start_with_rules(&root, snapshot, rules()).unwrap();
    expect(&mut watcher, &[], &[rule("", Some("ig/\n"))]);

    fs::rename(root.join("f.txt"), root.join("ig/f.txt")).unwrap();
    let polls = expect(
        &mut watcher,
        &[Change::Ignored {
            from: "f.txt".into(),
            to: "ig/f.txt".into(),
            kind: File,
            id,
        }],
        &[],
    );
    assert_eq!(polls.iter().flat_map(|poll| &poll.changes).count(), 1);
}

#[test]
fn rescan_watcher_applies_the_rules_and_reports_them() {
    let (dir, snapshot, _) = tree_with_an_ignored_folder();
    let root = dir.path();
    let id = snapshot.entries[Path::new("f.txt")].id;
    let mut watcher =
        RescanWatcher::with_rules(root, snapshot, Duration::from_millis(20), rules()).unwrap();
    let poll = watcher.poll(Duration::from_secs(5)).unwrap();
    assert_eq!(poll.changes, []);
    assert_eq!(poll.rules, [rule("", Some("ig/\n"))]);

    fs::rename(root.join("f.txt"), root.join("ig/f.txt")).unwrap();
    write(root, "ig/more.txt", "m");
    let poll = watcher.poll(Duration::from_secs(5)).unwrap();
    assert_eq!(
        poll.changes,
        [Change::Ignored {
            from: "f.txt".into(),
            to: "ig/f.txt".into(),
            kind: File,
            id,
        }]
    );
    assert_eq!(poll.rules, []);
    assert_eq!(known(watcher.snapshot()), [RULES_FILE, "d", "d/g.txt"]);
}

/// The inodes this process watches with inotify, from `/proc`.
#[cfg(target_os = "linux")]
fn watched_inodes() -> std::collections::BTreeSet<u64> {
    let mut inodes = std::collections::BTreeSet::new();
    for fd in fs::read_dir("/proc/self/fdinfo").unwrap() {
        let Ok(info) = fs::read_to_string(fd.unwrap().path()) else {
            continue;
        };
        for line in info.lines().filter(|line| line.starts_with("inotify ")) {
            let inode = line
                .split_whitespace()
                .find_map(|field| field.strip_prefix("ino:"))
                .unwrap();
            inodes.insert(u64::from_str_radix(inode, 16).unwrap());
        }
    }
    inodes
}

#[cfg(target_os = "linux")]
#[test]
fn no_inotify_watch_is_registered_below_an_ignored_folder() {
    use std::os::unix::fs::MetadataExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(&root, RULES_FILE, "ig/\n");
    write(&root, "ig/deep/x", "x");
    write(&root, "keep/sub/y", "y");
    let inode = |path: &str| fs::metadata(root.join(path)).unwrap().ino();
    let watched = |path: &str| watched_inodes().contains(&inode(path));

    let mut watcher = NativeWatcher::start_with_rules(&root, Snapshot::default(), rules()).unwrap();
    expect(&mut watcher, &[created("keep/sub/y", File)], &[]);
    for path in ["", "keep", "keep/sub"] {
        assert!(watched(path), "{path:?} is not watched");
    }
    for path in ["ig", "ig/deep"] {
        assert!(!watched(path), "{path:?} is watched");
    }

    write(&root, "keep/new/ig/deep/x", "x");
    write(&root, "keep/new/in/z", "z");
    fs::create_dir(root.join("ig/later")).unwrap();
    expect(&mut watcher, &[created("keep/new/in/z", File)], &[]);
    for path in ["keep/new", "keep/new/in"] {
        assert!(watched(path), "{path:?} is not watched");
    }
    for path in ["ig", "ig/later", "keep/new/ig", "keep/new/ig/deep"] {
        assert!(!watched(path), "{path:?} is watched");
    }

    write(&root, "keep/new/in/late", "l");
    expect(&mut watcher, &[created("keep/new/in/late", File)], &[]);
}

/// The driver resets a snapshot to what L holds (design §2.8), and L keeps the
/// entries of excluded nodes.
#[test]
fn a_reset_to_entries_the_rules_leave_out_reports_nothing_for_them() {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    let moved = snapshot.entries[Path::new("f.txt")].id;
    fs::rename(root.join("f.txt"), root.join("ig/f.txt")).unwrap();
    let mut held: BTreeMap<PathBuf, Known> = (Snapshot::scan(root).unwrap().entries.iter())
        .map(|(path, entry)| {
            (
                path.clone(),
                Known {
                    kind: entry.kind,
                    id: entry.id,
                },
            )
        })
        .collect();
    assert!(held.contains_key(Path::new("ig/deep")));
    held.remove(Path::new("ig/f.txt"));
    held.insert(
        "f.txt".into(),
        Known {
            kind: File,
            id: moved,
        },
    );

    snapshot.reset(Path::new(""), held);
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            Change::Ignored {
                from: "f.txt".into(),
                to: "ig/f.txt".into(),
                kind: File,
                id: moved
            },
            Change::Modified {
                path: RULES_FILE.into()
            },
            Change::Modified {
                path: "d/g.txt".into()
            },
        ]
    );
    assert_eq!(known(&snapshot), [RULES_FILE, "d", "d/g.txt"]);
}

/// A folder that a reset makes known is never reported as created, which is
/// how the watcher on Linux learns of a folder to register.
#[test]
fn native_watcher_reports_a_change_below_a_folder_that_a_reset_made_known() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(&root, "keep.txt", "k");
    let mut watcher = NativeWatcher::start_with_rules(&root, Snapshot::default(), rules()).unwrap();
    expect(&mut watcher, &[created("keep.txt", File)], &[]);

    fs::create_dir_all(root.join("made/sub")).unwrap();
    let known = (Snapshot::scan(&root).unwrap().entries.into_iter())
        .filter(|(path, _)| path.starts_with("made"))
        .map(|(path, entry)| {
            let (kind, id) = (entry.kind, entry.id);
            (path, Known { kind, id })
        })
        .collect();
    watcher.reset(Path::new("made"), known);
    let poll = watcher.poll(Duration::from_millis(500)).unwrap();
    assert_eq!(
        poll.changes,
        [],
        "the reset told the watcher of the folders"
    );

    write(&root, "made/sub/new.txt", "n");
    expect(&mut watcher, &[created("made/sub/new.txt", File)], &[]);
}

/// A folder moved over an empty folder while its rules file cannot be read
/// keeps the entries it had: the scan cannot list it at its new path, but what
/// lies below it is what lay below it before the move.
#[test]
fn a_folder_moved_over_an_empty_folder_with_unreadable_rules_keeps_its_entries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "drafts/.koiignore", "*.tmp\n");
    write(root, "drafts/one.txt", "one");
    write(root, "drafts/inner/two.txt", "two");
    fs::create_dir(root.join("final")).unwrap();
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    write(root, "drafts/.koiignore", &"x\n".repeat((1 << 19) + 1));
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.unreadable, Vec::<PathBuf>::new());

    fs::rename(root.join("drafts"), root.join("final")).unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            removed("final", Dir),
            Change::Renamed {
                from: "drafts".into(),
                to: "final".into(),
                kind: Dir,
            },
        ]
    );
    assert_eq!(rescan.unreadable, [PathBuf::from("final")]);
    let moved = [
        "final",
        "final/.koiignore",
        "final/inner",
        "final/inner/two.txt",
        "final/one.txt",
    ];
    assert_eq!(known(&snapshot), moved);

    write(root, "final/.koiignore", "*.tmp\n");
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [Change::Modified {
            path: "final/.koiignore".into()
        }]
    );
    assert_eq!(rescan.unreadable, Vec::<PathBuf>::new());
    assert_eq!(known(&snapshot), moved);
}

/// A file swapped through a third name with a file the rules leave out by
/// name: the file that took its path is created, and the file itself is
/// ignored, not modified into the other file.
#[test]
fn a_file_swapped_with_an_ignored_file_is_ignored_and_the_other_created() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, RULES_FILE, "old.txt\n");
    write(root, "old.txt", "o");
    write(root, "f.txt", "f");
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    let id = snapshot.entries[Path::new("f.txt")].id;
    for (from, to) in [("f.txt", "tmp"), ("old.txt", "f.txt"), ("tmp", "old.txt")] {
        fs::rename(root.join(from), root.join(to)).unwrap();
    }

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            created("f.txt", File),
            Change::Ignored {
                from: "f.txt".into(),
                to: "old.txt".into(),
                kind: File,
                id,
            }
        ]
    );
}

/// Each file moved out of the removed folder is carried by its own rename, so none of them is looked for in the
/// ignored folder. Comparing every held entry with every rename made this scan take seconds.
#[test]
#[ignore = "timing probe; run with --release -- --ignored"]
fn many_files_moved_out_of_a_removed_folder_are_renamed_in_less_than_a_second() {
    let files = 20_000;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, RULES_FILE, "ig/\n");
    write(root, "ig/deep/old.txt", "o");
    fs::create_dir(root.join("d")).unwrap();
    fs::create_dir(root.join("e")).unwrap();
    for i in 0..files {
        fs::write(root.join(format!("d/{i}")), "x").unwrap();
    }
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    for i in 0..files {
        fs::rename(root.join(format!("d/{i}")), root.join(format!("e/{i}"))).unwrap();
    }
    fs::remove_dir(root.join("d")).unwrap();

    let started = Instant::now();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let elapsed = started.elapsed();
    let renamed = (rescan.changes.iter())
        .filter(|change| matches!(change, Change::Renamed { .. }))
        .count();
    assert_eq!((rescan.changes.len(), renamed), (files + 1, files));
    assert!(
        elapsed < Duration::from_secs(1),
        "{files} files took {elapsed:?}"
    );
    eprintln!("{files} files: {elapsed:?}");
}

#[test]
fn a_folder_replaced_on_the_path_of_a_paused_search_keeps_the_move() {
    let deep: String = (0..30).map(|i| format!("c{i}/")).collect();
    for budget in (1..=8).chain(30..=50) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, RULES_FILE, "ig/\n");
        write(root, "f.txt", "f");
        write(root, &format!("ig/p/{deep}leaf"), "x");
        for i in 0..5 {
            write(root, &format!("ig/p/n{i}"), "x");
        }
        let mut rules = rules().with_search_budget(budget);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let id = snapshot.entries[Path::new("f.txt")].id;
        fs::rename(root.join("f.txt"), root.join("ig/p/f.txt")).unwrap();
        let mut paths = whole().to_vec();
        for rescan in 1.. {
            assert!(rescan < 1000, "budget {budget}: the search never ended");
            let result = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
            if !result.changes.is_empty() {
                assert_eq!(
                    result.changes,
                    [Change::Ignored {
                        from: "f.txt".into(),
                        to: "ig/p/f.txt".into(),
                        kind: File,
                        id,
                    }],
                    "budget {budget}"
                );
                break;
            }
            if rescan == 1 {
                fs::rename(root.join("ig/p"), root.join("ig/old")).unwrap();
                fs::create_dir(root.join("ig/p")).unwrap();
                fs::rename(root.join("ig/old/f.txt"), root.join("ig/p/f.txt")).unwrap();
            }
            paths = result.unsettled;
        }
    }
}

#[test]
fn a_deep_folder_renamed_while_a_search_is_paused_is_followed() {
    let deep: PathBuf = ["c"; 30].iter().collect();
    for budget in [3, 50] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, RULES_FILE, "ig/\n");
        write(root, "f.txt", "f");
        for k in 0..30 {
            let level: PathBuf = ["c"; 30].iter().take(k).collect();
            write(root, &format!("ig/chain/{}/s{k}", level.display()), "x");
        }
        fs::create_dir_all(root.join("ig/chain").join(&deep)).unwrap();
        let mut rules = rules().with_search_budget(budget);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let id = snapshot.entries[Path::new("f.txt")].id;
        fs::rename(
            root.join("f.txt"),
            root.join("ig/chain").join(&deep).join("f.txt"),
        )
        .unwrap();
        let mut paths = whole().to_vec();
        for rescan in 1.. {
            assert!(rescan < 10_000, "budget {budget}: the search never ended");
            let result = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
            if !result.changes.is_empty() {
                assert_eq!(
                    result.changes,
                    [Change::Ignored {
                        from: "f.txt".into(),
                        to: Path::new("ig/moved").join(&deep).join("f.txt"),
                        kind: File,
                        id,
                    }],
                    "budget {budget}"
                );
                break;
            }
            if rescan == 1 {
                fs::rename(root.join("ig/chain"), root.join("ig/moved")).unwrap();
            }
            paths = result.unsettled;
        }
    }
}

#[test]
fn a_scanned_folder_replaced_above_a_paused_search_keeps_the_move() {
    for budget in 1..=8 {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, RULES_FILE, "ig/\n");
        write(root, "f.txt", "f");
        write(root, "proj/keep.txt", "k");
        for i in 0..5 {
            write(root, &format!("proj/ig/p/n{i}"), "x");
        }
        let mut rules = rules().with_search_budget(budget);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        fs::rename(root.join("f.txt"), root.join("proj/ig/p/f.txt")).unwrap();
        for rescan in 1.. {
            assert!(rescan < 1000, "budget {budget}: the search never ended");
            let changes = snapshot
                .rescan_with_rules(root, whole(), &mut rules)
                .unwrap()
                .changes;
            if let Some(change) = changes.iter().find(|change| match change {
                Change::Removed { path, .. } | Change::Ignored { from: path, .. } => {
                    path == Path::new("f.txt")
                }
                _ => false,
            }) {
                assert!(
                    matches!(change, Change::Ignored { .. }),
                    "budget {budget}: {change:?}"
                );
                break;
            }
            if rescan == 1 {
                fs::rename(root.join("proj"), root.join("old")).unwrap();
                fs::create_dir_all(root.join("proj/ig/p")).unwrap();
                fs::rename(root.join("old/ig/p/f.txt"), root.join("proj/ig/p/f.txt")).unwrap();
            }
        }
    }
}

#[test]
fn an_unreadable_folder_past_the_open_folder_cap_does_not_stop_the_search() {
    use std::os::unix::fs::PermissionsExt;
    for depth in [3, 15, 16, 20, 30] {
        for budget in [50, 1000] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            write(root, RULES_FILE, "ig/\n");
            let locked: PathBuf = std::iter::once("ig")
                .chain(std::iter::repeat_n("c", depth))
                .chain(["locked"])
                .collect();
            write(root, &locked.join("x").to_string_lossy(), "x");
            write(root, "f.txt", "f");
            let mut rules = rules().with_search_budget(budget);
            let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
            fs::set_permissions(root.join(&locked), fs::Permissions::from_mode(0o000)).unwrap();
            fs::remove_file(root.join("f.txt")).unwrap();
            let mut paths = whole().to_vec();
            let mut changes = Vec::new();
            for _ in 0..1000 {
                let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
                if !rescan.changes.is_empty() {
                    changes = rescan.changes;
                    break;
                }
                paths = rescan.unsettled;
            }
            fs::set_permissions(root.join(&locked), fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(
                changes,
                [removed("f.txt", File)],
                "depth {depth} budget {budget}"
            );
        }
    }
}

#[test]
fn a_first_rescan_from_a_snapshot_another_watcher_held_does_not_finish_the_search_at_once() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "d/f.txt", "old");
    let mut held = rules().with_search_budget(100);
    let start = Snapshot::scan_with_rules(root, &mut held).unwrap();
    let id = start.entries[Path::new("d/f.txt")].id;
    let mut a = RescanWatcher::with_rules(root, start, Duration::ZERO, held).unwrap();
    fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
    assert_eq!(
        a.poll(Duration::ZERO).unwrap().pending,
        [PathBuf::from("d/f.txt")]
    );
    write(root, "d/f.txt", "new");

    let mut snapshot = a.snapshot().clone();
    let mut fresh = rules().with_search_budget(100);
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut fresh)
        .unwrap();
    assert_eq!(first.changes, []);
    assert_eq!(first.unsettled, [PathBuf::from("d/f.txt")]);
    let mut changes = Vec::new();
    let mut unsettled = first.unsettled;
    while !unsettled.is_empty() {
        let rescan = snapshot
            .rescan_with_rules(root, whole(), &mut fresh)
            .unwrap();
        changes.extend(rescan.changes);
        unsettled = rescan.unsettled;
    }
    changes.sort_by_key(|change| matches!(change, Change::Ignored { .. }));
    assert_eq!(
        changes,
        [
            created("d/f.txt", File),
            Change::Ignored {
                from: "d/f.txt".into(),
                to: "ig/zz/deep/f.txt".into(),
                kind: File,
                id,
            },
        ]
    );
}

fn save(root: &Path, path: &str, body: &str) {
    let temp = format!("{path}.tmp");
    write(root, &temp, body);
    fs::rename(root.join(&temp), root.join(path)).unwrap();
}

fn settle(snapshot: &mut Snapshot, root: &Path, rules: &mut Rules) -> Vec<Change> {
    let mut changes = Vec::new();
    for _ in 0..1000 {
        let rescan = snapshot.rescan_with_rules(root, whole(), rules).unwrap();
        changes.extend(rescan.changes);
        if rescan.unsettled.is_empty() {
            return changes;
        }
    }
    panic!("the search never ended: {changes:?}");
}

#[test]
fn a_save_by_rename_while_no_watcher_ran_is_a_modification() {
    for renamed in [false, true] {
        let dir = large_ignored_tree();
        let root = dir.path();
        write(root, "d/f.txt", "old");
        let mut before = rules().with_search_budget(100);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut before).unwrap();
        save(root, "d/f.txt", "new");
        let mut after = rules().with_search_budget(100);
        let first = snapshot
            .rescan_with_rules(root, whole(), &mut after)
            .unwrap();
        assert_eq!(first.changes, [], "{renamed}");
        if renamed {
            fs::rename(root.join("d"), root.join("e")).unwrap();
        }
        let changes = settle(&mut snapshot, root, &mut after);
        let expected = match renamed {
            false => vec![Change::Modified {
                path: "d/f.txt".into(),
            }],
            true => vec![
                Change::Renamed {
                    from: "d".into(),
                    to: "e".into(),
                    kind: Dir,
                },
                Change::Modified {
                    path: "e/f.txt".into(),
                },
            ],
        };
        assert_eq!(changes, expected, "{renamed}");
    }
}

#[test]
fn a_reset_after_a_move_into_an_ignored_folder_finds_it_again() {
    let dir = large_ignored_tree();
    let root = dir.path();
    write(root, "d/f.txt", "old");
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    let d = snapshot.entries[Path::new("d")];
    let x = snapshot.entries[Path::new("d/f.txt")];
    fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap();
    let first = settle(&mut snapshot, root, &mut rules);
    assert!(
        matches!(first.as_slice(), [Change::Ignored { .. }]),
        "{first:?}"
    );
    write(root, "d/f.txt", "new");
    let mut watcher = RescanWatcher::with_rules(root, snapshot, Duration::ZERO, rules).unwrap();
    let known: BTreeMap<PathBuf, Known> = [
        (
            PathBuf::from("d"),
            Known {
                kind: Dir,
                id: d.id,
            },
        ),
        (
            PathBuf::from("d/f.txt"),
            Known {
                kind: File,
                id: x.id,
            },
        ),
    ]
    .into();
    watcher.reset(Path::new("d"), known);
    let mut changes = Vec::new();
    loop {
        let poll = watcher.poll(Duration::ZERO).unwrap();
        changes.extend(poll.changes);
        if poll.pending.is_empty() {
            break;
        }
    }
    changes.sort_by_key(|change| matches!(change, Change::Ignored { .. }));
    assert_eq!(
        changes,
        [
            created("d/f.txt", File),
            Change::Ignored {
                from: "d/f.txt".into(),
                to: "ig/zz/deep/f.txt".into(),
                kind: File,
                id: x.id,
            },
        ]
    );
}

#[test]
fn a_held_removal_whose_path_the_rules_then_leave_out_is_decided() {
    for rules_after in ["ig/\nd/\n", "ig/\n*.txt\n"] {
        for (moved, restarted) in [(true, true), (true, false), (false, true), (false, false)] {
            let case = format!("{rules_after:?} moved {moved} restarted {restarted}");
            let dir = large_ignored_tree();
            let root = dir.path();
            write(root, "d/f.txt", "old");
            let mut rules = rules().with_search_budget(100);
            let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
            let x = snapshot.entries[Path::new("d/f.txt")];
            match moved {
                true => fs::rename(root.join("d/f.txt"), root.join("ig/zz/deep/f.txt")).unwrap(),
                false => fs::remove_file(root.join("d/f.txt")).unwrap(),
            }
            if restarted {
                write(root, "d/f.txt", "new");
                rules = self::rules().with_search_budget(100);
            }
            let first = snapshot
                .rescan_with_rules(root, whole(), &mut rules)
                .unwrap();
            assert_eq!(first.changes, [], "{case}");
            assert_eq!(first.unsettled, [PathBuf::from("d/f.txt")], "{case}");
            if !restarted {
                write(root, "d/f.txt", "new");
            }
            write(root, RULES_FILE, rules_after);
            let decided = match moved {
                true => Change::Ignored {
                    from: "d/f.txt".into(),
                    to: "ig/zz/deep/f.txt".into(),
                    kind: File,
                    id: x.id,
                },
                false => removed("d/f.txt", File),
            };
            assert_eq!(
                settle(&mut snapshot, root, &mut rules),
                [
                    Change::Modified {
                        path: RULES_FILE.into()
                    },
                    decided
                ],
                "{case}"
            );
        }
    }
}

#[test]
fn an_entry_a_rule_left_out_is_removed_once_it_leaves_the_disk() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "*.tmp\ncache/\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::remove_dir_all(root.join("sub/cache")).unwrap();
    fs::remove_file(root.join("c.tmp")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [removed("c.tmp", File), removed("sub/cache", Dir)]
    );
    assert_eq!(
        snapshot.left_out.keys().collect::<Vec<_>>(),
        [Path::new("sub/a.tmp")]
    );
}

#[test]
fn an_entry_a_rule_left_out_moved_to_an_included_name_is_renamed() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "cache/\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("sub/cache"), root.join("sub/kept")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            Change::Renamed {
                from: "sub/cache".into(),
                to: "sub/kept".into(),
                kind: Dir,
            },
            created("sub/kept/x", Dir),
            created("sub/kept/x/y.txt", File),
        ]
    );
    assert!(snapshot.left_out.is_empty());
}

#[test]
fn a_reset_to_an_entry_the_rules_leave_out_that_is_gone_reports_it_removed() {
    let (dir, mut snapshot, mut rules) = tree_with_an_ignored_folder();
    let root = dir.path();
    let held: BTreeMap<PathBuf, Known> = (Snapshot::scan(root).unwrap().entries.iter())
        .map(|(path, entry)| {
            (
                path.clone(),
                Known {
                    kind: entry.kind,
                    id: entry.id,
                },
            )
        })
        .collect();
    fs::remove_dir_all(root.join("ig")).unwrap();

    snapshot.reset(Path::new(""), held);
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    let gone: Vec<&Change> = (rescan.changes.iter())
        .filter(|change| matches!(change, Change::Removed { .. }))
        .collect();
    assert_eq!(gone, [&removed("ig", Dir)]);
    assert_eq!(known(&snapshot), [RULES_FILE, "d", "d/g.txt", "f.txt"]);
    assert!(snapshot.left_out.is_empty());
}

#[test]
fn a_rescan_of_a_sibling_reports_an_entry_a_rule_left_out_as_removed_once_it_is_gone() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "cache/\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::remove_dir_all(root.join("sub/cache")).unwrap();
    write(root, "sub/b.txt", "edited");

    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::from("sub/b.txt")], &mut rules)
        .unwrap();
    assert_eq!(
        rescan.changes,
        [
            removed("sub/cache", Dir),
            Change::Modified {
                path: "sub/b.txt".into()
            },
        ]
    );
    assert!(snapshot.left_out.is_empty());
}

#[test]
fn an_entry_a_rule_left_out_moved_to_another_excluded_name_is_removed_once_it_is_gone() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "cache/\nother/\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("sub/cache"), root.join("sub/other")).unwrap();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::remove_dir_all(root.join("sub/other")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, [removed("sub/other", Dir)]);
    assert!(snapshot.left_out.is_empty());
}

#[test]
fn an_entry_a_rule_left_out_in_a_renamed_folder_is_removed_once_it_is_gone() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "cache/\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("sub"), root.join("moved")).unwrap();
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::remove_dir_all(root.join("moved/cache")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, [removed("moved/cache", Dir)]);
    assert!(snapshot.left_out.is_empty());
}

#[test]
fn an_entry_renamed_to_a_name_the_rules_exclude_is_removed_once_it_is_gone() {
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "cache/\n*.log\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("sub/b.txt"), root.join("sub/b.log")).unwrap();
    let moved = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(
        matches!(&moved.changes[..], [Change::Ignored { from, to, .. }] if from == Path::new("sub/b.txt") && to == Path::new("sub/b.log")),
        "{moved:?}"
    );
    fs::remove_file(root.join("sub/b.log")).unwrap();

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.changes, [removed("sub/b.log", File)]);
    assert_eq!(
        snapshot.left_out.keys().collect::<Vec<_>>(),
        [Path::new("sub/cache")]
    );
}

#[test]
fn an_entry_a_rule_left_out_in_an_unreadable_folder_is_created_once_included_and_readable() {
    use std::os::unix::fs::PermissionsExt;

    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "b.txt\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::set_permissions(root.join("sub"), fs::Permissions::from_mode(0o000)).unwrap();
    write(root, RULES_FILE, "c.txt\n");
    let blocked = snapshot.rescan_with_rules(root, whole(), &mut rules);
    fs::set_permissions(root.join("sub"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(blocked.unwrap().unreadable, [PathBuf::from("sub")]);
    assert!(!snapshot.entries.contains_key(Path::new("sub/b.txt")));
    assert!(snapshot.left_out.contains_key(Path::new("sub/b.txt")));

    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(
        rescan.changes.contains(&created("sub/b.txt", File)),
        "{rescan:?}"
    );
    assert!(snapshot.entries.contains_key(Path::new("sub/b.txt")));
    assert!(snapshot.left_out.is_empty());
}

#[test]
fn an_entry_a_rule_left_out_in_a_renamed_unreadable_folder_is_created_once_included() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, mut snapshot, mut rules) = tree_without_rules();
    let root = dir.path();
    write(root, RULES_FILE, "b.txt\n");
    snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    fs::rename(root.join("sub"), root.join("moved")).unwrap();
    fs::set_permissions(root.join("moved"), fs::Permissions::from_mode(0o000)).unwrap();
    write(root, RULES_FILE, "c.txt\n");
    let blocked = snapshot.rescan_with_rules(root, whole(), &mut rules);
    fs::set_permissions(root.join("moved"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(blocked.unwrap().unreadable, [PathBuf::from("moved")]);
    assert!(!snapshot.entries.contains_key(Path::new("moved/b.txt")));
    assert!(snapshot.left_out.contains_key(Path::new("moved/b.txt")));
    assert!(!snapshot.left_out.contains_key(Path::new("sub/b.txt")));
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(
        rescan.changes.contains(&created("moved/b.txt", File)),
        "{rescan:?}"
    );
}
