// SPDX-License-Identifier: Apache-2.0

//! The mass-delete guard (docs/design/sync-engine.md section 6.1). It counts the local deletions
//! of the last ten minutes and holds every pending one under a question when together they would
//! remove too much of the space. It counts the removals that apply other devices' deletions the
//! same way and opens a notice when they cross a limit.

use std::collections::{BTreeMap, BTreeSet};

use crate::classify::Trees;
use crate::ids::IdGen;
use crate::question::{Answer, Held, Limit, Question, QuestionKind, Revisions};
use crate::tree::{Kind, NodeId};

/// How long a deletion counts.
pub const WINDOW_MS: u64 = 10 * 60 * 1000;

/// The most deletions the window takes without asking.
pub const MAX_DELETIONS: u64 = 1000;

/// The smallest space whose deletions are held for removing more than half of its files.
pub const MIN_SPACE_FILES: u64 = 20;

/// What the window counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub deletions: u64,
    /// The files the deletions remove.
    pub affected_files: u64,
    /// The files of the space before the deletions.
    pub space_files: u64,
}

impl Window {
    /// The limits these counts cross.
    pub fn exceeded(&self) -> Vec<Limit> {
        let mut exceeded = Vec::new();
        if self.deletions > MAX_DELETIONS {
            exceeded.push(Limit::Deletions);
        }
        if self.space_files >= MIN_SPACE_FILES && self.affected_files * 2 > self.space_files {
            exceeded.push(Limit::Share);
        }
        exceeded
    }
}

/// A deletion the window counts: when it was first planned, and the files it removed when it was
/// last planned. For another device's deletion, when this device last removed some of it, and the
/// files it removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counted {
    pub at_ms: u64,
    pub files: u64,
}

/// What [`GuardState::check`] decided for the deletions pending now.
#[derive(Debug, Default)]
pub(crate) struct Checked {
    /// The nodes the planner must leave alone.
    pub held: BTreeSet<NodeId>,
    /// The deletions the person answered with [`Answer::Restore`], by topmost node, with the nodes
    /// of the question that are still deleted. S is cleared for them, so the effects download them
    /// again.
    pub restored: BTreeMap<NodeId, Held>,
}

/// What the guard keeps between runs besides its question, which is one of the open questions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuardState {
    /// The deletions that count now, by topmost node. A deletion leaves when its window ends, when
    /// L holds its node again, and when the person answers a question about it.
    pub ledger: BTreeMap<NodeId, Counted>,
    /// Deletions the person confirmed, planned without counting until they are acknowledged.
    pub confirmed: BTreeMap<NodeId, Held>,
    /// The limits the questions of the confirmed deletions crossed. A question that holds one of
    /// them again after it changed names these limits, also when the window crosses none.
    pub confirmed_limits: BTreeSet<Limit>,
    /// The removals that applied other devices' deletions and count now, by deletion. A deletion
    /// leaves when the server no longer holds it, and when this device removed all of it and its
    /// last removal is older than the window, so a removal that stops and goes on later counts
    /// whole. None leaves by age while a notice about them is open.
    pub remote: BTreeMap<RemoteDeletion, Counted>,
    /// The deletions a dismissed notice counted. Later removals of one of them are not counted
    /// again. A deletion leaves when the server no longer holds it, or when this device removed
    /// all of it.
    pub dismissed: BTreeSet<RemoteDeletion>,
}

/// Another device's deletion: one trashing on the server, and its topmost node, the outermost node
/// it trashed that L still held when this device removed it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RemoteDeletion {
    pub top: NodeId,
    pub trash: Trash,
}

/// Which trashing a deletion is, as far as R tells.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trash {
    /// R holds the tombstones: the trash group the server gave the delete, and the seq of its
    /// commit, R's `meta_rev` of each node it trashed. A node trashed earlier keeps its group
    /// when a folder above it is deleted, and a node deleted again after a restore gets a new one.
    Group { group: String, seq: u64 },
    /// R holds no record of the removed node, as after a bootstrap that did not list the trash:
    /// the server trashed it at this seq or before, the seq up to which R holds every change.
    Unknown { no_later_than: u64 },
}

