// SPDX-License-Identifier: Apache-2.0

//! The three trees and the classification table of docs/design/sync-engine.md sections 2.2 and 2.3.

use flarebox_engine::classify::{Class, Trees, classify};
use flarebox_engine::tree::{
    Content, FileIdentity, Kind, LocalNode, Names, RemoteNode, Shape, StateError, Tree, TreeError,
    same_content,
};
use flarebox_proto::v1::{NodeKind, NodeState, NodeStatus, VersionInfo};
use proptest::prelude::*;

const ROOT: &str = "root";

fn content(digest: u8, size: u64) -> Content {
    Content {
        size,
        manifest_digest: vec![digest; 32],
        mtime_ns: 1_794_268_740_123_456_789,
        ..Default::default()
    }
}

fn file(parent: &str, name: &str, digest: u8) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::File,
        content: Some(content(digest, 3)),
    }
}

fn dir(parent: &str, name: &str) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::Dir,
        content: None,
    }
}

fn remote(shape: Shape) -> RemoteNode {
    RemoteNode {
        shape,
        trashed: false,
        content_rev: 1,
        meta_rev: 1,
        trash_group: String::new(),
    }
}

fn local(shape: Shape, ino: u64) -> LocalNode {
    LocalNode {
        shape,
        identity: FileIdentity {
            dev: 1,
            ino,
            birth_ns: 0,
        },
        parked: false,
    }
}

#[test]
fn contents_compare_by_size_digest_and_executable_bit_but_not_mtime() {
    let a = content(1, 3);
    let mut touched = a.clone();
    touched.mtime_ns += 1;
    assert!(same_content(&a, &touched));
    let mut executable = a.clone();
    executable.executable = true;
    assert!(!same_content(&a, &executable));
    assert!(!same_content(&a, &content(2, 3)));
    assert!(!same_content(&a, &content(1, 4)));
}

#[test]
fn the_classification_table_of_section_2_3() {
    let s = file(ROOT, "a", 1);
    let edited = file(ROOT, "a", 2);
    let moved = file(ROOT, "b", 1);
    let cases = [
        (Some(&s), Some(&s), Some(&s), Class::InSync),
        (Some(&s), Some(&s), Some(&edited), Class::Remote),
        (Some(&s), Some(&s), None, Class::Remote),
        (Some(&edited), Some(&s), Some(&s), Class::Local),
        (None, Some(&s), Some(&s), Class::Local),
        (Some(&edited), Some(&s), Some(&edited), Class::Same),
        (None, Some(&s), None, Class::Same),
        (Some(&edited), Some(&s), Some(&moved), Class::Concurrent),
        (None, Some(&s), Some(&moved), Class::Concurrent),
        (Some(&s), None, None, Class::Local),
        (None, None, Some(&s), Class::Remote),
        (Some(&s), None, Some(&edited), Class::Concurrent),
        (None, None, None, Class::InSync),
    ];
    for (i, (l, s, r, class)) in cases.into_iter().enumerate() {
        assert_eq!(classify(l, s, r), class, "case {i}");
    }
}

#[test]
fn a_tree_rejects_a_missing_parent_a_file_parent_and_a_cycle() {
    let mut tree: Tree<Shape> = Tree::new(ROOT);
    tree.insert("d".into(), dir(ROOT, "d"));
    tree.insert("f".into(), file("d", "f", 1));
    assert_eq!(tree.check(Names::ByKey), Ok(()));

    let mut orphan = tree.clone();
    orphan.insert("x".into(), file("gone", "x", 1));
    assert_eq!(
        orphan.check(Names::ByKey),
        Err(TreeError::MissingParent("x".into()))
    );

    let mut under_file = tree.clone();
    under_file.insert("x".into(), file("f", "x", 1));
    assert_eq!(
        under_file.check(Names::ByKey),
        Err(TreeError::ParentNotDir("x".into()))
    );

    let mut cycle = tree.clone();
    cycle.insert("p".into(), dir("q", "p"));
    cycle.insert("q".into(), dir("p", "q"));
    assert!(matches!(
        cycle.check(Names::ByKey),
        Err(TreeError::Cycle(_))
    ));

    let mut with_root = tree.clone();
    with_root.insert(ROOT.into(), dir(ROOT, ""));
    assert_eq!(with_root.check(Names::ByKey), Err(TreeError::RootStored));
}

#[test]
fn r_and_s_compare_names_by_key_while_l_compares_exact_names() {
    let mut tree: Tree<Shape> = Tree::new(ROOT);
    tree.insert("a".into(), file(ROOT, "Stra\u{df}e", 1));
    tree.insert("b".into(), file(ROOT, "STRASSE", 2));
    assert!(matches!(
        tree.check(Names::ByKey),
        Err(TreeError::NameTaken { .. })
    ));
    assert_eq!(tree.check(Names::Exact), Ok(()));
    tree.insert("c".into(), file(ROOT, "STRASSE", 3));
    assert!(matches!(
        tree.check(Names::Exact),
        Err(TreeError::NameTaken { .. })
    ));
}

#[test]
fn trashed_remote_nodes_count_as_absent() {
    let mut tree: Tree<RemoteNode> = Tree::new(ROOT);
    tree.insert("d".into(), remote(dir(ROOT, "d")));
    let mut trashed = remote(file(ROOT, "d", 1));
    trashed.trashed = true;
    tree.insert("t".into(), trashed);
    let mut child = remote(file("t", "c", 1));
    child.trashed = true;
    tree.insert("c".into(), child);
    assert_eq!(tree.check(Names::ByKey), Ok(()));
    assert!(tree.live("t").is_none());
    assert!(tree.get("t").is_some());
}

