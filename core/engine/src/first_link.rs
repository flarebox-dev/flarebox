// SPDX-License-Identifier: Apache-2.0

//! The first link of a folder to a space, by a device with no store (design section 6.4): there is
//! no S to compare L with, so L is matched against R by path and content, and what matches nothing
//! is held under a question until the person publishes or discards it.

use std::collections::{BTreeMap, BTreeSet};

use crate::classify::Trees;
use crate::commit::rekey;
use crate::ids::IdGen;
use crate::question::{Held, Question, QuestionKind, RecoveryGroup};
use crate::tree::{Kind, NodeId, Shape, same_content};

/// Matches L against R from the root down, and returns the topmost local nodes that match
/// nothing. A local entry matches the live node of R at the same parent with
/// exactly its name and kind, and for a file the same content. A matching entry is adopted: L's
/// node takes R's ID and S takes R's shape, and a matching folder's children are matched in turn,
/// so a folder whose contents differ is still adopted. Every other entry is returned with its
/// subtree, for the user to publish or discard: a name R does not hold, a name that only has the
/// same name key, another kind, or a file with other bytes. An entry the rules exclude here is
/// neither adopted nor returned.
pub fn adopt(trees: &mut Trees) -> Vec<NodeId> {
    let remote: BTreeMap<(NodeId, String), NodeId> = (trees.remote().iter())
        .filter(|(_, node)| !node.trashed)
        .map(|(id, node)| {
            (
                (node.shape.parent.clone(), node.shape.name.clone()),
                id.clone(),
            )
        })
        .collect();
    let mut held = Vec::new();
    let mut pending = vec![trees.local().root.clone()];
    while let Some(parent) = pending.pop() {
        let mut children: Vec<(String, NodeId)> = (trees.local_children(&parent))
            .filter_map(|id| Some((trees.local().get(id)?.shape.name.clone(), id.clone())))
            .collect();
        children.sort();
        for (name, id) in children {
            if trees.excluded(&id) {
                continue;
            }
            let local = trees.local().get(&id).expect("a child in L").shape.clone();
            let found = (remote.get(&(parent.clone(), name))).map(|remote_id| {
                (
                    remote_id,
                    &trees.remote().get(remote_id).expect("indexed").shape,
                )
            });
            let Some((remote_id, shape)) = found.filter(|(_, shape)| {
                shape.kind == local.kind
                    && match (&shape.content, &local.content) {
                        (Some(remote), Some(local)) => same_content(remote, local),
                        (remote, local) => remote.is_none() && local.is_none(),
                    }
            }) else {
                held.push(id);
                continue;
            };
            let (remote_id, shape) = (remote_id.clone(), shape.clone());
            if id != remote_id {
                rekey(trees, &id, &remote_id);
            }
            trees.set_synced(remote_id.clone(), Some(shape));
            if local.kind == Kind::Dir {
                pending.push(remote_id);
            }
        }
    }
    held
}

/// Whether the question holds what a first link found on this device and nothing on the server.
pub(crate) fn asks(question: &Question) -> bool {
    question.kind
        == QuestionKind::Recovery {
            group: RecoveryGroup::CreatedOnThisDevice,
        }
}

/// The question about the local entries `tops` and what is below them, as L holds them now, or none
/// when L holds none of them. An entry inside another's subtree is one of its members, and one the
/// user moved out of its folder is an item of its own. The question is
/// `asked`, with its answer, when it would hold the same nodes with the same shapes, and a new one
/// otherwise, so that an answer acts on what the person saw.
pub(crate) fn ask(
    trees: &Trees,
    tops: impl IntoIterator<Item = NodeId>,
    asked: Option<&Question>,
    ids: &mut IdGen,
    now_ms: u64,
) -> Option<Question> {
    let mut held = BTreeMap::new();
    let mut shown = BTreeMap::new();
    for top in tops {
        let members = unsynced(trees, &top);
        if members.is_empty() {
            continue;
        }
        let files = (members.values())
            .filter(|shape| shape.kind == Kind::File)
            .count() as u64;
        let held_members = members.keys().map(|id| (id.clone(), None)).collect();
        held.insert(
            top,
            Held {
                members: held_members,
                files,
            },
        );
        shown.extend(members);
    }
    let inner: BTreeSet<NodeId> = (held.iter())
        .flat_map(|(top, item)| item.members.keys().filter(move |id| *id != top))
        .cloned()
        .collect();
    held.retain(|top, _| !inner.contains(top));
    if held.is_empty() {
        return None;
    }
    if let Some(asked) = asked.filter(|asked| asked.held == held && asked.shown == shown) {
        return Some(asked.clone());
    }
    Some(Question {
        id: ids.uuid_v7(now_ms),
        asked_at_ms: now_ms,
        kind: QuestionKind::Recovery {
            group: RecoveryGroup::CreatedOnThisDevice,
        },
        held,
        shown,
        answer: None,
    })
}

/// `top` and the nodes of L below it that neither S nor R holds, with their shapes in L. A node S
/// or R holds is synced or the server's, so neither it nor what is below it is held. An excluded
/// node below `top` is not held either, but `top` stays held while the rules leave it out, since
/// it was held before they did.
fn unsynced(trees: &Trees, top: &str) -> BTreeMap<NodeId, Shape> {
    let mut members = BTreeMap::new();
    let mut pending = vec![top.to_owned()];
    while let Some(id) = pending.pop() {
        let Some(node) = trees.local().get(&id) else {
            continue;
        };
        let known = trees.synced().get(&id).is_some() || trees.remote().get(&id).is_some();
        if known || (id != top && trees.excluded(&id)) {
            continue;
        }
        pending.extend(trees.local_children(&id).cloned());
        members.insert(id, node.shape.clone());
    }
    members
}
