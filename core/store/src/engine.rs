// SPDX-License-Identifier: Apache-2.0

//! The engine state besides the nodes of R, L and S (design §2.8) and the rows that hold it. This
//! module is the one place that knows both the engine's state types and the tables of migration 3.

use std::collections::{BTreeMap, BTreeSet};

use flarebox_engine::commit::{Halt, InFlight};
use flarebox_engine::effects::{Base, Dir, Displaced, Effect, EffectRecord, Location};
use flarebox_engine::state::{CommitsState, EffectsState, TreesState};
use flarebox_engine::tree::{FileIdentity, Kind, NodeId, Shape};
use flarebox_proto::v1::{Commit, CommitResult, Content, NodeKind};
use prost::{Message, Oneof};
use rusqlite::{OptionalExtension, params};

use crate::{Error, Result, Tx, decode, non_empty};

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
            "SELECT node_id, parent_id, name, kind, content FROM waiting_updates",
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
            state.trees.waiting.insert(row.get(0)?, shape);
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
            "commit_in_flight",
            "commit_in_flight_deletes",
            "effect_state",
            "failed_effects",
        ] {
            self.0.execute(&format!("DELETE FROM {table}"), [])?;
        }
        let mut copy = state.clone();
        let mut stmt = self
            .0
            .prepare_cached("INSERT INTO node_sets (node_set, node_id) VALUES (?1, ?2)")?;
        for (name, set) in node_sets(&mut copy) {
            for node in set.iter() {
                stmt.execute(params![name, node])?;
            }
        }
        let mut stmt = self.0.prepare_cached(
            "INSERT INTO node_maps (node_map, node_id, target) VALUES (?1, ?2, ?3)",
        )?;
        for (name, map) in node_maps(&mut copy) {
            for (node, target) in map.iter() {
                stmt.execute(params![name, node, target])?;
            }
        }
        let mut stmt = self.0.prepare_cached(
            "INSERT INTO waiting_updates (node_id, parent_id, name, kind, content)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (node, shape) in &state.trees.waiting {
            stmt.execute(params![
                node,
                shape.as_ref().and_then(|shape| non_empty(&shape.parent)),
                shape.as_ref().map(|shape| &shape.name),
                shape.as_ref().map(|shape| node_kind(shape.kind)),
                shape
                    .as_ref()
                    .and_then(|shape| shape.content.as_ref())
                    .map(Content::encode_to_vec),
            ])?;
        }

        let commits = &state.commits;
        let (halt, code) = halt_row(commits.halted);
        self.0.execute(
            "INSERT INTO commit_state (id, accepted, halt, policy_code, cap) VALUES (1, ?1, ?2, ?3, ?4)",
            params![commits.accepted, halt, code, commits.cap],
        )?;
        if let Some(flight) = &commits.in_flight {
            self.0.execute(
                "INSERT INTO commit_in_flight (id, commit_message, result) VALUES (1, ?1, ?2)",
                params![
                    flight.commit.encode_to_vec(),
                    flight.result.as_ref().map(CommitResult::encode_to_vec),
                ],
            )?;
            let mut stmt = self.0.prepare_cached(
                "INSERT INTO commit_in_flight_deletes (target, position, node_id) VALUES (?1, ?2, ?3)",
            )?;
            for (target, nodes) in &flight.deletes {
                for (position, node) in nodes.iter().enumerate() {
                    stmt.execute(params![target, position, node])?;
                }
            }
        }

        let effects = &state.effects;
        self.0.execute(
            "INSERT INTO effect_state (id, cleared, in_flight) VALUES (1, ?1, ?2)",
            params![
                effects.cleared,
                effects
                    .in_flight
                    .as_ref()
                    .map(|record| StoredRecord::from(record).encode_to_vec()),
            ],
        )?;
        let mut stmt = self
            .0
            .prepare_cached("INSERT INTO failed_effects (node_id, effect) VALUES (?1, ?2)")?;
        for (node, effect) in &effects.failed {
            stmt.execute(params![node, StoredEffect::from(effect).encode_to_vec()])?;
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
        let mut deletes: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        let mut stmt = self.0.prepare_cached(
            "SELECT target, node_id FROM commit_in_flight_deletes ORDER BY target, position",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            deletes.entry(row.get(0)?).or_default().push(row.get(1)?);
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

fn node_kind(kind: Kind) -> i32 {
    match kind {
        Kind::Dir => NodeKind::Dir as i32,
        Kind::File => NodeKind::File as i32,
    }
}

fn kind(value: i32) -> Result<Kind> {
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
    /// `.flarebox/parked/<node>` itself.
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
            ino: identity.ino,
            birth_ns: identity.birth_ns,
        }
    }
}

impl From<StoredIdentity> for FileIdentity {
    fn from(identity: StoredIdentity) -> Self {
        Self {
            dev: identity.dev,
            ino: identity.ino,
            birth_ns: identity.birth_ns,
        }
    }
}
