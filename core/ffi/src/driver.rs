// SPDX-License-Identifier: Apache-2.0

//! The [`Core`] the app runs: the sync driver of `core/driver` (docs/design/driver.md), called
//! in-process on a Tokio runtime the session owns.
//!
//! The driver keeps the credential, the linked folders, their stores and the questions; this
//! module maps its [`Status`] and questions onto [`SessionState`] and sends what changed as
//! [`SessionEvent`]s. Calls the driver does not have (`connect`, `start_pairing`, `spaces`,
//! `create_space`) are one `core/net` call each. The pause is the driver stopped, remembered in
//! the profile so that it outlives a relaunch.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError, Weak};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use flarebox_driver::{
    self as driver, Credential, Driver, DriverError, FolderState as DriverFolderState, Paths,
    Status, api_for,
};
use flarebox_net::{Api, ApiError, Auth, PROTO_MAX, PROTO_MIN};
use flarebox_proto::v1::{CreateSpaceRequest, error::Code};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::watch;

use crate::core::{Core, Events};
use crate::types::{
    Account, Answer, DeviceList, FolderState, HeldItem, ItemDetail, LinkedFolder, MassDeleteLimit,
    NodeKind, PairingCode, Problem, ProblemKind, Question, QuestionKind, RecoveryCause,
    RecoveryGroup, RecoveryTrigger, ServerInfo, SessionError, SessionEvent, SessionState, Space,
    SyncStatus,
};

pub(crate) struct DriverCore {
    runtime: Runtime,
    paths: Paths,
    events: Events,
    inner: Mutex<Inner>,
    /// Taken by the calls that sign in, sign out, answer a question or change the linked folders,
    /// so that they run one at a time: an answer is written to a store that an unlink or a sign-out
    /// may be removing. The questions are not read while one runs, because reading them opens the
    /// stores too. The calls the app makes on its main thread never wait for it, and no
    /// call holds `inner` while it waits on the network.
    changes: Mutex<()>,
    closed: AtomicBool,
    watcher: Mutex<Option<JoinHandle<()>>>,
}

struct Inner {
    /// None once the core has closed.
    driver: Option<Driver>,
    /// The server [`Core::connect`] found, for signing in.
    server: Option<Api>,
    started: bool,
    paused: bool,
    /// The state as last sent to the listener.
    sent: SessionState,
    /// Each halted folder's reason, by space, with when it was first seen.
    halted: BTreeMap<String, (String, i64)>,
}

impl DriverCore {
    /// Opens the profile in `dir` and starts reading its state for changes. Nothing syncs until
    /// [`Core::start`].
    pub(crate) fn open(dir: &Path, events: Events) -> Result<Arc<Self>, SessionError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("flarebox-sync")
            .enable_all()
            .build()
            .map_err(|error| SessionError::Failed {
                reason: format!("the sync runtime cannot start: {error}"),
            })?;
        let paths = Paths::under(dir);
        let driver = Driver::open(paths.clone()).map_err(driver_error)?;
        let changes = driver.watch();
        let paused = paused_file(&paths).exists();
        let core = Arc::new(DriverCore {
            runtime,
            paths,
            events,
            inner: Mutex::new(Inner {
                driver: Some(driver),
                server: None,
                started: false,
                paused,
                sent: SessionState {
                    status: SyncStatus::Stopped,
                    progress: None,
                    account: None,
                    folders: Vec::new(),
                    recent_changes: Vec::new(),
                    questions: Vec::new(),
                    problems: Vec::new(),
                    notices: Vec::new(),
                },
                halted: BTreeMap::new(),
            }),
            changes: Mutex::new(()),
            closed: AtomicBool::new(false),
            watcher: Mutex::new(None),
        });
        {
            let mut inner = core.lock();
            inner.sent = core.read(&mut inner);
        }
        *core.watcher.lock().unwrap() = follow(
            Arc::downgrade(&core),
            changes,
            core.runtime.handle().clone(),
        );
        Ok(core)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Reads the state, sends every part that changed and then the status, and keeps it as sent.
    /// With `ask_again`, the questions are sent even if they did not change.
    fn refresh(&self, inner: &mut Inner, ask_again: bool) {
        let next = self.read(inner);
        let sent = &inner.sent;
        let mut events = Vec::new();
        if next.account != sent.account {
            events.push(SessionEvent::Account {
                account: next.account.clone(),
            });
        }
        if next.folders != sent.folders {
            events.push(SessionEvent::Folders {
                folders: next.folders.clone(),
            });
        }
        if ask_again || next.questions != sent.questions {
            events.push(SessionEvent::Questions {
                questions: next.questions.clone(),
            });
        }
        if next.problems != sent.problems {
            events.push(SessionEvent::Problems {
                problems: next.problems.clone(),
            });
        }
        if next.status != sent.status {
            events.push(SessionEvent::Status {
                status: next.status,
            });
        }
        inner.sent = next;
        for event in events {
            self.events.send(event);
        }
    }

