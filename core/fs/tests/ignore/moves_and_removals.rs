// SPDX-License-Identifier: Apache-2.0

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
