// SPDX-License-Identifier: Apache-2.0

//! The session against a loopback server that answers when the test lets it, so that a call can be
//! made while another one waits on the network. The app makes `state`, `start`, `stop`, `pause`
//! and `resume` on its main thread, so none of them may wait for a network call to end.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use flarebox_driver::{Credential, Paths};
use flarebox_ffi::{
    FolderState, ProblemKind, SessionEvent, SessionListener, SyncSession, SyncStatus,
};
use flarebox_store::{Link, Store};

/// Longer than any main-thread call may take, and shorter than the server waits for the test.
const PROMPT: Duration = Duration::from_secs(1);

const WAIT: Duration = Duration::from_secs(10);

/// `GET /v1/server-info` for a server that speaks protocol 1 only.
const SERVER_INFO: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n\x10\x01\x18\x01";

const FAILED: &[u8] = b"HTTP/1.1 500 Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

struct Ignore;

impl SessionListener for Ignore {
    fn on_event(&self, _: SessionEvent) {}
}

/// A server that answers each request with the next of `answers` and holds the last request until
/// the test sends on the returned channel. The first receiver hears when the last request arrives.
fn server(
    answers: &'static [&'static [u8]],
) -> (String, mpsc::Receiver<()>, mpsc::Sender<()>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let (arrived, held) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let worker = thread::spawn(move || {
        for (index, answer) in answers.iter().enumerate() {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 8192];
            let _ = socket.read(&mut request).unwrap();
            if index + 1 == answers.len() {
                arrived.send(()).unwrap();
                let _ = released.recv_timeout(WAIT);
            }
            socket.write_all(answer).unwrap();
        }
    });
    (url, held, release, worker)
}

/// A paired profile in `base` with one folder linked to space `a1` at `root`.
fn paired_profile(base: &Path, server: &str, root: PathBuf) -> PathBuf {
    let profile = base.join("profile");
    let paths = Paths::under(&profile);
    Credential {
        server: server.into(),
        user_id: "user".into(),
        user_name: None,
        device_id: "device".into(),
        device_name: "Mac".into(),
        refresh_credential: "not-a-secret".into(),
    }
    .save_new(&paths.credential())
    .unwrap();
    let store = paths.store("a1").unwrap();
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    Store::open(&store)
        .unwrap()
        .transaction(|tx| {
            tx.set_link(&Link {
                space_id: "a1".into(),
                space_name: "Documents".into(),
                root,
                root_dev: 1,
                server_url: server.into(),
                device_id: "device".into(),
                linked_at_ms: 1,
            })
        })
        .unwrap();
    profile
}

fn open(profile: &Path) -> Arc<SyncSession> {
    SyncSession::open(Box::new(Ignore), profile.to_str().unwrap().into()).unwrap()
}

fn timed<T>(call: impl FnOnce() -> T) -> (T, Duration) {
    let started = Instant::now();
    let result = call();
    (result, started.elapsed())
}

#[test]
fn the_state_is_read_while_a_claim_waits_on_the_server() {
    let (url, held, release, server) = server(&[SERVER_INFO, FAILED]);
    let dir = tempfile::tempdir().unwrap();
    let session = open(&dir.path().join("profile"));
    session.start();
    session.connect(url).unwrap();
    let claimant = Arc::clone(&session);
    let claim = thread::spawn(move || claimant.claim("CODE".into(), "Ada".into(), "Mac".into()));
    held.recv_timeout(WAIT).unwrap();

    let (state, took) = timed(|| session.state());
    let _ = release.send(());
    assert!(
        took < PROMPT,
        "state() took {took:?} while the claim waited"
    );
    assert_eq!(state.status, SyncStatus::NeedsSetup);
    assert!(claim.join().unwrap().is_err());
    server.join().unwrap();
}

#[test]
fn sync_pauses_and_resumes_while_a_folder_link_waits_on_the_server() {
    let (url, held, release, server) = server(&[FAILED]);
    let dir = tempfile::tempdir().unwrap();
    let profile = paired_profile(dir.path(), &url, dir.path().join("missing"));
    let folder = dir.path().join("new");
    std::fs::create_dir(&folder).unwrap();
    let session = open(&profile);
    session.start();
    let linker = Arc::clone(&session);
    let link =
        thread::spawn(move || linker.link_folder("a2".into(), folder.to_str().unwrap().into()));
    held.recv_timeout(WAIT).unwrap();

    let (paused, pause_took) = timed(|| session.pause());
    let (resumed, resume_took) = timed(|| session.resume());
    let (_, stop_took) = timed(|| session.stop());
    let _ = release.send(());
    paused.unwrap();
    resumed.unwrap();
    assert!(
        pause_took < PROMPT,
        "pause() took {pause_took:?} while the link waited"
    );
    assert!(
        resume_took < PROMPT,
        "resume() took {resume_took:?} while the link waited"
    );
    assert!(
        stop_took < PROMPT,
        "stop() took {stop_took:?} while the link waited"
    );
    assert!(link.join().unwrap().is_err());
    server.join().unwrap();
}

#[test]
fn a_session_whose_only_folder_halted_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let profile = paired_profile(
        dir.path(),
        "http://127.0.0.1:1/",
        dir.path().join("missing"),
    );
    let session = open(&profile);
    session.start();
    let started = Instant::now();
    let state = loop {
        let state = session.state();
        if state.folders[0].state == FolderState::Halted {
            break state;
        }
        assert!(
            started.elapsed() < WAIT,
            "the folder never halted: {state:?}"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(state.status, SyncStatus::Halted);
    let [problem] = state.problems.as_slice() else {
        panic!("one problem expected: {:?}", state.problems);
    };
    let ProblemKind::FolderHalted { reason } = &problem.kind else {
        panic!("a halted folder expected: {problem:?}");
    };
    assert!(reason.contains("missing"), "{reason}");
}
