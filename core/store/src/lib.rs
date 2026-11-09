// SPDX-License-Identifier: Apache-2.0

//! Local SQLite state for one synced space on one device: the server tree as
//! this device last applied it, the `(history_id, last_seq)` cursor, local ops
//! waiting to be committed, and the index of chunks cached on disk.
//!
//! Every read and write goes through [`Store::transaction`]. A transaction
//! either commits as a whole or leaves no trace, including when the process is
//! killed in the middle of it: the database runs in WAL mode with
//! `synchronous = FULL`, so a commit that returned is durable and an
//! interrupted one is rolled back the next time the file is opened.

use std::path::Path;
use std::time::Duration;

use flarebox_proto::v1::{NodeState, Op, VersionInfo};
use prost::Message;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

/// Schema migrations, applied in order. Migration `i` moves the database from
/// `user_version = i` to `user_version = i + 1`. Never edit a released entry;
/// append a new one.
const MIGRATIONS: &[&str] = &["
    CREATE TABLE cursor (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        history_id TEXT NOT NULL,
        last_seq INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE nodes (
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
    CREATE INDEX nodes_by_parent ON nodes (parent_id, name);

    CREATE TABLE pending_ops (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        op BLOB NOT NULL,
        commit_id TEXT
    ) STRICT;
    CREATE INDEX pending_ops_by_commit ON pending_ops (commit_id) WHERE commit_id IS NOT NULL;

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

/// A local op waiting to be committed. `commit_id` is set once the op is part
/// of a commit that may have been sent, so a restart resends the same
/// `commit_id` and the server's idempotency check applies.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingOp {
    pub id: i64,
    pub op: Op,
    pub commit_id: Option<String>,
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
                "SELECT history_id, last_seq FROM cursor WHERE id = 1",
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
            "INSERT INTO cursor (id, history_id, last_seq) VALUES (1, ?1, ?2)
             ON CONFLICT (id) DO UPDATE SET history_id = excluded.history_id, last_seq = excluded.last_seq",
            params![cursor.history_id, cursor.last_seq],
        )?;
        Ok(())
    }

    pub fn node(&self, node_id: &str) -> Result<Option<NodeState>> {
        self.0
            .query_row(
                &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE node_id = ?1"),
                [node_id],
                NodeRow::read,
            )
            .optional()?
            .map(NodeRow::into_state)
            .transpose()
    }

    /// The nodes whose parent is `parent_id`, live and trashed, ordered by name.
    pub fn children(&self, parent_id: &str) -> Result<Vec<NodeState>> {
        let mut stmt = self.0.prepare_cached(&format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE parent_id = ?1 ORDER BY name, node_id"
        ))?;
        let rows = stmt.query_map([parent_id], NodeRow::read)?;
        rows.map(|row| row?.into_state()).collect()
    }

    /// Inserts or replaces the node with `node.node_id`. An empty `parent_id`
    /// marks the root.
    pub fn put_node(&self, node: &NodeState) -> Result<()> {
        let current = node.current.as_ref().map(Message::encode_to_vec);
        self.0
            .prepare_cached(
                "INSERT OR REPLACE INTO nodes
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
                current,
            ])?;
        Ok(())
    }

    /// Removes the node row. Returns whether it existed.
    pub fn remove_node(&self, node_id: &str) -> Result<bool> {
        Ok(self
            .0
            .execute("DELETE FROM nodes WHERE node_id = ?1", [node_id])?
            > 0)
    }

    /// Appends an op to the pending queue and returns its id. Ids grow
    /// monotonically and are never reused.
    pub fn push_op(&self, op: &Op) -> Result<i64> {
        self.0.execute(
            "INSERT INTO pending_ops (op) VALUES (?1)",
            [op.encode_to_vec()],
        )?;
        Ok(self.0.last_insert_rowid())
    }

    /// All pending ops in the order they were pushed.
    pub fn pending_ops(&self) -> Result<Vec<PendingOp>> {
        let mut stmt = self
            .0
            .prepare_cached("SELECT id, op, commit_id FROM pending_ops ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (id, op, commit_id) = row?;
            Ok(PendingOp {
                id,
                op: Op::decode(op.as_slice())?,
                commit_id,
            })
        })
        .collect()
    }

    /// Rewrites a pending op in place, for example after the server merged a
    /// created folder into an existing one (spec §6.2 R2). Returns whether
    /// the op existed.
    pub fn replace_op(&self, id: i64, op: &Op) -> Result<bool> {
        Ok(self.0.execute(
            "UPDATE pending_ops SET op = ?2 WHERE id = ?1",
            params![id, op.encode_to_vec()],
        )? > 0)
    }

    /// Marks the given pending ops as part of the commit `commit_id`.
    pub fn assign_commit(&self, ids: &[i64], commit_id: &str) -> Result<()> {
        let mut stmt = self
            .0
            .prepare_cached("UPDATE pending_ops SET commit_id = ?2 WHERE id = ?1")?;
        for id in ids {
            stmt.execute(params![id, commit_id])?;
        }
        Ok(())
    }

    /// Drops the ops of a commit the server answered. Returns how many there were.
    pub fn remove_commit(&self, commit_id: &str) -> Result<usize> {
        Ok(self
            .0
            .execute("DELETE FROM pending_ops WHERE commit_id = ?1", [commit_id])?)
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
}

const NODE_COLUMNS: &str =
    "node_id, parent_id, name, kind, status, content_rev, meta_rev, trash_group, current";

struct NodeRow {
    state: NodeState,
    current: Option<Vec<u8>>,
}

impl NodeRow {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            state: NodeState {
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
            current: row.get(8)?,
        })
    }

    fn into_state(self) -> Result<NodeState> {
        let mut state = self.state;
        state.current = self
            .current
            .map(|bytes| VersionInfo::decode(bytes.as_slice()))
            .transpose()?;
        Ok(state)
    }
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