#[test]
fn reads_node_states_and_refuses_kinds_this_version_does_not_sync() {
    let state = NodeState {
        node_id: "n".into(),
        parent_id: ROOT.into(),
        name: "f".into(),
        kind: NodeKind::File as i32,
        status: NodeStatus::Trashed as i32,
        content_rev: 4,
        meta_rev: 6,
        trash_group: "g".into(),
        current: Some(VersionInfo {
            content: Some(content(1, 3)),
            ..Default::default()
        }),
    };
    let (id, node) = RemoteNode::from_state(&state).unwrap();
    assert_eq!(id, "n");
    assert!(node.trashed);
    assert_eq!((node.content_rev, node.meta_rev), (4, 6));
    assert_eq!(node.shape, file(ROOT, "f", 1));

    let symlink = NodeState {
        kind: NodeKind::Symlink as i32,
        ..state.clone()
    };
    assert_eq!(
        RemoteNode::from_state(&symlink),
        Err(StateError::UnknownKind(NodeKind::Symlink as i32))
    );
    let empty = NodeState {
        current: None,
        ..state.clone()
    };
    assert_eq!(
        RemoteNode::from_state(&empty),
        Err(StateError::MissingContent)
    );
    let dir_state = NodeState {
        kind: NodeKind::Dir as i32,
        current: None,
        ..state
    };
    assert_eq!(
        RemoteNode::from_state(&dir_state).unwrap().1.shape,
        dir(ROOT, "f")
    );
}

#[test]
fn settling_records_matching_changes_and_drops_nodes_in_sync() {
    let mut trees = Trees::new(ROOT);
    let base = file(ROOT, "a", 1);
    let edited = file(ROOT, "a", 2);
    for id in ["same", "synced", "remote", "local"] {
        trees.set_synced(id.into(), Some(base.clone().with_name(id)));
        trees.set_remote(id.into(), remote(base.clone().with_name(id)));
        trees.set_local(id.into(), local(base.clone().with_name(id), 1));
    }
    trees.set_local("same".into(), local(edited.clone().with_name("same"), 1));
    trees.set_remote("same".into(), remote(edited.clone().with_name("same")));
    trees.set_remote("remote".into(), remote(edited.clone().with_name("remote")));
    trees.remove_local("local");

    let work = trees.settle();
    assert_eq!(
        work,
        [
            ("local".to_owned(), Class::Local),
            ("remote".to_owned(), Class::Remote)
        ]
    );
    assert_eq!(trees.synced().get("same"), Some(&edited.with_name("same")));
    assert_eq!(trees.dirty().len(), 2);
    assert_eq!(trees.check(), Ok(()));
    assert_eq!(trees.settle().len(), 2);
}

trait WithName {
    fn with_name(self, name: &str) -> Self;
}

impl WithName for Shape {
    fn with_name(mut self, name: &str) -> Self {
        self.name = name.into();
        self
    }
}

fn shapes() -> impl Strategy<Value = Option<Shape>> {
    prop_oneof![
        Just(None),
        (0u8..3, 0u8..2, 0u8..2).prop_map(|(digest, name, parent)| {
            let parent = if parent == 0 { ROOT } else { "d" };
            Some(file(parent, ["a", "b"][name as usize], digest))
        }),
    ]
}

proptest! {
    /// Balasubramaniam and Pierce, Definition 4.1.2 in ID form: a side that did not change takes
    /// the other side's value, and a node changed on both sides is never settled by overwriting.
    #[test]
    fn classification_takes_only_the_side_that_changed(
        l in shapes(), s in shapes(), r in shapes()
    ) {
        let eq = |a: &Option<Shape>, b: &Option<Shape>| match (a, b) {
            (Some(a), Some(b)) => a.same(b),
            (None, None) => true,
            _ => false,
        };
        let class = classify(l.as_ref(), s.as_ref(), r.as_ref());
        let expected = match (eq(&l, &s), eq(&r, &s)) {
            (true, true) => Class::InSync,
            (true, false) => Class::Remote,
            (false, true) => Class::Local,
            (false, false) if eq(&l, &r) => Class::Same,
            (false, false) => Class::Concurrent,
        };
        prop_assert_eq!(class, expected);
    }

    #[test]
    fn settling_twice_changes_nothing_more(
        nodes in proptest::collection::vec((shapes(), shapes(), shapes()), 1..8)
    ) {
        let mut trees = Trees::new(ROOT);
        for (i, (l, s, r)) in nodes.into_iter().enumerate() {
            let id = format!("n{i}");
            let with = |shape: Shape| shape.with_name(&id);
            trees.set_synced(id.clone(), s.map(with));
            if let Some(r) = r {
                trees.set_remote(id.clone(), remote(with(r)));
            }
            if let Some(l) = l {
                trees.set_local(id.clone(), local(with(l), i as u64));
            }
        }
        let first = trees.settle();
        for (id, class) in &first {
            prop_assert!(matches!(class, Class::Remote | Class::Local | Class::Concurrent));
            prop_assert_eq!(trees.class(id), *class);
        }
        let snapshot = trees.clone();
        prop_assert_eq!(trees.settle(), first);
        prop_assert_eq!(trees, snapshot);
    }
}
