// SPDX-License-Identifier: Apache-2.0

//! A [`Core`] that syncs nothing and plays back a scripted state, so the app's windows can be
//! built and tested before the driver exists. It enforces the same rules the real core will:
//! codes have the server's lengths, folders must exist and must not overlap, a question only
//! takes one of its own answers, and a change that cannot be saved is not made.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::core::{Core, Events};
use crate::types::{
    Account, Answer, ChangeKind, Device, DeviceList, Direction, FolderPath, FolderState, HeldItem,
    ItemDetail, LinkedFolder, MassDeleteLimit, NodeKind, Notice, NoticeKind, PairingCode, Problem,
    ProblemKind, Question, QuestionKind, RECENT_CHANGES, RecentChange, RecoveryCause,
    RecoveryGroup, RecoveryTrigger, ServerInfo, SessionError, SessionEvent, SessionState, Space,
    SyncProgress, SyncStatus,
};

const STATE_FILE: &str = "fake-core.json";
const CLAIM_CODE_LEN: usize = 12;
const PAIRING_CODE_LEN: usize = 8;
const PAIRING_CODE_MS: i64 = 10 * 60_000;
const NOT_WRITABLE_ID: &str = "problem-state-not-writable";

/// What a running, set-up fake session is doing, which decides its [`SyncStatus`] unless it is
/// paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FakeActivity {
    Connecting,
    Syncing,
    Idle,
    Offline,
}

/// A whole state the fake can switch to at once. Every scenario but [`FakeScenario::FirstRun`]
/// is signed in to `https://box.example.com` with two linked folders, Documents and Photos. The
/// device list is unavailable in every scenario, as it is with every server today.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FakeScenario {
    /// A fresh install: no server, no account, no folders, and the server is not claimed yet.
    FirstRun,
    /// Up to date, with a few recent changes.
    UpToDate,
    /// Syncing, with progress and recent changes.
    Syncing,
    /// Paused by the person.
    Paused,
    /// The server cannot be reached.
    Offline,
    /// One mass-delete question in Documents: 14 held deletions of 4,553 files in a space of
    /// 6,000, over half of the space.
    MassDelete,
    /// One ambiguous-disappearance question in Documents with 4 files.
    AmbiguousDisappearance,
    /// Photos is recovering after a rewind: a recovery notice and one question for each of the
    /// five recovery groups.
    Recovery,
    /// Photos recovered after a rewind with nothing held: a recovery notice and no questions.
    RecoveredWithoutQuestions,
    /// Another device deleted 1,200 files from Photos, more than the guard's limit; a notice
    /// reports it and nothing is asked.
    RemoteMassDelete,
    /// Long lists: a mass delete of 1,500 files, the five recovery groups with 300 items each,
    /// and 25 ambiguous-disappearance questions: 31 questions in all.
    ManyQuestions,
    /// One problem of every kind. Photos is halted because its folder is missing.
    Problems,
}

/// An answer the fake received, for tests.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct GivenAnswer {
    pub question_id: String,
    pub answer: Answer,
}

/// The part of the fake's state the engine would keep in its store.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Saved {
    server: Option<ServerInfo>,
    claimed: bool,
    account: Option<Account>,
    /// None while the server cannot list devices.
    devices: Option<Vec<Device>>,
    spaces: Vec<Space>,
    folders: Vec<LinkedFolder>,
    questions: Vec<Question>,
    problems: Vec<Problem>,
    notices: Vec<Notice>,
    paused: bool,
    next_id: u64,
}

struct State {
    saved: Saved,
    running: bool,
    activity: FakeActivity,
    progress: Option<SyncProgress>,
    recent: Vec<RecentChange>,
    answers: Vec<GivenAnswer>,
    fail_next: Option<SessionError>,
    fail_saves: bool,
    /// [`ProblemKind::StateNotWritable`] after a failed save, until a save succeeds. It is
    /// not saved itself.
    not_writable: Option<Problem>,
    status: SyncStatus,
}

impl Saved {
    fn next_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}-{}", self.next_id)
    }

    fn account(&self) -> Result<&Account, SessionError> {
        self.account.as_ref().ok_or(SessionError::NotSignedIn)
    }
}

impl State {
    fn derived_status(&self) -> SyncStatus {
        if !self.running {
            SyncStatus::Stopped
        } else if self.saved.account.is_none() || self.saved.folders.is_empty() {
            SyncStatus::NeedsSetup
        } else if self.saved.paused {
            SyncStatus::Paused
        } else {
            match self.activity {
                FakeActivity::Connecting => SyncStatus::Connecting,
                FakeActivity::Syncing => SyncStatus::Syncing,
                FakeActivity::Idle => SyncStatus::Idle,
                FakeActivity::Offline => SyncStatus::Offline,
            }
        }
    }

    fn problems(&self) -> Vec<Problem> {
        let mut problems = self.saved.problems.clone();
        problems.extend(self.not_writable.clone());
        problems
    }

    /// Records the outcome of a save. Returns whether the problem list changed.
    fn set_save_failure(&mut self, reason: Option<String>) -> bool {
        let next = reason.map(|reason| Problem {
            id: NOT_WRITABLE_ID.to_owned(),
            space_id: None,
            path: None,
            kind: ProblemKind::StateNotWritable { reason },
            since_ms: self
                .not_writable
                .as_ref()
                .map_or_else(now_ms, |p| p.since_ms),
        });
        let changed = next != self.not_writable;
        self.not_writable = next;
        changed
    }

    fn take_failure(&mut self) -> Result<(), SessionError> {
        self.fail_next.take().map_or(Ok(()), Err)
    }
}

/// The parts of [`SessionState`] a change touched, which the fake sends as events.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Progress,
    Account,
    Folders,
    Recent,
    Questions,
    Problems,
    Notices,
}

const ALL_PARTS: [Part; 7] = [
    Part::Progress,
    Part::Account,
    Part::Folders,
    Part::Recent,
    Part::Questions,
    Part::Problems,
    Part::Notices,
];

