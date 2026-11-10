// SPDX-License-Identifier: Apache-2.0

//! The task that syncs one linked folder.
//!
//! [`Folder`] is the folder while it syncs. It owns the engine, the store and the disk, and it is their only
//! writer. Each of its public methods takes one input: a session event, an observation batch, or the result of work
//! it asked for. A method runs its store transactions with no await and returns the [`Work`] to start. After every
//! input it decides what runs next, in the order of the engine's reference loop (`core/sim/tests/engine_sync.rs`):
//! an effect first, then a commit, and never an effect while a commit is in flight.
//!
//! [`FolderTask`] is the shell around it. It checks the root, claims the folder, starts the observer's thread and the
//! session, and then runs one `select!` that hands each input to the folder and runs the returned work as futures.
//! A future owns what it needs and borrows only the session and the transfer client, so none holds the store or
//! the engine across an await.

use std::collections::BTreeMap;
use std::fs::File;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use flarebox_engine::commit::{Commits, Failure, Halt as CommitHalt, Reconnect};
use flarebox_engine::state::CommitsState;
use flarebox_fs::{Snapshot, check_root};
use flarebox_net::{
    Auth, Backoff, CallError, ChunkError, Event, ServerError, Session, SessionConfig, Transfer,
};
use flarebox_proto::v1::error::Code;
use flarebox_proto::v1::resync_required::Reason;
use flarebox_proto::v1::{
    ChunkRef, Commit, CommitRecord, CommitResult, Error as ProtoError, TreePage, UploadReceipt,
    Welcome,
};
use flarebox_store::{Link, Store, Tx};
use tokio::sync::{Notify, mpsc, watch};

use crate::claim::{self, Flock, Owner};
use crate::disk::{Batch, Disk, Pending, Sections, Settled, Staged, Stall, Stop, Upload};
use crate::driver::check_placement;
use crate::engine::{Applied, Engine, Staging};
use crate::ignore::Matcher;
use crate::observe::{FsObserver, Observer};
use crate::policy::{self, Disposition};
use crate::status::{FolderState, FolderStatus};

/// The highest space format this build reads (spec v0 section 9.2).
pub const MAX_FORMAT: u32 = 1;
/// Nodes per `ListTree` page, the most spec v0 section 13 allows.
const TREE_PAGE: u32 = 5_000;
/// How often a folder that failed its root check is checked again.
const ROOT_RECHECK: Duration = Duration::from_secs(30);
/// How often a running folder checks that its path still names the folder it locked.
const CLAIM_RECHECK: Duration = Duration::from_secs(2);
/// The wait before a commit is built again after a file changed while its content was uploaded. The observer
/// reads the file again meanwhile.
const BUILD_AGAIN: Duration = Duration::from_secs(1);
/// The waits before a node whose disk effect failed is planned again: from about a second, doubling with each
/// failure of the node in a row, up to ten minutes.
const EFFECT_RETRY: Backoff = Backoff {
    initial: Duration::from_secs(1),
    max: Duration::from_secs(600),
};

/// Why a folder halts once its path no longer leads to the folder its task locked.
const REPLACED: &str = "the folder at this path is no longer the folder this task locked (it was moved, replaced or \
                        removed); restart to sync it again";

/// One linked folder while it syncs.
pub(crate) struct Folder {
    link: Link,
    engine: Engine,
    store: Store,
    disk: Disk,
    stream: Stream,
    flight: Flight,
    /// The observer has scanned the folder once. Nothing is planned before, because L does not describe the disk
    /// until then.
    scanned: bool,
    offline: bool,
    /// The history the server reported in its last `Welcome`.
    history_id: String,
    head_seq: Option<u64>,
    local_entries: Option<u64>,
    remote_nodes: u64,
    /// Commit sends and uploads that failed in a row, for the backoff.
    failures: u32,
    backoff: Backoff,
    /// What the last transient failure was.
    note: Option<String>,
    /// The disk effects of each node that failed in a row in this task, for [`EFFECT_RETRY`].
    effect_failures: BTreeMap<String, u32>,
    /// When the [`Work::RetryAfter`] that is out ends.
    retry_at: Option<Instant>,
}

/// Where R stands against the session's change stream (design §4.4). It is not stored: the next `Welcome` and the
/// store's cursor give it again.
enum Stream {
    /// No `Welcome` yet.
    Connecting,
    /// The tree is being listed for a bootstrap. R is untouched.
    Listing,
    /// The listing follows the log until it may replace R.
    Replaying(Staging),
    /// R follows the log. `head` is the server's head as of the last `CaughtUp` of this connection.
    Following { head: Option<u64> },
}

/// What this process is doing with the commit in flight. The commit itself is the engine's and is in the store.
enum Flight {
    /// No commit in flight, or one whose result waits until R has reached it.
    Idle,
    /// The store held a commit in flight when the task started. The server may have it, so it is only ever sent
    /// again unchanged, once a `Welcome` allows that (design §2.8).
    Unknown,
    /// The content the commit adds is being uploaded from these files. The server holds nothing of the commit.
    Uploading(Vec<PathBuf>),
    /// `Session::commit` is out with this copy of the commit, which carries the upload's receipts. The session
    /// resends it after a reconnect, so the folder starts no second call.
    Sending(Commit),
    /// A [`Work::ResendAfter`] is out.
    Waiting(Next),
}

/// What follows a wait.
enum Next {
    /// Upload the commit's content again. Only for a commit the server holds nothing of.
    Upload,
    /// Send the same commit again.
    Send(Commit),
    /// The commit was abandoned. Build the next one.
    Build,
}

/// Work a step asks the shell to start. Each owns what it needs.
pub(crate) enum Work {
    /// `Session::subscribe`.
    Subscribe { history_id: String, since_seq: u64 },
    /// Read every page of the tree. Answered with [`Folder::listed`].
    List,
    /// [`Pending::stage`]. Answered with [`Folder::staged`].
    Stage(Pending),
    /// The same after a wait.
    StageAfter(Duration, Pending),
    /// `Transfer::upload` of the chunks a commit adds. Answered with [`Folder::uploaded`].
    Upload(Upload),
    /// `Session::commit`. Answered with [`Folder::sent`].
    Send(Commit),
    /// Answered with [`Folder::resend`] after the wait.
    ResendAfter(Duration),
    /// Answered with [`Folder::retry`] after the wait. A later one replaces it, and always ends earlier.
    RetryAfter(Duration),
}

