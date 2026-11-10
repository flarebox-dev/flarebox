// SPDX-License-Identifier: Apache-2.0

//! The task that syncs one linked folder: it checks the root, claims the folder, observes the disk, and keeps the
//! session with the space, applying the server's log to the remote tree in the store.

use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use flarebox_fs::{Snapshot, check_root};
use flarebox_net::{Auth, Backoff, Event, Session, SessionConfig};
use flarebox_proto::v1::{CommitRecord, NodeState, change_record, resync_required::Reason};
use flarebox_store::{Cursor, Link, Store, Tx};
use tokio::sync::{mpsc, watch};

use crate::claim::{self, Owner};
use crate::driver::check_placement;
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
}

/// Why the task stopped syncing before it was asked to.
type Halt = String;

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

    async fn sync(&self) -> Halt {
        let mut store = match Store::open(&self.store) {
            Ok(store) => store,
            Err(error) => return format!("the folder's sync state cannot be opened: {error}"),
        };
        self.wait_for_root().await;
        if let Err(error) = check_placement(&self.link.root, &self.private_dirs, &self.linked) {
            return error.to_string();
        }
        let lock = match self.claim() {
            Ok(lock) => Arc::new(lock),
            Err(halt) => return halt,
        };
        let mut observations = self.observe(lock.clone());
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
        self.update(|status| status.state = FolderState::Connecting);
        let mut subscribed = false;
        let mut history_id = String::new();
        loop {
            tokio::select! {
                Some(entries) = observations.recv() => {
                    self.update(|status| status.local_entries = Some(entries));
                }
                event = events.next() => {
                    let Some(event) = event else {
                        return "the session ended".into();
                    };
                    if let Err(halt) = self
                        .handle(event, &session, &mut store, &mut subscribed, &mut history_id)
                        .await
                    {
                        session.close().await;
                        return halt;
                    }
                }
            }
        }
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
    fn claim(&self) -> Result<File, Halt> {
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

    /// Observes the disk on a thread of its own and reports how many entries the folder holds after each batch.
    /// The thread holds the folder's `lock` and ends with the first batch after the receiver is dropped.
    fn observe(&self, lock: Arc<File>) -> mpsc::UnboundedReceiver<u64> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let link = self.link.clone();
        let matcher = self.matcher.clone();
        let thread = std::thread::spawn(move || {
            let _lock = lock;
            let mut observer = match FsObserver::start(
                &link.root,
                &link.space_id,
                link.root_dev,
                Snapshot::default(),
                matcher,
            ) {
                Ok(observer) => observer,
                Err(error) => {
                    tracing::warn!(space_id = link.space_id, %error, "the folder cannot be observed");
                    return;
                }
            };
            while !sender.is_closed() {
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
                        let _ = sender.send(observer.snapshot().entries.len() as u64);
                    }
                    Err(error) => {
                        tracing::warn!(space_id = link.space_id, %error, "observing the folder failed");
                        std::thread::sleep(Duration::from_secs(5));
                    }
                }
            }
        });
        *self.observer.lock().unwrap() = Some(thread);
        receiver
    }

    async fn handle(
        &self,
        event: Event,
        session: &Session,
        store: &mut Store,
        subscribed: &mut bool,
        history_id: &mut String,
    ) -> Result<(), Halt> {
        match event {
            Event::Connected(welcome) => {
                *history_id = welcome.history_id.clone();
                self.update(|status| {
                    status.head_seq = Some(welcome.head_seq);
                    status.problem = None;
                });
                let cursor = transaction(store, |tx| tx.cursor())?;
                match cursor {
                    Some(cursor) if cursor.history_id != welcome.history_id => {
                        return Err(format!(
                            "the server's history of this space changed from {} to {} (a rewind or a restore); \
                             this build cannot recover the folder yet",
                            cursor.history_id, welcome.history_id
                        ));
                    }
                    Some(_) if *subscribed => {}
                    Some(cursor) => session.subscribe(cursor.history_id, cursor.last_seq),
                    None => {
                        let cursor = self.bootstrap(session, store, history_id).await?;
                        session.subscribe(cursor.history_id, cursor.last_seq);
                    }
                }
                *subscribed = true;
                self.update(|status| status.state = FolderState::CatchingUp);
            }
            Event::Changes(records) => {
                let applied = transaction(store, |tx| apply_changes(tx, &records))?;
                match applied {
                    Applied::Cursor(cursor) => self.show_cursor(store, &cursor)?,
                    Applied::Gap(cursor) => {
                        tracing::warn!(
                            space_id = self.link.space_id,
                            "the change stream skipped a seq; resuming"
                        );
                        session.subscribe(cursor.history_id, cursor.last_seq);
                    }
                    Applied::UnknownChange { seq } => {
                        return Err(format!(
                            "commit {seq} holds a kind of change this build does not know; update Flarebox"
                        ));
                    }
                    Applied::NotBootstrapped => {
                        return Err("changes arrived before the space's tree was read".into());
                    }
                }
            }
            Event::Committed(committed) => {
                tracing::warn!(
                    space_id = self.link.space_id,
                    commit_id = committed.commit_id,
                    "a commit result arrived, but this build sends no commits"
                );
            }
            Event::CaughtUp { head_seq } => self.update(|status| {
                status.state = FolderState::UpToDate;
                status.head_seq = Some(head_seq);
            }),
            Event::ResyncRequired(resync) => match Reason::try_from(resync.reason) {
                Ok(Reason::LogTruncated) => {
                    tracing::info!(
                        space_id = self.link.space_id,
                        "the server's log no longer reaches the store; reading the tree again"
                    );
                    let cursor = self.bootstrap(session, store, history_id).await?;
                    session.subscribe(cursor.history_id, cursor.last_seq);
                }
                Ok(Reason::HistoryChanged) => {
                    return Err(
                        "the space was rewound or restored on the server; this build cannot recover the folder yet"
                            .into(),
                    );
                }
                Ok(Reason::AheadOfServer) => {
                    return Err(
                        "the server is behind what this device already applied; this build cannot recover the folder yet"
                            .into(),
                    );
                }
                Ok(Reason::Unspecified) | Err(_) => {
                    return Err(format!(
                        "the server asked for a resync for an unknown reason ({})",
                        resync.reason
                    ));
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
                self.update(|status| {
                    status.state = FolderState::Offline;
                    status.problem = Some(format!(
                        "disconnected ({reason:?}); retrying in {retry_in:?}"
                    ));
                });
            }
            Event::Stopped(reason) => return Err(reason.to_string()),
        }
        Ok(())
    }

    /// Reads the whole tree with `ListTree` and replaces the remote tree in one transaction, with the cursor at the
    /// first page's `head_seq` (spec v0 section 7.5). Replaying the log from there fixes up nodes that changed
    /// between pages, so the tree is consistent once the stream has caught up.
    async fn bootstrap(
        &self,
        session: &Session,
        store: &mut Store,
        history_id: &str,
    ) -> Result<Cursor, Halt> {
        self.update(|status| status.state = FolderState::Bootstrapping);
        let backoff = Backoff::default();
        let mut nodes: Vec<NodeState> = Vec::new();
        let mut page_cursor = String::new();
        let mut start_seq = None;
        let mut attempt = 0;
        loop {
            match session.list_tree(page_cursor.clone(), TREE_PAGE).await {
                Ok(page) => {
                    attempt = 0;
                    start_seq.get_or_insert(page.head_seq);
                    nodes.extend(page.nodes);
                    if page.next_cursor.is_empty() {
                        break;
                    }
                    page_cursor = page.next_cursor;
                }
                Err(error) => {
                    let wait = match policy::after_call_error(&error) {
                        Disposition::RetryAfter(wait) => wait,
                        Disposition::Backoff => backoff.delay(attempt),
                        Disposition::Halt => {
                            let error = match &error {
                                flarebox_net::CallError::Server(refusal) => {
                                    policy::describe(refusal)
                                }
                                other => other.to_string(),
                            };
                            return Err(format!("reading the space's tree failed: {error}"));
                        }
                    };
                    tracing::info!(space_id = self.link.space_id, %error, ?wait, "reading the tree again later");
                    attempt += 1;
                    tokio::time::sleep(wait).await;
                }
            }
        }
        let cursor = Cursor {
            history_id: history_id.to_owned(),
            last_seq: start_seq.unwrap_or_default(),
        };
        transaction(store, |tx| replace_tree(tx, &cursor, &nodes))?;
        tracing::info!(
            space_id = self.link.space_id,
            nodes = nodes.len(),
            seq = cursor.last_seq,
            "read the space's tree"
        );
        self.show_cursor(store, &cursor)?;
        Ok(cursor)
    }

    fn show_cursor(&self, store: &mut Store, cursor: &Cursor) -> Result<(), Halt> {
        let nodes = transaction(store, |tx| tx.remote_node_count())?;
        self.update(|status| {
            status.history_id = Some(cursor.history_id.clone());
            status.last_seq = Some(cursor.last_seq);
            status.remote_nodes = nodes;
        });
        Ok(())
    }

    fn update(&self, change: impl FnOnce(&mut FolderStatus)) {
        change(&mut self.status.lock().unwrap());
    }
}