/// The fake core. Swift scripts it through [`SyncSession::fake_core`](crate::SyncSession::fake_core).
#[derive(uniffi::Object)]
pub struct FakeCore {
    state: Mutex<State>,
    events: Events,
    state_file: Option<PathBuf>,
}

impl FakeCore {
    /// Creates a stopped fake. With `state_dir`, it loads its saved state from that directory
    /// and saves every change there; a missing or unreadable state file starts a first run.
    /// Without it, saves always succeed in memory unless [`FakeCore::fail_saves`] says otherwise.
    pub fn new(events: Events, state_dir: Option<PathBuf>) -> Self {
        let state_file = state_dir.map(|dir| dir.join(STATE_FILE));
        let saved = state_file
            .as_deref()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            state: Mutex::new(State {
                saved,
                running: false,
                activity: FakeActivity::Idle,
                progress: None,
                recent: Vec::new(),
                answers: Vec::new(),
                fail_next: None,
                fail_saves: false,
                not_writable: None,
                status: SyncStatus::Stopped,
            }),
            events,
            state_file,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Saves `next`, then makes it the state and sends `parts`. If saving fails, the state
    /// stays as it was, so memory always matches what was saved; [`ProblemKind::StateNotWritable`]
    /// is listed until a save succeeds, and the call returns [`SessionError::NotSaved`].
    fn commit(&self, state: &mut State, next: Saved, parts: &[Part]) -> Result<(), SessionError> {
        let saved = if state.fail_saves {
            Err("saving is scripted to fail".to_owned())
        } else {
            match &self.state_file {
                Some(path) => save(path, &next).map_err(|e| e.to_string()),
                None => Ok(()),
            }
        };
        match saved {
            Ok(()) => {
                state.saved = next;
                let problems = state.set_save_failure(None);
                self.publish(state, parts, problems);
                Ok(())
            }
            Err(reason) => {
                let problems = state.set_save_failure(Some(reason.clone()));
                self.publish(state, &[], problems);
                Err(SessionError::NotSaved { reason })
            }
        }
    }

    /// Builds the next saved state with `change` from a copy, then [commits](Self::commit) it.
    fn update<T>(
        &self,
        state: &mut State,
        parts: &[Part],
        change: impl FnOnce(&mut Saved) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let mut next = state.saved.clone();
        let value = change(&mut next)?;
        self.commit(state, next, parts)?;
        Ok(value)
    }

    /// Sends the touched parts, and the problems if they changed, then the status if it changed.
    /// Runs under the lock, which keeps events in order.
    fn publish(&self, state: &mut State, parts: &[Part], problems: bool) {
        let extra = (problems && !parts.contains(&Part::Problems)).then_some(Part::Problems);
        for part in parts.iter().copied().chain(extra) {
            self.events.send(match part {
                Part::Progress => SessionEvent::Progress {
                    progress: state.progress.clone(),
                },
                Part::Account => SessionEvent::Account {
                    account: state.saved.account.clone(),
                },
                Part::Folders => SessionEvent::Folders {
                    folders: state.saved.folders.clone(),
                },
                Part::Recent => SessionEvent::RecentChanges {
                    changes: state.recent.clone(),
                },
                Part::Questions => SessionEvent::Questions {
                    questions: state.saved.questions.clone(),
                },
                Part::Problems => SessionEvent::Problems {
                    problems: state.problems(),
                },
                Part::Notices => SessionEvent::Notices {
                    notices: state.saved.notices.clone(),
                },
            });
        }
        let status = state.derived_status();
        if status != state.status {
            state.status = status;
            self.events.send(SessionEvent::Status { status });
        }
    }
}

fn save(path: &Path, saved: &Saved) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(saved)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension("json.partial");
    fs::write(&partial, bytes)?;
    fs::rename(&partial, path)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn required(name: &str) -> Result<String, SessionError> {
    let name = name.trim();
    if name.is_empty() {
        Err(SessionError::EmptyName)
    } else {
        Ok(name.to_owned())
    }
}

/// Scripting. A method that changes what the engine would keep in its store stands for an
/// engine output, so it saves first like every other change: if saving fails, it changes
/// nothing and returns [`SessionError::NotSaved`].
#[uniffi::export]
impl FakeCore {
    /// Replaces the whole state with a scenario and sends every part. A running session keeps
    /// running.
    pub fn load_scenario(&self, scenario: FakeScenario) -> Result<(), SessionError> {
        let mut state = self.lock();
        let script = scenario::build(scenario, now_ms());
        self.commit(&mut state, script.saved, &[])?;
        state.activity = script.activity;
        state.progress = script.progress;
        state.recent = script.recent;
        state.answers.clear();
        state.fail_next = None;
        self.publish(&mut state, &ALL_PARTS, false);
        Ok(())
    }

    pub fn set_activity(&self, activity: FakeActivity) {
        let mut state = self.lock();
        state.activity = activity;
        self.publish(&mut state, &[], false);
    }

    pub fn set_progress(&self, progress: Option<SyncProgress>) {
        let mut state = self.lock();
        state.progress = progress;
        self.publish(&mut state, &[Part::Progress], false);
    }

    /// Adds a change at the top of the recent changes, keeping the newest
    /// [`RECENT_CHANGES`](crate::RECENT_CHANGES).
    pub fn add_recent_change(&self, change: RecentChange) {
        let mut state = self.lock();
        state.recent.insert(0, change);
        state.recent.truncate(RECENT_CHANGES);
        self.publish(&mut state, &[Part::Recent], false);
    }

    /// Sets whether the server reports itself claimed, which decides between claiming and
    /// pairing in the first-run setup.
    pub fn set_server_claimed(&self, claimed: bool) -> Result<(), SessionError> {
        self.update(&mut self.lock(), &[], |saved| {
            saved.claimed = claimed;
            if let Some(server) = &mut saved.server {
                server.claimed = claimed;
            }
            Ok(())
        })
    }