    fn read(&self, inner: &mut Inner) -> SessionState {
        let Some(driver) = &inner.driver else {
            return SessionState {
                status: SyncStatus::Stopped,
                ..inner.sent.clone()
            };
        };
        let status = driver.status();
        // Reading the questions opens each folder's store, which a change may be removing.
        let unchanging = match self.changes.try_lock() {
            Ok(unchanging) => Some(unchanging),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        };
        let questions = match unchanging.map(|_unchanging| driver.questions()) {
            Some(Ok(questions)) => questions.into_iter().map(question).collect(),
            Some(Err(error)) => {
                tracing::warn!(%error, "the questions cannot be read");
                inner.sent.questions.clone()
            }
            None => inner.sent.questions.clone(),
        };
        SessionState {
            status: sync_status(&status, inner.started, inner.paused),
            progress: None,
            account: status.account.as_ref().map(account),
            folders: status.folders.iter().map(folder).collect(),
            recent_changes: Vec::new(),
            questions,
            problems: problems(&status, &mut inner.halted),
            notices: Vec::new(),
        }
    }

    fn driver<'a>(&self, inner: &'a Inner) -> Result<&'a Driver, SessionError> {
        inner.driver.as_ref().ok_or_else(|| SessionError::Failed {
            reason: "the session has closed".into(),
        })
    }

    /// Runs `change` while no other change runs, then sends the state it left. The state is read
    /// after `changes` is released, so that it includes the questions.
    fn change<T>(
        &self,
        change: impl FnOnce(&Driver) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let changed = {
            let _change = self
                .changes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let driver = self.driver(&self.lock())?.clone();
            change(&driver)
        };
        self.refresh(&mut self.lock(), false);
        changed
    }

    /// The stored credential's API and authentication, for the calls the driver does not make.
    fn signed_in(&self) -> Result<(Api, Auth), SessionError> {
        let credential = Credential::load(&self.paths.credential())
            .map_err(|error| SessionError::Failed {
                reason: error.to_string(),
            })?
            .ok_or(SessionError::NotSignedIn)?;
        let api = credential.api().map_err(api_error)?;
        let auth = Auth::new(api.clone(), credential.net());
        Ok((api, auth))
    }

    fn start_driver(&self, driver: &Driver) {
        let _entered = self.runtime.enter();
        driver.start();
    }
}

impl Drop for DriverCore {
    fn drop(&mut self) {
        self.close();
    }
}

/// Reads the state each time the driver marks `changes`, until the core closes, so changes the
/// driver makes on its own reach the listener.
fn follow(
    core: Weak<DriverCore>,
    mut changes: watch::Receiver<()>,
    runtime: Handle,
) -> Option<JoinHandle<()>> {
    let spawned = thread::Builder::new()
        .name("flarebox-state".into())
        .spawn(move || {
            while runtime.block_on(changes.changed()).is_ok() {
                let Some(core) = core.upgrade() else { return };
                if core.closed.load(Ordering::SeqCst) {
                    return;
                }
                let mut inner = core.lock();
                core.refresh(&mut inner, false);
            }
        });
    spawned
        .inspect_err(|error| tracing::error!(%error, "the state reader cannot start"))
        .ok()
}

