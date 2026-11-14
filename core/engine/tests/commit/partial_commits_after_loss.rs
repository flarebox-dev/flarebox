// SPDX-License-Identifier: Apache-2.0

use proptest::prelude::*;

const LOST: [(&str, &str); 3] = [("a", "n0"), ("b", "n1"), ("c", "n2")];

fn served(held: &BTreeMap<String, RemoteNode>, op: &op::Op, seq: u64) -> (String, RemoteNode) {
    let id = match op {
        op::Op::Move(moved) => &moved.node_id,
        op::Op::UpdateContent(update) => &update.node_id,
        op::Op::CreateFile(create) => {
            let shape = Shape {
                content: create.content.clone(),
                ..file(&create.parent_id, &create.name, 0)
            };
            return (create.node_id.clone(), remote_at(shape, seq));
        }
        other => panic!("a commit of these files sent {other:?}"),
    };
    let held = held.get(id).expect("R holds every file").clone();
    let node = match op {
        op::Op::Move(moved) => RemoteNode {
            shape: Shape {
                parent: moved.new_parent_id.clone(),
                name: moved.new_name.clone(),
                ..held.shape.clone()
            },
            meta_rev: seq,
            meta_edit_rev: if moved.from.is_none() { seq } else { held.meta_edit_rev },
            ..held
        },
        op::Op::UpdateContent(update) => RemoteNode {
            shape: Shape {
                content: update.content.clone(),
                ..held.shape.clone()
            },
            content_rev: seq,
            edit_rev: if update.resend { held.edit_rev } else { seq },
            version_id: format!("v{seq}"),
            ..held
        },
        _ => unreachable!(),
    };
    (id.clone(), node)
}

proptest! {
    #![proptest_config(koidrive_test_support::regression_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/commit.proptest-regressions"),
        256,
    ))]

    #[test]
    fn every_field_a_restore_lost_reaches_the_server_through_partial_commits(
        changes in prop::collection::vec((any::<bool>(), any::<bool>()), 1..=3),
        swap in any::<bool>(),
        cap in 1..=3usize,
        records_first in any::<bool>(),
        folder_lost in any::<bool>(),
        name_taken in any::<bool>(),
        edited_back in any::<bool>(),
    ) {
        let files = &LOST[..changes.len()];
        let swapped = |i: usize| swap && changes.len() >= 2 && i < 2;
        let after = |i: usize| {
            let (moved, edited) = changes[i];
            let name = if swapped(i) { files[1 - i].1 } else { files[i].1 };
            let digest = if edited && !edited_back { 10 + i as u8 } else { i as u8 };
            (moved || swapped(i), edited, file(if moved { "f" } else { ROOT }, name, digest))
        };
        let mut rows = vec![synced("f", dir(ROOT, "f"))];
        rows.extend((0..files.len()).map(|i| synced(files[i].0, after(i).2)));
        let name_taken = name_taken && changes[0].0 && !swapped(0);
        if name_taken {
            rows.push(synced("x", file(ROOT, files[0].1, 7)));
        }
        let mut trees = trees(&rows);
        if folder_lost {
            trees.set_remote("f".into(), remote_at(dir(ROOT, "f"), 3));
        }
        if name_taken {
            trees.set_remote("x".into(), remote_at(file(ROOT, files[0].1, 7), 6));
        }
        for (i, (id, _)) in files.iter().enumerate() {
            let (moved, edited, shape) = after(i);
            let rev = |changed: bool| if changed { 5 } else { 1 };
            trees.set_remote(
                (*id).into(),
                RemoteNode {
                    content_rev: rev(edited),
                    edit_rev: rev(edited),
                    version_id: format!("v{}", rev(edited)),
                    ..remote_at(shape.clone(), rev(moved))
                },
            );
            if moved {
                trees.record_moved(id, 5);
            }
            if edited {
                trees.record_edited(id, shape.content.as_ref());
            }
        }
        let moved = trees.moved_here();
        let edited = trees.edited_here();
        let taken = trees.taken_from_others(|seq| seq == 5);
        let records = trees.remote_records();
        if folder_lost {
            trees.remove_remote("f");
        }
        if name_taken {
            trees.set_listed_remote(
                "x".into(),
                RemoteNode {
                    version_id: "v1".into(),
                    ..remote(file(ROOT, "x", 7))
                },
            );
        }
        for (i, (id, name)) in files.iter().enumerate() {
            trees.set_listed_remote(
                (*id).into(),
                RemoteNode {
                    version_id: "v1".into(),
                    ..remote(file(ROOT, name, i as u8))
                },
            );
        }
        trees.forget_lost(vec![], edited, taken, moved, vec![], records, 1, 1);
        trees.settle();
        let mut seq = 1;
        if folder_lost {
            seq = 2;
            trees.set_remote(
                "f".into(),
                RemoteNode {
                    content_rev: 2,
                    edit_rev: 2,
                    ..remote_at(dir(ROOT, "f"), 2)
                },
            );
            trees.settle();
        }
        let mut commits = Commits::from_state(CommitsState {
            accepted: 1,
            slots: [(1, 1)].into(),
            cap: Some(cap),
            ..Default::default()
        })
        .unwrap();
        let mut ids = IdGen::new(1);
        while let Some(commit) = commits.build(&mut trees, &mut ids, NOW, HISTORY, seq).cloned() {
            seq += 1;
            prop_assert!(seq < 20, "the commits never ended");
            let sent = ops(&commit);
            for op in &sent {
                if let op::Op::UpdateContent(update) = op {
                    prop_assert!(!update.resend, "an edit of this device went as a resend: {op:?}");
                }
            }
            let mut held = trees.remote_records();
            let mut answered = Vec::new();
            for op in &sent {
                let (id, node) = served(&held, op, seq);
                held.insert(id.clone(), node.clone());
                answered.push((id, node));
            }
            if records_first {
                for (id, node) in &answered {
                    trees.set_remote(id.clone(), node.clone());
                }
            }
            commits
                .received(result(seq, vec![outcome(op_result::Code::Applied); sent.len()]))
                .unwrap();
            if !records_first {
                for (id, node) in &answered {
                    trees.set_remote(id.clone(), node.clone());
                }
            }
            prop_assert!(commits.acknowledge(&mut trees, seq));
            trees.settle();
        }
        for (id, local) in trees.local().iter().filter(|(id, _)| id.as_str() != "f" && id.as_str() != "x") {
            let local = &local.shape;
            let remote = &trees.remote().get(id).unwrap().shape;
            prop_assert!(
                local.same_position(remote) && local.content == remote.content,
                "the server lacks a change of {id}: L {local:?}, R {remote:?}"
            );
        }
        for (i, (id, _)) in files.iter().enumerate() {
            let remote = trees.remote().get(id).unwrap();
            prop_assert!(
                !changes[i].1 || trees.local().get(id).is_none() || remote.edit_rev > 1,
                "the server holds no edit of {id} made after the restore: {remote:?}"
            );
        }
    }
}