/// The part of a folder's status the task knows.
#[derive(Debug, PartialEq)]
pub(crate) struct View {
    pub state: FolderState,
    pub history_id: Option<String>,
    pub last_seq: Option<u64>,
    pub head_seq: Option<u64>,
    pub remote_nodes: u64,
    pub local_entries: Option<u64>,
    pub problem: Option<String>,
}

impl Folder {
    /// Opens the folder's store, loads the engine and starts the disk through `lock`, the descriptor the task
    /// locked. [`Disk::start`] recovers an effect that was in flight before anything is observed or planned. The
    /// engine then takes the restores asked for while no task ran. Every node whose effect failed is unblocked, so each
    /// is planned again once after the first scan of this start.
    pub fn open(link: Link, lock: &File, store: &Path) -> Result<Folder, Stop> {
        let mut store = Store::open(store)?;
        let mut engine = store.transaction(|tx| Engine::load(tx, &link.space_id))?;
        let disk = Disk::start(lock, &link, &mut engine, &mut store)?;
        for node in engine.effects.blocked().clone().into_keys() {
            engine.effects.unblock(&node);
        }
        let remote_nodes = store.transaction(|tx| {
            engine.accept_restores(tx)?;
            engine.checkpoint(tx)?;
            tx.remote_node_count()
        })?;
        let flight = match engine.commits.in_flight() {
            Some(flight) if flight.result.is_none() => Flight::Unknown,
            _ => Flight::Idle,
        };
        Ok(Folder {
            link,
            engine,
            store,
            disk,
            stream: Stream::Connecting,
            flight,
            scanned: false,
            offline: false,
            history_id: String::new(),
            head_seq: None,
            local_entries: None,
            remote_nodes,
            failures: 0,
            backoff: Backoff::default(),
            note: None,
            effect_failures: BTreeMap::new(),
            retry_at: None,
        })
    }

    /// What the observer's thread shares with the effect sections.
    pub fn sections(&self) -> Arc<Sections> {
        self.disk.sections()
    }

    /// The snapshot the observer starts from: the disk as L describes it.
    pub fn snapshot(&self) -> Snapshot {
        self.engine.snapshot()
    }

    /// One event of the session.
    pub fn event(&mut self, event: Event) -> Result<Vec<Work>, Stop> {
        let mut work = Vec::new();
        match event {
            Event::Connected(welcome) => self.connected(&welcome, &mut work)?,
            Event::Changes(records) => self.changes(&records, &mut work)?,
            Event::Committed(committed) => self.result(&committed.commit_id, committed.result)?,
            Event::CaughtUp { head_seq } => {
                self.head_seq = Some(head_seq);
                match &mut self.stream {
                    Stream::Replaying(staging) => staging.caught_up(head_seq),
                    Stream::Following { head } => *head = Some(head_seq),
                    Stream::Connecting | Stream::Listing => {}
                }
                self.publish_remote()?;
            }
            Event::ResyncRequired(resync) => match Reason::try_from(resync.reason) {
                Ok(Reason::LogTruncated) => {
                    tracing::info!(
                        space_id = self.link.space_id,
                        "the server's log no longer reaches the store; reading the tree again"
                    );
                    if !matches!(self.stream, Stream::Listing) {
                        self.stream = Stream::Listing;
                        work.push(Work::List);
                    }
                }
                Ok(Reason::HistoryChanged) => {
                    return Err(Stop::Recovery(
                        "the space was rewound or restored on the server".into(),
                    ));
                }
                Ok(Reason::AheadOfServer) => {
                    return Err(Stop::Recovery(
                        "the server is behind what this device already applied".into(),
                    ));
                }
                Ok(Reason::Unspecified) | Err(_) => {
                    return Err(Stop::Protocol(format!(
                        "the server asked for a resync for an unknown reason ({})",
                        resync.reason
                    )));
                }
            },
            Event::Notice(notice) => {
                tracing::info!(
                    space_id = self.link.space_id,
                    kind = notice.kind,
                    retry_after_ms = notice.retry_after_ms,
                    message = notice.message,
                    "notice from the server"
                );
            }
            Event::Disconnected { reason, retry_in } => {
                tracing::info!(
                    space_id = self.link.space_id,
                    ?reason,
                    ?retry_in,
                    "disconnected from the server"
                );
                self.offline = true;
                self.note = Some(format!(
                    "disconnected ({reason:?}); retrying in {retry_in:?}"
                ));
            }
            Event::Stopped(reason) => return Err(Stop::Session(reason)),
        }
        self.drive(&mut work)?;
        Ok(work)
    }

    /// The pages of a bootstrap, read in order. The log is replayed onto them from the first page's `head_seq`
    /// before they replace R.
    pub fn listed(&mut self, pages: Result<Vec<TreePage>, Stop>) -> Result<Vec<Work>, Stop> {
        let pages = pages?;
        if !matches!(self.stream, Stream::Listing) {
            return Ok(Vec::new());
        }
        let staging = Staging::new(self.history_id.clone(), pages);
        tracing::info!(
            space_id = self.link.space_id,
            seq = staging.last_seq(),
            "read the space's tree"
        );
        let work = vec![Work::Subscribe {
            history_id: self.history_id.clone(),
            since_seq: staging.last_seq(),
        }];
        self.stream = Stream::Replaying(staging);
        Ok(work)
    }

    /// One observation batch, with the number of entries the folder holds after it.
    pub fn observed(&mut self, batch: Batch, entries: u64) -> Result<Vec<Work>, Stop> {
        self.local_entries = Some(entries);
        self.disk
            .observed(batch, &mut self.engine, &mut self.store)?;
        let mut work = Vec::new();
        self.drive(&mut work)?;
        Ok(work)
    }

    /// The observer has scanned the folder once. Design §2.7 leaves a node the server refused an op for out of
    /// commits until the next rescan. This scan is that rescan, so the engine may plan every such node again from
    /// here on. Nothing else clears them.
    pub fn scanned(&mut self) -> Result<Vec<Work>, Stop> {
        self.scanned = true;
        if !self.engine.commits.problems().is_empty() {
            self.step(|engine, _| {
                for node in engine.commits.problems().clone() {
                    engine.commits.clear_problem(&node);
                }
                Ok(())
            })?;
        }
        self.publish_remote()?;
        let mut work = Vec::new();
        self.drive(&mut work)?;
        Ok(work)
    }

