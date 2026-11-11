// SPDX-License-Identifier: Apache-2.0

//! Local SQLite state for one synced space on one device, as the sync engine
//! design lays it out (docs/design/sync-engine.md §2.2 and §2.8):
//!
//! - the remote tree R: the `NodeState` records this device has applied, with
//!   the `(history_id, last_seq)` cursor;
//! - the synced tree S: per node, the shape on which disk and server last agreed;
//! - the local tree L: the disk as last observed, with file identities and the
//!   parked flag;
//! - the rest of the engine's state ([`EngineState`]): the commit in flight
//!   with its buffered result, the accepted counter, the halt, the effect in
//!   flight with the effect counter, the node sets and maps the trees, the
//!   commits and the effects keep, and the restores a person asked for with
//!   their outcomes;
//! - the restore requests the engine has not taken yet;
//! - the engine's ID generator state and the index of chunks cached on disk;
//! - the [`Link`]: the folder the store syncs and the device credential that
//!   writes it, and the questions the engine asked with their answers.
//!
//! The engine keeps the trees in memory and reports row-level changes after each
//! input; the driver writes them with the methods of [`Tx`] inside one
//! [`Store::transaction`]. A transaction either commits as a whole or leaves no
//! trace, including when the process is killed in the middle of it: the
//! database runs in WAL mode with `synchronous = FULL`, so a commit that
//! returned is durable and an interrupted one is rolled back the next time the
//! file is opened. On macOS the store also enables `fullfsync`, so each commit
//! ends with `F_FULLFSYNC` and survives power loss on drives that honor it;
//! `fsync` alone may leave the data in the drive's cache. Process-kill tests
//! cover the rollback, not power loss.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use engine::EngineState;
use koidrive_fs::FileId;
use koidrive_proto::v1::{Content, NodeState, VersionInfo};
use prost::Message;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

mod engine;
mod questions;
mod restore;