impl Core for DriverCore {
    fn state(&self) -> SessionState {
        let mut inner = self.lock();
        self.refresh(&mut inner, false);
        inner.sent.clone()
    }

    fn start(&self) {
        let mut inner = self.lock();
        if !inner.started {
            inner.started = true;
            if !inner.paused
                && let Some(driver) = &inner.driver
            {
                self.start_driver(driver);
            }
        }
        self.refresh(&mut inner, true);
    }

    fn stop(&self) {
        let mut inner = self.lock();
        if inner.started {
            inner.started = false;
            if let Some(driver) = &inner.driver {
                self.runtime.block_on(driver.stop());
            }
        }
        self.refresh(&mut inner, false);
    }

    fn pause(&self) -> Result<(), SessionError> {
        let mut inner = self.lock();
        if !inner.started || inner.paused {
            return Ok(());
        }
        let path = paused_file(&self.paths);
        fs::write(&path, b"paused\n").map_err(not_saved(&path))?;
        inner.paused = true;
        if let Some(driver) = &inner.driver {
            self.runtime.block_on(driver.stop());
        }
        self.refresh(&mut inner, false);
        Ok(())
    }

    fn resume(&self) -> Result<(), SessionError> {
        let mut inner = self.lock();
        if !inner.paused {
            return Ok(());
        }
        let path = paused_file(&self.paths);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(not_saved(&path)(error)),
        }
        inner.paused = false;
        if inner.started
            && let Some(driver) = &inner.driver
        {
            self.start_driver(driver);
        }
        self.refresh(&mut inner, false);
        Ok(())
    }

    fn connect(&self, url: &str) -> Result<ServerInfo, SessionError> {
        let api = api_for(url.trim()).map_err(|_| SessionError::InvalidServerUrl)?;
        let info = self
            .runtime
            .block_on(api.server_info())
            .map_err(api_error)?;
        if info.proto_max < PROTO_MIN || info.proto_min > PROTO_MAX {
            return Err(SessionError::ServerIncompatible {
                server_version: info.server_version,
            });
        }
        let server = ServerInfo {
            url: base_url(&api),
            server_version: info.server_version,
            claimed: info.claimed,
        };
        self.lock().server = Some(api);
        Ok(server)
    }

    fn claim(
        &self,
        code: &str,
        user_name: &str,
        device_name: &str,
    ) -> Result<Account, SessionError> {
        self.change(|driver| {
            let api = self.lock().server.clone().ok_or(SessionError::NoServer)?;
            let claimed = self
                .runtime
                .block_on(driver.claim(api.base().as_str(), code.trim(), user_name, device_name))
                .map_err(sign_in_error)?;
            Ok(account(&claimed))
        })
    }

    fn pair(&self, code: &str, device_name: &str) -> Result<Account, SessionError> {
        self.change(|driver| {
            let api = self.lock().server.clone().ok_or(SessionError::NoServer)?;
            let paired = self
                .runtime
                .block_on(driver.pair(api.base().as_str(), code.trim(), device_name))
                .map_err(sign_in_error)?;
            Ok(account(&paired))
        })
    }

    /// Signing out stops the driver. It starts again here if the session runs, so that a sign-in
    /// that follows syncs the folders it links, but not after a failed sign-out: the folders it
    /// could not unlink stay stopped until the person signs out again or resumes.
    fn sign_out(&self) -> Result<(), SessionError> {
        self.change(|driver| {
            self.runtime
                .block_on(driver.sign_out())
                .map_err(driver_error)?;
            let inner = self.lock();
            if inner.started && !inner.paused {
                self.start_driver(driver);
            }
            Ok(())
        })
    }

    fn devices(&self) -> Result<DeviceList, SessionError> {
        self.signed_in()?;
        Ok(DeviceList::Unavailable)
    }

    fn start_pairing(&self) -> Result<PairingCode, SessionError> {
        let (api, auth) = self.signed_in()?;
        let started = self
            .runtime
            .block_on(api.pair_start(&auth))
            .map_err(api_error)?;
        Ok(PairingCode {
            code: started.code,
            expires_at_ms: started.expires_at_ms,
        })
    }

    fn spaces(&self) -> Result<Vec<Space>, SessionError> {
        let (api, auth) = self.signed_in()?;
        let list = self
            .runtime
            .block_on(api.list_spaces(&auth))
            .map_err(api_error)?;
        Ok(list
            .spaces
            .into_iter()
            .map(|space| Space {
                space_id: space.space_id,
                name: space.name,
            })
            .collect())
    }

    fn create_space(&self, name: &str) -> Result<Space, SessionError> {
        if name.trim().is_empty() {
            return Err(SessionError::EmptyName);
        }
        let (api, auth) = self.signed_in()?;
        let space = self
            .runtime
            .block_on(api.create_space(
                &auth,
                &CreateSpaceRequest {
                    name: name.trim().to_owned(),
                },
            ))
            .map_err(api_error)?;
        Ok(Space {
            space_id: space.space_id,
            name: space.name,
        })
    }

    fn link_folder(&self, space_id: &str, path: &str) -> Result<LinkedFolder, SessionError> {
        self.change(|driver| {
            let linked = self
                .runtime
                .block_on(driver.link(Path::new(path), space_id))
                .map_err(driver_error)?;
            Ok(folder(&linked))
        })
    }

    fn unlink_folder(&self, space_id: &str) -> Result<(), SessionError> {
        self.change(|driver| {
            let path = driver
                .status()
                .folders
                .into_iter()
                .find(|folder| folder.space_id == space_id)
                .map(|folder| PathBuf::from(folder.path))
                .ok_or(SessionError::FolderNotLinked)?;
            self.runtime
                .block_on(driver.unlink(&path))
                .map_err(driver_error)?;
            Ok(())
        })
    }

    fn answer(&self, question_id: &str, answer: Answer) -> Result<(), SessionError> {
        self.change(|driver| {
            driver
                .answer(question_id, driver_answer(answer))
                .map_err(driver_error)
        })
    }

    fn dismiss_notice(&self, _notice_id: &str) -> Result<(), SessionError> {
        Err(SessionError::NoticeNotFound)
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let driver = self.lock().driver.take();
        if let Some(driver) = driver {
            self.runtime.block_on(driver.stop());
        }
        let watcher = self.watcher.lock().unwrap().take();
        if let Some(watcher) = watcher
            && watcher.thread().id() != thread::current().id()
        {
            let _ = watcher.join();
        }
    }
}