    /// `Driver::restore` recorded a request. The engine takes what the inbox holds, and the next commit carries
    /// its `Restore` op (design §6.3).
    pub fn requested(&mut self) -> Result<Vec<Work>, Stop> {
        self.step(|engine, tx| Ok(engine.accept_restores(tx)?))?;
        let mut work = Vec::new();
        self.drive(&mut work)?;
        Ok(work)
    }

    /// The staging of `pending` ended. A staged effect is applied here, in the effect section. A transfer that
    /// failed for now is staged again from the same `pending` after its wait, since nothing else can stage the
    /// effect in flight.
    pub fn staged(
        &mut self,
        pending: Pending,
        staged: Result<Staged, Stall>,
    ) -> Result<Vec<Work>, Stop> {
        let staged = match staged {
            Ok(staged) => staged,
            Err(Stall::Later(wait)) => return Ok(vec![Work::StageAfter(wait, pending)]),
            Err(Stall::Halt(stop)) => return Err(stop),
        };
        let node = (self.engine.effects.in_flight()).map(|record| record.node.clone());
        let settled = self.disk.apply(staged, &mut self.engine, &mut self.store)?;
        let mut work = Vec::new();
        if let Some(node) = node {
            match settled {
                Settled::Done => {
                    self.effect_failures.remove(&node);
                }
                Settled::Failed(why) if self.engine.effects.blocked().contains_key(&node) => {
                    let failures = self.effect_failures.entry(node).or_default();
                    let wait = EFFECT_RETRY.delay(*failures);
                    *failures += 1;
                    tracing::warn!(
                        space_id = self.link.space_id,
                        why,
                        ?wait,
                        "an effect failed; its node is planned again after the wait, or once what is planned for \
                         it changes"
                    );
                    let at = Instant::now() + wait;
                    if self.retry_at.is_none_or(|out| at < out) {
                        self.retry_at = Some(at);
                        work.push(Work::RetryAfter(wait));
                    }
                }
                Settled::Failed(why) => {
                    tracing::info!(
                        space_id = self.link.space_id,
                        why,
                        "an effect failed and is planned again"
                    );
                }
            }
        }
        self.drive(&mut work)?;
        Ok(work)
    }

    /// A [`Work::RetryAfter`] ended. Every node whose effect failed is unblocked, so the engine plans its effect again
    /// even when it is the one that failed: the disk may have changed in a way the observer does not report, such as
    /// a name that was taken for a moment. A node that fails again waits longer the next time.
    pub fn retry(&mut self) -> Result<Vec<Work>, Stop> {
        self.retry_at = None;
        if !self.engine.effects.blocked().is_empty() {
            self.step(|engine, _| {
                for node in engine.effects.blocked().clone().into_keys() {
                    engine.effects.unblock(&node);
                }
                Ok(())
            })?;
        }
        let mut work = Vec::new();
        self.drive(&mut work)?;
        Ok(work)
    }

    /// The upload of the content a commit adds ended. With its receipts the commit is sent. A file that changed
    /// since it was chunked, or that can no longer be read, abandons the commit, which the server holds nothing
    /// of (design §4.2), and the observer reads the commit's files again.
    pub fn uploaded(
        &mut self,
        receipts: Result<Vec<UploadReceipt>, ChunkError>,
    ) -> Result<Vec<Work>, Stop> {
        let mut work = Vec::new();
        let Flight::Uploading(paths) = std::mem::replace(&mut self.flight, Flight::Idle) else {
            return Ok(work);
        };
        let error = match receipts {
            Ok(receipts) => {
                self.failures = 0;
                if let Some(flight) = self.engine.commits.in_flight() {
                    let commit = Commit {
                        receipts,
                        ..flight.commit.clone()
                    };
                    self.flight = Flight::Sending(commit.clone());
                    work.push(Work::Send(commit));
                }
                return Ok(work);
            }
            Err(error) => error,
        };
        let disposition = match &error {
            ChunkError::SourceChanged { .. } | ChunkError::Io { .. } => {
                tracing::info!(
                    space_id = self.link.space_id,
                    %error,
                    "a file changed while it was uploaded; planning its commit again"
                );
                self.step(|engine, _| Ok(engine.commits.abandon()))?;
                self.disk.rescan(&self.engine, paths);
                self.flight = Flight::Waiting(Next::Build);
                work.push(Work::ResendAfter(BUILD_AGAIN));
                return Ok(work);
            }
            ChunkError::Call(CallError::Stopped(_)) => {
                self.flight = Flight::Uploading(paths);
                return Ok(work);
            }
            ChunkError::Call(call) => policy::after_call_error(call),
            ChunkError::Refused {
                error: Some(refusal),
                ..
            } => policy::after_refusal(refusal),
            ChunkError::Refused { .. } | ChunkError::Unavailable { .. } => Disposition::Backoff,
            ChunkError::NoTarget { .. } | ChunkError::Corrupt { .. } => Disposition::Halt,
        };
        let wait = match disposition {
            Disposition::RetryAfter(wait) => wait,
            Disposition::Backoff => self.backoff.delay(self.failures),
            Disposition::Halt => {
                return Err(Stop::Refused(format!(
                    "uploading a file's content failed: {error}"
                )));
            }
        };
        tracing::info!(space_id = self.link.space_id, %error, ?wait, "uploading again later");
        self.note = Some(format!("uploading a file's content failed: {error}"));
        self.failures += 1;
        self.flight = Flight::Waiting(Next::Upload);
        work.push(Work::ResendAfter(wait));
        Ok(work)
    }