/// Schema migrations, applied in order. Migration `i` moves the database from
/// `user_version = i` to `user_version = i + 1`. Never edit an entry once it
/// has shipped; append a new one.
const MIGRATIONS: &[&str] = &[
    "
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
",
    "
    CREATE TABLE link (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        space_id TEXT NOT NULL,
        space_name TEXT NOT NULL,
        root BLOB NOT NULL,
        root_dev INTEGER NOT NULL,
        server_url TEXT NOT NULL,
        device_id TEXT NOT NULL,
        linked_at_ms INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE questions (
        question_id TEXT PRIMARY KEY,
        asked_at_ms INTEGER NOT NULL,
        body BLOB NOT NULL,
        answer TEXT,
        answered_at_ms INTEGER,
        CHECK ((answer IS NULL) = (answered_at_ms IS NULL))
    ) STRICT;
",
    "
    DROP TABLE commit_in_flight_shapes;
    DROP TABLE problems;
    ALTER TABLE commit_in_flight ADD COLUMN result BLOB;

    CREATE TABLE commit_in_flight_deletes (
        target TEXT PRIMARY KEY
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE commit_in_flight_delete_members (
        target TEXT NOT NULL,
        position INTEGER NOT NULL,
        node_id TEXT NOT NULL,
        PRIMARY KEY (target, position)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE commit_state (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        accepted INTEGER NOT NULL,
        halt TEXT CHECK (halt IN ('read_only', 'recovery', 'policy')),
        policy_code INTEGER,
        cap INTEGER CHECK (cap > 0),
        CHECK ((halt IS 'policy') = (policy_code IS NOT NULL))
    ) STRICT;

    CREATE TABLE effect_state (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        cleared INTEGER NOT NULL,
        in_flight BLOB
    ) STRICT;

    CREATE TABLE failed_effects (
        node_id TEXT PRIMARY KEY,
        effect BLOB NOT NULL
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE node_sets (
        node_set TEXT NOT NULL
            CHECK (node_set IN ('dirty', 'unsearched', 'reverted', 'problems', 'split')),
        node_id TEXT NOT NULL,
        PRIMARY KEY (node_set, node_id)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE node_maps (
        node_map TEXT NOT NULL CHECK (node_map IN ('parked_as', 'aliases')),
        node_id TEXT NOT NULL,
        target TEXT NOT NULL,
        PRIMARY KEY (node_map, node_id)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE waiting_updates (
        node_id TEXT PRIMARY KEY,
        parent_id TEXT,
        name TEXT,
        kind INTEGER,
        content BLOB,
        CHECK ((name IS NULL) = (kind IS NULL))
    ) STRICT, WITHOUT ROWID;
",
    "
    CREATE TABLE restore_requests (
        restore_id TEXT PRIMARY KEY,
        node_id TEXT NOT NULL,
        version_id TEXT,
        target_parent_id TEXT,
        name TEXT,
        CHECK (version_id IS NULL OR (target_parent_id IS NULL AND name IS NULL))
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE restore_intents (
        restore_id TEXT PRIMARY KEY,
        node_id TEXT NOT NULL,
        version_id TEXT,
        target_parent_id TEXT,
        name TEXT,
        position INTEGER UNIQUE,
        outcome TEXT
            CHECK (outcome IN ('restored', 'renamed', 'copied', 'rejected', 'does_not_fit')),
        outcome_node_id TEXT,
        outcome_name TEXT,
        reject_reason TEXT,
        CHECK (version_id IS NULL OR (target_parent_id IS NULL AND name IS NULL)),
        CHECK ((outcome IS NULL) = (position IS NULL)),
        CHECK ((outcome IS 'renamed' OR outcome IS 'copied') = (outcome_name IS NOT NULL)),
        CHECK ((outcome IS 'copied') = (outcome_node_id IS NOT NULL)),
        CHECK ((outcome IS 'rejected') = (reject_reason IS NOT NULL))
    ) STRICT, WITHOUT ROWID;
",
    "
    ALTER TABLE remote_cursor ADD COLUMN ahead_of_server INTEGER NOT NULL DEFAULT 0
        CHECK (ahead_of_server IN (0, 1));
",
    "
    CREATE TABLE watched_contents (
        node_id TEXT PRIMARY KEY,
        changed INTEGER NOT NULL CHECK (changed IN (0, 1))
    ) STRICT, WITHOUT ROWID;
",
    "
    CREATE TABLE trashed_here (
        node_id TEXT PRIMARY KEY,
        seq INTEGER NOT NULL
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE commit_slots (
        device_seq INTEGER PRIMARY KEY,
        seq INTEGER NOT NULL
    ) STRICT;
",
    "
    CREATE TABLE open_questions (
        question_id TEXT PRIMARY KEY,
        position INTEGER NOT NULL UNIQUE,
        asked_at_ms INTEGER NOT NULL,
        kind TEXT NOT NULL,
        answer TEXT
    ) STRICT;

    CREATE TABLE open_question_items (
        question_id TEXT NOT NULL REFERENCES open_questions (question_id),
        top_id TEXT NOT NULL,
        files INTEGER NOT NULL,
        PRIMARY KEY (question_id, top_id)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE open_question_members (
        question_id TEXT NOT NULL,
        top_id TEXT NOT NULL,
        node_id TEXT NOT NULL,
        content_rev INTEGER,
        meta_rev INTEGER,
        PRIMARY KEY (question_id, top_id, node_id),
        FOREIGN KEY (question_id, top_id) REFERENCES open_question_items (question_id, top_id),
        CHECK ((content_rev IS NULL) = (meta_rev IS NULL))
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE guard_ledger (
        node_id TEXT PRIMARY KEY,
        at_ms INTEGER NOT NULL,
        files INTEGER NOT NULL
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE guard_confirmed (
        top_id TEXT PRIMARY KEY,
        files INTEGER NOT NULL
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE guard_confirmed_members (
        top_id TEXT NOT NULL REFERENCES guard_confirmed (top_id),
        node_id TEXT NOT NULL,
        content_rev INTEGER,
        meta_rev INTEGER,
        PRIMARY KEY (top_id, node_id),
        CHECK ((content_rev IS NULL) = (meta_rev IS NULL))
    ) STRICT, WITHOUT ROWID;
",
    "
    CREATE TABLE open_question_shapes (
        question_id TEXT NOT NULL REFERENCES open_questions (question_id),
        node_id TEXT NOT NULL,
        parent_id TEXT NOT NULL,
        name TEXT NOT NULL,
        kind INTEGER NOT NULL,
        content BLOB,
        PRIMARY KEY (question_id, node_id)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE discarded_nodes (
        node_id TEXT PRIMARY KEY
    ) STRICT, WITHOUT ROWID;
",
    "
    CREATE TABLE recorded_nodes (
        node_id TEXT PRIMARY KEY,
        displaced INTEGER NOT NULL CHECK (displaced IN (0, 1)),
        dev INTEGER,
        ino INTEGER,
        birth_ns INTEGER,
        CHECK ((displaced = 1) = (dev IS NOT NULL)),
        CHECK ((dev IS NULL) = (ino IS NULL)),
        CHECK (dev IS NOT NULL OR birth_ns IS NULL)
    ) STRICT, WITHOUT ROWID;
",
    "
    CREATE TABLE edited_here (
        node_id TEXT PRIMARY KEY,
        seq INTEGER NOT NULL
    ) STRICT, WITHOUT ROWID;
",
    "
    CREATE TABLE guard_remote (
        top_id TEXT NOT NULL,
        known INTEGER NOT NULL CHECK (known IN (0, 1)),
        trash_group TEXT NOT NULL,
        trashed_seq INTEGER NOT NULL,
        at_ms INTEGER NOT NULL,
        files INTEGER NOT NULL,
        PRIMARY KEY (top_id, known, trash_group, trashed_seq)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE guard_remote_dismissed (
        top_id TEXT NOT NULL,
        known INTEGER NOT NULL CHECK (known IN (0, 1)),
        trash_group TEXT NOT NULL,
        trashed_seq INTEGER NOT NULL,
        PRIMARY KEY (top_id, known, trash_group, trashed_seq)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE guard_confirmed_limits (
        limit_crossed TEXT PRIMARY KEY
    ) STRICT, WITHOUT ROWID;
",
    "
    CREATE TABLE watched_positions (
        node_id TEXT PRIMARY KEY,
        rev INTEGER
    ) STRICT, WITHOUT ROWID;
",
    "
    ALTER TABLE watched_positions ADD COLUMN parent_id TEXT;
    ALTER TABLE watched_positions ADD COLUMN name TEXT;
    UPDATE watched_positions
    SET parent_id = synced_nodes.parent_id, name = synced_nodes.name
    FROM synced_nodes
    WHERE synced_nodes.node_id = watched_positions.node_id AND watched_positions.rev IS NOT NULL;
    UPDATE watched_positions SET rev = NULL WHERE name IS NULL;
",
];

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
    #[error(
        "database schema version {found} is older than this build's ({supported}); the daemon updates it when it starts"
    )]
    SchemaTooOld { found: u32, supported: u32 },
    #[error(
        "{0} holds changes that are not in the database file yet; the daemon applies them when it starts"
    )]
    LogNotApplied(PathBuf),
    #[error("the database did not accept the durability settings: {0:?}")]
    NotDurable(Durability),
    #[error("the stored engine state is invalid: {0}")]
    InvalidEngineState(String),
}

/// The connection settings that make a returned commit durable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Durability {
    pub journal_mode: String,
    /// `PRAGMA synchronous`: 2 is `FULL`.
    pub synchronous: i64,
    pub fullfsync: bool,
}

impl Durability {
    /// What [`Store::open`] requires on this platform.
    pub fn required() -> Self {
        Self {
            journal_mode: "wal".into(),
            synchronous: 2,
            fullfsync: cfg!(target_os = "macos"),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Where this device stands in the space's commit log (spec §7.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub history_id: String,
    pub last_seq: u64,
    /// The server reported its log below `last_seq` (`AHEAD_OF_SERVER`), and no bootstrap has replaced R
    /// since: the server lost commits this device applied.
    pub ahead_of_server: bool,
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
/// node sits in `.koidrive/parked/<node_id>` while `shape` keeps the
/// position the engine compares it at.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalNode {
    pub node_id: String,
    pub shape: Shape,
    pub identity: FileId,
    pub parked: bool,
}

/// The folder this store syncs and the credential that writes it. One device
/// credential writes a store, and a store belongs to one linked folder: a
/// driver refuses to sync a store whose `device_id` is not its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub space_id: String,
    pub space_name: String,
    /// The linked folder's absolute path.
    pub root: PathBuf,
    /// The device (`st_dev`) the folder was on when it was linked, for the
    /// root check of design §6.2.
    pub root_dev: u64,
    pub server_url: String,
    pub device_id: String,
    pub linked_at_ms: i64,
}

/// A question the engine asked the person (`AskUser`, design §1), with its
/// answer once given. The body is opaque to the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredQuestion {
    pub question_id: String,
    pub asked_at_ms: i64,
    pub body: Vec<u8>,
    pub answer: Option<GivenAnswer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GivenAnswer {
    pub answer: String,
    pub answered_at_ms: i64,
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
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        #[cfg(target_os = "macos")]
        conn.pragma_update(None, "fullfsync", true)?;
        let mut store = Self { conn };
        let durability = store.durability()?;
        if durability != Durability::required() {
            return Err(Error::NotDurable(durability));
        }
        migrate(&mut store.conn)?;
        Ok(store)
    }

    /// Opens the database at `path` to read it without writing anything: no migration, and no journal or shared-memory
    /// file. SQLite takes the file as unchanging, so the caller makes sure that no process writes it meanwhile. A
    /// database of another schema version is refused, and so is one whose write-ahead log still holds changes, which
    /// only a writable open applies.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut log = path.as_os_str().to_owned();
        log.push("-wal");
        let log = PathBuf::from(log);
        if std::fs::metadata(&log).is_ok_and(|log| log.len() > 0) {
            return Err(Error::LogNotApplied(log));
        }
        let mut uri = String::from("file:");
        for &byte in path.as_os_str().as_bytes() {
            match byte {
                b'/' | b'-' | b'_' | b'.' | b'~' | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' => {
                    uri.push(char::from(byte))
                }
                _ => uri.push_str(&format!("%{byte:02X}")),
            }
        }
        uri.push_str("?immutable=1");
        let conn = Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let found: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let supported = SCHEMA_VERSION;
        if found > supported {
            return Err(Error::SchemaTooNew { found, supported });
        }
        if found < supported {
            return Err(Error::SchemaTooOld { found, supported });
        }
        Ok(Self { conn })
    }

    /// The durability settings in effect on this connection.
    pub fn durability(&self) -> Result<Durability> {
        let pragma = |name: &str| {
            self.conn
                .pragma_query_value(None, name, |row| row.get::<_, rusqlite::types::Value>(0))
        };
        let journal_mode = match pragma("journal_mode")? {
            rusqlite::types::Value::Text(mode) => mode.to_ascii_lowercase(),
            other => format!("{other:?}"),
        };
        let integer = |value| match value {
            rusqlite::types::Value::Integer(n) => n,
            _ => -1,
        };
        Ok(Durability {
            journal_mode,
            synchronous: integer(pragma("synchronous")?),
            fullfsync: integer(pragma("fullfsync")?) == 1,
        })
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
                "SELECT history_id, last_seq, ahead_of_server FROM remote_cursor WHERE id = 1",
                [],
                |row| {
                    Ok(Cursor {
                        history_id: row.get(0)?,
                        last_seq: row.get(1)?,
                        ahead_of_server: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn set_cursor(&self, cursor: &Cursor) -> Result<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO remote_cursor (id, history_id, last_seq, ahead_of_server)
             VALUES (1, ?1, ?2, ?3)",
            params![cursor.history_id, cursor.last_seq, cursor.ahead_of_server],
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

    /// Removes every node of the remote tree, before a bootstrap replaces it.
    pub fn clear_remote_nodes(&self) -> Result<()> {
        self.0.execute("DELETE FROM remote_nodes", [])?;
        Ok(())
    }

    /// How many nodes the remote tree holds, live and trashed.
    pub fn remote_node_count(&self) -> Result<u64> {
        Ok(self
            .0
            .query_row("SELECT COUNT(*) FROM remote_nodes", [], |row| row.get(0))?)
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

    pub fn link(&self) -> Result<Option<Link>> {
        Ok(self
            .0
            .query_row(
                "SELECT space_id, space_name, root, root_dev, server_url, device_id, linked_at_ms
                 FROM link WHERE id = 1",
                [],
                |row| {
                    Ok(Link {
                        space_id: row.get(0)?,
                        space_name: row.get(1)?,
                        root: PathBuf::from(OsStr::from_bytes(&row.get::<_, Vec<u8>>(2)?)),
                        root_dev: row.get(3)?,
                        server_url: row.get(4)?,
                        device_id: row.get(5)?,
                        linked_at_ms: row.get(6)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn set_link(&self, link: &Link) -> Result<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO link
             (id, space_id, space_name, root, root_dev, server_url, device_id, linked_at_ms)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                link.space_id,
                link.space_name,
                link.root.as_os_str().as_bytes(),
                link.root_dev,
                link.server_url,
                link.device_id,
                link.linked_at_ms,
            ],
        )?;
        Ok(())
    }

    /// Every question, answered or not, oldest first.
    pub fn questions(&self) -> Result<Vec<StoredQuestion>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT question_id, asked_at_ms, body, answer, answered_at_ms
             FROM questions ORDER BY asked_at_ms, question_id",
        )?;
        let rows = stmt.query_map([], |row| {
            let answer: Option<String> = row.get(3)?;
            let answered_at_ms: Option<i64> = row.get(4)?;
            Ok(StoredQuestion {
                question_id: row.get(0)?,
                asked_at_ms: row.get(1)?,
                body: row.get(2)?,
                answer: answer
                    .zip(answered_at_ms)
                    .map(|(answer, answered_at_ms)| GivenAnswer {
                        answer,
                        answered_at_ms,
                    }),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Inserts or replaces the question with `question.question_id`.
    pub fn put_question(&self, question: &StoredQuestion) -> Result<()> {
        let (answer, answered_at_ms) = match &question.answer {
            Some(given) => (Some(given.answer.as_str()), Some(given.answered_at_ms)),
            None => (None, None),
        };
        self.0.execute(
            "INSERT OR REPLACE INTO questions
             (question_id, asked_at_ms, body, answer, answered_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                question.question_id,
                question.asked_at_ms,
                question.body,
                answer,
                answered_at_ms,
            ],
        )?;
        Ok(())
    }

    /// Records the answer to an unanswered question. Returns false, changing
    /// nothing, when the question does not exist or already has an answer.
    pub fn answer_question(&self, question_id: &str, answer: &GivenAnswer) -> Result<bool> {
        Ok(self.0.execute(
            "UPDATE questions SET answer = ?2, answered_at_ms = ?3
             WHERE question_id = ?1 AND answer IS NULL",
            params![question_id, answer.answer, answer.answered_at_ms],
        )? > 0)
    }

    /// Replaces how a stored question is shown, keeping any answer it was given.
    pub fn reshow_question(&self, question_id: &str, body: &[u8]) -> Result<()> {
        self.0.execute(
            "UPDATE questions SET body = ?2 WHERE question_id = ?1",
            params![question_id, body],
        )?;
        Ok(())
    }

    pub fn remove_question(&self, question_id: &str) -> Result<bool> {
        Ok(self.0.execute(
            "DELETE FROM questions WHERE question_id = ?1",
            [question_id],
        )? > 0)
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