impl RemoteDeletion {
    /// The deletion a removal applies: by the record R holds of the removed node, the first of
    /// `within`, with `last_seq` the seq up to which R holds every change. Its topmost node is
    /// the outermost of `within` that R holds in the same trash group, or with no record either.
    fn of(trees: &Trees, within: &[NodeId], last_seq: u64) -> Self {
        let trash = |id: &NodeId| {
            (trees.remote().get(id).filter(|node| node.trashed)).map(|node| Trash::Group {
                group: node.trash_group.clone(),
                seq: node.meta_rev,
            })
        };
        let own = trash(&within[0]);
        let top = (within.iter()).take_while(|id| trash(id) == own).last();
        RemoteDeletion {
            top: top.expect("the removed node").clone(),
            trash: own.unwrap_or(Trash::Unknown {
                no_later_than: last_seq,
            }),
        }
    }

    /// Whether `removal`, a removal of the deletion `removed` under the folders `within`, is a
    /// removal of this deletion. Two trash groups must be the same. A deletion whose group R did
    /// not tell takes a removal under its topmost node, when that group is the same, or one this
    /// device could not tell apart: trashed at the seq R held then or before.
    fn takes(&self, removed: &RemoteDeletion, within: &[NodeId]) -> bool {
        match (&self.trash, &removed.trash) {
            (Trash::Group { group: a, .. }, Trash::Group { group: b, .. }) => a == b,
            (Trash::Unknown { no_later_than }, Trash::Group { seq, .. }) => {
                seq <= no_later_than && within.contains(&self.top)
            }
            (_, Trash::Unknown { .. }) => within.contains(&self.top),
        }
    }

    /// Whether the server no longer holds this deletion: R holds its topmost node, and no node
    /// trashed in its group, because the server restored them or lost the commit that deleted
    /// them. Restoring some of its nodes does not end it. Without a group, R holds the topmost
    /// node live or trashed by a later deletion.
    fn undone(&self, trees: &Trees) -> bool {
        let Some(node) = trees.remote().get(&self.top) else {
            return false;
        };
        match &self.trash {
            Trash::Group { group, .. } => !trees.trashes_group(group),
            Trash::Unknown { no_later_than } => !node.trashed || node.meta_rev > *no_later_than,
        }
    }

    /// Whether this device removed all of this deletion: L holds no node R holds in its trash
    /// group, and no longer holds its topmost node while that node is still this deletion's. The
    /// node is no longer this deletion's when R holds it live again, as when the server restored it
    /// on its own for a node below it, or trashed in another group, as when that restore was
    /// followed by a later delete. A delete the server applied in part, keeping a folder for a
    /// change the deleting device had not seen, trashes nodes under no common folder (spec R7),
    /// and a purge leaves R no record of the nodes L still holds under the topmost node.
    fn removed(&self, trees: &Trees) -> bool {
        let top = trees.remote().get(&self.top);
        let (group, ours) = match &self.trash {
            Trash::Group { group, .. } => (
                trees.holds_trash_group(group),
                top.is_none_or(|node| node.trashed && node.trash_group == *group),
            ),
            Trash::Unknown { .. } => (false, top.is_none_or(|node| node.trashed)),
        };
        (!ours || trees.local().get(&self.top).is_none()) && !group
    }
}

/// A removal that applied another device's deletion: the node it removed and each folder above it
/// in L that R no longer holds, innermost first, and the files it removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRemoval {
    pub within: Vec<NodeId>,
    pub files: u64,
}

