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

use flarebox_net::{Api, Auth, Credentials, Event, Session, SessionConfig};
use flarebox_proto::v1::{
    ClaimRequest, Commit, CreateDir, CreateSpaceRequest, Op, PairCompleteRequest, TokenRequest, op,
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
        "pair",
        "--server",
        &server,
        "--code",
        &pairing.code,
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
    let again = machine.fails(&[
        "pair",
        "--server",
        &server,
        "--code",
        "AAAA-BBBB",
        "--name",
        "again",
    ]);
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
    assert!(unpaired.contains("not paired"), "{unpaired}");
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    machine.run(&[
        "pair",
        "--server",
        &server,
        "--code",
        &pairing.code,
        "--name",
        "desk",
    ]);
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
    assert!(unpaired.contains("not paired"), "{unpaired}");
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    let server = server_url().to_string();
    machine.run(&[
        "pair",
        "--server",
        &server,
        "--code",
        &pairing.code,
        "--name",
        "desk",
    ]);
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