    /// `Session::commit` returned. A result is acknowledged once R has reached it. A refusal goes to the engine,
    /// which says whether the same commit is sent again, its changes are planned again, or committing stops.
    pub fn sent(&mut self, result: Result<CommitResult, CallError>) -> Result<Vec<Work>, Stop> {
        let mut work = Vec::new();
        let Flight::Sending(commit) = std::mem::replace(&mut self.flight, Flight::Idle) else {
            return Ok(work);
        };
        match result {
            Ok(result) => {
                self.failures = 0;
                self.note = None;
                self.result(&commit.commit_id, result)?;
            }
            Err(CallError::Server(refusal)) => self.refused(commit, &refusal, &mut work)?,
            Err(CallError::Stopped(_)) => {
                self.flight = Flight::Unknown;
                return Ok(work);
            }
            Err(CallError::NotResent(why)) => {
                return Err(Stop::Recovery(format!(
                    "a commit was not sent again after reconnecting: {why}"
                )));
            }
            Err(CallError::TooLarge { bytes }) => {
                let refusal = ServerError(ProtoError {
                    code: Code::LimitExceeded as i32,
                    message: format!("the commit is {bytes} bytes, more than a message may hold"),
                    ..ProtoError::default()
                });
                self.refused(commit, &refusal, &mut work)?;
            }
            Err(error @ CallError::UnexpectedResult) => {
                return Err(Stop::Protocol(format!("sending a commit failed: {error}")));
            }
        }
        self.drive(&mut work)?;
        Ok(work)
    }

    /// A wait is over: upload or send the commit again, or build the next one.
    pub fn resend(&mut self) -> Result<Vec<Work>, Stop> {
        let mut work = Vec::new();
        match std::mem::replace(&mut self.flight, Flight::Idle) {
            Flight::Waiting(Next::Send(commit)) => {
                self.flight = Flight::Sending(commit.clone());
                work.push(Work::Send(commit));
            }
            Flight::Waiting(Next::Upload) => {
                if let Some(commit) = self.in_flight() {
                    self.start(commit, &mut work);
                }
            }
            Flight::Waiting(Next::Build) => {}
            other => self.flight = other,
        }
        self.drive(&mut work)?;
        Ok(work)
    }

    /// The folder's status as the task knows it.
    pub fn view(&self) -> View {
        let state = match &self.stream {
            _ if self.offline => FolderState::Offline,
            Stream::Connecting => FolderState::Connecting,
            Stream::Listing | Stream::Replaying(_) => FolderState::Bootstrapping,
            Stream::Following { .. } if self.settled() => FolderState::UpToDate,
            Stream::Following { .. } => FolderState::CatchingUp,
        };
        let stuck = self.engine.effects.blocked().len() + self.engine.commits.problems().len();
        let problem = match self.engine.commits.halted() {
            _ if self.note.is_some() => self.note.clone(),
            Some(CommitHalt::ReadOnly) => {
                Some("the server takes no changes from this device".into())
            }
            Some(CommitHalt::Policy { code }) => Some(format!(
                "the server refused this device's changes (code {code}); nothing more is sent"
            )),
            Some(CommitHalt::Recovery) => {
                Some("the folder's sync state does not match the server".into())
            }
            None if stuck > 0 => Some(format!("{stuck} items could not be synced")),
            None => None,
        };
        let last_seq = self.engine.cursor.as_ref().map(|c| c.last_seq);
        View {
            state,
            history_id: self.engine.cursor.as_ref().map(|c| c.history_id.clone()),
            last_seq,
            // The server reports its head only in a `Welcome` and a `CaughtUp`, and R may be past that since.
            head_seq: self.head_seq.max(last_seq),
            remote_nodes: self.remote_nodes,
            local_entries: self.local_entries,
            problem,
        }
    }

    /// True when nothing is left to move: R holds the server's head, the folder was scanned, no effect and no
    /// commit is in flight, every node the trees still differ on is one that cannot be synced as it is, and so is
    /// the node of every restore that waits.
    fn settled(&self) -> bool {
        let stuck = |id: &String| {
            self.engine.effects.blocked().contains_key(id)
                || self.engine.commits.problems().contains(id)
        };
        let commits = &self.engine.commits;
        self.caught_up()
            && self.scanned
            && self.engine.effects.in_flight().is_none()
            && matches!(self.flight, Flight::Idle)
            && self.engine.commits.in_flight().is_none()
            && self.engine.trees.dirty().iter().all(stuck)
            && commits.held_restores().count() == commits.restores().len()
    }

    /// True once R holds everything the server had at the last `CaughtUp` of this connection (design §4.4).
    fn caught_up(&self) -> bool {
        match (&self.stream, &self.engine.cursor) {
            (Stream::Following { head: Some(head) }, Some(cursor)) => cursor.last_seq >= *head,
            _ => false,
        }
    }

    fn in_flight(&self) -> Option<Commit> {
        (self.engine.commits.in_flight()).map(|flight| flight.commit.clone())
    }