impl GuardState {
    /// Counts `pending`, every deletion planning would make now, and returns the nodes the
    /// planner must leave alone. `asked` is the guard's open question. A pending deletion that is
    /// not confirmed is counted, and when the window's deletions cross a limit all of them are held
    /// under a new question. An open question holds its deletions until it is answered. When they
    /// change, because the person put a node back, a node changed remotely, or more were deleted,
    /// the ones left are held under a new question, also when they no longer cross a limit. The
    /// question ends when none is left. The person's answer waits on the question until this
    /// check: [`Answer::Delete`] confirms the deletions only when they are still exactly the ones
    /// the question listed, and is dropped otherwise. [`Answer::Restore`] restores every node of
    /// the question that is still deleted, under whichever topmost node it is now, and every
    /// other pending node stays held under a new question. A confirmed deletion stays confirmed
    /// while each of its nodes that is still pending keeps its revisions. When one changes, the
    /// deletion is held under a new question, also below the limits, rather than counted as new.
    /// It also forgets the remote deletions the server no longer holds, and the dismissed ones this
    /// device removed.
    pub(crate) fn check(
        &mut self,
        asked: &mut Option<Question>,
        pending: BTreeMap<NodeId, Held>,
        trees: &Trees,
        ids: &mut IdGen,
        now_ms: u64,
    ) -> Checked {
        self.dismissed
            .retain(|deletion| !deletion.undone(trees) && !deletion.removed(trees));
        self.remote.retain(|deletion, _| !deletion.undone(trees));
        for counted in self.ledger.values_mut() {
            counted.at_ms = counted.at_ms.min(now_ms);
        }
        self.ledger.retain(|id, counted| {
            counted.at_ms + WINDOW_MS > now_ms && trees.local().get(id).is_none()
        });
        let mut unconfirmed = BTreeSet::new();
        self.confirmed.retain(|id, held| {
            let kept = pending.get(id).is_some_and(|left| {
                (left.members.iter())
                    .all(|(node, revisions)| held.members.get(node) == Some(revisions))
            });
            if !kept {
                unconfirmed.extend(held.members.keys().cloned());
            }
            kept
        });
        let mut open: BTreeMap<NodeId, Held> = (pending.into_iter())
            .filter(|(id, _)| !self.confirmed.contains_key(id))
            .collect();
        for (id, held) in &open {
            let counted = self.ledger.entry(id.clone()).or_insert(Counted {
                at_ms: now_ms,
                files: 0,
            });
            counted.files = held.files;
        }
        let mut carried = match asked {
            Some(Question {
                kind: QuestionKind::MassDelete { exceeded, .. },
                ..
            }) => exceeded.clone(),
            _ => Vec::new(),
        };
        if !unconfirmed.is_empty() {
            carried.extend(&self.confirmed_limits);
        }
        if self.confirmed.is_empty() {
            self.confirmed_limits.clear();
        }
        let mut checked = Checked::default();
        let was_asked = match asked.take() {
            Some(question) if question.held == open && question.answer.is_none() => {
                checked.held = holds(&open);
                *asked = Some(question);
                return checked;
            }
            Some(question) if question.held == open && question.answer == Some(Answer::Delete) => {
                self.ledger.retain(|top, _| !open.contains_key(top));
                self.confirmed.extend(open);
                self.confirmed_limits.extend(carried);
                return checked;
            }
            Some(question) if question.answer == Some(Answer::Restore) => {
                let answered: BTreeSet<&NodeId> = question.holds().collect();
                let (restored, left): (BTreeSet<NodeId>, BTreeSet<NodeId>) = (open.values())
                    .flat_map(|held| held.members.keys().cloned())
                    .partition(|node| answered.contains(node));
                self.ledger.retain(|top, _| !open.contains_key(top));
                checked.restored = deletions(trees, &restored);
                open = deletions(trees, &left);
                for (id, held) in &open {
                    let counted = self.ledger.entry(id.clone()).or_insert(Counted {
                        at_ms: now_ms,
                        files: 0,
                    });
                    counted.files = held.files;
                }
                true
            }
            asked => asked.is_some(),
        };
        let held_before = was_asked
            || (open.values())
                .any(|held| held.members.keys().any(|node| unconfirmed.contains(node)));
        let window = self.window(trees);
        let mut exceeded = window.exceeded();
        if open.is_empty() || (!held_before && exceeded.is_empty()) {
            return checked;
        }
        exceeded.extend(carried);
        exceeded.sort();
        exceeded.dedup();
        checked.held = holds(&open);
        *asked = Some(Question {
            id: ids.uuid_v7(now_ms),
            asked_at_ms: now_ms,
            kind: QuestionKind::MassDelete {
                deletions: window.deletions,
                affected_files: window.affected_files,
                space_files: window.space_files,
                exceeded,
            },
            held: open,
            answer: None,
            shown: BTreeMap::new(),
        });
        checked
    }