    /// Sets the device list the server reports, or none for a server that cannot list devices.
    pub fn set_devices(&self, devices: Option<Vec<Device>>) -> Result<(), SessionError> {
        self.update(&mut self.lock(), &[], |saved| {
            saved.devices = devices;
            Ok(())
        })
    }

    pub fn set_folder_state(
        &self,
        space_id: String,
        folder_state: FolderState,
    ) -> Result<(), SessionError> {
        self.update(&mut self.lock(), &[Part::Folders], |saved| {
            for folder in &mut saved.folders {
                if folder.space_id == space_id {
                    folder.state = folder_state;
                }
            }
            Ok(())
        })
    }

    /// Asks a question and returns its ID. The answers follow from the kind.
    pub fn ask(
        &self,
        space_id: String,
        kind: QuestionKind,
        items: Vec<HeldItem>,
    ) -> Result<String, SessionError> {
        self.update(&mut self.lock(), &[Part::Questions], |saved| {
            let id = saved.next_id("question");
            let answers = kind.answers();
            saved.questions.push(Question {
                id: id.clone(),
                space_id,
                asked_at_ms: now_ms(),
                kind,
                items,
                answers,
            });
            Ok(id)
        })
    }

    /// Withdraws a question without an answer, as the engine does when its held set changes or
    /// a certain case now applies (design §6.1, §6.4).
    pub fn withdraw_question(&self, question_id: String) -> Result<(), SessionError> {
        self.update(&mut self.lock(), &[Part::Questions], |saved| {
            saved.questions.retain(|q| q.id != question_id);
            Ok(())
        })
    }

    /// Adds a notice and returns its ID.
    pub fn notify(&self, space_id: String, kind: NoticeKind) -> Result<String, SessionError> {
        self.update(&mut self.lock(), &[Part::Notices], |saved| {
            let id = saved.next_id("notice");
            saved.notices.push(Notice {
                id: id.clone(),
                space_id,
                at_ms: now_ms(),
                kind,
            });
            Ok(id)
        })
    }

    /// Reports a problem and returns its ID.
    pub fn report_problem(
        &self,
        space_id: Option<String>,
        path: Option<String>,
        kind: ProblemKind,
    ) -> Result<String, SessionError> {
        self.update(&mut self.lock(), &[Part::Problems], |saved| {
            let id = saved.next_id("problem");
            saved.problems.push(Problem {
                id: id.clone(),
                space_id,
                path,
                kind,
                since_ms: now_ms(),
            });
            Ok(id)
        })
    }

    pub fn resolve_problem(&self, problem_id: String) -> Result<(), SessionError> {
        self.update(&mut self.lock(), &[Part::Problems], |saved| {
            saved.problems.retain(|p| p.id != problem_id);
            Ok(())
        })
    }

    /// Makes the next call that can fail return `error` instead.
    pub fn fail_next(&self, error: SessionError) {
        self.lock().fail_next = Some(error);
    }

    /// Makes every save fail, as an unwritable store would, until called with false.
    pub fn fail_saves(&self, fail: bool) {
        self.lock().fail_saves = fail;
    }

    /// The answers given so far, in order. An answer that could not be saved is not among them.
    pub fn given_answers(&self) -> Vec<GivenAnswer> {
        self.lock().answers.clone()
    }
}

impl Core for FakeCore {
    fn state(&self) -> SessionState {
        let state = self.lock();
        SessionState {
            status: state.status,
            progress: state.progress.clone(),
            account: state.saved.account.clone(),
            folders: state.saved.folders.clone(),
            recent_changes: state.recent.clone(),
            questions: state.saved.questions.clone(),
            problems: state.problems(),
            notices: state.saved.notices.clone(),
        }
    }

    fn start(&self) {
        let mut state = self.lock();
        if state.running {
            return;
        }
        state.running = true;
        self.publish(&mut state, &[Part::Questions, Part::Notices], false);
    }

    fn stop(&self) {
        let mut state = self.lock();
        state.running = false;
        self.publish(&mut state, &[], false);
    }

    fn pause(&self) -> Result<(), SessionError> {
        let mut state = self.lock();
        if !state.running || state.saved.paused {
            return Ok(());
        }
        self.update(&mut state, &[], |saved| {
            saved.paused = true;
            Ok(())
        })
    }

    fn resume(&self) -> Result<(), SessionError> {
        let mut state = self.lock();
        if !state.saved.paused {
            return Ok(());
        }
        self.update(&mut state, &[], |saved| {
            saved.paused = false;
            Ok(())
        })
    }

    fn connect(&self, url: &str) -> Result<ServerInfo, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        let url = url.trim().trim_end_matches('/');
        let host = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .ok_or(SessionError::InvalidServerUrl)?;
        if host.is_empty() || host.contains(char::is_whitespace) {
            return Err(SessionError::InvalidServerUrl);
        }
        self.update(&mut state, &[], |saved| {
            let server = ServerInfo {
                url: url.to_owned(),
                server_version: env!("CARGO_PKG_VERSION").to_owned(),
                claimed: saved.claimed,
            };
            saved.server = Some(server.clone());
            Ok(server)
        })
    }