    /// Runs one change of the engine and its checkpoint in one store transaction.
    fn step<T>(
        &mut self,
        change: impl FnOnce(&mut Engine, &Tx<'_>) -> Result<T, Stop>,
    ) -> Result<T, Stop> {
        let engine = &mut self.engine;
        self.store.transaction(|tx| {
            let out = change(engine, tx)?;
            engine.checkpoint(tx)?;
            Ok(out)
        })
    }

    /// A `Welcome` (design §4.4 and §2.8). A cursor of another history stops the task before anything is replaced.
    /// The engine then compares the server's count of this device's commits with the store. A folder that was
    /// never synced lists the tree, and any other subscribes from its cursor once. After a reconnect the session
    /// resumes by itself.
    ///
    /// A store that never synced (no cursor, no commit in flight and none acknowledged) holds no commit of its own
    /// to account for. When the server counts commits of this device at such a store's first `Welcome`, the device
    /// synced the space with a store that is gone, as after an unlink and a second link, and the new store continues
    /// from the server's count (design §6.4, step 2). RFC 0002 section 2 sends every count but the store's own to
    /// recovery, and every other store still goes there.
    fn connected(&mut self, welcome: &Welcome, work: &mut Vec<Work>) -> Result<(), Stop> {
        self.offline = false;
        self.note = None;
        self.head_seq = Some(welcome.head_seq);
        self.history_id.clone_from(&welcome.history_id);
        if let Some(cursor) = &self.engine.cursor
            && cursor.history_id != welcome.history_id
        {
            return Err(Stop::Recovery(format!(
                "the server's history of this space changed from {} to {} (a rewind or a restore)",
                cursor.history_id, welcome.history_id
            )));
        }
        let commits = &self.engine.commits;
        let never_synced = self.engine.cursor.is_none()
            && commits.in_flight().is_none()
            && commits.accepted() == 0;
        let adopt =
            matches!(self.stream, Stream::Connecting) && never_synced && welcome.device_seq != 0;
        if adopt {
            tracing::info!(
                space_id = self.link.space_id,
                device_seq = welcome.device_seq,
                "this device synced the space with a store that is gone; continuing from the server's count"
            );
        }
        let reconnect = self.step(|engine, _| {
            if adopt {
                let state = CommitsState {
                    accepted: welcome.device_seq,
                    ..engine.commits.state()
                };
                engine.commits = Commits::from_state(state).expect("no commit is in flight");
            }
            Ok(engine.commits.connected(welcome))
        })?;
        match reconnect {
            Reconnect::Recovery => {
                return Err(Stop::Recovery(
                    "the server's count of this device's commits does not match the folder's sync state"
                        .into(),
                ));
            }
            Reconnect::Resend => {
                if let (Flight::Unknown, Some(commit)) = (&self.flight, self.in_flight()) {
                    self.flight = Flight::Sending(commit.clone());
                    work.push(Work::Send(commit));
                }
            }
            Reconnect::Idle => {
                let waits =
                    (self.engine.commits.in_flight()).is_some_and(|flight| flight.result.is_none());
                if matches!(self.flight, Flight::Unknown) && !waits {
                    self.flight = Flight::Idle;
                }
            }
        }
        match (&mut self.stream, &self.engine.cursor) {
            (Stream::Connecting, Some(cursor)) => {
                work.push(Work::Subscribe {
                    history_id: cursor.history_id.clone(),
                    since_seq: cursor.last_seq,
                });
                self.stream = Stream::Following { head: None };
            }
            (Stream::Connecting, None) => {
                self.stream = Stream::Listing;
                work.push(Work::List);
            }
            (Stream::Following { head }, _) => *head = None,
            (Stream::Listing | Stream::Replaying(_), _) => {}
        }
        Ok(())
    }

    /// Commits of the log. A gap subscribes again from the last commit applied.
    fn changes(&mut self, records: &[CommitRecord], work: &mut Vec<Work>) -> Result<(), Stop> {
        let (applied, history_id, since_seq) = match &mut self.stream {
            Stream::Connecting | Stream::Listing => return Ok(()),
            Stream::Replaying(staging) => (
                staging.replay(records)?,
                staging.history_id().to_owned(),
                staging.last_seq(),
            ),
            Stream::Following { .. } => {
                let (applied, nodes) = self.step(|engine, tx| {
                    Ok((engine.apply_records(tx, records)?, tx.remote_node_count()?))
                })?;
                self.remote_nodes = nodes;
                let cursor = self.engine.cursor.as_ref().expect("R follows the log");
                (applied, cursor.history_id.clone(), cursor.last_seq)
            }
        };
        if applied == Applied::Gap {
            tracing::warn!(
                space_id = self.link.space_id,
                "the change stream skipped a seq; resuming"
            );
            work.push(Work::Subscribe {
                history_id,
                since_seq,
            });
        }
        self.publish_remote()
    }

    /// The result of this device's commit `commit_id`, from the call or from the change stream, whichever comes
    /// first (design §2.7 and §4.4). Its records are applied when it is the commit after the cursor, and skipped
    /// when R already holds it. The engine keeps the result once, and acknowledges it when R has reached its seq.
    /// The second delivery finds no commit waiting for a result and changes nothing.
    fn result(&mut self, commit_id: &str, result: CommitResult) -> Result<(), Stop> {
        let ours = (self.engine.commits.in_flight())
            .is_some_and(|flight| flight.commit.commit_id == commit_id && flight.result.is_none());
        let receive = |engine: &mut Engine, result: CommitResult| {
            (engine.commits.received(result)).map_err(|error| {
                Stop::Protocol(format!("the result of commit {commit_id}: {error:?}"))
            })
        };
        if let Stream::Replaying(staging) = &mut self.stream {
            if result.seq == staging.last_seq() + 1 {
                staging.replay_commit(result.seq, &result.changes)?;
            }
            if ours {
                self.step(|engine, _| receive(engine, result))?;
            }
            return self.publish_remote();
        }
        self.remote_nodes = self.step(|engine, tx| {
            let next = (engine.cursor.as_ref()).is_some_and(|c| result.seq == c.last_seq + 1);
            if next {
                engine.apply_commit(tx, result.seq, &result.changes)?;
            }
            if ours {
                receive(engine, result)?;
            }
            if let Some(cursor) = &engine.cursor {
                engine
                    .commits
                    .acknowledge(&mut engine.trees, cursor.last_seq);
            }
            Ok(tx.remote_node_count()?)
        })?;
        Ok(())
    }

    /// Replaces R with the bootstrap's tree once the log has caught it up (design §4.4). A folder that was never
    /// synced waits for its first scan, and stops when the folder and the space hold an entry of the same name key
    /// under their roots, because matching the two (design §2.8 and §6.4) is not built: syncing them as they are
    /// would upload or fork everything both hold. Two trees that share no such name sync as they are.
    fn publish_remote(&mut self) -> Result<(), Stop> {
        let Stream::Replaying(staging) = &self.stream else {
            return Ok(());
        };
        if !staging.ready() {
            return Ok(());
        }
        if self.engine.cursor.is_none() {
            if !self.scanned {
                return Ok(());
            }
            let root = &self.link.space_id;
            let shared = (self.engine.trees.local().iter())
                .filter(|(_, node)| node.shape.parent == *root && !node.parked)
                .find(|(_, node)| staging.holds(root, &node.shape.name));
            if let Some((_, node)) = shared {
                return Err(Stop::Unsupported(format!(
                    "the folder and the space it was linked to both hold {:?}",
                    node.shape.name
                )));
            }
        }
        let head = Some(staging.last_seq());
        let Stream::Replaying(staging) =
            std::mem::replace(&mut self.stream, Stream::Following { head })
        else {
            unreachable!("checked above");
        };
        self.remote_nodes = self.step(|engine, tx| {
            engine.replace_remote(tx, staging)?;
            Ok(tx.remote_node_count()?)
        })?;
        Ok(())
    }

    /// The server refused `commit`.
    fn refused(
        &mut self,
        commit: Commit,
        refusal: &ServerError,
        work: &mut Vec<Work>,
    ) -> Result<(), Stop> {
        let failure = self.step(|engine, _| {
            (engine.commits.failed(&refusal.0))
                .map_err(|error| Stop::Protocol(format!("a refused commit: {error:?}")))
        })?;
        let described = policy::describe(refusal);
        tracing::info!(
            space_id = self.link.space_id,
            refusal = described,
            ?failure,
            "a commit was refused"
        );
        match failure {
            Failure::Resend if refusal.code() == Code::MissingChunks => {
                if let Some(commit) = self.in_flight() {
                    self.start(commit, work);
                }
            }
            Failure::Resend => {
                let wait =
                    (refusal.retry_after()).unwrap_or_else(|| self.backoff.delay(self.failures));
                self.failures += 1;
                self.note = Some(format!("the server did not take a commit: {described}"));
                self.flight = Flight::Waiting(Next::Send(commit));
                work.push(Work::ResendAfter(wait));
            }
            Failure::Abandoned | Failure::Split | Failure::Halted(CommitHalt::ReadOnly) => {}
            Failure::Resync => {
                return Err(Stop::Recovery(format!(
                    "the server is behind what this device already applied ({described})"
                )));
            }
            Failure::Halted(CommitHalt::Recovery) => return Err(Stop::Recovery(described)),
            Failure::Halted(CommitHalt::Policy { .. }) => {
                return Err(Stop::Refused(format!(
                    "the server refused this device's changes: {described}"
                )));
            }
        }
        Ok(())
    }

    /// Starts the upload of what `commit` adds, or its send when it adds no content.
    fn start(&mut self, commit: Commit, work: &mut Vec<Work>) {
        let upload = self.disk.upload(&commit, &self.engine);
        if upload.chunks.is_empty() {
            self.flight = Flight::Sending(commit.clone());
            work.push(Work::Send(commit));
        } else {
            self.flight = Flight::Uploading(upload.paths());
            work.push(Work::Upload(upload));
        }
    }

    /// Decides what runs next, after every input.
    ///
    /// Nothing is planned before the first scan, while R does not follow the log, or while an effect is in flight:
    /// its staging is out, or waits to be staged again. An effect comes first, and none is planned while a commit is
    /// in flight unless committing has stopped, because the engine's property test never runs an effect between
    /// building a commit and acknowledging it. A commit is built only when no effect is planned, R is caught up and
    /// the session is connected, and only when a node is dirty or a restore is queued. Building can change the
    /// trees without giving a commit, so the effects are planned once more after it.
    fn drive(&mut self, work: &mut Vec<Work>) -> Result<(), Stop> {
        let staging = self.engine.effects.in_flight().is_some();
        if !self.scanned || staging || !matches!(self.stream, Stream::Following { .. }) {
            return Ok(());
        }
        let commits = &self.engine.commits;
        if commits.in_flight().is_some() && commits.halted().is_none() {
            return Ok(());
        }
        if self.plan_effect(work)? {
            return Ok(());
        }
        let trees = &self.engine.trees;
        let nothing_local = trees.dirty().is_empty()
            && trees.waiting().is_empty()
            && self.engine.commits.restores().is_empty();
        let may_build = matches!(self.flight, Flight::Idle)
            && self.engine.commits.in_flight().is_none()
            && self.caught_up()
            && !self.offline
            && !nothing_local;
        if !may_build {
            return Ok(());
        }
        let now_ms = (SystemTime::now().duration_since(UNIX_EPOCH))
            .map_or(0, |since| since.as_millis() as u64);
        let built = self.step(|engine, _| {
            let cursor = engine.cursor.as_ref().expect("R follows the log");
            let built = engine.commits.build(
                &mut engine.trees,
                &mut engine.ids,
                now_ms,
                &cursor.history_id,
                cursor.last_seq,
            );
            Ok(built.cloned())
        })?;
        match built {
            Some(commit) => self.start(commit, work),
            None => {
                self.plan_effect(work)?;
            }
        }
        Ok(())
    }

    /// Plans the next effect, if there is one, and asks for its staging.
    fn plan_effect(&mut self, work: &mut Vec<Work>) -> Result<bool, Stop> {
        let Some(pending) = self.disk.next(&mut self.engine, &mut self.store)? else {
            return Ok(false);
        };
        work.push(Work::Stage(pending));
        Ok(true)
    }
}

/// What the observer's thread sends to the task.
pub(crate) enum FromDisk {
    /// A stamped batch, with the number of entries the folder holds after it.
    Batch(Batch, u64),
    /// The first scan of the folder is done. Sent once, after the batch of that scan if it had one.
    Scanned,
    /// The folder cannot be observed at all.
    Failed(String),
}

pub(crate) struct FolderTask {
    pub link: Link,
    pub store: PathBuf,
    pub auth: Arc<Auth>,
    pub status: Arc<Mutex<FolderStatus>>,
    pub matcher: Arc<dyn Matcher>,
    /// The profile's private directories, which the folder must neither hold nor sit inside.
    pub private_dirs: Vec<PathBuf>,
    /// The roots of the profile's other linked folders.
    pub linked: Vec<PathBuf>,
    /// This profile's device, which must be the one that linked the folder.
    pub owner: Owner,
    /// The observer's thread, joined before the task ends so the folder's lock is released with it.
    pub observer: Mutex<Option<JoinHandle<()>>>,
    /// Marked whenever the folder's status changes (`Driver::watch`).
    pub changed: Arc<watch::Sender<()>>,
    /// Notified when `Driver::restore` has recorded a request in the folder's store.
    pub requests: Arc<Notify>,
}

/// Why the task stopped syncing before it was asked to.
type Halt = String;

/// Work that is running, or nothing.
type Slot<'a, T> = Option<Pin<Box<dyn Future<Output = T> + Send + 'a>>>;

/// Waits for the work in `slot` and empties it. Never returns for an empty slot.
async fn finished<T>(slot: &mut Slot<'_, T>) -> T {
    match slot {
        Some(work) => {
            let out = work.as_mut().await;
            *slot = None;
            out
        }
        None => std::future::pending().await,
    }
}

impl FolderTask {
    pub async fn run(self, mut stop: watch::Receiver<bool>) {
        let space_id = self.link.space_id.clone();
        let halted = tokio::select! {
            halted = self.sync() => Some(halted),
            _ = stop.wait_for(|stop| *stop) => None,
        };
        let Some(halted) = halted else {
            self.join_observer().await;
            return;
        };
        tracing::warn!(space_id, reason = %halted, "sync of a folder halted");
        self.update(|status| {
            status.state = FolderState::Halted;
            status.problem = Some(halted);
        });
        self.join_observer().await;
        let _ = stop.wait_for(|stop| *stop).await;
    }

