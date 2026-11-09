// SPDX-License-Identifier: Apache-2.0

//! Local SQLite state for one synced space on one device, as the sync engine
//! design lays it out (docs/design/sync-engine.md §2.2 and §2.8):
//!
//! - the remote tree R: the `NodeState` records this device has applied, with
//!   the `(history_id, last_seq)` cursor;
//! - the synced tree S: per node, the shape on which disk and server last agreed;
//! - the local tree L: the disk as last observed, with file identities and the
//!   parked flag;
//! - the one commit in flight, with the shape each op sent;
//! - problem nodes, the engine's ID generator state, and the index of chunks
//!   cached on disk.
//!
//! The engine keeps the trees in memory and reports row-level changes after each
//! input; the driver writes them with the methods of [`Tx`] inside one
//! [`Store::transaction`]. A transaction either commits as a whole or leaves no
//! trace, including when the process is killed in the middle of it: the
//! database runs in WAL mode with `synchronous = FULL`, so a commit that
//! returned is durable and an interrupted one is rolled back the next time the
//! file is opened.

use std::path::Path;
use std::time::Duration;

use flarebox_fs::FileId;
use flarebox_proto::v1::{Commit, Content, NodeState, VersionInfo};
use prost::Message;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

/// Schema migrations, applied in order. Migration `i` moves the database from
/// `user_version = i` to `user_version = i + 1`. Never edit an entry once it
/// has shipped; append a new one.
const MIGRATIONS: &[&str] = &["
    CREATE TABLE remote_cursor (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        history_id TEXT NOT NULL,
        last_seq INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE remote_nodes (
        node_id TEXT PRIMARY KEY,
        parent_id TEXT,
        name TEXT NOT NULL,
        kind INTEGER NOT NULL,
        status INTEGER NOT NULL,
        content_rev INTEGER NOT NULL,
        meta_rev INTEGER NOT NULL,
        trash_group TEXT,
        current BLOB
    ) STRICT;

    CREATE TABLE synced_nodes (
        node_id TEXT PRIMARY KEY,
        parent_id TEXT,
        name TEXT NOT NULL,
        kind INTEGER NOT NULL,
        content BLOB
    ) STRICT;

    CREATE TABLE local_nodes (
        node_id TEXT PRIMARY KEY,
        parent_id TEXT,
        name TEXT NOT NULL,
        kind INTEGER NOT NULL,
        content BLOB,
        dev INTEGER NOT NULL,
        ino INTEGER NOT NULL,
        birth_ns INTEGER,
        parked INTEGER NOT NULL CHECK (parked IN (0, 1))
    ) STRICT;

    CREATE TABLE commit_in_flight (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        commit_message BLOB NOT NULL
    ) STRICT;

    CREATE TABLE commit_in_flight_shapes (
        op_index INTEGER PRIMARY KEY,
        parent_id TEXT,
        name TEXT NOT NULL,
        kind INTEGER NOT NULL,
        content BLOB
    ) STRICT;

    CREATE TABLE problems (
        node_id TEXT PRIMARY KEY,
        reason TEXT NOT NULL
    ) STRICT;

    CREATE TABLE engine_state (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        id_generator BLOB NOT NULL
    ) STRICT;

    CREATE TABLE chunk_cache (
        hash BLOB PRIMARY KEY CHECK (length(hash) = 32),
        size INTEGER NOT NULL,
        last_used_ms INTEGER NOT NULL
    ) STRICT, WITHOUT ROWID;
    CREATE INDEX chunk_cache_by_use ON chunk_cache (last_used_ms);
"];

/// The schema version this build writes.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored protobuf does not decode: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("database schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: u32, supported: u32 },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Where this device stands in the space's commit log (spec §7.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub history_id: String,
    pub last_seq: u64,
}

/// A node's position, kind and content: what the synced tree records per node
/// and what each op of a commit sent. An empty `parent_id` marks the root;
/// `kind` holds a `NodeKind` value as in `NodeState`; `content` is set for files.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Shape {
    pub parent_id: String,
    pub name: String,
    pub kind: i32,
    pub content: Option<Content>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncedNode {
    pub node_id: String,
    pub shape: Shape,
}

/// A node of the local tree. `shape.name` is the exact name on disk. A parked
/// node sits in `.flarebox/parked/<node_id>` while `shape` keeps the
/// position the engine compares it at.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalNode {
    pub node_id: String,
    pub shape: Shape,
    pub identity: FileId,
    pub parked: bool,
}

