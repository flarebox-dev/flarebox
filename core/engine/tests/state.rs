// SPDX-License-Identifier: Apache-2.0

//! Exporting and restoring the state the store keeps (docs/design/sync-engine.md section 2.8). A
//! valid state restores to the same value, and a state that breaks one invariant is refused with
//! the error that names it.

use flarebox_engine::classify::Trees;
use flarebox_engine::commit::{Commits, Halt, InFlight, MAX_OPS};
use flarebox_engine::effects::{Effect, EffectRecord, Effects, Location};
use flarebox_engine::state::{CommitsState, EffectsState, StateError, TreesState};
use flarebox_engine::tree::{FileIdentity, Kind, LocalNode, RemoteNode, Shape, TreeError};
use flarebox_proto::v1::{Commit, CommitResult, Delete, Move, Op, OpResult, op};

const ROOT: &str = "root";

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

/// Trees as the store loads them, with `a` placed and `b` parked.
fn trees() -> Trees {
    let mut trees = Trees::new(ROOT);
    for (ino, (id, parked)) in [("a", false), ("b", true)].into_iter().enumerate() {
        let shape = dir(ROOT, id);
        let identity = FileIdentity {
            dev: 1,
            ino: ino as u64,
            birth_ns: Some(0),
        };
        trees.set_remote(id.into(), remote(shape.clone()));
        trees.set_local(
            id.into(),
            LocalNode {
                shape: shape.clone(),
                identity,
                parked,
            },
        );
        trees.set_synced(id.into(), Some(shape));
    }
    trees
}

fn trees_state() -> TreesState {
    TreesState {
        dirty: ["b".into()].into(),
        unsearched: ["b".into()].into(),
        reverted: ["a".into()].into(),
        parked_as: [("b".into(), "c".into())].into(),
        waiting: [("a".into(), Some(dir(ROOT, "x")))].into(),
    }
}

fn commits_state() -> CommitsState {
    let delete = Delete {
        node_id: "d".into(),
    };
    let moved = Move {
        node_id: "m".into(),
        new_parent_id: ROOT.into(),
        new_name: "n".into(),
    };
    let ops = vec![
        Op {
            op: Some(op::Op::Delete(delete)),
        },
        Op {
            op: Some(op::Op::Move(moved)),
        },
    ];
    CommitsState {
        accepted: 5,
        in_flight: Some(InFlight {
            commit: Commit {
                commit_id: "commit".into(),
                device_seq: 6,
                ops,
                ..Default::default()
            },
            deletes: [("d".into(), vec!["d".into(), "e".into()])].into(),
            result: Some(CommitResult {
                results: vec![OpResult::default(); 2],
                ..Default::default()
            }),
        }),
        problems: ["p".into()].into(),
        halted: Some(Halt::Recovery),
        cap: Some(4),
        split: ["s".into()].into(),
        aliases: [("f".into(), "g".into())].into(),
    }
}

fn effects_state() -> EffectsState {
    let create = Effect::CreateDir {
        at: Location::Parked("a".into()),
    };
    EffectsState {
        in_flight: Some(EffectRecord {
            effect_id: "effect".into(),
            node: "a".into(),
            counter: 4,
            effect: create.clone(),
            after: Some(dir(ROOT, "a")),
        }),
        cleared: 3,
        failed: [("b".into(), create)].into(),
    }
}

/// `value` changed by `change`.
fn broken<T>(mut value: T, change: impl FnOnce(&mut T)) -> T {
    change(&mut value);
    value
}

#[test]
fn trees_restore_a_valid_state_and_refuse_each_broken_one() {
    let mut restored = trees();
    assert_eq!(restored.restore(trees_state()), Ok(()));
    assert_eq!(restored.state(), trees_state());

    let orphan = broken(trees(), |trees| {
        trees.set_remote("o".into(), remote(dir("gone", "o")));
    });
    let cases = [
        (
            "a node of R whose parent is missing",
            orphan,
            trees_state(),
            StateError::Tree('R', TreeError::MissingParent("o".into())),
        ),
        (
            "an unsearched node that is not dirty",
            trees(),
            broken(trees_state(), |state| {
                state.unsearched.insert("a".into());
            }),
            StateError::UnsearchedNotDirty("a".into()),
        ),
        (
            "a parked name for a placed node",
            trees(),
            broken(trees_state(), |state| {
                state.parked_as.insert("a".into(), "z".into());
            }),
            StateError::NotParked("a".into()),
        ),
    ];
    for (name, mut trees, state, error) in cases {
        assert_eq!(trees.restore(state), Err(error), "{name}");
    }
}