    /// Waits for the observer's thread, which ends within one observation once `sync` has dropped its receiver.
    async fn join_observer(&self) {
        let thread = self.observer.lock().unwrap().take();
        if let Some(thread) = thread {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }

    /// Syncs the folder until something stops it. In order: the root check, the placement check, the claim, the
    /// store with the engine and the disk, the observer, the session, and then the loop that takes one input at a
    /// time. Dropping this future, which is how the task is stopped, drops every transfer with it. The store
    /// holds the effect and the commit in flight, so the next start goes on from them.
    async fn sync(&self) -> Halt {
        self.wait_for_root().await;
        if let Err(error) = check_placement(&self.link.root, &self.private_dirs, &self.linked) {
            return error.to_string();
        }
        let lock = match self.claim() {
            Ok(lock) => Arc::new(lock),
            Err(halt) => return halt,
        };
        let mut folder = match Folder::open(self.link.clone(), &lock, &self.store) {
            Ok(folder) => folder,
            Err(stop) => return stop.to_string(),
        };
        let mut from_disk = self.observe(lock.clone(), folder.sections(), folder.snapshot());
        let mut config = SessionConfig::new(
            &self.link.space_id,
            "flarebox",
            env!("CARGO_PKG_VERSION"),
            MAX_FORMAT,
        );
        config.backoff = Backoff::default();
        let (session, mut events) = match Session::start(self.auth.clone(), config) {
            Ok(started) => started,
            Err(reason) => return reason.to_string(),
        };
        let transfer = Transfer::new(self.auth.api());
        self.publish(&folder);
        let stop = {
            let (session, transfer) = (&session, &transfer);
            let mut list: Slot<'_, Result<Vec<TreePage>, Stop>> = None;
            let mut stage: Slot<'_, (Pending, Result<Staged, Stall>)> = None;
            let mut upload: Slot<'_, Result<Vec<UploadReceipt>, ChunkError>> = None;
            let mut send: Slot<'_, Result<CommitResult, CallError>> = None;
            let mut wait: Slot<'_, ()> = None;
            let mut retry: Slot<'_, ()> = None;
            let mut recheck = tokio::time::interval(CLAIM_RECHECK);
            loop {
                let step = tokio::select! {
                    event = events.next() => match event {
                        Some(event) => folder.event(event),
                        None => Err(Stop::Protocol("the session ended".into())),
                    },
                    Some(seen) = from_disk.recv() => match seen {
                        FromDisk::Batch(batch, entries) => folder.observed(batch, entries),
                        FromDisk::Scanned => folder.scanned(),
                        // The observer reaches the folder by its path, so a folder replaced before the
                        // observer started fails there before the recheck below sees it.
                        FromDisk::Failed(_) if !claim::still_locked(&lock, &self.link.root) => {
                            break REPLACED.to_owned();
                        }
                        FromDisk::Failed(why) => Err(Stop::Io(io::Error::other(why))),
                    },
                    pages = finished(&mut list) => folder.listed(pages),
                    (pending, staged) = finished(&mut stage) => folder.staged(pending, staged),
                    receipts = finished(&mut upload) => folder.uploaded(receipts),
                    result = finished(&mut send) => folder.sent(result),
                    () = finished(&mut wait) => folder.resend(),
                    () = finished(&mut retry) => folder.retry(),
                    () = self.requests.notified() => folder.requested(),
                    _ = recheck.tick() => {
                        if !claim::still_locked(&lock, &self.link.root) {
                            break REPLACED.to_owned();
                        }
                        Ok(Vec::new())
                    }
                };
                let work = match step {
                    Ok(work) => work,
                    Err(stop) => break stop.to_string(),
                };
                for work in work {
                    match work {
                        Work::Subscribe {
                            history_id,
                            since_seq,
                        } => session.subscribe(history_id, since_seq),
                        Work::List => {
                            list = Some(Box::pin(list_tree(session, &self.link.space_id)))
                        }
                        Work::Stage(pending) => {
                            stage = Some(Box::pin(stage_effect(session, transfer, pending, None)));
                        }
                        Work::StageAfter(wait, pending) => {
                            stage = Some(Box::pin(stage_effect(
                                session,
                                transfer,
                                pending,
                                Some(wait),
                            )));
                        }
                        Work::Upload(chunks) => {
                            upload = Some(Box::pin(upload_chunks(session, transfer, chunks)));
                        }
                        Work::Send(commit) => send = Some(Box::pin(session.commit(commit))),
                        Work::ResendAfter(after) => {
                            wait = Some(Box::pin(tokio::time::sleep(after)))
                        }
                        Work::RetryAfter(after) => {
                            retry = Some(Box::pin(tokio::time::sleep(after)))
                        }
                    }
                }
                self.publish(&folder);
            }
        };
        session.close().await;
        stop
    }

    /// Waits until the root passes the check of design §6.2: nothing is synced for a folder that is missing,
    /// unmounted or marked for another space.
    async fn wait_for_root(&self) {
        loop {
            match check_root(&self.link.root, &self.link.space_id, self.link.root_dev) {
                Ok(_) => {
                    self.update(|status| status.problem = None);
                    return;
                }
                Err(problem) => {
                    tracing::warn!(space_id = self.link.space_id, %problem, "the folder fails its root check");
                    self.update(|status| {
                        status.state = FolderState::Halted;
                        status.problem = Some(format!("{problem}; checking again in 30 seconds"));
                    });
                    tokio::time::sleep(ROOT_RECHECK).await;
                }
            }
        }
    }

    /// Takes the folder's lock and checks that this profile's device linked it, recording the device if no record
    /// is there. Another process that syncs the folder, or a record of another device, halts the folder.
    fn claim(&self) -> Result<Flock, Halt> {
        let root = &self.link.root;
        let lock = claim::lock(root)
            .map_err(|error| format!("the folder's lock cannot be taken: {error}"))?
            .ok_or("another Flarebox process is syncing this folder; stop it, or unlink the folder there")?;
        match claim::owner(root) {
            Ok(Some(owner)) if owner != self.owner => Err(format!(
                "the folder is linked by device {} of {}; unlink it there first, or remove its .flarebox/device.json",
                owner.device_id, owner.server
            )),
            Ok(Some(_)) => Ok(lock),
            Ok(None) => claim::set_owner(root, &self.owner)
                .map(|()| lock)
                .map_err(|error| format!("the folder's owner cannot be recorded: {error}")),
            Err(error) => Err(format!("the folder's owner record cannot be read: {error}")),
        }
    }

    /// Observes the disk on a thread of its own, starting from `previous`. Before each observation the thread
    /// resets the scopes the effect sections asked for, then takes the stamp the batch is sent with, so the stamp
    /// covers the batch's scans and content reads. The thread holds the folder's `lock` and ends with the first
    /// observation after the receiver is dropped.
    fn observe(
        &self,
        lock: Arc<Flock>,
        sections: Arc<Sections>,
        previous: Snapshot,
    ) -> mpsc::UnboundedReceiver<FromDisk> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let link = self.link.clone();
        let matcher = self.matcher.clone();
        let thread = std::thread::spawn(move || {
            let _lock = lock;
            let started =
                FsObserver::start(&link.root, &link.space_id, link.root_dev, previous, matcher);
            let mut observer = match started {
                Ok(observer) => observer,
                Err(error) => {
                    tracing::warn!(space_id = link.space_id, %error, "the folder cannot be observed");
                    let _ = sender.send(FromDisk::Failed(format!(
                        "the folder cannot be observed: {error}"
                    )));
                    return;
                }
            };
            let mut scanned = false;
            while !sender.is_closed() {
                observer.reset(sections.take_resets());
                let stamp = sections.stamp();
                match observer.observe(Duration::from_secs(1)) {
                    Ok(None) => {}
                    Ok(Some(observation)) => {
                        tracing::debug!(
                            space_id = link.space_id,
                            changes = observation.changes.len(),
                            busy = observation.busy.len(),
                            unreadable = observation.unreadable.len(),
                            root_ok = observation.root.is_ok(),
                            "observed the folder"
                        );
                        let entries = observer.snapshot().entries.len() as u64;
                        let batch = Batch { stamp, observation };
                        let _ = sender.send(FromDisk::Batch(batch, entries));
                    }
                    Err(error) => {
                        tracing::warn!(space_id = link.space_id, %error, "observing the folder failed");
                        std::thread::sleep(Duration::from_secs(5));
                    }
                }
                if !scanned && observer.scanned().is_some() {
                    scanned = true;
                    let _ = sender.send(FromDisk::Scanned);
                }
            }
        });
        *self.observer.lock().unwrap() = Some(thread);
        receiver
    }