fn transaction<T>(
    store: &mut Store,
    f: impl FnOnce(&Tx<'_>) -> flarebox_store::Result<T>,
) -> Result<T, Halt> {
    store
        .transaction(f)
        .map_err(|error| format!("the folder's sync state cannot be written: {error}"))
}

/// What applying a `Changes` batch did.
#[derive(Debug, PartialEq)]
pub(crate) enum Applied {
    /// Every record was applied or already in the store; the cursor now stands here.
    Cursor(Cursor),
    /// A record does not follow the cursor. Nothing after the gap was applied; resume from the cursor.
    Gap(Cursor),
    /// A record holds a change of a kind this build does not know. Nothing from that record on was applied, and the
    /// folder must stop applying the log (spec v0 section 9.1).
    UnknownChange { seq: u64 },
    /// The store has no cursor, so the records have nothing to apply to.
    NotBootstrapped,
}

/// Applies commit records to the remote tree in seq order, skipping records the store already holds, and moves the
/// cursor. Records are post-images, so applying one sets each node it names to its state after that commit.
pub(crate) fn apply_changes(
    tx: &Tx<'_>,
    records: &[CommitRecord],
) -> flarebox_store::Result<Applied> {
    let Some(mut cursor) = tx.cursor()? else {
        return Ok(Applied::NotBootstrapped);
    };
    let mut outcome = None;
    for record in records {
        if record.seq <= cursor.last_seq {
            continue;
        }
        if record.seq != cursor.last_seq + 1 {
            outcome = Some(Applied::Gap(cursor.clone()));
            break;
        }
        if record.changes.iter().any(|change| change.change.is_none()) {
            outcome = Some(Applied::UnknownChange { seq: record.seq });
            break;
        }
        for change in &record.changes {
            match &change.change {
                Some(change_record::Change::Node(node)) => tx.put_remote_node(node)?,
                Some(change_record::Change::Purged(purged)) => {
                    tx.remove_remote_node(&purged.node_id)?;
                }
                Some(change_record::Change::Space(_) | change_record::Change::Member(_)) | None => {
                }
            }
        }
        cursor.last_seq = record.seq;
    }
    tx.set_cursor(&cursor)?;
    Ok(outcome.unwrap_or(Applied::Cursor(cursor)))
}

/// Replaces the remote tree with a bootstrap's nodes and sets the cursor.
pub(crate) fn replace_tree(
    tx: &Tx<'_>,
    cursor: &Cursor,
    nodes: &[NodeState],
) -> flarebox_store::Result<()> {
    tx.clear_remote_nodes()?;
    for node in nodes {
        tx.put_remote_node(node)?;
    }
    tx.set_cursor(cursor)
}

#[cfg(test)]
mod tests {
    use flarebox_proto::v1::{ChangeRecord, NodeKind, NodePurged, NodeStatus, SpaceUpdated};

    use super::*;

    fn node(id: &str, name: &str) -> NodeState {
        NodeState {
            node_id: id.into(),
            parent_id: "root".into(),
            name: name.into(),
            kind: NodeKind::File as i32,
            status: NodeStatus::Live as i32,
            ..Default::default()
        }
    }

    fn record(seq: u64, changes: Vec<change_record::Change>) -> CommitRecord {
        CommitRecord {
            seq,
            changes: changes
                .into_iter()
                .map(|change| ChangeRecord {
                    change: Some(change),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn cursor(seq: u64) -> Cursor {
        Cursor {
            history_id: "h".into(),
            last_seq: seq,
        }
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store.sqlite")).unwrap();
        (dir, store)
    }

    fn names(tx: &Tx<'_>) -> Vec<String> {
        tx.remote_nodes()
            .unwrap()
            .into_iter()
            .map(|node| node.name)
            .collect()
    }

    #[test]
    fn records_are_applied_in_seq_order_and_replays_are_skipped() {
        let (_dir, mut store) = store();
        store
            .transaction(|tx| {
                replace_tree(tx, &cursor(4), &[node("a", "a"), node("b", "b")])?;
                let records = [
                    record(4, vec![change_record::Change::Node(node("a", "stale"))]),
                    record(5, vec![change_record::Change::Node(node("a", "a2"))]),
                    record(
                        6,
                        vec![
                            change_record::Change::Purged(NodePurged {
                                node_id: "b".into(),
                            }),
                            change_record::Change::Node(node("c", "c")),
                            change_record::Change::Space(SpaceUpdated {}),
                        ],
                    ),
                ];
                assert_eq!(apply_changes(tx, &records)?, Applied::Cursor(cursor(6)));
                assert_eq!(names(tx), ["a2", "c"]);
                assert_eq!(tx.cursor()?, Some(cursor(6)));
                Ok::<_, flarebox_store::Error>(())
            })
            .unwrap();
    }

    #[test]
    fn a_gap_stops_at_the_last_contiguous_record() {
        let (_dir, mut store) = store();
        store
            .transaction(|tx| {
                replace_tree(tx, &cursor(1), &[])?;
                let records = [
                    record(2, vec![change_record::Change::Node(node("a", "a"))]),
                    record(4, vec![change_record::Change::Node(node("b", "b"))]),
                ];
                assert_eq!(apply_changes(tx, &records)?, Applied::Gap(cursor(2)));
                assert_eq!(names(tx), ["a"]);
                assert_eq!(tx.cursor()?, Some(cursor(2)));
                Ok::<_, flarebox_store::Error>(())
            })
            .unwrap();
    }

    #[test]
    fn an_unknown_kind_of_change_stops_before_its_record() {
        let (_dir, mut store) = store();
        store
            .transaction(|tx| {
                replace_tree(tx, &cursor(0), &[])?;
                let mut unknown = record(2, vec![change_record::Change::Node(node("b", "b"))]);
                unknown.changes.push(ChangeRecord { change: None });
                let records = [
                    record(1, vec![change_record::Change::Node(node("a", "a"))]),
                    unknown,
                ];
                assert_eq!(
                    apply_changes(tx, &records)?,
                    Applied::UnknownChange { seq: 2 }
                );
                assert_eq!(names(tx), ["a"]);
                assert_eq!(tx.cursor()?, Some(cursor(1)));
                Ok::<_, flarebox_store::Error>(())
            })
            .unwrap();
    }

    #[test]
    fn a_bootstrap_replaces_the_whole_tree() {
        let (_dir, mut store) = store();
        store
            .transaction(|tx| {
                replace_tree(tx, &cursor(3), &[node("a", "a"), node("b", "b")])?;
                replace_tree(tx, &cursor(9), &[node("c", "c")])?;
                assert_eq!(names(tx), ["c"]);
                assert_eq!(tx.cursor()?, Some(cursor(9)));
                Ok::<_, flarebox_store::Error>(())
            })
            .unwrap();
    }
}
