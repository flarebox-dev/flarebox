// SPDX-License-Identifier: Apache-2.0

//! Property tests: incremental updates against evaluating everything again, the invariants of
//! design §5.10 on random trees, and the parser on arbitrary bytes.

use std::collections::BTreeMap;

use flarebox_ignore::{Change, Kind, LocalRules, Matcher, NameKey, Reason, RuleFile, Status};
use proptest::prelude::*;
use proptest::sample::Index;

struct Folded;

impl NameKey for Folded {
    fn key(name: &str) -> String {
        flarebox_engine::names::name_key(name)
    }
}

type Tree = BTreeMap<Vec<String>, Kind>;

fn name() -> impl Strategy<Value = String> {
    prop_oneof![
        8 => "[aAb][aAb.]?",
        1 => Just(".fbignore".to_owned()),
        1 => Just(".DS_Store".to_owned()),
        1 => Just(".flarebox".to_owned()),
    ]
}

fn tree() -> impl Strategy<Value = Tree> {
    prop::collection::vec((prop::collection::vec(name(), 1..5), any::<bool>()), 1..40).prop_map(
        |paths| {
            let mut tree = Tree::new();
            'paths: for (path, is_dir) in paths {
                for depth in 1..=path.len() {
                    let kind = if depth < path.len() || is_dir {
                        Kind::Dir
                    } else {
                        Kind::File
                    };
                    match tree.get(&path[..depth]) {
                        Some(Kind::File) if depth < path.len() => continue 'paths,
                        Some(_) => {}
                        None => {
                            tree.insert(path[..depth].to_vec(), kind);
                        }
                    }
                }
            }
            tree
        },
    )
}

fn line() -> impl Strategy<Value = String> {
    prop_oneof![
        proptest::sample::select(vec![
            "*",
            "a",
            "A",
            "b",
            "*.a",
            "a*",
            "/a",
            "a/",
            "!a",
            "!*",
            "!b/",
            "a/b",
            "**/b",
            "a/**",
            "?",
            "[ab]",
            "!.fbignore",
            ".fbignore",
            "!.DS_Store",
            "*/",
            "!*/",
            "b/*",
            "!b/a",
        ])
        .prop_map(str::to_owned),
        "[aAb*?!/\\[\\]]{1,5}",
    ]
}

fn rules() -> impl Strategy<Value = String> {
    prop::collection::vec(line(), 0..5).prop_map(|lines| lines.join("\n"))
}

fn dirs(tree: &Tree) -> Vec<Vec<String>> {
    std::iter::once(Vec::new())
        .chain(
            tree.iter()
                .filter(|(_, k)| **k == Kind::Dir)
                .map(|(p, _)| p.clone()),
        )
        .collect()
}

#[derive(Clone, Debug)]
enum Update {
    Rules(Index, Option<String>),
    Local(Vec<Index>, Vec<String>),
}

fn update() -> impl Strategy<Value = Update> {
    prop_oneof![
        3 => (any::<Index>(), prop::option::of(rules())).prop_map(|(d, r)| Update::Rules(d, r)),
        1 => (prop::collection::vec(any::<Index>(), 0..3), prop::collection::vec(line(), 0..3))
            .prop_map(|(i, e)| Update::Local(i, e)),
    ]
}

fn local(tree: &Tree, include: &[Index], exclude: &[String]) -> LocalRules<Folded> {
    let paths: Vec<&Vec<String>> = tree.keys().collect();
    let include: Vec<String> = include.iter().map(|i| i.get(&paths).join("/")).collect();
    LocalRules::new(&include, exclude).unwrap_or_default()
}

fn statuses(matcher: &Matcher<Folded>, tree: &Tree) -> Vec<Status> {
    tree.iter()
        .map(|(path, kind)| matcher.status(path, *kind))
        .collect()
}

fn entries(tree: &Tree) -> Vec<(Vec<String>, Kind)> {
    tree.iter().map(|(p, k)| (p.clone(), *k)).collect()
}

fn build(tree: &Tree, files: &[(Index, String)], local: LocalRules<Folded>) -> Matcher<Folded> {
    let dirs = dirs(tree);
    let mut matcher = Matcher::new(local);
    for (dir, text) in files {
        matcher.set_rules(dir.get(&dirs), Some(RuleFile::parse(text.as_bytes())), []);
    }
    matcher
}

