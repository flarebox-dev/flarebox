// SPDX-License-Identifier: Apache-2.0

//! Questions the engine asks the person (`AskUser`, design §1) and their answers. The engine persists every
//! question with what it holds, in the folder's store, and asks again after a restart until it is answered (§2.8).
//! These types mirror the macOS surface in `core/ffi` (`Question`, `QuestionKind`, `Answer`), so one driver serves the
//! app and the CLI.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// Unique per question. A question whose held set changes is replaced by one with a new id, so an answer
    /// always refers to the set the person saw.
    pub id: String,
    pub space_id: String,
    pub asked_at_ms: i64,
    pub kind: QuestionKind,
    /// Every node the answer applies to, in path order.
    pub items: Vec<HeldItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuestionKind {
    /// The mass-delete guard held local deletions (design §6.1).
    MassDelete {
        deletions: u64,
        affected_files: u64,
        space_files: u64,
    },
    /// Files disappeared while changes were not tracked and may have moved into an ignored folder (design §5.10).
    AmbiguousDisappearance,
    /// One group of files a recovery holds (design §6.4).
    Recovery { group: RecoveryGroup },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryGroup {
    CreatedAfterRestorePoint,
    ChangedOnThisDevice,
    CreatedOnThisDevice,
    DeletedOnThisDevice,
    RequestedBeforeReset,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldItem {
    pub node_id: String,
    /// The path relative to the linked folder.
    pub path: String,
    /// Files the answer affects through this item: 1 for a file, the files it holds for a folder.
    pub files: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_round_trip_through_their_names() {
        for answer in Answer::ALL {
            assert_eq!(answer.as_str().parse::<Answer>(), Ok(answer));
            assert_eq!(
                serde_json::to_string(&answer).unwrap(),
                format!("\"{answer}\"")
            );
        }
        assert!("Delete".parse::<Answer>().is_err());
    }

    #[test]
    fn a_question_serializes_with_a_tagged_kind() {
        let question = Question {
            id: "q1".into(),
            space_id: "s1".into(),
            asked_at_ms: 5,
            kind: QuestionKind::MassDelete {
                deletions: 1_200,
                affected_files: 1_200,
                space_files: 2_000,
            },
            items: vec![HeldItem {
                node_id: "n1".into(),
                path: "a/b".into(),
                files: 1,
            }],
        };
        let json = serde_json::to_value(&question).unwrap();
        assert_eq!(json["kind"]["type"], "mass_delete");
        assert_eq!(serde_json::from_value::<Question>(json).unwrap(), question);
        assert_eq!(question.kind.answers(), [Answer::Delete, Answer::Restore]);
    }
}
