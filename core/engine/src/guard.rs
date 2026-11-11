// SPDX-License-Identifier: Apache-2.0

//! The mass-delete guard (docs/design/sync-engine.md section 6.1). It counts the local deletions
//! of the last ten minutes and holds every pending one under a question when together they would
//! remove too much of the space.

use std::collections::{BTreeMap, BTreeSet};

use crate::classify::Trees;
use crate::ids::IdGen;
use crate::question::{Answer, Held, Question, QuestionKind, Revisions};
use crate::tree::{Kind, NodeId};

/// How long a deletion counts.
pub const WINDOW_MS: u64 = 10 * 60 * 1000;

/// The most deletions the window takes without asking.
pub const MAX_DELETIONS: u64 = 1000;

/// The smallest space whose deletions are held for removing more than half of its files.
pub const MIN_SPACE_FILES: u64 = 20;

/// The limit a set of deletions crossed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// More than [`MAX_DELETIONS`] deletions in the window.
    Deletions,
    /// More than half of the files of a space of at least [`MIN_SPACE_FILES`] files.
    Share,
}

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
    /// The limit these counts cross, if any.
    pub fn limit(&self) -> Option<Limit> {
        if self.deletions > MAX_DELETIONS {
            Some(Limit::Deletions)
        } else if self.space_files >= MIN_SPACE_FILES && self.affected_files * 2 > self.space_files
        {
            Some(Limit::Share)
        } else {
            None
        }
    }
}

/// A deletion the window counts: when it was first planned, and the files it removed when it was
/// last planned.
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
    pub(crate) fn check(
        &mut self,
        asked: &mut Option<Question>,
        pending: BTreeMap<NodeId, Held>,
        trees: &Trees,
        ids: &mut IdGen,
        now_ms: u64,
    ) -> Checked {
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
        if open.is_empty() || (!held_before && window.limit().is_none()) {
            return checked;
        }
        checked.held = holds(&open);
        *asked = Some(Question {
            id: ids.uuid_v7(now_ms),
            asked_at_ms: now_ms,
            kind: QuestionKind::MassDelete {
                deletions: window.deletions,
                affected_files: window.affected_files,
                space_files: window.space_files,
            },
            held: open,
            answer: None,
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
        let live = (trees.synced().iter())
            .filter(|(_, shape)| shape.kind == Kind::File)
            .count() as u64;
        Window {
            deletions: self.ledger.len() as u64,
            affected_files: self.ledger.values().map(|counted| counted.files).sum(),
            space_files: live + gone,
        }
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
