// SPDX-License-Identifier: Apache-2.0

//! The rows of the engine's open questions (`AskUser`, design §1) and of the mass-delete guard
//! (§6.1): each question with its kind, the items it holds and an answer it took but has not
//! applied yet, the deletions in the guard's window
//! and the deletions the person confirmed. Every held node keeps the revisions R held for it when
//! it was asked, or none when R did not hold it. They are engine state, part of
//! [`CommitsState`], and [`Tx::set_engine_state`] writes them with the commit or the hold that
//! changes them.
//!
//! [`CommitsState`]: flarebox_engine::state::CommitsState

use std::collections::BTreeMap;

use flarebox_engine::guard::{Counted, GuardState};
use flarebox_engine::question::{Answer, Held, Question, QuestionKind, Revisions};
use flarebox_engine::tree::NodeId;
use rusqlite::{Row, params};

use crate::{Error, Result, Tx};

/// Held items by owner: a question's ID, or the empty string for the guard's confirmed deletions.
type HeldBy = BTreeMap<String, BTreeMap<NodeId, Held>>;

impl Tx<'_> {
    pub(crate) fn open_questions(&self) -> Result<Vec<Question>> {
        let mut held = self.held(
            "SELECT question_id, top_id, files FROM open_question_items",
            "SELECT question_id, top_id, node_id, content_rev, meta_rev FROM open_question_members",
        )?;
        let mut stmt = self.0.prepare_cached(
            "SELECT question_id, asked_at_ms, kind, answer FROM open_questions ORDER BY position",
        )?;
        let mut rows = stmt.query([])?;
        let mut questions = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let kind = serde_json::from_str::<QuestionKind>(&row.get::<_, String>(2)?).map_err(
                |error| {
                    Error::InvalidEngineState(format!(
                        "question {id:?} has an unknown kind: {error}"
                    ))
                },
            )?;
            let answer = (row.get::<_, Option<String>>(3)?)
                .map(|answer| answer.parse::<Answer>())
                .transpose()
                .map_err(Error::InvalidEngineState)?;
            questions.push(Question {
                held: held.remove(&id).unwrap_or_default(),
                id,
                asked_at_ms: row.get(1)?,
                kind,
                answer,
            });
        }
        if let Some(id) = held.keys().next() {
            return Err(Error::InvalidEngineState(format!(
                "items held by an unknown question {id:?}"
            )));
        }
        Ok(questions)
    }

    pub(crate) fn set_open_questions(&self, questions: &[Question]) -> Result<()> {
        for table in [
            "open_question_members",
            "open_question_items",
            "open_questions",
        ] {
            self.0.execute(&format!("DELETE FROM {table}"), [])?;
        }
        let mut stmt = self.0.prepare_cached(
            "INSERT INTO open_questions (question_id, position, asked_at_ms, kind, answer)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut items = self.0.prepare_cached(
            "INSERT INTO open_question_items (question_id, top_id, files) VALUES (?1, ?2, ?3)",
        )?;
        let mut members = self.0.prepare_cached(
            "INSERT INTO open_question_members (question_id, top_id, node_id, content_rev, meta_rev)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (position, question) in questions.iter().enumerate() {
            let kind = serde_json::to_string(&question.kind).expect("a question kind serializes");
            stmt.execute(params![
                question.id,
                position as u64,
                question.asked_at_ms,
                kind,
                question.answer.map(Answer::as_str),
            ])?;
            for (top, item) in &question.held {
                items.execute(params![question.id, top, item.files])?;
                for (node, revisions) in &item.members {
                    let (content_rev, meta_rev) = split(revisions);
                    members.execute(params![question.id, top, node, content_rev, meta_rev])?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn guard(&self) -> Result<GuardState> {
        let mut guard = GuardState::default();
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, at_ms, files FROM guard_ledger")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let counted = Counted {
                at_ms: row.get(1)?,
                files: row.get(2)?,
            };
            guard.ledger.insert(row.get(0)?, counted);
        }
        let mut confirmed = self.held(
            "SELECT '', top_id, files FROM guard_confirmed",
            "SELECT '', top_id, node_id, content_rev, meta_rev FROM guard_confirmed_members",
        )?;
        guard.confirmed = confirmed.remove("").unwrap_or_default();
        Ok(guard)
    }

    pub(crate) fn set_guard(&self, guard: &GuardState) -> Result<()> {
        for table in ["guard_ledger", "guard_confirmed_members", "guard_confirmed"] {
            self.0.execute(&format!("DELETE FROM {table}"), [])?;
        }
        let mut stmt = self.0.prepare_cached(
            "INSERT INTO guard_ledger (node_id, at_ms, files) VALUES (?1, ?2, ?3)",
        )?;
        for (id, counted) in &guard.ledger {
            stmt.execute(params![id, counted.at_ms, counted.files])?;
        }
        let mut items = self
            .0
            .prepare_cached("INSERT INTO guard_confirmed (top_id, files) VALUES (?1, ?2)")?;
        let mut members = self.0.prepare_cached(
            "INSERT INTO guard_confirmed_members (top_id, node_id, content_rev, meta_rev)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (top, item) in &guard.confirmed {
            items.execute(params![top, item.files])?;
            for (node, revisions) in &item.members {
                let (content_rev, meta_rev) = split(revisions);
                members.execute(params![top, node, content_rev, meta_rev])?;
            }
        }
        Ok(())
    }

    /// The held items that `items` and `members` select, by owner. Each item row is the owner,
    /// the topmost node and its files, and each member row the owner, the topmost node, the node
    /// and its two revisions.
    fn held(&self, items: &str, members: &str) -> Result<HeldBy> {
        let mut held = HeldBy::new();
        let mut stmt = self.0.prepare_cached(items)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let item = Held {
                members: BTreeMap::new(),
                files: row.get(2)?,
            };
            held.entry(row.get(0)?)
                .or_default()
                .insert(row.get(1)?, item);
        }
        let mut stmt = self.0.prepare_cached(members)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let (owner, top): (String, NodeId) = (row.get(0)?, row.get(1)?);
            let item = (held.get_mut(&owner))
                .and_then(|items| items.get_mut(&top))
                .ok_or_else(|| {
                    Error::InvalidEngineState(format!("a held member of an unknown item {top:?}"))
                })?;
            item.members.insert(row.get(2)?, revisions(row)?);
        }
        Ok(held)
    }
}

fn split(revisions: &Option<Revisions>) -> (Option<u64>, Option<u64>) {
    match revisions {
        Some(revisions) => (Some(revisions.content_rev), Some(revisions.meta_rev)),
        None => (None, None),
    }
}

fn revisions(row: &Row) -> Result<Option<Revisions>> {
    Ok(match (row.get(3)?, row.get(4)?) {
        (Some(content_rev), Some(meta_rev)) => Some(Revisions {
            content_rev,
            meta_rev,
        }),
        _ => None,
    })
}
