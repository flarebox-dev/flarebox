// SPDX-License-Identifier: Apache-2.0

/// This device creates a lost file n again while another device does the same, and the other
/// create comes first. R holds n when the result comes, and the server refused this create with
/// ID_EXISTS. n is no problem, and the next commit sends L's name as a move.
#[test]
fn a_create_refused_because_another_device_created_the_node_again_is_no_problem() {
    let mut trees = trees(&[("n", Some(file(ROOT, "a", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
    trees.set_remote("n".into(), remote(file(ROOT, "b", 1)));
    let exists = OpResult {
        reject_reason: "ID_EXISTS".into(),
        ..outcome(op_result::Code::Rejected)
    };
    acknowledge(&mut commits, &mut trees, vec![exists]);
    assert!(commits.problems().is_empty());
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [mv("n", ROOT, "a")]);
}

#[test]
fn a_create_refused_because_another_device_created_and_deleted_the_node_again_is_no_problem() {
    let mut trees = trees(&[("n", Some(file(ROOT, "a", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert!(matches!(&ops(&commit)[..], [op::Op::CreateFile(c)] if c.node_id == "n"));
    trees.set_remote("n".into(), trashed(file(ROOT, "a", 1)));
    let exists = OpResult {
        reject_reason: "ID_EXISTS".into(),
        ..outcome(op_result::Code::Rejected)
    };
    acknowledge(&mut commits, &mut trees, vec![exists]);
    assert!(commits.problems().is_empty());
    trees.settle();
    assert_eq!(trees.class("n"), Class::Remote);
    assert!(build(&mut commits, &mut trees, &mut ids).is_none());
}

#[test]
fn a_create_refused_for_a_node_another_device_deleted_sends_an_edit_made_since() {
    let mut trees = trees(&[("n", Some(file(ROOT, "a", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_local("n".into(), local(file(ROOT, "a", 2), 1));
    trees.set_remote("n".into(), trashed(file(ROOT, "a", 1)));
    let exists = OpResult {
        reject_reason: "ID_EXISTS".into(),
        ..outcome(op_result::Code::Rejected)
    };
    acknowledge(&mut commits, &mut trees, vec![exists]);
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
}

#[test]
fn a_create_refused_for_a_node_another_device_renamed_and_deleted_follows_the_delete() {
    let mut trees = trees(&[("n", Some(file(ROOT, "a", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("n".into(), trashed(file(ROOT, "b", 1)));
    let exists = OpResult {
        reject_reason: "ID_EXISTS".into(),
        ..outcome(op_result::Code::Rejected)
    };
    acknowledge(&mut commits, &mut trees, vec![exists]);
    trees.settle();
    assert_eq!(trees.class("n"), Class::Remote);
    assert!(build(&mut commits, &mut trees, &mut ids).is_none());
}

#[test]
fn a_create_refused_for_a_node_another_device_created_edited_and_deleted_follows_the_delete() {
    let mut trees = trees(&[("n", Some(file(ROOT, "a", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_remote("n".into(), remote(file(ROOT, "a", 1)));
    trees.settle();
    trees.set_remote("n".into(), trashed(file(ROOT, "a", 2)));
    let exists = OpResult {
        reject_reason: "ID_EXISTS".into(),
        ..outcome(op_result::Code::Rejected)
    };
    acknowledge(&mut commits, &mut trees, vec![exists]);
    trees.settle();
    assert_eq!(trees.class("n"), Class::Remote);
    assert!(build(&mut commits, &mut trees, &mut ids).is_none());
}

#[test]
fn a_create_refused_for_a_node_another_device_deleted_sends_an_edit_made_since_to_its_bytes() {
    let mut trees = trees(&[("n", Some(file(ROOT, "a", 1)), None, None)]);
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    build(&mut commits, &mut trees, &mut ids).unwrap();
    trees.set_local("n".into(), local(file(ROOT, "a", 2), 1));
    trees.set_remote("n".into(), trashed(file(ROOT, "a", 2)));
    let exists = OpResult {
        reject_reason: "ID_EXISTS".into(),
        ..outcome(op_result::Code::Rejected)
    };
    acknowledge(&mut commits, &mut trees, vec![exists]);
    trees.settle();
    let commit = build(&mut commits, &mut trees, &mut ids).unwrap();
    assert_eq!(ops(&commit), [update("n", 2)]);
}
