// SPDX-License-Identifier: Apache-2.0

//! Signing out of a profile whose folders are linked, against a server that never answers: the
//! folders' tasks run, but sign-out does not need the server.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use koidrive_driver::{Credential, Limit, Paths, Question, QuestionKind};
use koidrive_ffi::{Answer, SessionEvent, SessionListener, SyncSession, SyncStatus};
use koidrive_store::{Link, Store, StoredQuestion};

/// Nothing listens on the discard port.
const SERVER: &str = "http://127.0.0.1:9/";

const WAIT: Duration = Duration::from_secs(5);

struct Ignore;

impl SessionListener for Ignore {
    fn on_event(&self, _: SessionEvent) {}
}

/// A paired profile in `base` with a folder linked at `base/<space>` for each of `spaces`, each
/// holding one file of the person's and asked `questions` mass-delete questions, `<space>-<n>`.
fn paired_profile(base: &Path, spaces: &[&str], questions: usize) -> PathBuf {
    let profile = base.join("profile");
    let paths = Paths::under(&profile);
    Credential {
        server: SERVER.into(),
        user_id: "user".into(),
        user_name: None,
        device_id: "device".into(),
        device_name: "Mac".into(),
        refresh_credential: "not-a-secret".into(),
    }
    .save_new(&paths.credential())
    .unwrap();
    for space in spaces {
        let root = base.join(space);
        fs::create_dir_all(root.join(".koidrive")).unwrap();
        fs::write(
            root.join(".koidrive/space.json"),
            format!("{{\"space_id\":\"{space}\",\"effects\":0}}"),
        )
        .unwrap();
        fs::write(root.join("kept.txt"), b"kept").unwrap();
        let store = paths.store(space).unwrap();
        fs::create_dir_all(store.parent().unwrap()).unwrap();
        Store::open(&store)
            .unwrap()
            .transaction(|tx| {
                tx.set_link(&Link {
                    space_id: (*space).into(),
                    space_name: (*space).into(),
                    root: root.clone(),
                    root_dev: fs::metadata(&root).unwrap().dev(),
                    server_url: SERVER.into(),
                    device_id: "device".into(),
                    linked_at_ms: 1,
                })?;
                for n in 0..questions {
                    let id = format!("{space}-{n}");
                    let question = Question {
                        id: id.clone(),
                        space_id: (*space).into(),
                        asked_at_ms: 1,
                        kind: QuestionKind::MassDelete {
                            deletions: 1_200,
                            affected_files: 1_500,
                            space_files: 2_000,
                            exceeded: vec![Limit::Deletions, Limit::Share],
                        },
                        items: Vec::new(),
                    };
                    tx.put_question(&StoredQuestion {
                        question_id: id,
                        asked_at_ms: 1,
                        body: serde_json::to_vec(&question).unwrap(),
                        answer: None,
                    })?;
                }
                Ok::<_, koidrive_store::Error>(())
            })
            .unwrap();
    }
    profile
}

fn open(profile: &Path) -> Arc<SyncSession> {
    SyncSession::open(Box::new(Ignore), profile.to_str().unwrap().into()).unwrap()
}

/// Waits until `path` exists, or returns false after `WAIT`.
fn appears(path: &Path) -> bool {
    let deadline = Instant::now() + WAIT;
    while !path.exists() {
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    true
}

#[test]
fn signing_out_while_the_app_reads_the_state_unlinks_every_folder() {
    for _ in 0..20 {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let session = open(&paired_profile(&base, &["a1", "b2"], 0));
        session.start();
        let done = Arc::new(AtomicBool::new(false));
        let reader = {
            let (session, done) = (session.clone(), done.clone());
            thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    session.state();
                }
            })
        };
        let signed_out = session.sign_out(true);
        done.store(true, Ordering::SeqCst);
        reader.join().unwrap();
        assert_eq!(signed_out, Ok(()));
        let state = session.state();
        assert_eq!(state.account, None);
        assert!(state.folders.is_empty());
        assert_eq!(state.status, SyncStatus::NeedsSetup);
        assert_eq!(fs::read(base.join("a1/kept.txt")).unwrap(), b"kept");
    }
}

#[test]
fn signing_out_while_the_person_answers_questions_unlinks_every_folder() {
    for _ in 0..20 {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let session = open(&paired_profile(&base, &["a1", "b2"], 50));
        session.start();
        let done = Arc::new(AtomicBool::new(false));
        let answering = {
            let (session, done) = (session.clone(), done.clone());
            thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    for n in 0..50 {
                        let _ = session.answer(format!("b2-{n}"), Answer::Delete);
                    }
                }
            })
        };
        let signed_out = session.sign_out(true);
        done.store(true, Ordering::SeqCst);
        answering.join().unwrap();
        assert_eq!(signed_out, Ok(()));
        let state = session.state();
        assert!(state.folders.is_empty());
        assert!(state.questions.is_empty());
    }
}

#[test]
fn a_sign_out_that_cannot_unlink_a_folder_leaves_sync_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let session = open(&paired_profile(&base, &["a1", "b2"], 0));
    session.start();
    let owner = base.join("b2/.koidrive/device.json");
    assert!(appears(&owner), "the folder's task did not start");
    fs::remove_file(&owner).unwrap();

    let private = base.join("a1/.koidrive");
    fs::set_permissions(&private, fs::Permissions::from_mode(0o000)).unwrap();
    let refused = session.sign_out(true);
    fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(refused.is_err(), "the first folder's cleanup should fail");
    assert!(
        !appears(&owner),
        "the other folder synced again after the sign-out failed"
    );
    let state = session.state();
    assert!(state.account.is_some());
    assert_eq!(state.status, SyncStatus::Stopped);

    assert_eq!(session.sign_out(true), Ok(()));
    let state = session.state();
    assert_eq!(state.account, None);
    assert!(state.folders.is_empty());
    assert_eq!(state.status, SyncStatus::NeedsSetup);
    assert_eq!(fs::read(base.join("b2/kept.txt")).unwrap(), b"kept");
}
