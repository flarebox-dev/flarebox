// SPDX-License-Identifier: Apache-2.0

//! The app's session against the real Worker in `server/`, run by `core/ffi/scripts/integration-test.sh`,
//! which starts `wrangler dev` and sets `FLAREBOX_TEST_SERVER` (its URL) and `FLAREBOX_TEST_SERVER_LOG`
//! (its output, where the Worker prints the claim code). Each device is a [`SyncSession`] on a profile
//! of its own, driven only through the calls the app makes.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use flarebox_driver as driver;
use flarebox_ffi::{
    Answer, FolderState, LinkedFolder, Problem, ProblemKind, Question, QuestionKind, RecoveryGroup,
    SessionError, SessionEvent, SessionListener, SessionState, SyncSession, SyncStatus,
};
use flarebox_net::Auth;
use flarebox_store::{Store, StoredQuestion};

const WAIT: Duration = Duration::from_secs(30);

fn server_url() -> String {
    std::env::var("FLAREBOX_TEST_SERVER").expect(
        "FLAREBOX_TEST_SERVER is not set; run core/ffi/scripts/integration-test.sh, which starts the Worker",
    )
}

/// Issues a claim code through `GET /setup` and reads it from the Worker's log.
fn claim_code() -> String {
    let log =
        std::env::var("FLAREBOX_TEST_SERVER_LOG").expect("FLAREBOX_TEST_SERVER_LOG is not set");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let setup = runtime
        .block_on(reqwest::get(format!("{}/setup", server_url())))
        .unwrap();
    assert!(
        setup.status().is_success(),
        "GET /setup answered {}",
        setup.status()
    );
    let started = Instant::now();
    loop {
        let text = fs::read_to_string(&log).unwrap_or_default();
        if let Some(code) = text.lines().rev().find_map(code_on) {
            return code;
        }
        assert!(started.elapsed() < WAIT, "no claim code in {log}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The claim code on a Worker log line that mentions one, such as `ABCD-EFGH-JKLM`.
fn code_on(line: &str) -> Option<String> {
    if !line.to_ascii_lowercase().contains("claim code") {
        return None;
    }
    line.split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        .rfind(|word| {
            word.contains('-')
                && word.len() >= 8
                && word.split('-').all(|group| {
                    !group.is_empty() && group.chars().all(|c| c.is_ascii_alphanumeric())
                })
        })
        .map(str::to_owned)
}

#[derive(Default)]
struct Seen {
    events: Mutex<Vec<SessionEvent>>,
    changed: Condvar,
}

struct Recorder(Arc<Seen>);

impl SessionListener for Recorder {
    fn on_event(&self, event: SessionEvent) {
        self.0.events.lock().unwrap().push(event);
        self.0.changed.notify_all();
    }
}

/// The state the app builds: the state it read when it opened the session, updated by every event
/// since.
#[derive(Debug)]
struct Mirror {
    status: SyncStatus,
    signed_in: bool,
    folders: Vec<LinkedFolder>,
    questions: Vec<Question>,
    problems: Vec<Problem>,
}

impl Mirror {
    fn of(opened: &SessionState, events: &[SessionEvent]) -> Self {
        let mut mirror = Mirror {
            status: opened.status,
            signed_in: opened.account.is_some(),
            folders: opened.folders.clone(),
            questions: opened.questions.clone(),
            problems: opened.problems.clone(),
        };
        for event in events {
            match event {
                SessionEvent::Status { status } => mirror.status = *status,
                SessionEvent::Account { account } => mirror.signed_in = account.is_some(),
                SessionEvent::Folders { folders } => mirror.folders = folders.clone(),
                SessionEvent::Questions { questions } => mirror.questions = questions.clone(),
                SessionEvent::Problems { problems } => mirror.problems = problems.clone(),
                _ => {}
            }
        }
        mirror
    }
}

/// One device: a profile directory and the session open on it, with every event it sent.
struct Device {
    home: tempfile::TempDir,
    session: Option<Arc<SyncSession>>,
    seen: Arc<Seen>,
    opened: Option<SessionState>,
}

impl Device {
    fn new() -> Self {
        let mut device = Device {
            home: tempfile::tempdir().unwrap(),
            session: None,
            seen: Arc::default(),
            opened: None,
        };
        device.reopen();
        device
    }

    fn profile(&self) -> PathBuf {
        self.home.path().join("profile")
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.home.path().join(rel)
    }

    fn session(&self) -> &SyncSession {
        self.session.as_ref().unwrap()
    }

    /// Closes the session, as quitting the app does, and opens the profile again.
    fn reopen(&mut self) {
        self.session = None;
        self.seen = Arc::default();
        let opened = SyncSession::open(
            Box::new(Recorder(self.seen.clone())),
            path_str(&self.profile()),
        );
        let session = opened.unwrap();
        self.seen.events.lock().unwrap().clear();
        self.opened = Some(session.state());
        self.session = Some(session);
    }

    /// Waits until the state read at opening, updated by the events sent since, is one that `done`
    /// accepts. It never reads the state again, so it fails if a change is not sent as an event.
    fn until(&self, what: &str, done: impl Fn(&Mirror) -> bool) -> Mirror {
        let started = Instant::now();
        let mut events = self.seen.events.lock().unwrap();
        loop {
            let mirror = Mirror::of(self.opened.as_ref().unwrap(), &events);
            if done(&mirror) {
                return mirror;
            }
            let left = WAIT.saturating_sub(started.elapsed());
            assert!(
                !left.is_zero(),
                "{what} did not happen; events: {events:#?}"
            );
            events = self.seen.changed.wait_timeout(events, left).unwrap().0;
        }
    }
}

/// Copies the credential of the profile at `profile` to `copy`, to ask the server about it after a sign-out.
fn keep_credential(profile: &Path, copy: &Path) {
    fs::copy(driver::Paths::under(profile).credential(), copy).unwrap();
}

/// Whether the server still accepts the credential kept at `copy`.
fn accepted(copy: &Path) -> bool {
    let credential = driver::Credential::load(copy).unwrap().unwrap();
    let auth = Auth::new(credential.api().unwrap(), credential.net());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    match runtime.block_on(auth.api().list_spaces(&auth)) {
        Ok(_) => true,
        Err(error) if error.is_unauthenticated() => false,
        Err(error) => panic!("the server did not answer: {error}"),
    }
}

fn path_str(path: &Path) -> String {
    path.to_str().unwrap().to_owned()
}

#[test]
#[ignore = "needs the Worker; run core/ffi/scripts/integration-test.sh"]
fn the_app_claims_pairs_links_and_follows_the_driver() {
    let server = server_url();

    let mut mac = Device::new();
    assert!(matches!(
        SyncSession::open(Box::new(Recorder(Arc::default())), path_str(&mac.profile())),
        Err(SessionError::ProfileInUse)
    ));
    mac.session().start();
    mac.until("setup being needed", |m| m.status == SyncStatus::NeedsSetup);
    assert_eq!(mac.session().spaces(), Err(SessionError::NotSignedIn));
    assert_eq!(
        mac.session()
            .claim("ABCD-EFGH".into(), "Ada".into(), "Mac".into()),
        Err(SessionError::NoServer)
    );

    let unclaimed = mac.session().connect(format!("{server}/")).unwrap();
    assert_eq!(unclaimed.url, server.trim_end_matches('/'));
    assert!(!unclaimed.claimed);
    assert_eq!(
        mac.session().connect("files.example.com".into()),
        Err(SessionError::InvalidServerUrl)
    );
    let code = claim_code();
    assert_eq!(
        mac.session()
            .claim("WRONG-CODE".into(), "Ada".into(), "Mac".into()),
        Err(SessionError::InvalidCode)
    );
    let owner = mac
        .session()
        .claim(code, "Ada".into(), "Ada's Mac".into())
        .unwrap();
    assert_eq!(owner.device_name, "Ada's Mac");
    assert_eq!(owner.user_name, "Ada");
    assert!(mac.session().connect(server.clone()).unwrap().claimed);
    mac.until("the account arriving", |m| m.signed_in);

    let space = mac.session().create_space("Documents".into()).unwrap();
    assert!(
        mac.session()
            .spaces()
            .unwrap()
            .iter()
            .any(|listed| listed.space_id == space.space_id)
    );
    let usage = mac.session().space_usage(space.space_id.clone()).unwrap();
    assert_eq!(
        (usage.live_files, usage.live_bytes, usage.unreferenced_bytes),
        (0, 0, 0)
    );
    let folder = mac.path("Documents");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("notes.txt"), b"notes").unwrap();
    assert!(matches!(
        mac.session()
            .link_folder(space.space_id.clone(), path_str(&mac.profile())),
        Err(SessionError::FolderOverlaps { .. })
    ));
    assert_eq!(
        mac.session()
            .link_folder(space.space_id.clone(), path_str(&mac.path("Missing"))),
        Err(SessionError::FolderNotFound)
    );
    let linked = mac
        .session()
        .link_folder(space.space_id.clone(), path_str(&folder))
        .unwrap();
    assert_eq!(linked.space_name, "Documents");
    let synced = mac.until("the first sync", |m| {
        m.status == SyncStatus::Idle
            && m.folders.len() == 1
            && m.folders[0].state == FolderState::UpToDate
    });
    assert!(synced.problems.is_empty());

    mac.session().pause().unwrap();
    mac.until("the pause", |m| m.status == SyncStatus::Paused);
    mac.reopen();
    assert_eq!(mac.session().state().status, SyncStatus::Stopped);
    mac.session().start();
    let relaunched = mac.until("the pause after a relaunch", |m| {
        m.status == SyncStatus::Paused
    });
    assert!(relaunched.signed_in);
    assert_eq!(relaunched.folders.len(), 1);
    mac.session().resume().unwrap();
    mac.until("sync after resuming", |m| m.status == SyncStatus::Idle);

    let pairing = mac.session().start_pairing().unwrap();
    let other = Device::new();
    other.session().connect(server.clone()).unwrap();
    assert_eq!(
        other.session().pair("AAAA-BBBB".into(), "Laptop".into()),
        Err(SessionError::InvalidCode)
    );
    let paired = other.session().pair(pairing.code, "Laptop".into()).unwrap();
    assert_eq!(paired.user_id, owner.user_id);
    assert_ne!(paired.device_id, owner.device_id);
    let other_folder = other.path("Documents");
    fs::create_dir_all(&other_folder).unwrap();
    other
        .session()
        .link_folder(space.space_id.clone(), path_str(&other_folder))
        .unwrap();
    other.session().start();
    other.until("the paired device's first sync", |m| {
        m.status == SyncStatus::Idle
    });
    let kept = other.path("credential");
    keep_credential(&other.profile(), &kept);
    other.session().sign_out(false).unwrap();
    other.until("the paired device's sign-out", |m| !m.signed_in);
    assert!(!accepted(&kept));
    drop(other);

    let listed = |device: &Device| {
        let mut devices: Vec<(String, bool)> = (device.session().devices().unwrap().into_iter())
            .map(|device| (device.name, device.this_device))
            .collect();
        devices.sort();
        devices
    };
    assert_eq!(listed(&mac), [("Ada's Mac".to_owned(), true)]);
    let old = Device::new();
    old.session().connect(server.clone()).unwrap();
    let pairing = mac.session().start_pairing().unwrap();
    let old_account = old.session().pair(pairing.code, "Old Mac".into()).unwrap();
    assert_eq!(
        listed(&mac),
        [
            ("Ada's Mac".to_owned(), true),
            ("Old Mac".to_owned(), false)
        ]
    );
    assert_eq!(
        listed(&old),
        [
            ("Ada's Mac".to_owned(), false),
            ("Old Mac".to_owned(), true)
        ]
    );
    assert_eq!(
        mac.session().revoke_device(owner.device_id.clone()),
        Err(SessionError::ThisDevice)
    );
    assert_eq!(
        mac.session()
            .revoke_device(format!("{}?confirm=last-device", owner.device_id)),
        Err(SessionError::DeviceNotFound)
    );
    assert_eq!(
        listed(&mac),
        [
            ("Ada's Mac".to_owned(), true),
            ("Old Mac".to_owned(), false)
        ]
    );
    let old_kept = old.path("credential");
    keep_credential(&old.profile(), &old_kept);
    mac.session()
        .revoke_device(old_account.device_id.clone())
        .unwrap();
    assert!(!accepted(&old_kept));
    assert_eq!(listed(&mac), [("Ada's Mac".to_owned(), true)]);
    assert_eq!(
        mac.session()
            .revoke_device("0190b6a5-0000-7000-8000-000000000000".into()),
        Err(SessionError::DeviceNotFound)
    );
    drop(old);

    let held = driver::Question {
        id: "q-mass-delete".into(),
        space_id: space.space_id.clone(),
        asked_at_ms: 1,
        kind: driver::QuestionKind::MassDelete {
            deletions: 1_200,
            affected_files: 1_200,
            space_files: 2_000,
        },
        items: vec![driver::HeldItem {
            node_id: "n1".into(),
            path: "Archive".into(),
            files: 1_200,
        }],
    };
    let store = mac
        .profile()
        .join(format!("state/spaces/{}/store.sqlite", space.space_id));
    mac.session().stop();
    Store::open(&store)
        .unwrap()
        .transaction(|tx| {
            tx.put_question(&StoredQuestion {
                question_id: held.id.clone(),
                asked_at_ms: held.asked_at_ms,
                body: serde_json::to_vec(&held).unwrap(),
                answer: None,
            })
        })
        .unwrap();
    mac.session().start();
    let asked = mac.until("the stored question", |m| m.questions.len() == 1);
    assert_eq!(
        asked.questions[0].answers,
        vec![Answer::Delete, Answer::Restore]
    );
    assert_eq!(
        mac.session().answer(held.id.clone(), Answer::Keep),
        Err(SessionError::AnswerNotAllowed)
    );
    mac.session()
        .answer(held.id.clone(), Answer::Restore)
        .unwrap();
    mac.until("the answered question leaving", |m| m.questions.is_empty());
    assert_eq!(
        mac.session().answer(held.id, Answer::Restore),
        Err(SessionError::QuestionNotFound)
    );

    mac.session().stop();
    fs::remove_file(folder.join(".flarebox/space.json")).unwrap();
    mac.session().start();
    let halted = mac.until("the missing marker halting the folder", |m| {
        m.folders.first().map(|folder| folder.state) == Some(FolderState::Halted)
            && !m.problems.is_empty()
    });
    assert!(matches!(
        halted.problems[0].kind,
        ProblemKind::FolderHalted { .. }
    ));
    assert_eq!(
        halted.problems[0].space_id.as_deref(),
        Some(space.space_id.as_str())
    );

    mac.session().unlink_folder(space.space_id.clone()).unwrap();
    let unlinked = mac.until("the unlink", |m| {
        m.folders.is_empty() && m.problems.is_empty()
    });
    assert_eq!(unlinked.status, SyncStatus::NeedsSetup);
    assert_eq!(fs::read(folder.join("notes.txt")).unwrap(), b"notes");
    assert_eq!(
        mac.session().unlink_folder(space.space_id.clone()),
        Err(SessionError::FolderNotLinked)
    );

    let again = mac.path("Again");
    fs::create_dir_all(&again).unwrap();
    fs::write(again.join("kept.txt"), b"kept").unwrap();
    mac.session()
        .link_folder(space.space_id, path_str(&again))
        .unwrap();
    let asked = mac.until("the second link's question", |m| m.questions.len() == 1);
    let question = &asked.questions[0];
    assert!(matches!(
        question.kind,
        QuestionKind::Recovery {
            group: RecoveryGroup::CreatedOnThisDevice,
            ..
        }
    ));
    let held: Vec<&str> = (question.items.iter())
        .map(|item| item.path.as_str())
        .collect();
    assert_eq!(held, ["kept.txt"]);
    mac.session()
        .answer(question.id.clone(), Answer::Publish)
        .unwrap();
    mac.until("the second link's sync", |m| {
        m.status == SyncStatus::Idle && m.questions.is_empty()
    });
    let spare = Device::new();
    spare.session().connect(server).unwrap();
    let pairing = mac.session().start_pairing().unwrap();
    spare.session().pair(pairing.code, "Spare".into()).unwrap();
    let kept = mac.path("credential");
    keep_credential(&mac.profile(), &kept);
    let spare_kept = spare.path("credential");
    keep_credential(&spare.profile(), &spare_kept);
    let (mac_out, spare_out) = thread::scope(|scope| {
        let spare_out = scope.spawn(|| spare.session().sign_out(false));
        (mac.session().sign_out(false), spare_out.join().unwrap())
    });
    let (last, last_kept) = match (&mac_out, &spare_out) {
        (Err(SessionError::LastDevice), Ok(())) => (&mac, &kept),
        (Ok(()), Err(SessionError::LastDevice)) => (&spare, &spare_kept),
        outcomes => {
            panic!("one of the last two devices signs out and the other must confirm: {outcomes:?}")
        }
    };
    assert!(last.session().state().account.is_some());
    assert!(accepted(last_kept));
    last.session().sign_out(true).unwrap();
    assert!(!accepted(&spare_kept));
    let signed_out = mac.until("the sign-out", |m| {
        !m.signed_in && m.folders.is_empty() && m.status == SyncStatus::NeedsSetup
    });
    assert!(signed_out.problems.is_empty());
    assert_eq!(fs::read(again.join("kept.txt")).unwrap(), b"kept");
    assert_eq!(mac.session().spaces(), Err(SessionError::NotSignedIn));
    assert!(!accepted(&kept));
    mac.reopen();
    assert_eq!(mac.opened.as_ref().unwrap().account, None);
}