    /// The counts of the deletions in the window. The space is its live files in S and the files
    /// of the window's deletions S no longer holds, the space before the window's deletions.
    fn window(&self, trees: &Trees) -> Window {
        let gone: u64 = (self.ledger.iter())
            .filter(|(id, _)| trees.synced().live(id).is_none())
            .map(|(_, counted)| counted.files)
            .sum();
        Window {
            deletions: self.ledger.len() as u64,
            affected_files: self.ledger.values().map(|counted| counted.files).sum(),
            space_files: trees.synced_files() + gone,
        }
    }

    /// Counts `removal` and returns the counts of the removals that applied other devices'
    /// deletions in the window, or `None` for a removal of a deletion a dismissed notice counted.
    /// `last_seq` is the seq up to which R holds every change. `noticed` says a notice about them
    /// is open, so none leaves the window by age. S no longer holds the files of the removals, so
    /// the space is its live files and those.
    pub(crate) fn count_remote(
        &mut self,
        removal: RemoteRemoval,
        last_seq: u64,
        noticed: bool,
        trees: &Trees,
        now_ms: u64,
    ) -> Option<Window> {
        let removed = RemoteDeletion::of(trees, &removal.within, last_seq);
        let takes = |deletion: &RemoteDeletion| deletion.takes(&removed, &removal.within);
        let dismissed = self.dismissed.iter().any(takes);
        self.dismissed
            .retain(|deletion| !deletion.undone(trees) && !deletion.removed(trees));
        if dismissed {
            return None;
        }
        for counted in self.remote.values_mut() {
            counted.at_ms = counted.at_ms.min(now_ms);
        }
        self.remote.retain(|deletion, _| !deletion.undone(trees));
        let held = self.remote.keys().find(|deletion| takes(deletion)).cloned();
        if !noticed {
            self.remote.retain(|deletion, counted| {
                Some(deletion) == held.as_ref()
                    || !deletion.removed(trees)
                    || counted.at_ms + WINDOW_MS > now_ms
            });
        }
        let counted = (self.remote.entry(held.unwrap_or(removed))).or_insert(Counted {
            at_ms: now_ms,
            files: 0,
        });
        counted.at_ms = now_ms;
        counted.files += removal.files;
        let affected_files = self.remote.values().map(|counted| counted.files).sum();
        Some(Window {
            deletions: self.remote.len() as u64,
            affected_files,
            space_files: trees.synced_files() + affected_files,
        })
    }

    /// The person dismissed the notice: the deletions it counted are not counted again.
    pub(crate) fn dismiss_remote(&mut self) {
        self.dismissed
            .extend(std::mem::take(&mut self.remote).into_keys());
    }
}

/// Every deletion planning would make, by topmost node, from `deleted`, the nodes it would
/// delete, with the revisions R holds for each.
pub(crate) fn deletions(trees: &Trees, deleted: &BTreeSet<NodeId>) -> BTreeMap<NodeId, Held> {
    let mut children: BTreeMap<&str, Vec<&NodeId>> = BTreeMap::new();
    let mut tops = Vec::new();
    for id in deleted {
        match trees.synced().live(id).map(|shape| shape.parent.as_str()) {
            Some(parent) if deleted.contains(parent) => {
                children.entry(parent).or_default().push(id)
            }
            _ => tops.push(id),
        }
    }
    let revisions = |node: &str| {
        (trees.remote().get(node)).map(|remote| Revisions {
            content_rev: remote.content_rev,
            meta_rev: remote.meta_rev,
        })
    };
    tops.into_iter()
        .map(|top| {
            let mut members = BTreeMap::new();
            let mut below = vec![top];
            while let Some(node) = below.pop() {
                members.insert(node.clone(), revisions(node));
                below.extend(children.get(node.as_str()).into_iter().flatten());
            }
            let files = (members.keys())
                .filter(|node| {
                    trees
                        .synced()
                        .live(node)
                        .is_some_and(|s| s.kind == Kind::File)
                })
                .count() as u64;
            (top.clone(), Held { members, files })
        })
        .collect()
}

fn holds(held: &BTreeMap<NodeId, Held>) -> BTreeSet<NodeId> {
    (held.values())
        .flat_map(|held| held.members.keys().cloned())
        .collect()
}
