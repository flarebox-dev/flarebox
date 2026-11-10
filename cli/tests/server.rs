// SPDX-License-Identifier: Apache-2.0

//! The `flarebox` command against the real Worker in `server/`, run by `cli/scripts/integration-test.sh`, which starts
//! `wrangler dev` and sets `FLAREBOX_TEST_SERVER` (its URL) and `FLAREBOX_TEST_SERVER_LOG` (its output, where the
//! Worker prints the claim code). The owner's device is driven through `flarebox-net`; the device under test only
//! through the command.

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use flarebox_chunk::{ChunkerConfig, chunk_hash};
use flarebox_net::{Api, Auth, Credentials, Event, Session, SessionConfig, Transfer};
use flarebox_proto::v1::{
    ChunkRef, ClaimRequest, Commit, Content, CreateDir, CreateFile, CreateSpaceRequest, Delete, Op,
    PairCompleteRequest, TokenRequest, UpdateContent, op, op_result,
};
use serde_json::Value;
use tokio::sync::OnceCell;
use url::Url;

const WAIT: Duration = Duration::from_secs(30);

fn server_url() -> Url {
    std::env::var("FLAREBOX_TEST_SERVER")
        .expect("FLAREBOX_TEST_SERVER is not set; run cli/scripts/integration-test.sh, which starts the Worker")
        .parse()
        .expect("FLAREBOX_TEST_SERVER is a URL")
}

fn api() -> Api {
    Api::local_development(server_url()).unwrap()
}

static OWNER: OnceCell<Credentials> = OnceCell::const_new();

/// The deployment's owner, claimed once for every test in this binary.
async fn owner() -> Credentials {
    OWNER.get_or_init(claim).await.clone()
}