#[test]
fn commits_restore_a_valid_state_and_refuse_each_broken_one() {
    let restored = Commits::from_state(commits_state()).map(|commits| commits.state());
    assert_eq!(restored, Ok(commits_state()));

    let flight = |change: fn(&mut InFlight)| {
        broken(commits_state(), |state| {
            change(state.in_flight.as_mut().unwrap());
        })
    };
    let cases = [
        (
            "a commit in flight that does not take the next slot",
            CommitsState {
                accepted: 6,
                ..commits_state()
            },
            StateError::DeviceSeq {
                accepted: 6,
                device_seq: 6,
            },
        ),
        (
            "a result with an outcome missing",
            flight(|flight| {
                flight.result.as_mut().unwrap().results.pop();
            }),
            StateError::ResultMismatch,
        ),
        (
            "planned deletes for a moved node",
            flight(|flight| {
                flight.deletes.insert("m".into(), vec!["m".into()]);
            }),
            StateError::DeleteWithoutOp("m".into()),
        ),
        (
            "a cap of 0",
            CommitsState {
                cap: Some(0),
                ..commits_state()
            },
            StateError::Cap(0),
        ),
        (
            "a cap of MAX_OPS",
            CommitsState {
                cap: Some(MAX_OPS),
                ..commits_state()
            },
            StateError::Cap(MAX_OPS),
        ),
        (
            "a folder merged into itself",
            broken(commits_state(), |state| {
                state.aliases.insert("h".into(), "h".into());
            }),
            StateError::SelfAlias("h".into()),
        ),
    ];
    for (name, state, error) in cases {
        assert_eq!(Commits::from_state(state), Err(error), "{name}");
    }
}

#[test]
fn effects_restore_a_valid_state_and_refuse_each_broken_one() {
    let restored = Effects::from_state(effects_state()).map(|effects| effects.state());
    assert_eq!(restored, Ok(effects_state()));

    let mut state = effects_state();
    state.cleared = 4;
    let error = StateError::EffectCounter {
        cleared: 4,
        counter: 4,
    };
    assert_eq!(Effects::from_state(state), Err(error));
}

/// An entry on a filesystem that reports no birth time keeps an unknown birth time through a
/// restore, and its identity never equals one with a known birth time.
#[test]
fn an_identity_without_a_birth_time_round_trips() {
    let unknown = FileIdentity {
        dev: 1,
        ino: 7,
        birth_ns: None,
    };
    assert_ne!(
        unknown,
        FileIdentity {
            birth_ns: Some(0),
            ..unknown
        }
    );
    let shape = dir(ROOT, "u");
    let mut trees = Trees::new(ROOT);
    trees.set_remote("u".into(), remote(shape.clone()));
    trees.set_local(
        "u".into(),
        LocalNode {
            shape: shape.clone(),
            identity: unknown,
            parked: false,
        },
    );
    trees.set_synced("u".into(), Some(shape));
    let mut loaded = Trees::new(ROOT);
    for (id, node) in trees.remote().iter() {
        loaded.set_remote(id.clone(), node.clone());
    }
    for (id, node) in trees.local().iter() {
        loaded.set_local(id.clone(), node.clone());
    }
    for (id, shape) in trees.synced().iter() {
        loaded.set_synced(id.clone(), Some(shape.clone()));
    }
    assert_eq!(loaded.restore(trees.state()), Ok(()));
    assert_eq!(loaded, trees);
    assert_eq!(
        loaded.local().get("u").map(|node| node.identity),
        Some(unknown)
    );

    let state = EffectsState {
        in_flight: Some(EffectRecord {
            effect_id: "effect".into(),
            node: "u".into(),
            counter: 1,
            effect: Effect::Move {
                from: Location::Parked("u".into()),
                identity: unknown,
                to: Location::Parked("v".into()),
            },
            after: None,
        }),
        cleared: 0,
        failed: [(
            "w".into(),
            Effect::RemoveDir {
                at: Location::Parked("w".into()),
                identity: unknown,
            },
        )]
        .into(),
    };
    let restored = Effects::from_state(state.clone()).map(|effects| effects.state());
    assert_eq!(restored, Ok(state));
}
