// SPDX-License-Identifier: Apache-2.0

//! Questions the engine asks the person (`AskUser`, design §1) and their answers. The engine persists every
//! question with what it holds, in the folder's store, and asks again after a restart until it is answered (§2.8).
//! These types mirror the macOS surface in `core/ffi` (`Question`, `QuestionKind`, `Answer`), so one driver serves the
//! app and the CLI.

pub use flarebox_engine::question::{Answer, QuestionKind, RecoveryGroup};
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
pub struct HeldItem {
    pub node_id: String,
    /// The path relative to the linked folder.
    pub path: String,
    /// Files the answer affects through this item: 1 for a file, the files it holds for a folder.
    pub files: u64,
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