    /// Shows what the folder knows in the status, and marks the status changed when it differs.
    fn publish(&self, folder: &Folder) {
        let view = folder.view();
        let mut status = self.status.lock().unwrap();
        let shown = View {
            state: status.state,
            history_id: status.history_id.clone(),
            last_seq: status.last_seq,
            head_seq: status.head_seq,
            remote_nodes: status.remote_nodes,
            local_entries: status.local_entries,
            problem: status.problem.clone(),
        };
        if shown == view {
            return;
        }
        status.state = view.state;
        status.history_id = view.history_id;
        status.last_seq = view.last_seq;
        status.head_seq = view.head_seq;
        status.remote_nodes = view.remote_nodes;
        status.local_entries = view.local_entries;
        status.problem = view.problem;
        drop(status);
        self.changed.send_replace(());
    }

    fn update(&self, change: impl FnOnce(&mut FolderStatus)) {
        change(&mut self.status.lock().unwrap());
        self.changed.send_replace(());
    }
}

/// Reads the whole tree with `ListTree`, page by page (spec v0 section 7.5), waiting out the refusals the server
/// asks to retry.
async fn list_tree(session: &Session, space_id: &str) -> Result<Vec<TreePage>, Stop> {
    let backoff = Backoff::default();
    let mut pages = Vec::new();
    let mut cursor = String::new();
    let mut attempt = 0;
    loop {
        match session.list_tree(cursor.clone(), TREE_PAGE).await {
            Ok(page) => {
                attempt = 0;
                cursor.clone_from(&page.next_cursor);
                pages.push(page);
                if cursor.is_empty() {
                    return Ok(pages);
                }
            }
            Err(error) => {
                let wait = match policy::after_call_error(&error) {
                    Disposition::RetryAfter(wait) => wait,
                    Disposition::Backoff => backoff.delay(attempt),
                    Disposition::Halt => {
                        let error = match &error {
                            CallError::Server(refusal) => policy::describe(refusal),
                            other => other.to_string(),
                        };
                        return Err(Stop::Refused(format!(
                            "reading the space's tree failed: {error}"
                        )));
                    }
                };
                tracing::info!(space_id, %error, ?wait, "reading the tree again later");
                attempt += 1;
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// Stages `pending`, after `wait` when there is one, and gives it back with how the staging ended.
async fn stage_effect(
    session: &Session,
    transfer: &Transfer,
    pending: Pending,
    wait: Option<Duration>,
) -> (Pending, Result<Staged, Stall>) {
    if let Some(wait) = wait {
        tokio::time::sleep(wait).await;
    }
    let staged = pending.stage(&(session, transfer)).await;
    (pending, staged)
}

/// Uploads the chunks of `upload` the server lacks. Each is read on a thread that may block.
async fn upload_chunks(
    session: &Session,
    transfer: &Transfer,
    upload: Upload,
) -> Result<Vec<UploadReceipt>, ChunkError> {
    let upload = Arc::new(upload);
    let read = |chunk: &ChunkRef| {
        let (upload, chunk) = (upload.clone(), chunk.clone());
        async move {
            tokio::task::spawn_blocking(move || upload.read(&chunk))
                .await
                .map_err(io::Error::other)?
        }
    };
    transfer.upload(session, &upload.chunks, read).await
}

#[cfg(test)]
mod tests;