    fn claim(
        &self,
        code: &str,
        user_name: &str,
        device_name: &str,
    ) -> Result<Account, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &[Part::Account], |saved| {
            let server = saved.server.clone().ok_or(SessionError::NoServer)?;
            if saved.claimed {
                return Err(SessionError::AlreadyClaimed);
            }
            if normalize_code(code).len() != CLAIM_CODE_LEN {
                return Err(SessionError::InvalidCode);
            }
            let (user_name, device_name) = (required(user_name)?, required(device_name)?);
            let account = Account {
                server_url: server.url,
                user_id: saved.next_id("user"),
                user_name,
                device_id: saved.next_id("device"),
                device_name,
            };
            saved.claimed = true;
            saved.account = Some(account.clone());
            Ok(account)
        })
    }

    fn pair(&self, code: &str, device_name: &str) -> Result<Account, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &[Part::Account], |saved| {
            let server = saved.server.clone().ok_or(SessionError::NoServer)?;
            if !saved.claimed {
                return Err(SessionError::NotClaimed);
            }
            if normalize_code(code).len() != PAIRING_CODE_LEN {
                return Err(SessionError::InvalidCode);
            }
            let account = Account {
                server_url: server.url,
                user_id: saved.next_id("user"),
                user_name: String::new(),
                device_id: saved.next_id("device"),
                device_name: required(device_name)?,
            };
            if saved.spaces.is_empty() {
                saved.spaces = vec![Space {
                    space_id: saved.next_id("space"),
                    name: "Shared".to_owned(),
                }];
            }
            saved.account = Some(account.clone());
            Ok(account)
        })
    }

    fn sign_out(&self) -> Result<(), SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &ALL_PARTS, |saved| {
            saved.account()?;
            *saved = Saved {
                server: saved.server.take(),
                claimed: saved.claimed,
                devices: saved.devices.take(),
                next_id: saved.next_id,
                ..Saved::default()
            };
            Ok(())
        })?;
        state.progress = None;
        state.recent.clear();
        self.publish(&mut state, &[Part::Progress, Part::Recent], false);
        Ok(())
    }

    fn devices(&self) -> Result<DeviceList, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        state.saved.account()?;
        Ok(match &state.saved.devices {
            None => DeviceList::Unavailable,
            Some(devices) => DeviceList::Available {
                devices: devices.clone(),
            },
        })
    }

    fn start_pairing(&self) -> Result<PairingCode, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &[], |saved| {
            saved.account()?;
            let n = saved.next_id;
            saved.next_id += 1;
            let code = (0..PAIRING_CODE_LEN)
                .map(|i| {
                    let alphabet = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
                    alphabet[(n as usize * 7 + i * 13) % alphabet.len()] as char
                })
                .collect();
            Ok(PairingCode {
                code,
                expires_at_ms: now_ms() + PAIRING_CODE_MS,
            })
        })
    }

    fn spaces(&self) -> Result<Vec<Space>, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        state.saved.account()?;
        Ok(state.saved.spaces.clone())
    }

    fn create_space(&self, name: &str) -> Result<Space, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &[], |saved| {
            saved.account()?;
            let space = Space {
                name: required(name)?,
                space_id: saved.next_id("space"),
            };
            saved.spaces.push(space.clone());
            Ok(space)
        })
    }

    fn link_folder(&self, space_id: &str, path: &str) -> Result<LinkedFolder, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &[Part::Folders], |saved| {
            saved.account()?;
            let space = saved
                .spaces
                .iter()
                .find(|s| s.space_id == space_id)
                .cloned()
                .ok_or(SessionError::SpaceNotFound)?;
            let folder = Path::new(path);
            if !folder.is_absolute() || !folder.is_dir() {
                return Err(SessionError::FolderNotFound);
            }
            if saved.folders.iter().any(|f| f.space_id == space_id) {
                return Err(SessionError::SpaceAlreadyLinked);
            }
            if let Some(other) = saved.folders.iter().find(|f| {
                let other = Path::new(&f.path);
                folder.starts_with(other) || other.starts_with(folder)
            }) {
                return Err(SessionError::FolderOverlaps {
                    other: other.path.clone(),
                });
            }
            let linked = LinkedFolder {
                space_id: space.space_id,
                space_name: space.name,
                path: folder.to_string_lossy().into_owned(),
                state: FolderState::UpToDate,
            };
            saved.folders.push(linked.clone());
            Ok(linked)
        })
    }

    fn unlink_folder(&self, space_id: &str) -> Result<(), SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        let parts = [
            Part::Folders,
            Part::Questions,
            Part::Problems,
            Part::Notices,
        ];
        self.update(&mut state, &parts, |saved| {
            saved.account()?;
            if !saved.folders.iter().any(|f| f.space_id == space_id) {
                return Err(SessionError::FolderNotLinked);
            }
            saved.folders.retain(|f| f.space_id != space_id);
            saved.questions.retain(|q| q.space_id != space_id);
            saved.notices.retain(|n| n.space_id != space_id);
            saved
                .problems
                .retain(|p| p.space_id.as_deref() != Some(space_id));
            Ok(())
        })
    }

    fn answer(&self, question_id: &str, answer: Answer) -> Result<(), SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        let parts = [Part::Questions, Part::Folders];
        self.update(&mut state, &parts, |saved| {
            let index = saved
                .questions
                .iter()
                .position(|q| q.id == question_id)
                .ok_or(SessionError::QuestionNotFound)?;
            if !saved.questions[index].answers.contains(&answer) {
                return Err(SessionError::AnswerNotAllowed);
            }
            let space_id = saved.questions.remove(index).space_id;
            let recovering = saved
                .questions
                .iter()
                .any(|q| q.space_id == space_id && matches!(q.kind, QuestionKind::Recovery { .. }));
            if !recovering {
                for folder in &mut saved.folders {
                    if folder.space_id == space_id && folder.state == FolderState::Recovering {
                        folder.state = FolderState::UpToDate;
                    }
                }
            }
            Ok(())
        })?;
        state.answers.push(GivenAnswer {
            question_id: question_id.to_owned(),
            answer,
        });
        Ok(())
    }

    fn dismiss_notice(&self, notice_id: &str) -> Result<(), SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &[Part::Notices], |saved| {
            if !saved.notices.iter().any(|n| n.id == notice_id) {
                return Err(SessionError::NoticeNotFound);
            }
            saved.notices.retain(|n| n.id != notice_id);
            Ok(())
        })
    }
}

mod scenario;

#[cfg(test)]
mod tests {
    use super::scenario::{DOCUMENTS, PHOTOS};
    use super::*;
    use crate::core::SessionListener;
    use std::sync::Arc;

    struct Recorder(Arc<Mutex<Vec<SessionEvent>>>);