/// The one commit sent or about to be sent. `sent[i]` is the shape op `i`
/// sent, or `None` for ops that send no shape (deletes and restores). Resend
/// it unchanged after a restart; its `commit_id` makes the server return the
/// stored result if it was already applied.
#[derive(Debug, Clone, PartialEq)]
pub struct InFlightCommit {
    pub commit: Commit,
    pub sent: Vec<Option<Shape>>,
}

/// A node left out of planning until a rescan or a user action, with the
/// engine's reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub node_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedChunk {
    pub hash: [u8; 32],
    pub size: u64,
    pub last_used_ms: i64,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens the database at `path`, creating it if needed, and applies any
    /// migrations it lacks. Fails with [`Error::SchemaTooNew`] if a newer build
    /// already migrated it.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    pub fn schema_version(&self) -> Result<u32> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    /// Runs `f` in one write transaction. The transaction commits if `f`
    /// returns `Ok` and rolls back if it returns `Err` or panics.
    pub fn transaction<T, E: From<Error>>(
        &mut self,
        f: impl FnOnce(&Tx<'_>) -> Result<T, E>,
    ) -> Result<T, E> {
        let tx = Tx(self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(Error::from)?);
        let value = f(&tx)?;
        tx.0.commit().map_err(Error::from)?;
        Ok(value)
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    loop {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found: u32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if found > SCHEMA_VERSION {
            return Err(Error::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            });
        }
        let Some(sql) = MIGRATIONS.get(found as usize) else {
            return Ok(());
        };
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", found + 1)?;
        tx.commit()?;
    }
}

/// An open transaction. Obtain one with [`Store::transaction`].
pub struct Tx<'a>(rusqlite::Transaction<'a>);