fn paused_file(paths: &Paths) -> PathBuf {
    paths.state_dir.join("paused")
}

fn base_url(api: &Api) -> String {
    api.base().as_str().trim_end_matches('/').to_owned()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

fn not_saved(path: &Path) -> impl FnOnce(io::Error) -> SessionError + '_ {
    move |error| SessionError::NotSaved {
        reason: format!("{}: {error}", path.display()),
    }
}

/// The session's status: what its folders are doing, unless the session is stopped (also by a
/// sign-out that failed), paused or has nothing to sync. A halted folder outranks the others, and the session is idle only when
/// every folder is up to date.
fn sync_status(status: &Status, started: bool, paused: bool) -> SyncStatus {
    if !started {
        return SyncStatus::Stopped;
    }
    if paused {
        return SyncStatus::Paused;
    }
    if !status.running {
        return SyncStatus::Stopped;
    }
    if status.account.is_none() || status.folders.is_empty() {
        return SyncStatus::NeedsSetup;
    }
    let any = |state| status.folders.iter().any(|folder| folder.state == state);
    if any(DriverFolderState::Halted) {
        SyncStatus::Halted
    } else if any(DriverFolderState::Bootstrapping) || any(DriverFolderState::CatchingUp) {
        SyncStatus::Syncing
    } else if any(DriverFolderState::Connecting) || any(DriverFolderState::Stopped) {
        SyncStatus::Connecting
    } else if any(DriverFolderState::Offline) {
        SyncStatus::Offline
    } else {
        SyncStatus::Idle
    }
}

fn account(account: &driver::Account) -> Account {
    Account {
        server_url: account.server.trim_end_matches('/').to_owned(),
        user_id: account.user_id.clone(),
        user_name: account.user_name.clone().unwrap_or_default(),
        device_id: account.device_id.clone(),
        device_name: account.device_name.clone(),
    }
}