proptest! {
    /// The changes an update reports are exactly the entries whose status flipped, and a rules
    /// file only changes entries below its directory.
    #[test]
    fn updates_report_exactly_what_changed(
        tree in tree(),
        files in prop::collection::vec((any::<Index>(), rules()), 0..4),
        updates in prop::collection::vec(update(), 1..4),
    ) {
        let mut matcher = build(&tree, &files, LocalRules::default());
        for update in updates {
            let before = statuses(&matcher, &tree);
            let (changes, dir) = match &update {
                Update::Rules(dir, text) => {
                    let dir = dir.get(&dirs(&tree)).clone();
                    let rules = text.as_ref().map(|t| RuleFile::parse(t.as_bytes()));
                    (matcher.set_rules(&dir, rules, entries(&tree)), dir)
                }
                Update::Local(include, exclude) => {
                    let local = local(&tree, include, exclude);
                    (matcher.set_local(local, entries(&tree)), Vec::new())
                }
            };
            let after = statuses(&matcher, &tree);
            let expected: Vec<Change> = tree
                .keys()
                .zip(before.iter().zip(&after))
                .filter_map(|(path, statuses)| match statuses {
                    (Status::Excluded(_), Status::Included) => Some(Change::Included { path: path.clone() }),
                    (Status::Included, Status::Excluded(reason)) => {
                        Some(Change::Excluded { path: path.clone(), reason: *reason })
                    }
                    _ => None,
                })
                .collect();
            prop_assert_eq!(&changes, &expected, "after {:?}", update);
            for change in &changes {
                let (Change::Included { path } | Change::Excluded { path, .. }) = change;
                prop_assert!(path.len() > dir.len());
                prop_assert!(path.iter().zip(&dir).all(|(a, b)| a.eq_ignore_ascii_case(b)));
            }
        }
    }

    /// Nothing below an excluded directory is included, built-in exclusions always hold, and a
    /// rules file in an included directory is included. Siblings may share a key (`A` and `a`),
    /// as on a case-sensitive volume; they are then one directory to the rules.
    #[test]
    fn exclusion_invariants_hold(
        tree in tree(),
        files in prop::collection::vec((any::<Index>(), rules()), 0..4),
        include in prop::collection::vec(any::<Index>(), 0..3),
        exclude in prop::collection::vec(line(), 0..3),
    ) {
        let local = local(&tree, &include, &exclude);
        let matcher = build(&tree, &files, local);
        let status: BTreeMap<&Vec<String>, Status> =
            tree.iter().map(|(p, k)| (p, matcher.status(p, *k))).collect();
        for (path, kind) in &tree {
            let name = path.last().unwrap().to_ascii_lowercase();
            let parent = &path[..path.len() - 1];
            let parent_included = parent.is_empty()
                || status[&parent.to_vec()] == Status::Included;
            if !parent_included {
                prop_assert_eq!(status[path], status[&parent.to_vec()]);
            } else if name == ".ds_store" || (path.len() == 1 && name == ".flarebox") {
                prop_assert_eq!(status[path], Status::Excluded(Reason::BuiltIn));
            } else if *kind == Kind::File && name == ".fbignore" {
                prop_assert_eq!(status[path], Status::Included);
            }
        }
    }

    /// Any bytes parse, without panicking, into rules that can be matched.
    #[test]
    fn any_bytes_parse(
        bytes in prop_oneof![
            prop::collection::vec(any::<u8>(), 0..400),
            prop::collection::vec(proptest::sample::select(b"*?[]!^/\\:#\n\r\t -aZ\xc3\xa9\0".to_vec()), 0..400),
        ],
        path in prop::collection::vec("[a-zA-Z*?\\[\\]!:#\\\\ é-]{1,4}", 1..4),
        is_dir in any::<bool>(),
    ) {
        let file = RuleFile::<Folded>::parse(&bytes);
        let lines = bytes.split(|&b| b == b'\n').count() as u32;
        let numbers: Vec<u32> = file.invalid_lines().iter().map(|l| l.line).collect();
        prop_assert!(numbers.windows(2).all(|w| w[0] < w[1]));
        prop_assert!(numbers.iter().all(|&n| n >= 1 && n <= lines));
        let mut matcher = Matcher::new(LocalRules::default());
        matcher.set_rules(&[] as &[&str], Some(file), []);
        let kind = if is_dir { Kind::Dir } else { Kind::File };
        let _ = matcher.status(&path, kind);
        prop_assert_eq!(matcher.status(&[".fbignore"], Kind::File), Status::Included);
    }
}