impl Tx<'_> {
    pub fn cursor(&self) -> Result<Option<Cursor>> {
        Ok(self
            .0
            .query_row(
                "SELECT history_id, last_seq FROM remote_cursor WHERE id = 1",
                [],
                |row| {
                    Ok(Cursor {
                        history_id: row.get(0)?,
                        last_seq: row.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn set_cursor(&self, cursor: &Cursor) -> Result<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO remote_cursor (id, history_id, last_seq) VALUES (1, ?1, ?2)",
            params![cursor.history_id, cursor.last_seq],
        )?;
        Ok(())
    }

    /// Every node of the remote tree, live and trashed, ordered by `node_id`.
    pub fn remote_nodes(&self) -> Result<Vec<NodeState>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, parent_id, name, kind, status, content_rev, meta_rev, trash_group, current
             FROM remote_nodes ORDER BY node_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                NodeState {
                    node_id: row.get(0)?,
                    parent_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    name: row.get(2)?,
                    kind: row.get(3)?,
                    status: row.get(4)?,
                    content_rev: row.get(5)?,
                    meta_rev: row.get(6)?,
                    trash_group: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
                    current: None,
                },
                row.get::<_, Option<Vec<u8>>>(8)?,
            ))
        })?;
        rows.map(|row| {
            let (mut node, current) = row?;
            node.current = decode(current)?;
            Ok(node)
        })
        .collect()
    }

    /// Inserts or replaces the remote node with `node.node_id`.
    pub fn put_remote_node(&self, node: &NodeState) -> Result<()> {
        self.0
            .prepare_cached(
                "INSERT OR REPLACE INTO remote_nodes
                 (node_id, parent_id, name, kind, status, content_rev, meta_rev, trash_group, current)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?
            .execute(params![
                node.node_id,
                non_empty(&node.parent_id),
                node.name,
                node.kind,
                node.status,
                node.content_rev,
                node.meta_rev,
                non_empty(&node.trash_group),
                node.current.as_ref().map(VersionInfo::encode_to_vec),
            ])?;
        Ok(())
    }

    /// Removes the remote node, for example after `NodePurged`. Returns whether
    /// it existed.
    pub fn remove_remote_node(&self, node_id: &str) -> Result<bool> {
        self.remove("remote_nodes", node_id)
    }

    /// Every node of the synced tree, ordered by `node_id`.
    pub fn synced_nodes(&self) -> Result<Vec<SyncedNode>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, parent_id, name, kind, content FROM synced_nodes ORDER BY node_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, ShapeRow::read(row, 1)?))
        })?;
        rows.map(|row| {
            let (node_id, shape) = row?;
            Ok(SyncedNode {
                node_id,
                shape: shape.into_shape()?,
            })
        })
        .collect()
    }

    pub fn put_synced_node(&self, node: &SyncedNode) -> Result<()> {
        let shape = &node.shape;
        self.0
            .prepare_cached(
                "INSERT OR REPLACE INTO synced_nodes (node_id, parent_id, name, kind, content)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![
                node.node_id,
                non_empty(&shape.parent_id),
                shape.name,
                shape.kind,
                shape.content.as_ref().map(Content::encode_to_vec),
            ])?;
        Ok(())
    }

    /// Removes the synced node (S := ⊥). Returns whether it existed.
    pub fn remove_synced_node(&self, node_id: &str) -> Result<bool> {
        self.remove("synced_nodes", node_id)
    }

    /// Every node of the local tree, ordered by `node_id`.
    pub fn local_nodes(&self) -> Result<Vec<LocalNode>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT node_id, parent_id, name, kind, content, dev, ino, birth_ns, parked
             FROM local_nodes ORDER BY node_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                ShapeRow::read(row, 1)?,
                FileId {
                    dev: row.get::<_, i64>(5)? as u64,
                    ino: row.get::<_, i64>(6)? as u64,
                    birth_ns: row.get(7)?,
                },
                row.get::<_, bool>(8)?,
            ))
        })?;
        rows.map(|row| {
            let (node_id, shape, identity, parked) = row?;
            Ok(LocalNode {
                node_id,
                shape: shape.into_shape()?,
                identity,
                parked,
            })
        })
        .collect()
    }

    /// Inserts or replaces the local node. `dev` and `ino` are stored as their
    /// two's-complement `i64` bit patterns, so every `u64` round-trips.
    pub fn put_local_node(&self, node: &LocalNode) -> Result<()> {
        let shape = &node.shape;
        self.0
            .prepare_cached(
                "INSERT OR REPLACE INTO local_nodes
                 (node_id, parent_id, name, kind, content, dev, ino, birth_ns, parked)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?
            .execute(params![
                node.node_id,
                non_empty(&shape.parent_id),
                shape.name,
                shape.kind,
                shape.content.as_ref().map(Content::encode_to_vec),
                node.identity.dev as i64,
                node.identity.ino as i64,
                node.identity.birth_ns,
                node.parked,
            ])?;
        Ok(())
    }

    pub fn remove_local_node(&self, node_id: &str) -> Result<bool> {
        self.remove("local_nodes", node_id)
    }

    pub fn commit_in_flight(&self) -> Result<Option<InFlightCommit>> {
        let Some(commit) = self
            .0
            .query_row(
                "SELECT commit_message FROM commit_in_flight WHERE id = 1",
                [],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let commit = Commit::decode(commit.as_slice())?;
        let mut sent = vec![None; commit.ops.len()];
        let mut stmt = self.0.prepare_cached(
            "SELECT op_index, parent_id, name, kind, content FROM commit_in_flight_shapes",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, usize>(0)?, ShapeRow::read(row, 1)?))
        })?;
        for row in rows {
            let (index, shape) = row?;
            if let Some(slot) = sent.get_mut(index) {
                *slot = Some(shape.into_shape()?);
            }
        }
        Ok(Some(InFlightCommit { commit, sent }))
    }

    /// Records the commit in flight, replacing any previous one, or clears it
    /// with `None` once its result is applied.
    pub fn set_commit_in_flight(&self, commit: Option<&InFlightCommit>) -> Result<()> {
        self.0.execute("DELETE FROM commit_in_flight", [])?;
        self.0.execute("DELETE FROM commit_in_flight_shapes", [])?;
        let Some(in_flight) = commit else {
            return Ok(());
        };
        self.0.execute(
            "INSERT INTO commit_in_flight (id, commit_message) VALUES (1, ?1)",
            [in_flight.commit.encode_to_vec()],
        )?;
        let mut stmt = self.0.prepare_cached(
            "INSERT INTO commit_in_flight_shapes (op_index, parent_id, name, kind, content)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (index, shape) in in_flight.sent.iter().enumerate() {
            let Some(shape) = shape else { continue };
            stmt.execute(params![
                index,
                non_empty(&shape.parent_id),
                shape.name,
                shape.kind,
                shape.content.as_ref().map(Content::encode_to_vec),
            ])?;
        }
        Ok(())
    }

    /// Every problem node, ordered by `node_id`.
    pub fn problems(&self) -> Result<Vec<Problem>> {
        let mut stmt = self
            .0
            .prepare_cached("SELECT node_id, reason FROM problems ORDER BY node_id")?;
        let rows = stmt.query_map([], |row| {
            Ok(Problem {
                node_id: row.get(0)?,
                reason: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn put_problem(&self, problem: &Problem) -> Result<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO problems (node_id, reason) VALUES (?1, ?2)",
            params![problem.node_id, problem.reason],
        )?;
        Ok(())
    }

    pub fn remove_problem(&self, node_id: &str) -> Result<bool> {
        self.remove("problems", node_id)
    }

    /// The engine's ID generator state, opaque to the store.
    pub fn id_generator(&self) -> Result<Option<Vec<u8>>> {
        Ok(self
            .0
            .query_row(
                "SELECT id_generator FROM engine_state WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set_id_generator(&self, state: &[u8]) -> Result<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO engine_state (id, id_generator) VALUES (1, ?1)",
            [state],
        )?;
        Ok(())
    }

    /// Records that the chunk is cached, or refreshes its size and last use.
    pub fn put_chunk(&self, hash: &[u8; 32], size: u64, now_ms: i64) -> Result<()> {
        self.0
            .prepare_cached(
                "INSERT OR REPLACE INTO chunk_cache (hash, size, last_used_ms) VALUES (?1, ?2, ?3)",
            )?
            .execute(params![hash.as_slice(), size, now_ms])?;
        Ok(())
    }

    pub fn chunk(&self, hash: &[u8; 32]) -> Result<Option<CachedChunk>> {
        Ok(self
            .0
            .query_row(
                "SELECT hash, size, last_used_ms FROM chunk_cache WHERE hash = ?1",
                [hash.as_slice()],
                read_chunk,
            )
            .optional()?)
    }

    /// Updates the chunk's last use. Returns whether it is cached.
    pub fn touch_chunk(&self, hash: &[u8; 32], now_ms: i64) -> Result<bool> {
        Ok(self.0.execute(
            "UPDATE chunk_cache SET last_used_ms = ?2 WHERE hash = ?1",
            params![hash.as_slice(), now_ms],
        )? > 0)
    }

    pub fn remove_chunk(&self, hash: &[u8; 32]) -> Result<bool> {
        Ok(self
            .0
            .execute("DELETE FROM chunk_cache WHERE hash = ?1", [hash.as_slice()])?
            > 0)
    }

    /// Total bytes of all cached chunks.
    pub fn chunk_cache_size(&self) -> Result<u64> {
        Ok(self.0.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM chunk_cache",
            [],
            |row| row.get(0),
        )?)
    }

    /// Up to `limit` cached chunks, least recently used first: the eviction order.
    pub fn least_recently_used(&self, limit: usize) -> Result<Vec<CachedChunk>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT hash, size, last_used_ms FROM chunk_cache ORDER BY last_used_ms, hash LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], read_chunk)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn remove(&self, table: &str, node_id: &str) -> Result<bool> {
        Ok(self
            .0
            .prepare_cached(&format!("DELETE FROM {table} WHERE node_id = ?1"))?
            .execute([node_id])?
            > 0)
    }
}

/// The shape columns `parent_id, name, kind, content` starting at column
/// `first`, with the content still encoded.
struct ShapeRow {
    shape: Shape,
    content: Option<Vec<u8>>,
}

impl ShapeRow {
    fn read(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            shape: Shape {
                parent_id: row.get::<_, Option<String>>(first)?.unwrap_or_default(),
                name: row.get(first + 1)?,
                kind: row.get(first + 2)?,
                content: None,
            },
            content: row.get(first + 3)?,
        })
    }

    fn into_shape(self) -> Result<Shape> {
        let mut shape = self.shape;
        shape.content = decode(self.content)?;
        Ok(shape)
    }
}

fn decode<M: Message + Default>(bytes: Option<Vec<u8>>) -> Result<Option<M>> {
    Ok(bytes.map(|bytes| M::decode(bytes.as_slice())).transpose()?)
}

fn read_chunk(row: &rusqlite::Row<'_>) -> rusqlite::Result<CachedChunk> {
    Ok(CachedChunk {
        hash: row.get(0)?,
        size: row.get(1)?,
        last_used_ms: row.get(2)?,
    })
}

fn non_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}