    impl SessionListener for Recorder {
        fn on_event(&self, event: SessionEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    struct Harness {
        core: FakeCore,
        seen: Arc<Mutex<Vec<SessionEvent>>>,
        _listener: Arc<Box<dyn SessionListener>>,
    }

    impl Harness {
        fn new(dir: Option<&Path>) -> Self {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let listener: Arc<Box<dyn SessionListener>> =
                Arc::new(Box::new(Recorder(seen.clone())));
            let core = FakeCore::new(Events::new(&listener), dir.map(Path::to_path_buf));
            Self {
                core,
                seen,
                _listener: listener,
            }
        }

        fn take(&self) -> Vec<SessionEvent> {
            std::mem::take(&mut *self.seen.lock().unwrap())
        }

        fn statuses(&self) -> Vec<SyncStatus> {
            self.take()
                .into_iter()
                .filter_map(|e| match e {
                    SessionEvent::Status { status } => Some(status),
                    _ => None,
                })
                .collect()
        }

        fn question_ids(&self) -> Vec<String> {
            self.core
                .state()
                .questions
                .into_iter()
                .map(|q| q.id)
                .collect()
        }
    }

    fn folder_of(core: &FakeCore, space: &str) -> LinkedFolder {
        core.state()
            .folders
            .into_iter()
            .find(|f| f.space_id == space)
            .unwrap()
    }

    #[test]
    fn lifecycle_reports_each_status_once() {
        let h = Harness::new(None);
        assert_eq!(h.core.state().status, SyncStatus::Stopped);
        h.core.start();
        h.core.start();
        assert_eq!(h.statuses(), [SyncStatus::NeedsSetup]);

        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        h.core.pause().unwrap();
        h.core.pause().unwrap();
        h.core.resume().unwrap();
        h.core.set_activity(FakeActivity::Syncing);
        h.core.set_activity(FakeActivity::Offline);
        h.core.stop();
        h.core.stop();
        assert_eq!(
            h.statuses(),
            [
                SyncStatus::Idle,
                SyncStatus::Paused,
                SyncStatus::Idle,
                SyncStatus::Syncing,
                SyncStatus::Offline,
                SyncStatus::Stopped,
            ]
        );
    }

    #[test]
    fn pause_needs_a_running_session_and_outlives_a_restart() {
        let h = Harness::new(None);
        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        h.core.pause().unwrap();
        h.core.start();
        assert_eq!(h.core.state().status, SyncStatus::Idle);

        h.core.pause().unwrap();
        h.core.stop();
        h.core.start();
        assert_eq!(h.core.state().status, SyncStatus::Paused);
    }

    #[test]
    fn claiming_signs_in_the_owner() {
        let h = Harness::new(None);
        h.core.start();
        assert_eq!(
            h.core.claim("ABCD-EFGH-JKLM", "Ada", "Mac"),
            Err(SessionError::NoServer)
        );
        assert_eq!(
            h.core.connect("ftp://box"),
            Err(SessionError::InvalidServerUrl)
        );
        assert_eq!(
            h.core.connect("https://"),
            Err(SessionError::InvalidServerUrl)
        );
        let server = h.core.connect(" https://box.example.com/ ").unwrap();
        assert_eq!(server.url, "https://box.example.com");
        assert!(!server.claimed);

        assert_eq!(
            h.core.pair("ABCD-EFGH", "Mac"),
            Err(SessionError::NotClaimed)
        );
        assert_eq!(
            h.core.claim("ABCD", "Ada", "Mac"),
            Err(SessionError::InvalidCode)
        );
        assert_eq!(
            h.core.claim("abcd-efgh-jklm", " ", "Mac"),
            Err(SessionError::EmptyName)
        );
        let account = h.core.claim("abcd-efgh-jklm", "Ada", "Mac").unwrap();
        assert_eq!(account.server_url, "https://box.example.com");
        assert_eq!(account.user_name, "Ada");
        assert_eq!(h.core.state().account, Some(account.clone()));
        assert_eq!(h.core.devices(), Ok(DeviceList::Unavailable));
        assert!(h.take().contains(&SessionEvent::Account {
            account: Some(account)
        }));
        assert!(h.core.connect("https://box.example.com").unwrap().claimed);
    }

    #[test]
    fn pairing_signs_in_another_device() {
        let h = Harness::new(None);
        h.core.set_server_claimed(true).unwrap();
        h.core.connect("https://box.example.com").unwrap();
        assert_eq!(
            h.core.claim("ABCD-EFGH-JKLM", "Ada", "Mac"),
            Err(SessionError::AlreadyClaimed)
        );
        assert_eq!(h.core.start_pairing(), Err(SessionError::NotSignedIn));
        assert_eq!(
            h.core.pair("ABCDEFGHJ", "Mac"),
            Err(SessionError::InvalidCode)
        );
        h.core.pair("abcd efgh", "Mac").unwrap();
        assert_eq!(h.core.devices(), Ok(DeviceList::Unavailable));
        assert_eq!(h.core.spaces().unwrap().len(), 1);

        let code = h.core.start_pairing().unwrap();
        assert_eq!(normalize_code(&code.code).len(), PAIRING_CODE_LEN);
        assert!(code.expires_at_ms > now_ms());
    }

    #[test]
    fn scripted_failures_apply_to_the_next_call_only() {
        let h = Harness::new(None);
        let unreachable = SessionError::ServerUnreachable {
            reason: "timed out".to_owned(),
        };
        h.core.fail_next(unreachable.clone());
        assert_eq!(h.core.connect("https://box.example.com"), Err(unreachable));
        assert!(h.core.connect("https://box.example.com").is_ok());
    }

    #[test]
    fn linking_checks_the_space_and_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join("inside");
        fs::create_dir(&inside).unwrap();
        let h = Harness::new(None);
        h.core.start();
        assert_eq!(h.core.spaces(), Err(SessionError::NotSignedIn));
        h.core.connect("https://box.example.com").unwrap();
        h.core.claim("ABCDEFGHJKLM", "Ada", "Mac").unwrap();
        assert_eq!(h.core.create_space(""), Err(SessionError::EmptyName));
        let one = h.core.create_space("One").unwrap();
        let two = h.core.create_space("Two").unwrap();
        assert_eq!(h.core.spaces().unwrap(), [one.clone(), two.clone()]);

        let path = dir.path().to_str().unwrap();
        assert_eq!(
            h.core.link_folder("nope", path),
            Err(SessionError::SpaceNotFound)
        );
        assert_eq!(
            h.core.link_folder(&one.space_id, "relative"),
            Err(SessionError::FolderNotFound)
        );
        assert_eq!(
            h.core
                .link_folder(&one.space_id, &format!("{path}/missing")),
            Err(SessionError::FolderNotFound)
        );
        h.take();
        let linked = h.core.link_folder(&one.space_id, path).unwrap();
        assert_eq!(linked.space_name, "One");
        assert_eq!(h.core.state().status, SyncStatus::Idle);
        assert_eq!(
            h.take(),
            [
                SessionEvent::Folders {
                    folders: vec![linked]
                },
                SessionEvent::Status {
                    status: SyncStatus::Idle
                },
            ]
        );
        assert_eq!(
            h.core.link_folder(&one.space_id, inside.to_str().unwrap()),
            Err(SessionError::SpaceAlreadyLinked)
        );
        assert_eq!(
            h.core.link_folder(&two.space_id, inside.to_str().unwrap()),
            Err(SessionError::FolderOverlaps {
                other: path.to_owned()
            })
        );

        assert_eq!(
            h.core.unlink_folder(&two.space_id),
            Err(SessionError::FolderNotLinked)
        );
        h.core.unlink_folder(&one.space_id).unwrap();
        assert!(h.core.state().folders.is_empty());
        assert_eq!(h.core.state().status, SyncStatus::NeedsSetup);
    }