fn folder(folder: &driver::FolderStatus) -> LinkedFolder {
    LinkedFolder {
        space_id: folder.space_id.clone(),
        space_name: folder.space_name.clone(),
        path: folder.path.clone(),
        state: match folder.state {
            DriverFolderState::Stopped
            | DriverFolderState::Connecting
            | DriverFolderState::Offline => FolderState::Waiting,
            DriverFolderState::Bootstrapping | DriverFolderState::CatchingUp => {
                FolderState::Syncing
            }
            DriverFolderState::UpToDate => FolderState::UpToDate,
            DriverFolderState::Halted => FolderState::Halted,
        },
    }
}

/// A problem for each halted folder, oldest first, dated when its reason was first seen.
// interim until driver part 2 reports typed problems: the driver gives a halted folder's reason as
// text.
fn problems(status: &Status, halted: &mut BTreeMap<String, (String, i64)>) -> Vec<Problem> {
    let now = now_ms();
    let current: BTreeMap<String, (String, i64)> = status
        .folders
        .iter()
        .filter(|folder| folder.state == DriverFolderState::Halted)
        .map(|folder| {
            let reason = folder.problem.clone().unwrap_or_default();
            let since = match halted.get(&folder.space_id) {
                Some((seen, since)) if *seen == reason => *since,
                _ => now,
            };
            (folder.space_id.clone(), (reason, since))
        })
        .collect();
    *halted = current;
    let mut problems: Vec<Problem> = halted
        .iter()
        .map(|(space_id, (reason, since))| Problem {
            id: format!("halted-{space_id}"),
            space_id: Some(space_id.clone()),
            path: None,
            kind: ProblemKind::FolderHalted {
                reason: reason.clone(),
            },
            since_ms: *since,
        })
        .collect();
    problems.sort_by_key(|problem| problem.since_ms);
    problems
}

// interim until driver part 2 reports each item's kind and detail, the mass-delete limits crossed
// and the recovery cause: they are inferred here.
fn question(question: driver::Question) -> Question {
    let kind = match question.kind {
        driver::QuestionKind::MassDelete {
            deletions,
            affected_files,
            space_files,
        } => {
            let mut exceeded = Vec::new();
            if deletions > 1_000 {
                exceeded.push(MassDeleteLimit::Deletions { limit: 1_000 });
            }
            if space_files >= 20 && affected_files * 2 > space_files {
                exceeded.push(MassDeleteLimit::ShareOfSpace { percent: 50 });
            }
            if exceeded.is_empty() {
                exceeded.push(MassDeleteLimit::Deletions { limit: 1_000 });
            }
            QuestionKind::MassDelete {
                deletions,
                affected_files,
                space_files,
                exceeded,
            }
        }
        driver::QuestionKind::AmbiguousDisappearance => QuestionKind::AmbiguousDisappearance,
        driver::QuestionKind::Recovery { group } => QuestionKind::Recovery {
            cause: RecoveryCause {
                trigger: RecoveryTrigger::NoSyncState,
                restore_point_ms: None,
            },
            group: match group {
                driver::RecoveryGroup::CreatedAfterRestorePoint => {
                    RecoveryGroup::CreatedAfterRestorePoint
                }
                driver::RecoveryGroup::ChangedOnThisDevice => RecoveryGroup::ChangedOnThisDevice,
                driver::RecoveryGroup::CreatedOnThisDevice => RecoveryGroup::CreatedOnThisDevice,
                driver::RecoveryGroup::DeletedOnThisDevice => RecoveryGroup::DeletedOnThisDevice,
                driver::RecoveryGroup::RequestedBeforeReset => RecoveryGroup::RequestedBeforeReset,
            },
        },
    };
    Question {
        id: question.id,
        space_id: question.space_id,
        asked_at_ms: question.asked_at_ms,
        answers: kind.answers(),
        kind,
        items: question
            .items
            .into_iter()
            .map(|item| HeldItem {
                node_id: item.node_id,
                path: item.path,
                kind: if item.files == 1 {
                    NodeKind::File
                } else {
                    NodeKind::Folder
                },
                files: item.files,
                detail: ItemDetail::Plain,
            })
            .collect(),
    }
}

