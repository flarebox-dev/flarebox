// SPDX-License-Identifier: Apache-2.0

//! The JSON envelope and the exit statuses of commands that need no server: each prints exactly one envelope on
//! stdout, and nothing asks a question without a terminal.

use std::process::{Command, Output, Stdio};

use serde_json::Value;

mod schema;

struct Profile {
    home: tempfile::TempDir,
}

impl Profile {
    fn new() -> Self {
        Profile {
            home: tempfile::tempdir().unwrap(),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let home = self.home.path();
        let mut command = Command::new(env!("CARGO_BIN_EXE_koi"));
        command
            .args(args)
            .current_dir(home)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("XDG_RUNTIME_DIR", home.join("run"))
            .env_remove("KOIDRIVE_LOG")
            .stdin(Stdio::null());
        command
    }

    fn output(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run koi")
    }

    /// The exit status and the one JSON object on stdout.
    fn json(&self, args: &[&str]) -> (i32, Value) {
        let output = self.output(args);
        let envelope: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{args:?} printed no single JSON object ({error}): {}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
        schema::check(&envelope);
        for field in [
            "command",
            "outcome",
            "data",
            "error",
            "decisions",
            "next_commands",
        ] {
            assert!(
                envelope.get(field).is_some(),
                "{field} missing: {envelope:#}"
            );
        }
        (output.status.code().unwrap(), envelope)
    }
}

fn argv(next: &Value) -> Vec<&str> {
    next["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap())
        .collect()
}

#[test]
fn status_reads_a_profile_that_is_not_set_up_and_check_exits_4() {
    let profile = Profile::new();
    let (code, envelope) = profile.json(&["status", "--json"]);
    assert_eq!(code, 0, "{envelope:#}");
    assert_eq!(envelope["command"], "status");
    assert_eq!(envelope["outcome"], "ok");
    assert_eq!(envelope["error"], Value::Null);
    assert_eq!(envelope["data"]["account"], Value::Null);
    assert_eq!(envelope["data"]["folders"], serde_json::json!([]));
    assert_eq!(
        envelope["data"]["conditions"],
        serde_json::json!([{ "condition": "not_set_up" }])
    );
    assert_eq!(argv(&envelope["next_commands"][0]), ["koi", "setup"]);

    let (code, _) = profile.json(&["--json", "status", "--check"]);
    assert_eq!(code, 4);
    assert_eq!(
        profile.output(&["status", "--check"]).status.code(),
        Some(4)
    );
    assert_eq!(profile.output(&["status"]).status.code(), Some(0));
}

#[test]
fn errors_are_envelopes_with_their_code_and_exit_status() {
    let profile = Profile::new();

    let (code, envelope) = profile.json(&["resolve", "q1", "delete", "--json"]);
    assert_eq!(code, 7, "{envelope:#}");
    assert_eq!(envelope["outcome"], "error");
    assert_eq!(envelope["error"]["code"], "stale_decision");
    assert_eq!(envelope["error"]["retryable"], false);
    assert_eq!(
        argv(&envelope["next_commands"][0]),
        ["koi", "resolve", "--json"]
    );

    let (code, envelope) = profile.json(&["space", "list", "--json"]);
    assert_eq!(code, 4, "{envelope:#}");
    assert_eq!(envelope["command"], "space list");
    assert_eq!(envelope["error"]["code"], "auth_required");

    let (code, envelope) = profile.json(&["--json", "folder", "retry", "dir"]);
    assert_eq!(code, 2, "{envelope:#}");
    assert_eq!(envelope["command"], "folder retry");
    assert_eq!(envelope["error"]["code"], "usage");

    let (code, envelope) = profile.json(&["history", "versions", "a.txt", "--json"]);
    assert_eq!(code, 2, "{envelope:#}");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not in a linked folder"),
        "{envelope:#}"
    );

    let (code, envelope) = profile.json(&["status", "--json=2"]);
    assert_eq!(code, 2, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "usage");
}

#[test]
fn unlinking_a_folder_that_is_not_linked_is_unchanged() {
    let profile = Profile::new();
    let (code, envelope) = profile.json(&["folder", "unlink", "Docs", "--json"]);
    assert_eq!(code, 0, "{envelope:#}");
    assert_eq!(envelope["outcome"], "unchanged");
    assert_eq!(envelope["error"], Value::Null);
    let path = (std::fs::canonicalize(profile.home.path()).unwrap())
        .join("Docs")
        .display()
        .to_string();
    let mut data = envelope["data"].clone();
    data.as_object_mut().unwrap().remove("service");
    assert_eq!(data, serde_json::json!({ "path": path }));
    let output = profile.output(&["folder", "unlink", "Docs"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{path} is not linked, so there is nothing to unlink.\n")
    );
}

#[test]
fn a_change_with_a_request_id_that_failed_records_nothing() {
    let profile = Profile::new();
    let requests = profile.home.path().join("state/koidrive/requests");
    for args in [
        &["folder", "restart", "Docs", "--request-id", "r1", "--json"][..],
        &["folder", "restart", "Docs", "--request-id", "r1", "--json"],
        &[
            "folder",
            "retry",
            "Docs",
            "--resend",
            "--request-id",
            "r2",
            "--json",
        ],
    ] {
        let (code, envelope) = profile.json(args);
        assert_eq!(code, 2, "{args:?}: {envelope:#}");
        assert!(
            (envelope["error"]["message"].as_str().unwrap()).contains("is not a linked folder"),
            "{envelope:#}"
        );
        let left: Vec<_> = std::fs::read_dir(&requests)
            .map(|entries| entries.map(|entry| entry.unwrap().file_name()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "{left:?}");
    }
    let (code, envelope) =
        profile.json(&["folder", "restart", "Docs", "--request-id", "a/b", "--json"]);
    assert_eq!(
        (code, &envelope["error"]["code"]),
        (2, &Value::from("usage"))
    );
}

#[test]
fn help_and_the_version_are_envelopes_under_json() {
    let profile = Profile::new();
    let (code, envelope) = profile.json(&["--json"]);
    assert_eq!(code, 0);
    assert_eq!(envelope["command"], "");
    assert!(
        envelope["data"]["help"]
            .as_str()
            .unwrap()
            .contains("Usage: koi"),
        "{envelope:#}"
    );
    let (code, envelope) = profile.json(&["folder", "--json"]);
    assert_eq!((code, &envelope["command"]), (0, &Value::from("folder")));
    let help = envelope["data"]["help"].as_str().unwrap();
    assert!(help.contains("Usage: koi folder"), "{help}");
    assert!(help.contains("retry"), "{help}");
    let (code, envelope) = profile.json(&["history", "--help", "--json"]);
    assert_eq!(code, 0);
    assert!(
        envelope["data"]["help"]
            .as_str()
            .unwrap()
            .contains("requests"),
        "{envelope:#}"
    );
    let (code, envelope) = profile.json(&["help", "--json"]);
    assert_eq!((code, &envelope["command"]), (0, &Value::from("help")));
    assert!(
        (envelope["data"]["help"].as_str().unwrap()).contains("Usage: koi"),
        "{envelope:#}"
    );
    let (code, envelope) = profile.json(&["help", "folder", "--json"]);
    assert_eq!(
        (code, &envelope["command"]),
        (0, &Value::from("help folder"))
    );
    let help = envelope["data"]["help"].as_str().unwrap();
    assert!(help.contains("Usage: koi folder"), "{help}");
    let (code, envelope) = profile.json(&["--json", "help", "folder", "link"]);
    assert_eq!(code, 0, "{envelope:#}");
    assert!(
        (envelope["data"]["help"].as_str().unwrap()).contains("Usage: koi folder link"),
        "{envelope:#}"
    );
    for args in [
        &["help", "--json=2"][..],
        &["help", "folder", "--json=bogus"],
        &["--help", "--json=2"],
        &["folder", "--help", "--json=2"],
        &["--version", "--json=2"],
    ] {
        let (code, envelope) = profile.json(args);
        assert_eq!(code, 2, "{args:?}: {envelope:#}");
        assert_eq!(envelope["error"]["code"], "usage", "{args:?}");
    }
    let (code, envelope) = profile.json(&["help", "nothing", "--json"]);
    assert_eq!(
        (code, &envelope["error"]["code"]),
        (2, &Value::from("usage"))
    );
    let (code, envelope) = profile.json(&["--version", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(envelope["data"]["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn setup_without_a_terminal_lists_the_choices_and_asks_nothing() {
    let profile = Profile::new();
    let output = profile.output(&["setup"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.contains("koi join <CODE>"), "{text}");
    assert!(text.contains("koi server deploy --interactive"), "{text}");

    let (code, envelope) = profile.json(&["setup", "--json"]);
    assert_eq!(code, 0, "{envelope:#}");
    assert_eq!(envelope["data"]["choices"].as_array().unwrap().len(), 5);
    assert_eq!(
        argv(&envelope["next_commands"][1]),
        ["koi", "server", "deploy", "--interactive"]
    );

    let output = profile.output(&["setup", "--interactive"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("needs a terminal"), "{stderr}");
    let (code, envelope) = profile.json(&["setup", "--interactive", "--json"]);
    assert_eq!(code, 2, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "usage");
}

#[test]
fn owner_login_commands_never_log_in_under_json_or_without_their_flags() {
    let profile = Profile::new();
    let (code, envelope) = profile.json(&["server", "deploy", "--json"]);
    assert_eq!(code, 4, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "auth_required");
    assert_eq!(
        argv(&envelope["next_commands"][0]),
        ["koi", "server", "deploy", "--interactive"]
    );
    let target = [
        "server",
        "delete",
        "--bucket",
        "b",
        "--jurisdiction",
        "eu",
        "--yes",
        "--no-browser",
        "--port",
        "8976",
    ];
    let (code, envelope) = profile.json(&[&["--json=1"][..], &target].concat());
    assert_eq!(code, 4, "{envelope:#}");
    assert_eq!(
        argv(&envelope["next_commands"][0]),
        [
            "koi",
            "server",
            "delete",
            "--bucket",
            "b",
            "--jurisdiction",
            "eu",
            "--interactive",
            "--no-browser",
            "--port",
            "8976"
        ]
    );
    let (code, envelope) = profile.json(&["server", "deploy", "--name", "desk", "--json"]);
    assert_eq!(code, 4, "{envelope:#}");
    assert_eq!(
        argv(&envelope["next_commands"][0]),
        ["koi", "server", "deploy", "--name", "desk", "--interactive"]
    );

    let pasted = profile.output(&["server", "add-device", "--no-browser"]);
    assert_eq!(pasted.status.code(), Some(4));
    for args in [
        &["server", "deploy"][..],
        &["server", "upgrade", "--worker", "koidrive", "--yes"],
        &["server", "delete", "--yes"],
        &["server", "add-device", "--interactive"],
    ] {
        let output = profile.output(args);
        assert_eq!(output.status.code(), Some(4), "{args:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("--interactive on a terminal"),
            "{args:?}: {stderr}"
        );
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn a_path_echoed_in_an_error_reaches_stderr_escaped() {
    let profile = Profile::new();
    let typed = profile.home.path().join("\u{1b}]0;owned\u{7}notes\n.txt");
    let output = profile.output(&["history", "versions", typed.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stderr.contains('\u{1b}') && !stderr.contains('\u{7}'),
        "{stderr:?}"
    );
    assert!(
        stderr.contains("\\u{1b}]0;owned\\u{7}notes\\u{a}.txt is not in a linked folder"),
        "{stderr:?}"
    );
    assert_eq!(stderr.lines().count(), 1, "{stderr:?}");

    let output = profile.output(&["folder", "unlink", "x\u{1b}[31m\nFORGED"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
    assert!(!stdout.contains('\u{1b}'), "{stdout:?}");
    assert!(stdout.contains("x\\u{1b}[31m\\u{a}FORGED"), "{stdout:?}");

    for args in [
        &["status", "--bad\nFORGED\u{1b}[2J"][..],
        &["device", "revoke", "x\nFORGED"],
    ] {
        let output = profile.output(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            !stderr.contains('\u{1b}') && !stderr.lines().any(|line| line.starts_with("FORGED")),
            "{args:?}: {stderr}"
        );
        assert!(stderr.contains("\\u{a}FORGED"), "{args:?}: {stderr}");
    }
}

#[test]
fn status_and_resolve_on_an_empty_home_create_nothing() {
    let profile = Profile::new();
    let (code, envelope) = profile.json(&["status", "--json"]);
    assert_eq!(code, 0, "{envelope:#}");
    assert_eq!(envelope["data"]["account"], Value::Null);
    assert_eq!(envelope["data"]["running"], false);
    let (code, envelope) = profile.json(&["resolve", "--json"]);
    assert_eq!(code, 0, "{envelope:#}");
    assert_eq!(envelope["decisions"], serde_json::json!([]));
    let output = profile.output(&["status"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    let created: Vec<_> = std::fs::read_dir(profile.home.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(created.is_empty(), "{created:?}");
}

#[test]
fn ctrl_c_ends_a_command_with_an_interrupted_error_and_exit_130() {
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("http://{}", silent.local_addr().unwrap());
    for json in [true, false] {
        let profile = Profile::new();
        let mut args = vec!["join", "K7QM-2XPD", "--server", &server];
        if json {
            args.push("--json");
        }
        let child = (profile.command(&args))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (_held, _) = silent.accept().expect("the join calls the server");
        let killed = Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        let output = child.wait_with_output().unwrap();

        assert_eq!(output.status.code(), Some(130), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        if json {
            let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
            schema::check(&envelope);
            assert_eq!(envelope["command"], "join", "{envelope:#}");
            assert_eq!(envelope["outcome"], "error", "{envelope:#}");
            assert_eq!(envelope["error"]["code"], "interrupted", "{envelope:#}");
            assert_eq!(envelope["error"]["retryable"], false, "{envelope:#}");
        } else {
            assert!(output.stdout.is_empty(), "{stderr}");
            assert!(stderr.contains("koi: interrupted"), "{stderr}");
        }
    }
}

#[test]
fn ctrl_c_during_setup_under_json_prints_the_interrupted_envelope() {
    let profile = Profile::new();
    let run = profile.home.path().join("run").join("koidrive");
    std::fs::create_dir_all(&run).unwrap();
    let silent = std::os::unix::net::UnixListener::bind(run.join("daemon.sock")).unwrap();
    let child = (profile.command(&["setup", "--json"]))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (_held, _) = silent
        .accept()
        .expect("setup asks the daemon for decisions");
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(130), "{output:?}");
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    schema::check(&envelope);
    assert_eq!(envelope["command"], "setup", "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "interrupted", "{envelope:#}");
}

#[test]
fn a_help_or_version_flag_before_a_subcommand_reports_the_command_it_belongs_to() {
    let profile = Profile::new();
    for (args, command, field) in [
        (&["--version", "space", "list"][..], "", "version"),
        (&["-V", "folder", "link"], "", "version"),
        (&["-Vh", "space", "list"], "", "version"),
        (&["-hV", "space", "list"], "", "help"),
        (&["-h", "space", "list"], "", "help"),
        (&["space", "--help", "list"], "space", "help"),
    ] {
        let (code, envelope) = profile.json(&[&["--json"], args].concat());
        assert_eq!(code, 0, "{args:?} {envelope:#}");
        assert_eq!(envelope["command"], command, "{args:?} {envelope:#}");
        assert!(envelope["data"][field].is_string(), "{args:?} {envelope:#}");
        schema::check(&envelope);
    }
}

#[test]
fn every_command_prints_its_help_as_an_envelope_its_schema_takes() {
    let profile = Profile::new();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/json-schema/v1");
    let mut commands = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let schema: Value =
            serde_json::from_slice(&std::fs::read(entry.unwrap().path()).unwrap()).unwrap();
        if let Some(command) = schema["properties"]["command"]["const"].as_str() {
            commands.push(command.to_owned());
        }
    }
    assert!(commands.len() > 30, "{commands:?}");
    for command in commands {
        let mut args: Vec<&str> = command.split_whitespace().collect();
        args.extend(["--help", "--json"]);
        let (code, envelope) = profile.json(&args);
        assert_eq!(code, 0, "{envelope:#}");
        assert_eq!(envelope["command"], command.as_str(), "{envelope:#}");
        assert!(envelope["data"]["help"].is_string(), "{envelope:#}");
    }
}