    #[test]
    fn signing_out_unlinks_everything() {
        let h = Harness::new(None);
        h.core.load_scenario(FakeScenario::MassDelete).unwrap();
        h.core.start();
        h.core.sign_out().unwrap();
        let state = h.core.state();
        assert_eq!(state.status, SyncStatus::NeedsSetup);
        assert!(state.account.is_none() && state.folders.is_empty() && state.questions.is_empty());
        assert_eq!(h.core.sign_out(), Err(SessionError::NotSignedIn));
    }

    #[test]
    fn every_question_kind_takes_only_its_own_answers() {
        let h = Harness::new(None);
        h.core.load_scenario(FakeScenario::ManyQuestions).unwrap();
        let questions = h.core.state().questions;
        assert_eq!(questions.len(), 31);
        for question in &questions {
            assert_eq!(question.answers.len(), 2);
            let wrong = [
                Answer::Delete,
                Answer::Restore,
                Answer::Keep,
                Answer::Discard,
                Answer::Publish,
                Answer::Repeat,
                Answer::Cancel,
            ]
            .into_iter()
            .find(|a| !question.answers.contains(a))
            .unwrap();
            assert_eq!(
                h.core.answer(&question.id, wrong),
                Err(SessionError::AnswerNotAllowed)
            );
        }
        assert_eq!(h.core.state().questions.len(), 31);

        for question in &questions {
            h.core.answer(&question.id, question.answers[1]).unwrap();
            assert_eq!(
                h.core.answer(&question.id, question.answers[1]),
                Err(SessionError::QuestionNotFound)
            );
        }
        assert!(h.core.state().questions.is_empty());
        assert_eq!(h.core.given_answers().len(), 31);
        assert_eq!(h.core.given_answers()[0].answer, Answer::Restore);
    }

    #[test]
    fn scenarios_hold_long_lists() {
        let h = Harness::new(None);
        h.core.load_scenario(FakeScenario::ManyQuestions).unwrap();
        let questions = h.core.state().questions;
        assert_eq!(questions[0].items.len(), 1_500);
        let recovery: Vec<_> = questions
            .iter()
            .filter_map(|q| match &q.kind {
                QuestionKind::Recovery { group, .. } => Some((*group, q.items.len())),
                _ => None,
            })
            .collect();
        assert_eq!(recovery.len(), 5);
        assert!(recovery.iter().all(|(_, n)| *n == 300));
        assert_eq!(
            questions
                .iter()
                .filter(|q| q.kind == QuestionKind::AmbiguousDisappearance)
                .count(),
            25
        );
    }

    #[test]
    fn answering_the_last_recovery_question_ends_the_recovery() {
        let h = Harness::new(None);
        h.core.load_scenario(FakeScenario::Recovery).unwrap();
        assert_eq!(folder_of(&h.core, PHOTOS).state, FolderState::Recovering);
        let ids = h.question_ids();
        assert_eq!(ids.len(), 5);
        for question in h.core.state().questions {
            let QuestionKind::Recovery { cause, .. } = &question.kind else {
                panic!("not a recovery question");
            };
            assert_eq!(cause.trigger, RecoveryTrigger::SpaceRewound);
            assert!(cause.restore_point_ms.is_some());
        }
        for (i, question) in h.core.state().questions.into_iter().enumerate() {
            assert_eq!(
                folder_of(&h.core, PHOTOS).state,
                FolderState::Recovering,
                "{i}"
            );
            h.core.answer(&question.id, question.answers[0]).unwrap();
        }
        assert_eq!(folder_of(&h.core, PHOTOS).state, FolderState::UpToDate);
    }

    #[test]
    fn a_replaced_question_refuses_answers() {
        let h = Harness::new(None);
        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        let kind = QuestionKind::AmbiguousDisappearance;
        let item = |p: &str| HeldItem {
            node_id: p.to_owned(),
            path: p.to_owned(),
            kind: NodeKind::File,
            files: 1,
            detail: ItemDetail::Plain,
        };
        let old = h
            .core
            .ask(DOCUMENTS.to_owned(), kind.clone(), vec![item("a")])
            .unwrap();
        h.core.withdraw_question(old.clone()).unwrap();
        let new = h
            .core
            .ask(DOCUMENTS.to_owned(), kind, vec![item("a"), item("b")])
            .unwrap();
        assert_eq!(
            h.core.answer(&old, Answer::Delete),
            Err(SessionError::QuestionNotFound)
        );
        assert_eq!(h.question_ids(), std::slice::from_ref(&new));
        h.core.answer(&new, Answer::Delete).unwrap();
        assert_eq!(
            h.core.given_answers(),
            [GivenAnswer {
                question_id: new,
                answer: Answer::Delete
            }]
        );
    }