fn driver_answer(answer: Answer) -> driver::Answer {
    match answer {
        Answer::Delete => driver::Answer::Delete,
        Answer::Restore => driver::Answer::Restore,
        Answer::Keep => driver::Answer::Keep,
        Answer::Discard => driver::Answer::Discard,
        Answer::Publish => driver::Answer::Publish,
        Answer::Repeat => driver::Answer::Repeat,
        Answer::Cancel => driver::Answer::Cancel,
    }
}

fn driver_error(error: DriverError) -> SessionError {
    match error {
        DriverError::Busy(_) => SessionError::ProfileInUse,
        DriverError::NotPaired => SessionError::NotSignedIn,
        DriverError::EmptyName | DriverError::EmptyUserName => SessionError::EmptyName,
        DriverError::Api(error) => api_error(error),
        DriverError::SpaceNotFound(_) => SessionError::SpaceNotFound,
        DriverError::FolderNotFound(_) => SessionError::FolderNotFound,
        DriverError::NotAFolder(_) => SessionError::NotAFolder,
        DriverError::FolderHoldsProfile(other) | DriverError::FolderOverlaps(other) => {
            SessionError::FolderOverlaps {
                other: other.display().to_string(),
            }
        }
        DriverError::FolderInUse(_) => SessionError::FolderInUse,
        DriverError::FolderOfOtherDevice { device_id, .. } => {
            SessionError::FolderOfOtherDevice { device_id }
        }
        DriverError::SpaceAlreadyLinked(_) => SessionError::SpaceAlreadyLinked,
        DriverError::MarkerOfOtherSpace(_) => SessionError::MarkerOfOtherSpace,
        DriverError::UnsupportedFileSystem => SessionError::UnsupportedFileSystem,
        DriverError::NotLinked(_) => SessionError::FolderNotLinked,
        DriverError::QuestionNotFound(_) => SessionError::QuestionNotFound,
        DriverError::AnswerNotAllowed { .. } => SessionError::AnswerNotAllowed,
        error @ (DriverError::Credential(_) | DriverError::Store { .. }) => {
            SessionError::NotSaved {
                reason: error.to_string(),
            }
        }
        other => SessionError::Failed {
            reason: other.to_string(),
        },
    }
}

fn api_error(error: ApiError) -> SessionError {
    match error {
        ApiError::Url(_) => SessionError::InvalidServerUrl,
        ApiError::Transport(source) => SessionError::ServerUnreachable {
            reason: error_chain(&source),
        },
        error if error.is_unauthenticated() => SessionError::NotSignedIn,
        other => SessionError::Failed {
            reason: other.to_string(),
        },
    }
}

/// The error of a call that redeems a claim or pairing code.
/// A refused claim or pairing is the code's fault unless the server failed.
fn sign_in_error(error: DriverError) -> SessionError {
    match error {
        DriverError::Api(error) => code_error(error),
        other => driver_error(other),
    }
}