/// Claims the deployment with the code the Worker prints to its log.
async fn claim() -> Credentials {
    let log =
        std::env::var("FLAREBOX_TEST_SERVER_LOG").expect("FLAREBOX_TEST_SERVER_LOG is not set");
    let setup = reqwest::get(server_url().join("setup").unwrap())
        .await
        .unwrap();
    assert!(
        setup.status().is_success(),
        "GET /setup answered {}",
        setup.status()
    );
    let started = Instant::now();
    let code = loop {
        let text = fs::read_to_string(&log).unwrap_or_default();
        if let Some(code) = text.lines().rev().find_map(claim_code) {
            break code;
        }
        assert!(started.elapsed() < WAIT, "no claim code in {log}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let request = ClaimRequest {
        code,
        user_name: "Owner".into(),
        device_name: "owner".into(),
        platform: "test".into(),
        setup_attempt: String::new(),
    };
    Credentials::from(&api().claim(&request).await.expect("the claim succeeds"))
}

/// The claim code on a Worker log line that mentions one, such as `ABCD-EFGH-JKLM`.
fn claim_code(line: &str) -> Option<String> {
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

fn uuidv7() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let bits = (ms << 80) | (fastrand::u128(..) & ((1 << 80) - 1));
    let bits = (bits & !(0xf << 76)) | (0x7 << 76);
    let bits = (bits & !(0x3 << 62)) | (0x2 << 62);
    let hex = format!("{bits:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// A machine's home for the device under test, with its XDG directories.
struct Machine {
    home: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        Machine {
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.home.path().join(rel)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_flarebox"));
        command
            .args(args)
            .current_dir(self.home.path())
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_RUNTIME_DIR", self.path("run"))
            .env_remove("FLAREBOX_LOG");
        command
    }

    fn output(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run flarebox")
    }

    fn run(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "flarebox {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn fails(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(!output.status.success(), "flarebox {args:?} succeeded");
        String::from_utf8(output.stderr).unwrap()
    }

    fn status(&self) -> Value {
        serde_json::from_str(&self.run(&["status", "--json"])).unwrap()
    }

    /// Every entry of the profile's directories with its modification time and, for a file, its contents.
    fn profile(&self) -> Vec<(PathBuf, SystemTime, Vec<u8>)> {
        let mut found = Vec::new();
        let mut todo: Vec<PathBuf> = ["config", "state", "run"]
            .iter()
            .map(|dir| self.path(dir))
            .filter(|dir| dir.exists())
            .collect();
        while let Some(dir) = todo.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                let bytes = if meta.is_dir() {
                    todo.push(path.clone());
                    Vec::new()
                } else if meta.is_file() {
                    fs::read(&path).unwrap()
                } else {
                    Vec::new()
                };
                found.push((path, meta.modified().unwrap(), bytes));
            }
        }
        found.sort();
        found
    }

    /// Polls `flarebox status --json` until `done` accepts it.
    async fn status_until(&self, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let started = Instant::now();
        loop {
            let status = self.status();
            if done(&status) {
                return status;
            }
            assert!(
                started.elapsed() < WAIT,
                "{what} did not happen; last status: {status:#}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

fn stop(mut daemon: Child) -> std::process::ExitStatus {
    let killed = Command::new("kill")
        .args(["-TERM", &daemon.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let started = Instant::now();
    loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            return status;
        }
        assert!(
            started.elapsed() < WAIT,
            "the daemon did not stop after SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn a_headless_device_pairs_links_and_follows_the_change_stream() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let space_name = format!("agent work {}", uuidv7());
    let space = owner
        .api()
        .create_space(
            &owner,
            &CreateSpaceRequest {
                name: space_name.clone(),
            },
        )
        .await
        .unwrap();
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    let machine = Machine::new();

    let paired = machine.run(&[
        "join",
        &pairing.code,
        "--server",
        &server,
        "--name",
        "agent-vm",
    ]);
    assert!(paired.starts_with("Paired as agent-vm"), "{paired}");
    let credential_file = machine.path("state/flarebox/credential.json");
    assert_eq!(mode(&credential_file), 0o600);
    assert_eq!(mode(credential_file.parent().unwrap()), 0o700);
    let credential: Value =
        serde_json::from_str(&fs::read_to_string(&credential_file).unwrap()).unwrap();
    let refresh = credential["refresh_credential"]
        .as_str()
        .unwrap()
        .to_owned();
    let device_id = credential["device_id"].as_str().unwrap().to_owned();
    let again = machine.fails(&["join", "AAAA-BBBB", "--server", &server, "--name", "again"]);
    assert!(again.contains("already paired"), "{again}");

    fs::create_dir_all(machine.path("config/flarebox")).unwrap();
    fs::write(
        machine.path("config/flarebox/config.toml"),
        "[sync]\ninclude = [\"Projects/\"]\nexclude = [\"*.tmp\"]\n",
    )
    .unwrap();
    let folder = machine.path("Sync");
    fs::create_dir_all(folder.join("Projects")).unwrap();
    fs::write(folder.join("Projects/notes.txt"), b"agent notes").unwrap();
    let folder_arg = folder.to_str().unwrap();
    let linked = machine.run(&["link", folder_arg, "--space", &space_name]);
    assert!(linked.contains(&space.space_id), "{linked}");
    let marker: Value =
        serde_json::from_str(&fs::read_to_string(folder.join(".flarebox/space.json")).unwrap())
            .unwrap();
    assert_eq!(marker["space_id"], space.space_id.as_str());
    let refused = machine.fails(&[
        "link",
        machine.path("state").to_str().unwrap(),
        "--space",
        &space.space_id,
    ]);
    assert!(refused.contains("Flarebox's own files"), "{refused}");

    let status = machine.status();
    assert_eq!(status["running"], false);
    assert_eq!(status["account"]["device_id"], device_id.as_str());
    assert_eq!(status["folders"][0]["state"], "stopped");

    let log_path = machine.path("daemon.log");
    let daemon = machine
        .command(&["daemon", "--log-format", "json"])
        .stdout(Stdio::null())
        .stderr(File::create(&log_path).unwrap())
        .spawn()
        .unwrap();
    let socket = machine.path("run/flarebox/daemon.sock");
    let started = Instant::now();
    while !socket.exists() {
        assert!(
            started.elapsed() < WAIT,
            "the daemon did not open {}",
            socket.display()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(mode(&socket), 0o600);
    let second = machine.fails(&["daemon"]);
    assert!(second.contains("another Flarebox daemon"), "{second}");

    let status = machine
        .status_until("the first sync", |status| {
            let folder = &status["folders"][0];
            folder["state"] == "up_to_date"
                && folder["last_seq"].is_u64()
                && folder["local_entries"].as_u64() >= Some(2)
        })
        .await;
    assert_eq!(status["running"], true);
    let first_seq = status["folders"][0]["last_seq"].as_u64().unwrap();
    let first_nodes = status["folders"][0]["remote_nodes"].as_u64().unwrap();

    let mut config = SessionConfig::new(&space.space_id, "cli integration test", "0", 1);
    config.backoff.initial = Duration::from_millis(50);
    let (session, mut events) = Session::start(owner.clone(), config).unwrap();
    let welcome = loop {
        match tokio::time::timeout(WAIT, events.next())
            .await
            .unwrap()
            .unwrap()
        {
            Event::Connected(welcome) => break welcome,
            Event::Stopped(reason) => panic!("the owner's session stopped: {reason}"),
            _ => {}
        }
    };
    let mkdir = Op {
        op: Some(op::Op::CreateDir(CreateDir {
            node_id: uuidv7(),
            parent_id: space.space_id.clone(),
            name: "From the owner".into(),
        })),
    };
    let committed = session
        .commit(Commit {
            commit_id: uuidv7(),
            base_seq: welcome.head_seq,
            ops: vec![mkdir],
            history_id: welcome.history_id.clone(),
            device_seq: welcome.device_seq + 1,
            ..Default::default()
        })
        .await
        .unwrap();
    session.close().await;
    assert!(committed.seq > first_seq);
    let status = machine
        .status_until("the owner's commit arriving", |status| {
            status["folders"][0]["last_seq"].as_u64() >= Some(committed.seq)
        })
        .await;
    assert_eq!(
        status["folders"][0]["remote_nodes"].as_u64(),
        Some(first_nodes + 1)
    );
    assert_eq!(
        status["folders"][0]["history_id"],
        welcome.history_id.as_str()
    );

    let questions: Value = serde_json::from_str(&machine.run(&["questions", "--json"])).unwrap();
    assert_eq!(questions, Value::Array(vec![]));
    assert!(machine.run(&["questions"]).contains("No questions."));

    let restarted = machine.run(&["restart", "Sync"]);
    assert!(restarted.contains("again"), "{restarted}");
    machine
        .status_until("the restarted folder up to date", |status| {
            status["folders"][0]["state"] == "up_to_date"
        })
        .await;

    let unlinked = machine.run(&["unlink", "Sync"]);
    assert!(unlinked.contains(&space.space_id), "{unlinked}");
    assert_eq!(machine.status()["folders"], Value::Array(vec![]));
    assert!(
        !machine
            .path(&format!("state/flarebox/spaces/{}", space.space_id))
            .exists()
    );
    assert_eq!(
        fs::read(folder.join("Projects/notes.txt")).unwrap(),
        b"agent notes"
    );

    assert!(stop(daemon).success());
    assert!(!socket.exists());

    let log = fs::read_to_string(&log_path).unwrap();
    let lines: Vec<Value> = log
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|_| panic!("not a JSON log line: {line}"))
        })
        .collect();
    let messages: Vec<&str> = lines
        .iter()
        .filter_map(|line| line["fields"]["message"].as_str())
        .collect();
    for expected in [
        "the daemon is running",
        "read the space's tree",
        "unlinked a folder",
        "the daemon stopped",
    ] {
        assert!(
            messages.contains(&expected),
            "{expected:?} missing from the log: {messages:?}"
        );
    }
    assert!(
        !log.contains(&refresh),
        "the log holds the refresh credential"
    );
    assert!(
        !log.contains(&pairing.code),
        "the log holds the pairing code"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn a_device_revokes_a_lost_one_and_stops_syncing_when_another_revokes_it() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let machine = Machine::new();
    let unpaired = machine.fails(&["device", "list"]);
    assert!(unpaired.contains("not set up"), "{unpaired}");
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    machine.run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    let device_id = machine.status()["account"]["device_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let lost = api()
        .pair_complete(&PairCompleteRequest {
            code: pairing.code,
            device_name: "lost laptop".into(),
            platform: "test".into(),
        })
        .await
        .unwrap();

    let listed: Value = serde_json::from_str(&machine.run(&["device", "list", "--json"])).unwrap();
    let entry = |listed: &Value, id: &str| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|device| device["device_id"] == id)
            .unwrap_or_else(|| panic!("{id} is not listed: {listed:#}"))
            .clone()
    };
    assert_eq!(entry(&listed, &device_id)["current"], true);
    assert_eq!(entry(&listed, &lost.device_id)["name"], "lost laptop");
    assert_eq!(entry(&listed, &lost.device_id)["revoked_at_ms"], 0);
    let table = machine.run(&["device", "list"]);
    assert!(table.starts_with("ID "), "{table}");
    assert!(
        table
            .lines()
            .any(|line| line.starts_with(&device_id) && line.ends_with("this device")),
        "{table}"
    );

    let invalid = machine.fails(&["device", "revoke", "../spaces"]);
    assert!(invalid.contains("not a device ID"), "{invalid}");
    let own = machine.fails(&["device", "revoke", &device_id]);
    assert!(own.contains("cannot revoke itself"), "{own}");
    let revoked = machine.run(&["device", "revoke", &lost.device_id]);
    assert!(revoked.starts_with("Revoked device"), "{revoked}");
    let refresh = api()
        .token(&TokenRequest {
            device_id: lost.device_id.clone(),
            refresh_credential: lost.refresh_credential.clone(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(refresh.is_unauthenticated(), "{refresh:?}");
    let listed: Value = serde_json::from_str(&machine.run(&["device", "list", "--json"])).unwrap();
    assert!(entry(&listed, &lost.device_id)["revoked_at_ms"].as_i64() > Some(0));
    let table = machine.run(&["device", "list"]);
    assert!(
        table
            .lines()
            .any(|line| line.starts_with(&lost.device_id) && line.contains("revoked")),
        "{table}"
    );

    let space_name = format!("revocation {}", uuidv7());
    owner
        .api()
        .create_space(
            &owner,
            &CreateSpaceRequest {
                name: space_name.clone(),
            },
        )
        .await
        .unwrap();
    let folder = machine.path("Sync");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("kept.txt"), b"stays on the device").unwrap();
    machine.run(&["link", folder.to_str().unwrap(), "--space", &space_name]);
    let daemon = machine
        .command(&["daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !machine.path("run/flarebox/daemon.sock").exists() {
        assert!(
            started.elapsed() < WAIT,
            "the daemon did not open its socket"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    machine
        .status_until("the first sync", |status| {
            status["running"] == true && status["folders"][0]["state"] == "up_to_date"
        })
        .await;

    owner.api().revoke_device(&owner, &device_id).await.unwrap();
    let status = machine
        .status_until("the folder halting", |status| {
            status["folders"][0]["state"] == "halted"
        })
        .await;
    let problem = status["folders"][0]["problem"].as_str().unwrap();
    assert!(problem.contains("revoked"), "{problem}");
    assert_eq!(
        fs::read(folder.join("kept.txt")).unwrap(),
        b"stays on the device"
    );
    let refused = machine.fails(&["device", "list"]);
    assert!(refused.contains("Pair it again"), "{refused}");
    assert!(stop(daemon).success());
    assert_eq!(
        fs::read(folder.join("kept.txt")).unwrap(),
        b"stays on the device"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn usage_lists_what_each_space_stores() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let machine = Machine::new();
    let unpaired = machine.fails(&["usage"]);
    assert!(unpaired.contains("not set up"), "{unpaired}");
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    machine.run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    let name = format!("usage {}", uuidv7());
    let space = owner
        .api()
        .create_space(&owner, &CreateSpaceRequest { name: name.clone() })
        .await
        .unwrap();

    let listed: Value = serde_json::from_str(&machine.run(&["usage", "--json"])).unwrap();
    let entry = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["space_id"] == space.space_id.as_str())
        .unwrap_or_else(|| panic!("the new space is not listed: {listed:#}"));
    assert_eq!(entry["name"], name.as_str());
    for field in [
        "live_files",
        "live_bytes",
        "trashed_files",
        "trash_bytes",
        "old_versions",
        "old_version_bytes",
        "unreferenced_bytes",
        "total_bytes",
    ] {
        assert_eq!(entry[field], 0, "{field} of a new space: {entry:#}");
    }
    let report = machine.run(&["usage"]);
    assert!(
        report.contains(&format!("{name} ({})\n  Live files ", space.space_id)),
        "{report}"
    );
}

/// A one-chunk file of these bytes.
fn file_content(bytes: &[u8]) -> Content {
    let chunk = ChunkRef {
        hash: chunk_hash(bytes).to_vec(),
        size: bytes.len() as u32,
        encoding: 0,
    };
    flarebox_chunk::content(
        vec![chunk],
        1_700_000_000_000_000_000,
        false,
        &ChunkerConfig::V0,
    )
}

/// Uploads `files` and commits `ops` from the owner's device, as the daemon does not upload yet; returns the
/// commit's seq.
async fn commit_as_owner(
    owner: &Arc<Auth>,
    space_id: &str,
    files: &[&'static [u8]],
    ops: Vec<op::Op>,
) -> u64 {
    let config = SessionConfig::new(space_id, "cli integration test", "0", 1);
    let (session, mut events) = Session::start(owner.clone(), config).unwrap();
    let welcome = loop {
        match tokio::time::timeout(WAIT, events.next())
            .await
            .unwrap()
            .unwrap()
        {
            Event::Connected(welcome) => break welcome,
            Event::Stopped(reason) => panic!("the owner's session stopped: {reason}"),
            _ => {}
        }
    };
    let chunks: Vec<ChunkRef> = files
        .iter()
        .flat_map(|bytes| file_content(bytes).chunks)
        .collect();
    let receipts = Transfer::new(owner.api())
        .upload(&session, &chunks, |chunk: &ChunkRef| {
            let bytes = files
                .iter()
                .find(|bytes| chunk_hash(bytes)[..] == chunk.hash[..])
                .copied()
                .unwrap();
            async move { Ok(Bytes::from_static(bytes)) }
        })
        .await
        .unwrap();
    let committed = session
        .commit(Commit {
            commit_id: uuidv7(),
            base_seq: welcome.head_seq,
            ops: ops.into_iter().map(|op| Op { op: Some(op) }).collect(),
            receipts,
            history_id: welcome.history_id.clone(),
            device_seq: welcome.device_seq + 1,
            ..Default::default()
        })
        .await
        .unwrap();
    session.close().await;
    assert!(
        committed
            .results
            .iter()
            .all(|result| result.code == op_result::Code::Applied as i32),
        "{committed:?}"
    );
    committed.seq
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn trash_and_versions_list_what_the_space_keeps() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let space_name = format!("history {}", uuidv7());
    let space = owner
        .api()
        .create_space(
            &owner,
            &CreateSpaceRequest {
                name: space_name.clone(),
            },
        )
        .await
        .unwrap();
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    let machine = Machine::new();
    machine.run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    let folder = machine.path("Sync");
    fs::create_dir_all(&folder).unwrap();
    let folder_arg = folder.to_str().unwrap();
    machine.run(&["link", folder_arg, "--space", &space_name]);
    let daemon = machine
        .command(&["daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = machine.path("run/flarebox/daemon.sock");
    let started = Instant::now();
    while !socket.exists() {
        assert!(
            started.elapsed() < WAIT,
            "the daemon did not open its socket"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    machine
        .status_until("the first sync", |status| {
            status["folders"][0]["state"] == "up_to_date"
        })
        .await;

    let docs = uuidv7();
    let note = uuidv7();
    let gone = uuidv7();
    commit_as_owner(
        &owner,
        &space.space_id,
        &[b"first", b"deleted later", b"link", b"target text"],
        vec![
            op::Op::CreateDir(CreateDir {
                node_id: docs.clone(),
                parent_id: space.space_id.clone(),
                name: "docs".into(),
            }),
            op::Op::CreateFile(CreateFile {
                node_id: note.clone(),
                parent_id: docs.clone(),
                name: "note.txt".into(),
                content: Some(file_content(b"first")),
            }),
            op::Op::CreateFile(CreateFile {
                node_id: gone.clone(),
                parent_id: docs.clone(),
                name: "gone.txt".into(),
                content: Some(file_content(b"deleted later")),
            }),
            op::Op::CreateFile(CreateFile {
                node_id: uuidv7(),
                parent_id: docs.clone(),
                name: "link.txt".into(),
                content: Some(file_content(b"link")),
            }),
            op::Op::CreateFile(CreateFile {
                node_id: uuidv7(),
                parent_id: docs.clone(),
                name: "target.txt".into(),
                content: Some(file_content(b"target text")),
            }),
        ],
    )
    .await;
    let empty = machine.run(&["trash", folder_arg]);
    assert!(empty.contains("Nothing from"), "{empty}");

    commit_as_owner(
        &owner,
        &space.space_id,
        &[b"second!"],
        vec![
            op::Op::UpdateContent(UpdateContent {
                node_id: note,
                content: Some(file_content(b"second!")),
            }),
            op::Op::Delete(Delete { node_id: gone }),
        ],
    )
    .await;
    let note_path = folder.join("docs/note.txt");
    let note_arg = note_path.to_str().unwrap();
    let versions: Value =
        serde_json::from_str(&machine.run(&["versions", note_arg, "--json"])).unwrap();
    assert_eq!(versions.as_array().unwrap().len(), 2, "{versions:#}");
    assert_eq!(versions[0]["size"], 7, "{versions:#}");
    assert_eq!(versions[1]["size"], 5, "{versions:#}");
    let trash: Value =
        serde_json::from_str(&machine.run(&["trash", folder_arg, "--json"])).unwrap();
    assert_eq!(trash.as_array().unwrap().len(), 1, "{trash:#}");
    assert_eq!(trash[0]["path"], "docs/gone.txt", "{trash:#}");
    assert_eq!(trash[0]["kind"], "file");
    assert_eq!(trash[0]["bytes"], 13);
    let below = machine.run(&["trash", folder.join("docs").to_str().unwrap()]);
    assert!(below.contains("docs/gone.txt"), "{below}");
    let report = machine.run(&["versions", note_arg]);
    assert!(
        report.starts_with("docs/note.txt: 2 versions, newest first\ncurrent"),
        "{report}"
    );
    let deleted = machine.fails(&["versions", folder.join("docs/gone.txt").to_str().unwrap()]);
    assert!(
        deleted.contains("flarebox trash lists deleted ones"),
        "{deleted}"
    );
    let outside = machine.fails(&["trash", machine.path("state").to_str().unwrap()]);
    assert!(outside.contains("not in a linked folder"), "{outside}");
    let link = folder.join("docs/link.txt");
    let started = Instant::now();
    while fs::read(&link).ok().as_deref() != Some(&b"link"[..]) {
        assert!(
            started.elapsed() < WAIT,
            "the daemon did not download docs/link.txt"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(stop(daemon).success());

    let profile = machine.profile();
    fs::remove_file(&link).unwrap();
    let link_arg = link.to_str().unwrap();
    let size_of_link = |what: &str| {
        let versions: Value =
            serde_json::from_str(&machine.run(&["versions", link_arg, "--json"])).unwrap();
        assert_eq!(
            versions.as_array().unwrap().len(),
            1,
            "{what}: {versions:#}"
        );
        versions[0]["size"].clone()
    };
    std::os::unix::fs::symlink("target.txt", &link).unwrap();
    assert_eq!(size_of_link("a link to target.txt"), 4);
    fs::remove_file(&link).unwrap();
    let elsewhere = machine.path("elsewhere.txt");
    fs::write(&elsewhere, b"not synced").unwrap();
    std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
    assert_eq!(size_of_link("a link outside the folder"), 4);
    let stopped = machine.run(&["trash", folder_arg, "--json"]);
    assert!(stopped.contains("docs/gone.txt"), "{stopped}");
    let unlinked = machine.fails(&["trash", machine.path("elsewhere").to_str().unwrap()]);
    assert!(unlinked.contains("not in a linked folder"), "{unlinked}");
    let unlinked = machine.fails(&["versions", elsewhere.to_str().unwrap()]);
    assert!(unlinked.contains("not in a linked folder"), "{unlinked}");
    assert!(
        machine.profile() == profile,
        "the commands changed the profile without a daemon"
    );
}

async fn eventually(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < WAIT, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn restore_brings_back_a_deleted_file_and_an_earlier_version() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let space_name = format!("restore {}", uuidv7());
    let space = owner
        .api()
        .create_space(
            &owner,
            &CreateSpaceRequest {
                name: space_name.clone(),
            },
        )
        .await
        .unwrap();
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    let machine = Machine::new();
    machine.run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    let folder = machine.path("Sync");
    fs::create_dir_all(&folder).unwrap();
    let folder_arg = folder.to_str().unwrap();
    machine.run(&["link", folder_arg, "--space", &space_name]);
    let daemon = machine
        .command(&["daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = machine.path("run/flarebox/daemon.sock");
    eventually("the daemon to open its socket", || socket.exists()).await;
    machine
        .status_until("the first sync", |status| {
            status["folders"][0]["state"] == "up_to_date"
        })
        .await;

    let docs = uuidv7();
    let note = uuidv7();
    let gone = uuidv7();
    commit_as_owner(
        &owner,
        &space.space_id,
        &[b"first", b"deleted later"],
        vec![
            op::Op::CreateDir(CreateDir {
                node_id: docs.clone(),
                parent_id: space.space_id.clone(),
                name: "docs".into(),
            }),
            op::Op::CreateFile(CreateFile {
                node_id: note.clone(),
                parent_id: docs.clone(),
                name: "note.txt".into(),
                content: Some(file_content(b"first")),
            }),
            op::Op::CreateFile(CreateFile {
                node_id: gone.clone(),
                parent_id: docs,
                name: "gone.txt".into(),
                content: Some(file_content(b"deleted later")),
            }),
        ],
    )
    .await;
    let note_path = folder.join("docs/note.txt");
    let gone_path = folder.join("docs/gone.txt");
    eventually("docs/gone.txt to arrive", || {
        fs::read(&gone_path).ok().as_deref() == Some(&b"deleted later"[..])
    })
    .await;
    commit_as_owner(
        &owner,
        &space.space_id,
        &[b"second!"],
        vec![
            op::Op::UpdateContent(UpdateContent {
                node_id: note,
                content: Some(file_content(b"second!")),
            }),
            op::Op::Delete(Delete { node_id: gone }),
        ],
    )
    .await;
    eventually("the edit and the deletion to arrive", || {
        fs::read(&note_path).ok().as_deref() == Some(&b"second!"[..]) && !gone_path.exists()
    })
    .await;

    let note_arg = note_path.to_str().unwrap();
    let versions: Value =
        serde_json::from_str(&machine.run(&["versions", note_arg, "--json"])).unwrap();
    let current = versions[0]["version_id"].as_str().unwrap();
    let first = versions[1]["version_id"].as_str().unwrap();
    let again = machine.fails(&["restore", note_arg, "--version", current]);
    assert!(again.contains("already the current version"), "{again}");
    let asked = machine.run(&["restore", gone_path.to_str().unwrap()]);
    assert!(
        asked.starts_with("Asked to restore docs/gone.txt (restore "),
        "{asked}"
    );
    machine.run(&["restore", note_arg, "--version", first]);
    eventually(
        "the deleted file and the first version to come back",
        || {
            fs::read(&gone_path).ok().as_deref() == Some(&b"deleted later"[..])
                && fs::read(&note_path).ok().as_deref() == Some(&b"first"[..])
        },
    )
    .await;
    let restores = || -> Value {
        serde_json::from_str(&machine.run(&["restores", folder_arg, "--json"])).unwrap()
    };
    eventually("both restores to be reported", || {
        restores()
            .as_array()
            .unwrap()
            .iter()
            .filter(|restore| restore["state"]["type"] == "restored")
            .count()
            == 2
    })
    .await;
    let report = machine.run(&["restores", folder_arg]);
    assert!(report.contains("restored"), "{report}");
    assert!(!report.contains("pending"), "{report}");
    let trash = machine.run(&["trash", folder_arg]);
    assert!(trash.contains("Nothing from"), "{trash}");
    assert!(stop(daemon).success());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker; run cli/scripts/integration-test.sh"]
async fn a_device_creates_a_space_and_links_a_folder_to_it() {
    let owner = Arc::new(Auth::new(api(), owner().await));
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    let machine = Machine::new();
    machine.run(&["join", &pairing.code, "--server", &server, "--name", "desk"]);
    let name = format!("notes {}", uuidv7());

    let created = machine.run(&["space", "create", &format!(" {name} ")]);
    assert!(
        created.starts_with(&format!("Created the space {name} (")),
        "{created}"
    );
    let spaces: Value = serde_json::from_str(&machine.run(&["space", "list", "--json"])).unwrap();
    let ours: Vec<&Value> = spaces
        .as_array()
        .unwrap()
        .iter()
        .filter(|space| space["name"] == name.as_str())
        .collect();
    assert_eq!(ours.len(), 1, "{spaces:#}");
    let space_id = ours[0]["space_id"].as_str().unwrap();
    assert!(created.contains(space_id), "{created}");
    let listed = machine.run(&["space", "list"]);
    assert!(listed.contains(&format!("{space_id}  {name}")), "{listed}");
    let again = machine.fails(&["space", "create", &name]);
    assert!(again.contains("already has that name"), "{again}");
    let blank = machine.fails(&["space", "create", "  "]);
    assert!(blank.contains("a space needs a name"), "{blank}");

    let folder = machine.path("Notes");
    fs::create_dir_all(&folder).unwrap();
    let linked = machine.run(&["link", folder.to_str().unwrap(), "--space", &name]);
    assert!(linked.contains(space_id), "{linked}");
    let owners = owner.api().list_spaces(&owner).await.unwrap().spaces;
    assert!(
        owners.iter().any(|space| space.space_id == space_id),
        "the owner's other device does not see the space"
    );
}