    #[test]
    fn unanswered_questions_are_asked_again_after_a_relaunch() {
        let dir = tempfile::tempdir().unwrap();
        let first = Harness::new(Some(dir.path()));
        first.core.load_scenario(FakeScenario::Recovery).unwrap();
        first.core.start();
        first.core.pause().unwrap();
        let answered = first.core.state().questions[0].clone();
        first
            .core
            .answer(&answered.id, answered.answers[0])
            .unwrap();
        let open = first.core.state().questions;
        assert_eq!(open.len(), 4);
        drop(first);

        let second = Harness::new(Some(dir.path()));
        assert_eq!(second.core.state().questions, open);
        assert_eq!(second.core.state().status, SyncStatus::Stopped);
        second.core.start();
        assert_eq!(
            second.take(),
            [
                SessionEvent::Questions {
                    questions: open.clone()
                },
                SessionEvent::Notices {
                    notices: second.core.state().notices
                },
                SessionEvent::Status {
                    status: SyncStatus::Paused
                },
            ]
        );
        assert_eq!(
            folder_of(&second.core, PHOTOS).state,
            FolderState::Recovering
        );
    }

    #[test]
    fn a_missing_or_corrupt_state_file_starts_a_first_run() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(STATE_FILE), b"not json").unwrap();
        let h = Harness::new(Some(dir.path()));
        assert!(h.core.state().account.is_none());
        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        let h = Harness::new(Some(dir.path()));
        assert!(h.core.state().account.is_some());
    }

    #[test]
    fn recent_changes_keep_the_newest() {
        let h = Harness::new(None);
        for i in 0..(RECENT_CHANGES + 5) {
            h.core.add_recent_change(RecentChange {
                space_id: DOCUMENTS.to_owned(),
                path: format!("f{i}"),
                kind: NodeKind::File,
                change: ChangeKind::Added,
                direction: Direction::Uploaded,
                at_ms: i as i64,
            });
        }
        let recent = h.core.state().recent_changes;
        assert_eq!(recent.len(), RECENT_CHANGES);
        assert_eq!(recent[0].path, format!("f{}", RECENT_CHANGES + 4));
    }

    #[test]
    fn problems_scenario_has_every_kind_and_halts_the_missing_folder() {
        let h = Harness::new(None);
        h.core.load_scenario(FakeScenario::Problems).unwrap();
        let problems = h.core.state().problems;
        assert_eq!(problems.len(), 19);
        assert_eq!(folder_of(&h.core, PHOTOS).state, FolderState::Halted);
        let id = problems[0].id.clone();
        h.core.resolve_problem(id.clone()).unwrap();
        assert!(h.core.state().problems.iter().all(|p| p.id != id));

        let reported = h
            .core
            .report_problem(None, None, ProblemKind::SignInRequired)
            .unwrap();
        assert_eq!(h.core.state().problems.last().unwrap().id, reported);
    }

    #[test]
    fn events_stop_after_the_listener_is_gone() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let listener: Arc<Box<dyn SessionListener>> = Arc::new(Box::new(Recorder(seen.clone())));
        let core = FakeCore::new(Events::new(&listener), None);
        core.start();
        drop(listener);
        core.load_scenario(FakeScenario::UpToDate).unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [
                SessionEvent::Questions { questions: vec![] },
                SessionEvent::Notices { notices: vec![] },
                SessionEvent::Status {
                    status: SyncStatus::NeedsSetup
                },
            ]
        );
    }

    #[test]
    fn the_device_list_is_an_optional_capability() {
        let h = Harness::new(None);
        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        assert_eq!(h.core.devices(), Ok(DeviceList::Unavailable));
        let device = Device {
            device_id: "device-1".to_owned(),
            name: "Mac".to_owned(),
            platform: "macos".to_owned(),
            this_device: true,
            added_at_ms: 1,
        };
        h.core.set_devices(Some(vec![device.clone()])).unwrap();
        assert_eq!(
            h.core.devices(),
            Ok(DeviceList::Available {
                devices: vec![device]
            })
        );
    }

    #[test]
    fn a_recovery_without_questions_still_has_a_notice() {
        let h = Harness::new(None);
        h.core
            .load_scenario(FakeScenario::RecoveredWithoutQuestions)
            .unwrap();
        let state = h.core.state();
        assert!(state.questions.is_empty());
        let [notice] = state.notices.as_slice() else {
            panic!("expected one notice: {:?}", state.notices);
        };
        assert_eq!(notice.space_id, PHOTOS);
        let NoticeKind::Recovery { cause, held_files } = &notice.kind else {
            panic!("not a recovery notice");
        };
        assert_eq!(cause.trigger, RecoveryTrigger::SpaceRewound);
        assert_eq!(*held_files, 0);

        h.core.load_scenario(FakeScenario::Recovery).unwrap();
        let notices = h.core.state().notices;
        assert!(matches!(
            notices[0].kind,
            NoticeKind::Recovery { held_files: 30, .. }
        ));
    }

    #[test]
    fn a_remote_mass_delete_is_reported_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(Some(dir.path()));
        h.core
            .load_scenario(FakeScenario::RemoteMassDelete)
            .unwrap();
        let state = h.core.state();
        assert!(state.questions.is_empty());
        let notice = state.notices[0].clone();
        assert_eq!(
            notice.kind,
            NoticeKind::RemoteMassDelete {
                deletions: 3,
                affected_files: 1_200,
                space_files: 2_000,
                exceeded: vec![MassDeleteLimit::ShareOfSpace { percent: 50 }],
            }
        );
        drop(h);

        let h = Harness::new(Some(dir.path()));
        assert_eq!(h.core.state().notices, std::slice::from_ref(&notice));
        h.core.dismiss_notice(&notice.id).unwrap();
        assert!(h.core.state().notices.is_empty());
        assert_eq!(h.take(), [SessionEvent::Notices { notices: vec![] }]);
        assert_eq!(
            h.core.dismiss_notice(&notice.id),
            Err(SessionError::NoticeNotFound)
        );
        let id = h
            .core
            .notify(
                DOCUMENTS.to_owned(),
                NoticeKind::Recovery {
                    cause: RecoveryCause {
                        trigger: RecoveryTrigger::NoSyncState,
                        restore_point_ms: None,
                    },
                    held_files: 2,
                },
            )
            .unwrap();
        assert_eq!(
            Harness::new(Some(dir.path())).core.state().notices[0].id,
            id
        );
    }

    fn not_writable(core: &FakeCore) -> Option<ProblemKind> {
        core.state()
            .problems
            .into_iter()
            .find(|p| p.id == NOT_WRITABLE_ID)
            .map(|p| p.kind)
    }

    #[test]
    fn an_answer_that_cannot_be_saved_is_not_taken() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(Some(dir.path()));
        h.core.load_scenario(FakeScenario::MassDelete).unwrap();
        h.core.start();
        let question = h.core.state().questions[0].clone();
        h.take();

        h.core.fail_saves(true);
        let reason = "saving is scripted to fail".to_owned();
        assert_eq!(
            h.core.answer(&question.id, Answer::Delete),
            Err(SessionError::NotSaved {
                reason: reason.clone()
            })
        );
        assert_eq!(h.core.state().questions, std::slice::from_ref(&question));
        assert!(h.core.given_answers().is_empty());
        assert_eq!(
            not_writable(&h.core),
            Some(ProblemKind::StateNotWritable { reason })
        );
        assert!(matches!(
            h.take().as_slice(),
            [SessionEvent::Problems { .. }]
        ));
        assert!(matches!(h.core.pause(), Err(SessionError::NotSaved { .. })));
        assert_eq!(h.core.state().status, SyncStatus::Idle);
        assert!(matches!(
            h.core.dismiss_notice("none"),
            Err(SessionError::NoticeNotFound)
        ));
        drop(h);

        let h = Harness::new(Some(dir.path()));
        assert_eq!(h.core.state().questions, std::slice::from_ref(&question));
        assert_eq!(not_writable(&h.core), None);
        h.core.answer(&question.id, Answer::Delete).unwrap();
        assert!(
            Harness::new(Some(dir.path()))
                .core
                .state()
                .questions
                .is_empty()
        );
    }

    #[test]
    fn a_scripted_change_that_cannot_be_saved_changes_nothing() {
        let h = Harness::new(None);
        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        h.take();
        h.core.fail_saves(true);
        let kind = QuestionKind::AmbiguousDisappearance;
        assert!(matches!(
            h.core.ask(DOCUMENTS.to_owned(), kind.clone(), vec![]),
            Err(SessionError::NotSaved { .. })
        ));
        assert!(h.question_ids().is_empty());
        assert!(not_writable(&h.core).is_some());
        let since = h.core.state().problems.last().unwrap().since_ms;
        assert!(
            h.core
                .report_problem(None, None, ProblemKind::FileBusy)
                .is_err()
        );
        assert!(h.core.load_scenario(FakeScenario::MassDelete).is_err());
        assert_eq!(h.core.state().problems.len(), 1);
        assert_eq!(h.core.state().problems[0].since_ms, since);
        assert!(h.core.state().questions.is_empty());
        assert!(
            h.take()
                .iter()
                .all(|e| matches!(e, SessionEvent::Problems { .. })),
            "only the problem list may change"
        );

        h.core.fail_saves(false);
        h.core.ask(DOCUMENTS.to_owned(), kind, vec![]).unwrap();
        assert_eq!(not_writable(&h.core), None);
        assert_eq!(h.question_ids().len(), 1);
    }

    #[test]
    fn memory_matches_disk_after_failed_script_and_answer_saves() {
        let dir = tempfile::tempdir().unwrap();
        let h = Harness::new(Some(dir.path()));
        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        h.core.fail_saves(true);
        assert!(matches!(
            h.core.ask(
                DOCUMENTS.to_owned(),
                QuestionKind::AmbiguousDisappearance,
                vec![],
            ),
            Err(SessionError::NotSaved { .. })
        ));
        let in_memory = h.core.state().questions;
        drop(h);

        let persisted = Harness::new(Some(dir.path())).core.state().questions;
        assert_eq!(in_memory, persisted);
        assert!(persisted.is_empty());

        let h = Harness::new(Some(dir.path()));
        let id = h
            .core
            .ask(
                DOCUMENTS.to_owned(),
                QuestionKind::AmbiguousDisappearance,
                vec![],
            )
            .unwrap();
        h.core.fail_saves(true);
        assert!(matches!(
            h.core.answer(&id, Answer::Delete),
            Err(SessionError::NotSaved { .. })
        ));
        let in_memory = h.core.state().questions;
        drop(h);
        assert_eq!(
            Harness::new(Some(dir.path())).core.state().questions,
            in_memory
        );
        assert_eq!(in_memory.len(), 1);
    }

    #[test]
    fn an_unwritable_state_directory_refuses_changes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-directory");
        fs::write(&file, b"").unwrap();
        let h = Harness::new(Some(&file));
        h.core.start();
        let error = h.core.connect("https://box.example.com").unwrap_err();
        assert!(matches!(error, SessionError::NotSaved { .. }), "{error:?}");
        assert!(matches!(
            not_writable(&h.core),
            Some(ProblemKind::StateNotWritable { .. })
        ));
        assert_eq!(
            h.core.claim("ABCDEFGHJKLM", "Ada", "Mac"),
            Err(SessionError::NoServer)
        );
    }
}
