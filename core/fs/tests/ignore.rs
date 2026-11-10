// SPDX-License-Identifier: Apache-2.0

//! Scans and watchers that apply the rules of design §5.10, with the matcher
//! of `flarebox-ignore` over the engine's name key.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use flarebox_fs::{
    Change, EntryKind, Ignore, Ignored, Known, NativeWatcher, Poll, RULES_FILE, RescanWatcher,
    Rules, Snapshot, Watcher,
};
use flarebox_ignore::{Kind, LocalRules, Matcher, NameKey, Reason, RuleFile, Status};

use EntryKind::{Dir, File};

const WAIT: Duration = Duration::from_secs(10);

struct Folded;

impl NameKey for Folded {
    fn key(name: &str) -> String {
        flarebox_engine::names::name_key(name)
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
    write(root, "docs/.fbignore", "draft\n");
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
            "docs/.fbignore",
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
    write(root, "ig/.fbignore", "x\n");
    write(root, "ig/x", "x");
    write(root, "ig/y", "y");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.rules, [rule("ig", Some("x\n"))]);
    assert_eq!(known(&snapshot), ["ig", "ig/.fbignore", "ig/y"]);

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
    assert_eq!(known(&snapshot), ["ig", "ig/.fbignore", "ig/y"]);
}

#[test]
fn a_symbolic_link_or_a_folder_named_like_a_rules_file_is_not_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    write(dir.path(), "outside-rules", "x\n");
    write(&root, "linked/x", "x");
    write(&root, "folder/.fbignore/x", "x");
    write(&root, "folder/x", "x");
    std::os::unix::fs::symlink(
        dir.path().join("outside-rules"),
        root.join("linked/.fbignore"),
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
            "folder/.fbignore",
            "folder/.fbignore/x",
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
    write(root, "a/.fbignore", "x\n");
    write(root, "a/x", "x");
    write(root, "a/y", "y");
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    assert_eq!(known(&snapshot), ["a", "a/.fbignore", "a/y"]);

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
    assert_eq!(known(&snapshot), ["b", "b/.fbignore", "b/y"]);

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
    write(root, "a/.fbignore", "x\n");
    write(root, "a/x", "left out");
    let mut rules = rules();
    let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
    assert_eq!(known(&snapshot), ["a", "a/.fbignore"]);

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
        assert_eq!(known(&snapshot), ["A", "A/.fbignore"]);
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

    fs::rename(root.join(RULES_FILE), root.join(".FBIGNORE")).unwrap();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.rules, []);
    assert_eq!(known(&snapshot), [".FBIGNORE"]);

    write(root, ".FBIGNORE", "y\n");
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.rules, [rule("", Some("y\n"))]);
    assert_eq!(known(&snapshot), [".FBIGNORE", "x"]);
}

#[test]
fn a_rules_file_that_cannot_be_read_at_the_first_scan_leaves_its_folder_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "keep.txt", "k");
    write(root, "sub/.fbignore", &"x\n".repeat((1 << 19) + 1));
    write(root, "sub/x", "left out");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.unreadable, [PathBuf::from("sub")]);
    assert_eq!(rescan.rules, []);
    assert_eq!(known(&snapshot), ["keep.txt", "sub"]);

    write(root, "sub/.fbignore", "x\n");
    let rescan = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(rescan.unreadable, Vec::<PathBuf>::new());
    assert_eq!(rescan.rules, [rule("sub", Some("x\n"))]);
    assert_eq!(rescan.changes, [created("sub/.fbignore", File)]);
    assert_eq!(known(&snapshot), ["keep.txt", "sub", "sub/.fbignore"]);
}

/// A folder whose rules were unknown is reported read once a scan lists it, also with no rules file left to read and
/// no rules changed. A folder that is gone is not reported: the caller forgets it with the folder.
#[test]
fn a_folder_whose_rules_were_unknown_is_reported_once_they_are_known() {
    for how in ["the rules file is deleted", "the folder is deleted"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "sub/.fbignore", &"x\n".repeat((1 << 19) + 1));
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
            _ => fs::remove_file(sub.join(".fbignore")).unwrap(),
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
    write(root, ".flarebox/space.json", "{}");
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
    write(root, "PROJ/.fbignore", &"x\n".repeat((1 << 19) + 1));
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

/// On a case-sensitive volume a folder can hold `.fbignore` and `.FBIGNORE`, two rules files. Which of them other
/// devices would see is not known, so the folder's rules are unknown until one of them is gone.
#[cfg(target_os = "linux")]
#[test]
fn a_folder_with_two_rules_files_that_differ_only_in_case_is_unknown_until_one_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "sub/.fbignore", "a\n");
    write(root, "sub/.FBIGNORE", "b\n");
    write(root, "sub/b", "left out by .FBIGNORE only");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(first.unreadable, [PathBuf::from("sub")]);
    assert_eq!(first.read, [PathBuf::new()]);
    assert_eq!(known(&snapshot), ["sub"]);

    fs::remove_file(root.join("sub/.FBIGNORE")).unwrap();
    let resolved = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert!(resolved.unreadable.is_empty());
    assert_eq!(resolved.read, [PathBuf::new(), PathBuf::from("sub")]);
    assert_eq!(resolved.rules, [rule("sub", Some("a\n"))]);
    assert_eq!(known(&snapshot), ["sub", "sub/.fbignore", "sub/b"]);
}

#[test]
fn review_renaming_an_unread_folder_and_removing_its_rules_reports_the_current_path_known() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "proj/.fbignore", &"x\n".repeat((1 << 19) + 1));
    write(root, "proj/ok", "ok");
    let mut rules = rules();
    let mut snapshot = Snapshot::default();
    let first = snapshot
        .rescan_with_rules(root, whole(), &mut rules)
        .unwrap();
    assert_eq!(first.unreadable, [PathBuf::from("proj")]);
    fs::rename(root.join("proj"), root.join("PROJ")).unwrap();
    fs::remove_file(root.join("PROJ/.fbignore")).unwrap();
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