fn code_error(error: ApiError) -> SessionError {
    match &error {
        ApiError::Server { error: refusal, .. } => match refusal.code() {
            Code::RetryLater | Code::LimitExceeded => SessionError::TooManyAttempts,
            Code::Internal => api_error(error),
            _ => SessionError::InvalidCode,
        },
        _ => api_error(error),
    }
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use flarebox_net::ServerError;
    use flarebox_proto::v1::Error as ProtoError;

    use super::*;
    use crate::SyncSession;
    use crate::core::SessionListener;

    fn folder_in(state: DriverFolderState, problem: Option<&str>) -> driver::FolderStatus {
        driver::FolderStatus {
            space_id: "0190a5b0-0000-7000-8000-000000000001".into(),
            space_name: "Documents".into(),
            path: "/Users/ada/Documents".into(),
            state,
            history_id: None,
            last_seq: None,
            head_seq: None,
            remote_nodes: 0,
            local_entries: None,
            questions: 0,
            problem: problem.map(str::to_owned),
        }
    }

    fn status_of(states: &[DriverFolderState]) -> Status {
        Status {
            account: Some(driver::Account {
                server: "https://files.example.com/".into(),
                user_id: "u1".into(),
                user_name: None,
                device_id: "d1".into(),
                device_name: "Mac".into(),
            }),
            running: true,
            folders: states.iter().map(|state| folder_in(*state, None)).collect(),
        }
    }

    #[test]
    fn the_session_status_follows_its_folders() {
        use DriverFolderState::*;
        let up = status_of(&[UpToDate]);
        assert_eq!(sync_status(&up, false, false), SyncStatus::Stopped);
        assert_eq!(sync_status(&up, true, true), SyncStatus::Paused);
        assert_eq!(
            sync_status(&status_of(&[]), true, false),
            SyncStatus::NeedsSetup
        );
        let signed_out = Status {
            account: None,
            ..status_of(&[Halted])
        };
        assert_eq!(
            sync_status(&signed_out, true, false),
            SyncStatus::NeedsSetup
        );
        for (states, expected) in [
            (vec![UpToDate], SyncStatus::Idle),
            (vec![UpToDate, UpToDate], SyncStatus::Idle),
            (vec![Halted], SyncStatus::Halted),
            (vec![Halted, Halted], SyncStatus::Halted),
            (vec![UpToDate, Halted], SyncStatus::Halted),
            (vec![CatchingUp, Halted], SyncStatus::Halted),
            (vec![Offline, Halted], SyncStatus::Halted),
            (vec![UpToDate, CatchingUp], SyncStatus::Syncing),
            (vec![Bootstrapping, Connecting], SyncStatus::Syncing),
            (vec![UpToDate, Connecting], SyncStatus::Connecting),
            (vec![Offline, UpToDate], SyncStatus::Offline),
        ] {
            assert_eq!(
                sync_status(&status_of(&states), true, false),
                expected,
                "{states:?}"
            );
        }
        assert_eq!(
            folder(&folder_in(Offline, None)).state,
            FolderState::Waiting
        );
        assert_eq!(
            folder(&folder_in(CatchingUp, None)).state,
            FolderState::Syncing
        );
        assert_eq!(
            account(up.account.as_ref().unwrap()).server_url,
            "https://files.example.com"
        );
    }

    #[test]
    fn a_halted_folder_is_a_problem_dated_when_its_reason_first_appeared() {
        let mut halted = BTreeMap::new();
        let mut status = status_of(&[]);
        status.folders = vec![folder_in(
            DriverFolderState::Halted,
            Some("the marker is missing"),
        )];
        let first = problems(&status, &mut halted);
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0].kind,
            ProblemKind::FolderHalted {
                reason: "the marker is missing".into()
            }
        );
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(problems(&status, &mut halted), first);

        status.folders[0].problem = Some("the folder is missing".into());
        let changed = problems(&status, &mut halted);
        assert!(changed[0].since_ms > first[0].since_ms);

        status.folders[0].state = DriverFolderState::UpToDate;
        assert!(problems(&status, &mut halted).is_empty());
        assert!(halted.is_empty());
    }

    #[test]
    fn driver_questions_keep_their_answers_and_items() {
        let mapped = question(driver::Question {
            id: "q1".into(),
            space_id: "s1".into(),
            asked_at_ms: 7,
            kind: driver::QuestionKind::MassDelete {
                deletions: 1_200,
                affected_files: 1_500,
                space_files: 2_000,
            },
            items: vec![
                driver::HeldItem {
                    node_id: "n1".into(),
                    path: "Archive".into(),
                    files: 1_499,
                },
                driver::HeldItem {
                    node_id: "n2".into(),
                    path: "notes.txt".into(),
                    files: 1,
                },
            ],
        });
        assert_eq!(
            mapped.kind,
            QuestionKind::MassDelete {
                deletions: 1_200,
                affected_files: 1_500,
                space_files: 2_000,
                exceeded: vec![
                    MassDeleteLimit::Deletions { limit: 1_000 },
                    MassDeleteLimit::ShareOfSpace { percent: 50 }
                ],
            }
        );
        assert_eq!(mapped.answers, vec![Answer::Delete, Answer::Restore]);
        assert_eq!(mapped.items[0].kind, NodeKind::Folder);
        assert_eq!(mapped.items[1].kind, NodeKind::File);

        let recovery = question(driver::Question {
            id: "q2".into(),
            space_id: "s1".into(),
            asked_at_ms: 8,
            kind: driver::QuestionKind::Recovery {
                group: driver::RecoveryGroup::RequestedBeforeReset,
            },
            items: Vec::new(),
        });
        assert_eq!(recovery.answers, vec![Answer::Repeat, Answer::Cancel]);
        for answer in [
            Answer::Delete,
            Answer::Restore,
            Answer::Keep,
            Answer::Discard,
            Answer::Publish,
            Answer::Repeat,
            Answer::Cancel,
        ] {
            assert_eq!(
                format!("{answer:?}").to_lowercase(),
                driver_answer(answer).as_str()
            );
        }
    }

    #[test]
    fn refusals_become_session_errors() {
        let refused = |code: Code| ApiError::Server {
            status: 400,
            error: ServerError(ProtoError {
                code: code as i32,
                message: "refused".into(),
                ..Default::default()
            }),
        };
        assert_eq!(
            code_error(refused(Code::RetryLater)),
            SessionError::TooManyAttempts
        );
        assert_eq!(
            code_error(refused(Code::Unauthenticated)),
            SessionError::InvalidCode
        );
        assert!(matches!(
            code_error(refused(Code::Internal)),
            SessionError::Failed { .. }
        ));
        assert_eq!(
            api_error(ApiError::Url("x".into())),
            SessionError::InvalidServerUrl
        );
        assert_eq!(
            driver_error(DriverError::FolderHoldsProfile("/p/state".into())),
            SessionError::FolderOverlaps {
                other: "/p/state".into()
            }
        );
        assert_eq!(
            driver_error(DriverError::Busy("/p/state/lock".into())),
            SessionError::ProfileInUse
        );
    }

    struct Recorder(Arc<StdMutex<Vec<SessionEvent>>>);

    impl SessionListener for Recorder {
        fn on_event(&self, event: SessionEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[test]
    fn the_pause_outlives_a_relaunch_and_one_session_holds_a_profile() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().to_str().unwrap().to_owned();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let open = |seen: &Arc<StdMutex<Vec<SessionEvent>>>| {
            SyncSession::open(Box::new(Recorder(seen.clone())), profile.clone())
        };

        let session = open(&seen).unwrap();
        assert!(matches!(open(&seen), Err(SessionError::ProfileInUse)));
        assert_eq!(session.state().status, SyncStatus::Stopped);
        session.pause().unwrap();
        assert_eq!(session.state().status, SyncStatus::Stopped);
        session.start();
        assert_eq!(session.state().status, SyncStatus::NeedsSetup);
        assert_eq!(session.spaces(), Err(SessionError::NotSignedIn));
        assert_eq!(session.start_pairing(), Err(SessionError::NotSignedIn));
        assert_eq!(
            session.pair("ABCD-EFGH".into(), "Mac".into()),
            Err(SessionError::NoServer)
        );
        assert_eq!(
            session.connect("files.example.com".into()),
            Err(SessionError::InvalidServerUrl)
        );
        session.pause().unwrap();
        assert_eq!(session.state().status, SyncStatus::Paused);
        drop(session);

        seen.lock().unwrap().clear();
        let session = open(&seen).unwrap();
        session.start();
        assert_eq!(session.state().status, SyncStatus::Paused);
        session.resume().unwrap();
        assert_eq!(session.state().status, SyncStatus::NeedsSetup);
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [
                SessionEvent::Questions { questions: vec![] },
                SessionEvent::Status {
                    status: SyncStatus::Paused
                },
                SessionEvent::Status {
                    status: SyncStatus::NeedsSetup
                },
            ]
        );
        drop(session);
        let session = open(&seen).unwrap();
        session.start();
        assert_eq!(session.state().status, SyncStatus::NeedsSetup);
    }
}
