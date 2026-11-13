// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, Write};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use koidrive_driver::Paths;
use serde_json::{Value, json};

mod schema;

struct Profile {
    home: tempfile::TempDir,
    paths: Paths,
}

impl Profile {
    fn new(config: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::from_vars(|name| match name {
            "HOME" => Some(home.path().into()),
            "XDG_CONFIG_HOME" => Some(home.path().join("config").into()),
            "XDG_STATE_HOME" => Some(home.path().join("state").into()),
            "XDG_RUNTIME_DIR" => Some(home.path().join("run").into()),
            _ => None,
        })
        .unwrap();
        std::fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        std::fs::write(&paths.config_file, config).unwrap();
        Self { home, paths }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_koi"));
        command
            .args(args)
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_STATE_HOME", self.home.path().join("state"))
            .env("XDG_RUNTIME_DIR", self.home.path().join("run"))
            .env("DO_NOT_TRACK", "1")
            .env("KOIDRIVE_TELEMETRY", "0")
            .stdin(Stdio::null());
        command
    }

    fn json(&self, args: &[&str]) -> (Output, Value) {
        let output = self.command(args).output().unwrap();
        let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
        schema::check(&envelope);
        (output, envelope)
    }
}

#[test]
fn reconcile_unavailability_fails_with_its_service_report_but_config_off_succeeds() {
    let profile = Profile::new("not valid toml");
    let (output, envelope) = profile.json(&["_reconcile", "--json"]);
    assert_eq!(output.status.code(), Some(1), "{envelope:#}");
    assert_eq!(envelope["outcome"], "error");
    assert_eq!(envelope["data"]["service"]["state"], "unavailable");
    assert!(envelope["data"]["service"]["problem"].is_string());
    assert_eq!(envelope["error"]["code"], "failed");

    std::fs::write(
        &profile.paths.config_file,
        "[service]\nbackground = false\n",
    )
    .unwrap();
    let (output, envelope) = profile.json(&["_reconcile", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{envelope:#}");
    assert_eq!(envelope["data"]["service"]["state"], "off");
    assert_eq!(envelope["error"], Value::Null);
}

#[cfg(target_os = "macos")]
#[test]
fn reconcile_leaves_a_login_items_disabled_service_off_and_succeeds() {
    use std::os::unix::fs::PermissionsExt;

    let profile = Profile::new("");
    let bin = profile.home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let launchctl = bin.join("launchctl");
    std::fs::write(&launchctl, "#!/bin/sh\ncase \"$1:$2\" in\nprint:gui/*/*) exit 1;;\nprint:gui/*) exit 0;;\nprint-disabled:*) echo '{}';;\n*) exit 1;;\nesac\n").unwrap();
    std::fs::set_permissions(&launchctl, std::fs::Permissions::from_mode(0o755)).unwrap();
    let paths = &profile.paths;
    let store_path = paths.store("0190f0a0-0000-7000-8000-000000000001").unwrap();
    std::fs::create_dir_all(store_path.parent().unwrap()).unwrap();
    let mut store = koidrive_store::Store::open(&store_path).unwrap();
    store
        .transaction(|tx| {
            tx.set_link(&koidrive_store::Link {
                space_id: "0190f0a0-0000-7000-8000-000000000001".into(),
                space_name: "Work".into(),
                root: profile.home.path().join("Work"),
                root_dev: 1,
                server_url: "https://files.example.com/".into(),
                device_id: "d1".into(),
                linked_at_ms: 1,
            })
        })
        .unwrap();
    drop(store);
    let output = profile
        .command(&["status", "--json"])
        .env("PATH", &bin)
        .output()
        .unwrap();
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    let label = data["data"]["service"]["name"].as_str().unwrap();
    let agents = profile.home.path().join("Library/LaunchAgents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join(format!("{label}.plist")),
        "<plist><dict></dict></plist>",
    )
    .unwrap();
    let attempted = bin.join("attempted");
    std::fs::write(&launchctl, format!("#!/bin/sh\ncase \"$1:$2\" in\nprint:gui/*/*) exit 1;;\nprint:gui/*) exit 0;;\nprint-disabled:*) echo '\"{label}\" => true';;\n*) echo attempted > '{}'; exit 1;;\nesac\n", attempted.display())).unwrap();
    let output = profile
        .command(&["_reconcile", "--json"])
        .env("PATH", &bin)
        .output()
        .unwrap();
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    schema::check(&envelope);
    assert_eq!(output.status.code(), Some(0), "{envelope:#}");
    assert_eq!(envelope["data"]["service"]["state"], "switched_off");
    assert_eq!(envelope["error"], Value::Null);
    assert!(!attempted.exists());
}

struct Daemon {
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Daemon {
    fn start(paths: &Paths, status: Value) -> Self {
        use std::os::unix::fs::PermissionsExt;

        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        std::fs::set_permissions(&paths.runtime_dir, std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let listener = std::os::unix::net::UnixListener::bind(paths.socket()).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let thread = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                let mut line = String::new();
                std::io::BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let response = match request["request"].as_str().unwrap() {
                    "status" => json!({"response":"status", "status":status}),
                    "access" => json!({"response":"access", "blocked":[]}),
                    other => panic!("unexpected request {other}"),
                };
                writeln!(stream, "{response}").unwrap();
            }
        });
        Self {
            stopped,
            thread: Some(thread),
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn a_pause_adds_only_optional_status_fields_without_a_new_condition_discriminator() {
    let mut unpaused_data = None;
    for paused in [false, true] {
        let profile = Profile::new("[service]\nbackground = false\n");
        let _daemon = Daemon::start(
            &profile.paths,
            json!({
                "account": {"server":"https://files.example.com/", "user_id":"u1", "device_id":"d1", "device_name":"Mac"},
                "running":true,
                "paused":paused,
                "folders":[],
            }),
        );
        for args in [
            ["status", "--json"].as_slice(),
            ["status", "--check", "--json"].as_slice(),
        ] {
            let (output, envelope) = profile.json(args);
            assert_eq!(output.status.code(), Some(0), "{envelope:#}");
            assert_eq!(envelope["schema_version"], 1);
            assert_eq!(envelope["data"]["conditions"], json!([]));
            let mut data = envelope["data"].clone();
            data["service"].as_object_mut().unwrap().remove("name");
            if paused {
                assert_eq!(
                    data.as_object_mut().unwrap().remove("paused"),
                    Some(json!(true))
                );
                assert_eq!(Some(&data), unpaused_data.as_ref());
                assert!(
                    envelope["next_commands"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|next| next["argv"] == json!(["koi", "resume"]))
                );
            } else {
                assert!(data.get("paused").is_none());
                unpaused_data = Some(data);
            }
        }
    }
}

#[test]
fn a_paused_status_keeps_the_halted_condition_and_its_check_exit() {
    let profile = Profile::new("[service]\nbackground = false\n");
    let _daemon = Daemon::start(
        &profile.paths,
        json!({
            "account": {"server":"https://files.example.com/", "user_id":"u1", "device_id":"d1", "device_name":"Mac"},
            "running":true,
            "paused":true,
            "folders":[{
                "space_id":"0190f0a0-0000-7000-8000-000000000001",
                "space_name":"Work",
                "path":"/Work",
                "state":"halted",
                "history_id":null,
                "last_seq":null,
                "head_seq":null,
                "remote_nodes":0,
                "local_entries":null,
                "questions":0,
                "problem":"the folder was linked by another device",
            }],
        }),
    );
    let (output, envelope) = profile.json(&["status", "--check", "--json"]);
    assert_eq!(output.status.code(), Some(5), "{envelope:#}");
    assert_eq!(envelope["data"]["paused"], true);
    assert_eq!(
        envelope["data"]["conditions"],
        json!([{
            "condition":"halted",
            "folder":"/Work",
            "problem":"the folder was linked by another device",
        }])
    );
}
