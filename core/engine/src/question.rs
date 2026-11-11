// SPDX-License-Identifier: Apache-2.0

//! Questions the engine asks the person (`AskUser`, design §1): what each asks, the nodes it holds
//! until it is answered, and the answers. The planner leaves every node an open question holds
//! alone, and the answer is routed by the question's kind. With the `serde` feature the kinds and
//! answers serialize the way the driver stores and shows them.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use crate::tree::{NodeId, Shape};

/// An open question and what it holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    /// Unique per question. A question whose held nodes change is replaced by one with a new id, so
    /// an answer always refers to what the person saw.
    pub id: String,
    pub asked_at_ms: u64,
    pub kind: QuestionKind,
    /// What the question holds, by the topmost node of each item.
    pub held: BTreeMap<NodeId, Held>,
    /// An answer the engine took but applies only when it next plans, if what the question holds
    /// has not changed by then.
    pub answer: Option<Answer>,
    /// The shape L had for each held node R does not hold, when the question was asked. A change
    /// to any of them replaces the question.
    pub shown: BTreeMap<NodeId, Shape>,
}

impl Question {
    /// Every node the question holds.
    pub fn holds(&self) -> impl Iterator<Item = &NodeId> {
        self.held.values().flat_map(|held| held.members.keys())
    }
}

/// One item a question holds: its topmost node and the nodes below it that the answer applies to,
/// each with the revisions R held for it when it was asked, and the files among them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub members: BTreeMap<NodeId, Option<Revisions>>,
    pub files: u64,
}

/// A node's revisions in R. A held node R does not hold has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Revisions {
    pub content_rev: u64,
    pub meta_rev: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type", rename_all = "snake_case"))]
pub enum QuestionKind {
    /// The mass-delete guard held local deletions (design §6.1).
    MassDelete {
        deletions: u64,
        affected_files: u64,
        space_files: u64,
    },
    /// Files disappeared while changes were not tracked and may have moved into an ignored folder
    /// (design §5.10).
    AmbiguousDisappearance,
    /// One group of files a recovery holds (design §6.4).
    Recovery { group: RecoveryGroup },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum RecoveryGroup {
    CreatedAfterRestorePoint,
    ChangedOnThisDevice,
    CreatedOnThisDevice,
    DeletedOnThisDevice,
    RequestedBeforeReset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Answer {
    /// Delete the items on the server and on every device.
    Delete,
    /// Keep the items on the server and download them here again.
    Restore,
    /// Put files the rewind removed back on the server.
    Keep,
    /// Move this device's copies to the local trash and follow the server.
    Discard,
    /// Send this device's version to the server.
    Publish,
    /// Send a request again.
    Repeat,
    /// Drop a request.
    Cancel,
}

impl QuestionKind {
    /// The two answers this kind of question accepts, in the order to offer them.
    pub fn answers(&self) -> [Answer; 2] {
        match self {
            QuestionKind::MassDelete { .. } | QuestionKind::AmbiguousDisappearance => {
                [Answer::Delete, Answer::Restore]
            }
            QuestionKind::Recovery { group } => match group {
                RecoveryGroup::CreatedAfterRestorePoint => [Answer::Keep, Answer::Discard],
                RecoveryGroup::ChangedOnThisDevice | RecoveryGroup::CreatedOnThisDevice => {
                    [Answer::Publish, Answer::Discard]
                }
                RecoveryGroup::DeletedOnThisDevice => [Answer::Delete, Answer::Restore],
                RecoveryGroup::RequestedBeforeReset => [Answer::Repeat, Answer::Cancel],
            },
        }
    }
}

impl Answer {
    pub const ALL: [Answer; 7] = [
        Answer::Delete,
        Answer::Restore,
        Answer::Keep,
        Answer::Discard,
        Answer::Publish,
        Answer::Repeat,
        Answer::Cancel,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Answer::Delete => "delete",
            Answer::Restore => "restore",
            Answer::Keep => "keep",
            Answer::Discard => "discard",
            Answer::Publish => "publish",
            Answer::Repeat => "repeat",
            Answer::Cancel => "cancel",
        }
    }
}

impl fmt::Display for Answer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Answer {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Answer::ALL
            .into_iter()
            .find(|answer| answer.as_str() == text)
            .ok_or_else(|| {
                let names: Vec<&str> = Answer::ALL.iter().map(|answer| answer.as_str()).collect();
                format!("{text:?} is not an answer; use one of {}", names.join(", "))
            })
    }
}
