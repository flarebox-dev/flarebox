// SPDX-License-Identifier: Apache-2.0

//! The engine state besides the nodes of R, L and S (design §2.8) and the rows that hold it. This
//! module is the one place that knows both the engine's state types and the tables of migration 3.
//! The restores of the commit side have their rows in [`crate::restore`], and the open questions
//! and the mass-delete guard in [`crate::questions`].

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::classify::{Origin, Waiting};
use koidrive_engine::commit::{Halt, InFlight};
use koidrive_engine::effects::{Base, Dir, Displaced, Effect, EffectRecord, Location};
use koidrive_engine::state::{Agreed, CommitsState, EffectsState, MovedHere, Pending, TreesState};
use koidrive_engine::tree::{FileIdentity, Kind, NodeId, Shape};
use koidrive_proto::v1::{Commit, CommitResult, Content, NodeKind};
use prost::{Message, Oneof};
use rusqlite::{OptionalExtension, params};

use crate::{Error, Result, Tx, decode, ino_columns, ino_from_columns, non_empty};

/// What the engine needs to continue where it stopped, besides the nodes of R, L and S, which the
/// store keeps per node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EngineState {
    pub trees: TreesState,
    pub commits: CommitsState,
    pub effects: EffectsState,
}

/// The node sets of the state, by the name `node_sets` stores them under.
fn node_sets(state: &mut EngineState) -> [(&'static str, &mut BTreeSet<NodeId>); 5] {
    [
        ("dirty", &mut state.trees.dirty),
        ("unsearched", &mut state.trees.unsearched),
        ("reverted", &mut state.trees.reverted),
        ("problems", &mut state.commits.problems),
        ("split", &mut state.commits.split),
    ]
}

/// The node maps of the state, by the name `node_maps` stores them under.
fn node_maps(state: &mut EngineState) -> [(&'static str, &mut BTreeMap<NodeId, NodeId>); 2] {
    [
        ("parked_as", &mut state.trees.parked_as),
        ("aliases", &mut state.commits.aliases),
    ]
}

impl Tx<'_> {
    /// The stored engine state, or the default state of a space that never stored one.
    pub fn engine_state(&self) -> Result<EngineState> {
        let mut state = EngineState::default();
        let mut sets = node_sets(&mut state);
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_set, node_id FROM node_sets")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            let (_, set) = sets
                .iter_mut()
                .find(|(set, _)| *set == name)
                .ok_or_else(|| invalid(format!("unknown node set {name:?}")))?;
            set.insert(row.get(1)?);
        }
        let mut maps = node_maps(&mut state);
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_map, node_id, target FROM node_maps")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(0)?;
            let (_, map) = maps
                .iter_mut()
                .find(|(map, _)| *map == name)
                .ok_or_else(|| invalid(format!("unknown node map {name:?}")))?;
            map.insert(row.get(1)?, row.get(2)?);
        }

        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, parent_id, name, kind, content, slot FROM waiting_updates",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let shape = match row.get::<_, Option<String>>(2)? {
                None => None,
                Some(name) => Some(Shape {
                    parent: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    name,
                    kind: kind(row.get(3)?)?,
                    content: decode(row.get(4)?)?,
                }),
            };
            let origin = match row.get::<_, Option<u64>>(5)? {
                None => Origin::Other,
                Some(slot) => Origin::Commit(slot),
            };
            state
                .trees
                .waiting
                .insert(row.get(0)?, Waiting { shape, origin });
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, changed FROM watched_contents")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            state.trees.watched.insert(row.get(0)?, row.get(1)?);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, rev, parent_id, name FROM watched_positions")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let parent = row.get::<_, Option<String>>(2)?.unwrap_or_default();
            let agreed = (row.get::<_, Option<u64>>(1)?)
                .zip(row.get::<_, Option<String>>(3)?)
                .map(|(rev, name)| Agreed { rev, parent, name });
            state.trees.watched_positions.insert(row.get(0)?, agreed);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, seq FROM trashed_here")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            state.trees.trashed_here.insert(row.get(0)?, row.get(1)?);
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, parent_id, name, kind, content FROM discarded_nodes",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let shape = Shape {
                parent: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                name: row.get(2)?,
                kind: kind(row.get(3)?)?,
                content: decode(row.get(4)?)?,
            };
            state.trees.discarded.insert(row.get(0)?, shape);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id FROM published_nodes")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            state.commits.published.insert(row.get(0)?);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, sent, name, landed FROM pending_names")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let pending = Pending {
                sent: row.get(1)?,
                name: row.get(2)?,
                landed: row.get(3)?,
            };
            state.commits.pending.insert(row.get(0)?, pending);
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, dev, ino, ino_high, birth_ns, lost FROM recorded_nodes",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let node: NodeId = row.get(0)?;
            match row
                .get::<_, Option<i64>>(1)?
                .zip(row.get::<_, Option<i64>>(2)?)
            {
                Some((dev, ino)) => {
                    let identity = FileIdentity {
                        dev: dev as u64,
                        ino: ino_from_columns(ino, row.get(3)?),
                        birth_ns: row.get(4)?,
                    };
                    state.trees.displaced.insert(node, identity);
                }
                None => {
                    if row.get(5)? {
                        state.trees.lost.insert(node.clone());
                    }
                    state.trees.outside.insert(node);
                }
            }
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, seq FROM edited_here")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            state.trees.edited_here.insert(row.get(0)?, row.get(1)?);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, seq, upgraded FROM moved_here")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let moved = MovedHere {
                seq: row.get(1)?,
                upgraded: row.get(2)?,
            };
            state.trees.moved_here.insert(row.get(0)?, moved);
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, rev, parent_id, name FROM past_positions ORDER BY node_id, position",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let past = Agreed {
                rev: row.get(1)?,
                parent: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                name: row.get(3)?,
            };
            let node: NodeId = row.get(0)?;
            state
                .trees
                .past_positions
                .entry(node)
                .or_default()
                .push(past);
        }
        let mut stmt = self.0.prepare_cached("SELECT node_id FROM unsent_edits")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            state.trees.unsent_edits.insert(row.get(0)?);
        }
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, version_id FROM replaced_versions ORDER BY node_id, position",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            (state
                .trees
                .replaced_versions
                .entry(row.get(0)?)
                .or_default())
            .push(row.get(1)?);
        }
        let mut stmt = self.0.prepare_cached("SELECT node_id FROM resends")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            state.trees.resends.insert(row.get(0)?);
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, kept FROM kept_originals")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            state.trees.kept_originals.insert(row.get(0)?, row.get(1)?);
        }

        if let Some((accepted, halt, code, cap)) = self
            .0
            .query_row(
                "SELECT accepted, halt, policy_code, cap FROM commit_state WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, u64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i32>>(2)?,
                        row.get::<_, Option<usize>>(3)?,
                    ))
                },
            )
            .optional()?
        {
            state.commits.accepted = accepted;
            state.commits.halted = halt_from_row(halt, code)?;
            state.commits.cap = cap;
        }
        state.commits.in_flight = self.in_flight()?;
        (state.commits.restores, state.commits.resolved) = self.restores()?;
        let mut stmt = self
            .0
            .prepare_cached("SELECT device_seq, seq FROM commit_slots")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            state.commits.slots.insert(row.get(0)?, row.get(1)?);
        }
        state.commits.questions = self.open_questions()?;
        state.commits.guard = self.guard()?;

        if let Some((cleared, in_flight)) = self
            .0
            .query_row(
                "SELECT cleared, in_flight FROM effect_state WHERE id = 1",
                [],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
            )
            .optional()?
        {
            state.effects.cleared = cleared;
            state.effects.in_flight = decode::<StoredRecord>(in_flight)?
                .map(EffectRecord::try_from)
                .transpose()?;
        }
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, effect FROM failed_effects")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let effect = StoredEffect::decode(row.get::<_, Vec<u8>>(1)?.as_slice())?;
            state
                .effects
                .failed
                .insert(row.get(0)?, Effect::try_from(effect)?);
        }
        Ok(state)
    }

    /// Replaces the stored engine state with `state`.
    pub fn set_engine_state(&self, state: &EngineState) -> Result<()> {
        for table in [
            "node_sets",
            "node_maps",
            "waiting_updates",
            "commit_state",
            "commit_slots",
            "commit_in_flight",
            "commit_in_flight_deletes",
            "commit_in_flight_delete_members",
            "watched_contents",
            "watched_positions",
            "trashed_here",
            "discarded_nodes",
            "published_nodes",
            "pending_names",
            "recorded_nodes",
            "edited_here",
            "moved_here",
            "past_positions",
            "unsent_edits",
            "replaced_versions",
            "resends",
            "kept_originals",
            "effect_state",
            "failed_effects",
            "restore_intents",
            "open_question_shapes",
            "open_question_members",
            "open_question_items",
            "open_questions",
            "guard_ledger",
            "guard_confirmed_members",
            "guard_confirmed",
            "guard_confirmed_limits",
            "guard_remote",
            "guard_remote_dismissed",
        ] {
            self.0.execute(&format!("DELETE FROM {table}"), [])?;
        }
        self.update_engine_state(&EngineState::default(), state)
    }

    /// Changes the stored engine state from `before`, which must be the state the store holds, to
    /// `after`. Only the rows of what differs are written: a member of a node set or a node map, a
    /// waiting update and a failed effect by node, and each of the commit counters, the commit in
    /// flight, the restores, the open questions and the effect counters as a whole, and the
    /// mass-delete guard as [`Tx::update_guard`] writes it.
    pub fn update_engine_state(&self, before: &EngineState, after: &EngineState) -> Result<()> {
        let (mut old, mut new) = (before.clone(), after.clone());
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM node_sets WHERE node_set = ?1 AND node_id = ?2")?;
        let mut insert = self
            .0
            .prepare_cached("INSERT INTO node_sets (node_set, node_id) VALUES (?1, ?2)")?;
        for ((name, old), (_, new)) in node_sets(&mut old).into_iter().zip(node_sets(&mut new)) {
            for node in old.difference(new) {
                remove.execute(params![name, node])?;
            }
            for node in new.difference(old) {
                insert.execute(params![name, node])?;
            }
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM node_maps WHERE node_map = ?1 AND node_id = ?2")?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO node_maps (node_map, node_id, target) VALUES (?1, ?2, ?3)",
        )?;
        for ((name, old), (_, new)) in node_maps(&mut old).into_iter().zip(node_maps(&mut new)) {
            let (removed, changed) = differences(old, new);
            for node in removed {
                remove.execute(params![name, node])?;
            }
            for (node, target) in changed {
                put.execute(params![name, node, target])?;
            }
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM waiting_updates WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO waiting_updates (node_id, parent_id, name, kind, content, slot)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let (removed, changed) = differences(&before.trees.waiting, &after.trees.waiting);
        for node in removed {
            remove.execute([node])?;
        }
        for (node, Waiting { shape, origin }) in changed {
            let slot = match origin {
                Origin::Commit(slot) => Some(*slot),
                Origin::Other => None,
            };
            put.execute(params![
                node,
                shape.as_ref().and_then(|shape| non_empty(&shape.parent)),
                shape.as_ref().map(|shape| &shape.name),
                shape.as_ref().map(|shape| node_kind(shape.kind)),
                shape
                    .as_ref()
                    .and_then(|shape| shape.content.as_ref())
                    .map(Content::encode_to_vec),
                slot,
            ])?;
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM watched_contents WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO watched_contents (node_id, changed) VALUES (?1, ?2)",
        )?;
        let (removed, changed) = differences(&before.trees.watched, &after.trees.watched);
        for node in removed {
            remove.execute([node])?;
        }
        for (node, took) in changed {
            put.execute(params![node, took])?;
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM watched_positions WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO watched_positions (node_id, rev, parent_id, name)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        let (removed, changed) = differences(
            &before.trees.watched_positions,
            &after.trees.watched_positions,
        );
        for node in removed {
            remove.execute([node])?;
        }
        for (node, agreed) in changed {
            put.execute(params![
                node,
                agreed.as_ref().map(|agreed| agreed.rev),
                agreed.as_ref().and_then(|agreed| non_empty(&agreed.parent)),
                agreed.as_ref().map(|agreed| &agreed.name),
            ])?;
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM trashed_here WHERE node_id = ?1")?;
        let mut put = self
            .0
            .prepare_cached("INSERT OR REPLACE INTO trashed_here (node_id, seq) VALUES (?1, ?2)")?;
        let (removed, changed) = differences(&before.trees.trashed_here, &after.trees.trashed_here);
        for node in removed {
            remove.execute([node])?;
        }
        for (node, seq) in changed {
            put.execute(params![node, seq])?;
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM discarded_nodes WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO discarded_nodes (node_id, parent_id, name, kind, content)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let (removed, changed) = differences(&before.trees.discarded, &after.trees.discarded);
        for node in removed {
            remove.execute([node])?;
        }
        for (node, shape) in changed {
            put.execute(params![
                node,
                non_empty(&shape.parent),
                shape.name,
                node_kind(shape.kind),
                shape.content.as_ref().map(Content::encode_to_vec),
            ])?;
        }
        let recorded = |trees: &TreesState| -> BTreeMap<NodeId, (Option<FileIdentity>, bool)> {
            let kept = (trees.outside.iter())
                .map(|node| (node.clone(), (None, trees.lost.contains(node))));
            let displaced =
                (trees.displaced.iter()).map(|(node, held)| (node.clone(), (Some(*held), false)));
            kept.chain(displaced).collect()
        };
        let (old, new) = (recorded(&before.trees), recorded(&after.trees));
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM recorded_nodes WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO recorded_nodes (node_id, displaced, dev, ino, ino_high, birth_ns, lost)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        let (removed, changed) = differences(&old, &new);
        for node in removed {
            remove.execute([node])?;
        }
        for (node, (identity, lost)) in changed {
            put.execute(params![
                node,
                identity.is_some(),
                identity.map(|held| held.dev as i64),
                identity.map(|held| ino_columns(held.ino).0),
                identity.map_or(0, |held| ino_columns(held.ino).1),
                identity.and_then(|held| held.birth_ns),
                lost,
            ])?;
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM edited_here WHERE node_id = ?1")?;
        let mut put = self
            .0
            .prepare_cached("INSERT OR REPLACE INTO edited_here (node_id, seq) VALUES (?1, ?2)")?;
        let (removed, changed) = differences(&before.trees.edited_here, &after.trees.edited_here);
        for node in removed {
            remove.execute([node])?;
        }
        for (node, seq) in changed {
            put.execute(params![node, seq])?;
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM moved_here WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO moved_here (node_id, seq, upgraded) VALUES (?1, ?2, ?3)",
        )?;
        let (removed, changed) = differences(&before.trees.moved_here, &after.trees.moved_here);
        for node in removed {
            remove.execute([node])?;
        }
        for (node, moved) in changed {
            put.execute(params![node, moved.seq, moved.upgraded])?;
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM past_positions WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT INTO past_positions (node_id, position, rev, parent_id, name)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let (removed, changed) =
            differences(&before.trees.past_positions, &after.trees.past_positions);
        for node in removed {
            remove.execute([node])?;
        }
        for (node, past) in changed {
            remove.execute([node])?;
            for (index, position) in past.iter().enumerate() {
                put.execute(params![
                    node,
                    index,
                    position.rev,
                    non_empty(&position.parent),
                    &position.name
                ])?;
            }
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM unsent_edits WHERE node_id = ?1")?;
        let mut put = self
            .0
            .prepare_cached("INSERT OR IGNORE INTO unsent_edits (node_id) VALUES (?1)")?;
        let (old, new) = (&before.trees.unsent_edits, &after.trees.unsent_edits);
        for node in old.difference(new) {
            remove.execute([node])?;
        }
        for node in new.difference(old) {
            put.execute([node])?;
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM replaced_versions WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT INTO replaced_versions (node_id, position, version_id) VALUES (?1, ?2, ?3)",
        )?;
        let (removed, changed) = differences(
            &before.trees.replaced_versions,
            &after.trees.replaced_versions,
        );
        for node in removed {
            remove.execute([node])?;
        }
        for (node, versions) in changed {
            remove.execute([node])?;
            for (position, version) in versions.iter().enumerate() {
                put.execute(params![node, position, version])?;
            }
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM resends WHERE node_id = ?1")?;
        let mut put = self
            .0
            .prepare_cached("INSERT OR IGNORE INTO resends (node_id) VALUES (?1)")?;
        let (old, new) = (&before.trees.resends, &after.trees.resends);
        for node in old.difference(new) {
            remove.execute([node])?;
        }
        for node in new.difference(old) {
            put.execute([node])?;
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM kept_originals WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO kept_originals (node_id, kept) VALUES (?1, ?2)",
        )?;
        let (removed, changed) =
            differences(&before.trees.kept_originals, &after.trees.kept_originals);
        for node in removed {
            remove.execute([node])?;
        }
        for (node, kept) in changed {
            put.execute(params![node, kept])?;
        }

        let (old, new) = (&before.commits, &after.commits);
        if (old.accepted, old.halted, old.cap) != (new.accepted, new.halted, new.cap) {
            let (halt, code) = halt_row(new.halted);
            self.0.execute(
                "INSERT OR REPLACE INTO commit_state (id, accepted, halt, policy_code, cap)
                 VALUES (1, ?1, ?2, ?3, ?4)",
                params![new.accepted, halt, code, new.cap],
            )?;
        }
        if old.in_flight != new.in_flight {
            for table in [
                "commit_in_flight",
                "commit_in_flight_deletes",
                "commit_in_flight_delete_members",
            ] {
                self.0.execute(&format!("DELETE FROM {table}"), [])?;
            }
            if let Some(flight) = &new.in_flight {
                self.0.execute(
                    "INSERT INTO commit_in_flight (id, commit_message, result) VALUES (1, ?1, ?2)",
                    params![
                        flight.commit.encode_to_vec(),
                        flight.result.as_ref().map(CommitResult::encode_to_vec),
                    ],
                )?;
                let mut targets = self
                    .0
                    .prepare_cached("INSERT INTO commit_in_flight_deletes (target) VALUES (?1)")?;
                let mut members = self.0.prepare_cached(
                    "INSERT INTO commit_in_flight_delete_members (target, position, node_id) VALUES (?1, ?2, ?3)",
                )?;
                for (target, nodes) in &flight.deletes {
                    targets.execute([target])?;
                    for (position, node) in nodes.iter().enumerate() {
                        members.execute(params![target, position, node])?;
                    }
                }
            }
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM commit_slots WHERE device_seq = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO commit_slots (device_seq, seq) VALUES (?1, ?2)",
        )?;
        let (removed, changed) = differences(&old.slots, &new.slots);
        for slot in removed {
            remove.execute([slot])?;
        }
        for (slot, seq) in changed {
            put.execute(params![slot, seq])?;
        }
        if (&old.restores, &old.resolved) != (&new.restores, &new.resolved) {
            self.0.execute("DELETE FROM restore_intents", [])?;
            self.set_restores(&new.restores, &new.resolved)?;
        }
        if old.questions != new.questions {
            self.set_open_questions(&new.questions)?;
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM published_nodes WHERE node_id = ?1")?;
        for node in old.published.difference(&new.published) {
            remove.execute([node])?;
        }
        let mut insert = self
            .0
            .prepare_cached("INSERT INTO published_nodes (node_id) VALUES (?1)")?;
        for node in new.published.difference(&old.published) {
            insert.execute([node])?;
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM pending_names WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO pending_names (node_id, sent, name, landed) VALUES (?1, ?2, ?3, ?4)",
        )?;
        let (removed, changed) = differences(&old.pending, &new.pending);
        for node in removed {
            remove.execute([node])?;
        }
        for (node, pending) in changed {
            put.execute(params![node, pending.sent, pending.name, pending.landed])?;
        }
        if old.guard != new.guard {
            self.update_guard(&old.guard, &new.guard)?;
        }

        let (old, new) = (&before.effects, &after.effects);
        if (old.cleared, &old.in_flight) != (new.cleared, &new.in_flight) {
            self.0.execute(
                "INSERT OR REPLACE INTO effect_state (id, cleared, in_flight) VALUES (1, ?1, ?2)",
                params![
                    new.cleared,
                    new.in_flight
                        .as_ref()
                        .map(|record| StoredRecord::from(record).encode_to_vec()),
                ],
            )?;
        }
        let mut remove = self
            .0
            .prepare_cached("DELETE FROM failed_effects WHERE node_id = ?1")?;
        let mut put = self.0.prepare_cached(
            "INSERT OR REPLACE INTO failed_effects (node_id, effect) VALUES (?1, ?2)",
        )?;
        let (removed, changed) = differences(&old.failed, &new.failed);
        for node in removed {
            remove.execute([node])?;
        }
        for (node, effect) in changed {
            put.execute(params![node, StoredEffect::from(effect).encode_to_vec()])?;
        }
        Ok(())
    }

    fn in_flight(&self) -> Result<Option<InFlight>> {
        let Some((commit, result)) = self
            .0
            .query_row(
                "SELECT commit_message, result FROM commit_in_flight WHERE id = 1",
                [],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let mut deletes: BTreeMap<NodeId, Vec<NodeId>> = self
            .0
            .prepare_cached("SELECT target FROM commit_in_flight_deletes")?
            .query_map([], |row| Ok((row.get(0)?, Vec::new())))?
            .collect::<rusqlite::Result<_>>()?;
        let mut stmt = self.0.prepare_cached(
            "SELECT target, node_id FROM commit_in_flight_delete_members ORDER BY target, position",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let target: NodeId = row.get(0)?;
            let Some(nodes) = deletes.get_mut(&target) else {
                return Err(invalid(format!(
                    "delete members for {target}, which is not a delete target"
                )));
            };
            nodes.push(row.get(1)?);
        }
        Ok(Some(InFlight {
            commit: Commit::decode(commit.as_slice())?,
            deletes,
            result: decode(result)?,
        }))
    }
}

fn invalid(what: String) -> Error {
    Error::InvalidEngineState(what)
}

/// The keys of `before` that `after` lacks, and the entries of `after` that `before` lacks or holds
/// with another value.
fn differences<'a, K: Ord, V: PartialEq>(
    before: &'a BTreeMap<K, V>,
    after: &'a BTreeMap<K, V>,
) -> (
    impl Iterator<Item = &'a K>,
    impl Iterator<Item = (&'a K, &'a V)>,
) {
    let removed = before.keys().filter(|key| !after.contains_key(*key));
    let changed = after
        .iter()
        .filter(|(key, value)| before.get(*key) != Some(*value));
    (removed, changed)
}

fn halt_row(halt: Option<Halt>) -> (Option<&'static str>, Option<i32>) {
    match halt {
        None => (None, None),
        Some(Halt::ReadOnly) => (Some("read_only"), None),
        Some(Halt::Recovery) => (Some("recovery"), None),
        Some(Halt::Policy { code }) => (Some("policy"), Some(code)),
    }
}

fn halt_from_row(halt: Option<String>, code: Option<i32>) -> Result<Option<Halt>> {
    Ok(match (halt.as_deref(), code) {
        (None, None) => None,
        (Some("read_only"), None) => Some(Halt::ReadOnly),
        (Some("recovery"), None) => Some(Halt::Recovery),
        (Some("policy"), Some(code)) => Some(Halt::Policy { code }),
        _ => return Err(invalid(format!("halt {halt:?} with code {code:?}"))),
    })
}

pub(crate) fn node_kind(kind: Kind) -> i32 {
    match kind {
        Kind::Dir => NodeKind::Dir as i32,
        Kind::File => NodeKind::File as i32,
    }
}

pub(crate) fn kind(value: i32) -> Result<Kind> {
    match NodeKind::try_from(value) {
        Ok(NodeKind::Dir) => Ok(Kind::Dir),
        Ok(NodeKind::File) => Ok(Kind::File),
        _ => Err(invalid(format!("node kind {value}"))),
    }
}

fn required<T>(value: Option<T>, what: &str) -> Result<T> {
    value.ok_or_else(|| invalid(format!("{what} is missing")))
}

/// An [`EffectRecord`] as `effect_state.in_flight` stores it. The effect types are the engine's
/// own and have no wire format, so these messages give them one here. Their tags never change
/// meaning once shipped.
#[derive(Clone, PartialEq, Message)]
struct StoredRecord {
    #[prost(string, tag = "1")]
    effect_id: String,
    #[prost(string, tag = "2")]
    node: String,
    #[prost(uint64, tag = "3")]
    counter: u64,
    #[prost(message, optional, tag = "4")]
    effect: Option<StoredEffect>,
    #[prost(message, optional, tag = "5")]
    after: Option<StoredShape>,
}

#[derive(Clone, PartialEq, Message)]
struct StoredEffect {
    #[prost(oneof = "Action", tags = "1, 2, 3, 4, 5")]
    action: Option<Action>,
}

#[derive(Clone, PartialEq, Oneof)]
enum Action {
    #[prost(message, tag = "1")]
    CreateDir(StoredLocation),
    #[prost(message, tag = "2")]
    WriteFile(StoredWrite),
    #[prost(message, tag = "3")]
    Move(StoredMove),
    #[prost(message, tag = "4")]
    RemoveFile(StoredDisplaced),
    #[prost(message, tag = "5")]
    RemoveDir(StoredRemoveDir),
}

#[derive(Clone, PartialEq, Message)]
struct StoredWrite {
    #[prost(message, optional, tag = "1")]
    at: Option<StoredLocation>,
    #[prost(message, optional, tag = "2")]
    content: Option<Content>,
    #[prost(message, optional, tag = "3")]
    replaces: Option<StoredDisplaced>,
}

#[derive(Clone, PartialEq, Message)]
struct StoredMove {
    #[prost(message, optional, tag = "1")]
    from: Option<StoredLocation>,
    #[prost(message, optional, tag = "2")]
    identity: Option<StoredIdentity>,
    #[prost(message, optional, tag = "3")]
    to: Option<StoredLocation>,
}

#[derive(Clone, PartialEq, Message)]
struct StoredRemoveDir {
    #[prost(message, optional, tag = "1")]
    at: Option<StoredLocation>,
    #[prost(message, optional, tag = "2")]
    identity: Option<StoredIdentity>,
}

#[derive(Clone, PartialEq, Message)]
struct StoredDisplaced {
    #[prost(message, optional, tag = "1")]
    at: Option<StoredLocation>,
    #[prost(message, optional, tag = "2")]
    identity: Option<StoredIdentity>,
    #[prost(message, optional, tag = "3")]
    content: Option<Content>,
}

#[derive(Clone, PartialEq, Message)]
struct StoredLocation {
    #[prost(oneof = "Place", tags = "1, 2")]
    place: Option<Place>,
}

#[derive(Clone, PartialEq, Oneof)]
enum Place {
    /// `.koidrive/parked/<node>` itself.
    #[prost(string, tag = "1")]
    Parked(String),
    #[prost(message, tag = "2")]
    At(StoredAt),
}

#[derive(Clone, PartialEq, Message)]
struct StoredAt {
    /// The parked directory the folders start at, by its node; the root when absent.
    #[prost(message, optional, tag = "1")]
    parked: Option<StoredDir>,
    #[prost(message, repeated, tag = "2")]
    dirs: Vec<StoredDir>,
    #[prost(string, tag = "3")]
    name: String,
}

#[derive(Clone, PartialEq, Message)]
struct StoredDir {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(message, optional, tag = "2")]
    identity: Option<StoredIdentity>,
}

#[derive(Clone, PartialEq, Message)]
struct StoredIdentity {
    #[prost(uint64, tag = "1")]
    dev: u64,
    #[prost(uint64, tag = "2")]
    ino: u64,
    #[prost(int64, optional, tag = "3")]
    birth_ns: Option<i64>,
    #[prost(uint64, tag = "4")]
    ino_high: u64,
}

#[derive(Clone, PartialEq, Message)]
struct StoredShape {
    #[prost(string, tag = "1")]
    parent: String,
    #[prost(string, tag = "2")]
    name: String,
    #[prost(int32, tag = "3")]
    kind: i32,
    #[prost(message, optional, tag = "4")]
    content: Option<Content>,
}

impl From<&EffectRecord> for StoredRecord {
    fn from(record: &EffectRecord) -> Self {
        Self {
            effect_id: record.effect_id.clone(),
            node: record.node.clone(),
            counter: record.counter,
            effect: Some((&record.effect).into()),
            after: record.after.as_ref().map(|shape| StoredShape {
                parent: shape.parent.clone(),
                name: shape.name.clone(),
                kind: node_kind(shape.kind),
                content: shape.content.clone(),
            }),
        }
    }
}

impl TryFrom<StoredRecord> for EffectRecord {
    type Error = Error;

    fn try_from(record: StoredRecord) -> Result<Self> {
        let after = match record.after {
            None => None,
            Some(shape) => Some(Shape {
                parent: shape.parent,
                name: shape.name,
                kind: kind(shape.kind)?,
                content: shape.content,
            }),
        };
        Ok(Self {
            effect_id: record.effect_id,
            node: record.node,
            counter: record.counter,
            effect: required(record.effect, "the effect")?.try_into()?,
            after,
        })
    }
}

impl From<&Effect> for StoredEffect {
    fn from(effect: &Effect) -> Self {
        let action = match effect {
            Effect::CreateDir { at } => Action::CreateDir(at.into()),
            Effect::WriteFile {
                at,
                content,
                replaces,
            } => Action::WriteFile(StoredWrite {
                at: Some(at.into()),
                content: Some(content.clone()),
                replaces: replaces.as_ref().map(Into::into),
            }),
            Effect::Move { from, identity, to } => Action::Move(StoredMove {
                from: Some(from.into()),
                identity: Some(identity.into()),
                to: Some(to.into()),
            }),
            Effect::RemoveFile { displaced } => Action::RemoveFile(displaced.into()),
            Effect::RemoveDir { at, identity } => Action::RemoveDir(StoredRemoveDir {
                at: Some(at.into()),
                identity: Some(identity.into()),
            }),
        };
        Self {
            action: Some(action),
        }
    }
}

impl TryFrom<StoredEffect> for Effect {
    type Error = Error;

    fn try_from(effect: StoredEffect) -> Result<Self> {
        Ok(match required(effect.action, "the effect's action")? {
            Action::CreateDir(at) => Effect::CreateDir { at: at.try_into()? },
            Action::WriteFile(write) => Effect::WriteFile {
                at: required(write.at, "the written location")?.try_into()?,
                content: required(write.content, "the written content")?,
                replaces: write.replaces.map(TryInto::try_into).transpose()?,
            },
            Action::Move(moved) => Effect::Move {
                from: required(moved.from, "the move's source")?.try_into()?,
                identity: required(moved.identity, "the moved identity")?.into(),
                to: required(moved.to, "the move's target")?.try_into()?,
            },
            Action::RemoveFile(displaced) => Effect::RemoveFile {
                displaced: displaced.try_into()?,
            },
            Action::RemoveDir(removed) => Effect::RemoveDir {
                at: required(removed.at, "the removed location")?.try_into()?,
                identity: required(removed.identity, "the removed identity")?.into(),
            },
        })
    }
}

impl From<&Displaced> for StoredDisplaced {
    fn from(displaced: &Displaced) -> Self {
        Self {
            at: Some((&displaced.at).into()),
            identity: Some((&displaced.identity).into()),
            content: Some(displaced.content.clone()),
        }
    }
}

impl TryFrom<StoredDisplaced> for Displaced {
    type Error = Error;

    fn try_from(displaced: StoredDisplaced) -> Result<Self> {
        Ok(Self {
            at: required(displaced.at, "the displaced location")?.try_into()?,
            identity: required(displaced.identity, "the displaced identity")?.into(),
            content: required(displaced.content, "the displaced content")?,
        })
    }
}

impl From<&Location> for StoredLocation {
    fn from(location: &Location) -> Self {
        let place = match location {
            Location::Parked(node) => Place::Parked(node.clone()),
            Location::At { base, dirs, name } => Place::At(StoredAt {
                parked: match base {
                    Base::Root => None,
                    Base::Parked { node, identity } => Some(StoredDir {
                        name: node.clone(),
                        identity: Some(identity.into()),
                    }),
                },
                dirs: dirs
                    .iter()
                    .map(|dir| StoredDir {
                        name: dir.name.clone(),
                        identity: Some((&dir.identity).into()),
                    })
                    .collect(),
                name: name.clone(),
            }),
        };
        Self { place: Some(place) }
    }
}

impl TryFrom<StoredLocation> for Location {
    type Error = Error;

    fn try_from(location: StoredLocation) -> Result<Self> {
        Ok(match required(location.place, "the location")? {
            Place::Parked(node) => Location::Parked(node),
            Place::At(at) => Location::At {
                base: match at.parked {
                    None => Base::Root,
                    Some(dir) => Base::Parked {
                        node: dir.name,
                        identity: required(dir.identity, "the parked directory's identity")?.into(),
                    },
                },
                dirs: at
                    .dirs
                    .into_iter()
                    .map(|dir| {
                        Ok(Dir {
                            name: dir.name,
                            identity: required(dir.identity, "a directory's identity")?.into(),
                        })
                    })
                    .collect::<Result<_>>()?,
                name: at.name,
            },
        })
    }
}

impl From<&FileIdentity> for StoredIdentity {
    fn from(identity: &FileIdentity) -> Self {
        Self {
            dev: identity.dev,
            ino: identity.ino as u64,
            birth_ns: identity.birth_ns,
            ino_high: (identity.ino >> 64) as u64,
        }
    }
}

impl From<StoredIdentity> for FileIdentity {
    fn from(identity: StoredIdentity) -> Self {
        Self {
            dev: identity.dev,
            ino: (u128::from(identity.ino_high) << 64) | u128::from(identity.ino),
            birth_ns: identity.birth_ns,
        }
    }
}
