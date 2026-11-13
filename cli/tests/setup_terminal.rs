// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn setup(
    input: &str,
    args: &[&str],
    vars: &[(&str, &str)],
    paired: bool,
) -> (tempfile::TempDir, Output) {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let state = home.path().join("state/koidrive");
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir_all(home.path().join("config/koidrive")).unwrap();
    fs::write(
        home.path().join("config/koidrive/config.toml"),
        "[service]\nbackground = false\n",
    )
    .unwrap();
    let listener = UnixListener::bind(state.join("daemon.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (stop, stopped) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && stopped.try_recv().is_err() {
            let Ok((mut stream, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            };
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            if line.is_empty() {
                continue;
            }
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            let response = match request["request"].as_str().unwrap() {
                "pair" if paired => serde_json::json!({"response":"paired", "account":{
                    "server":"https://files.example.com/", "user_id":"owner",
                    "device_id":"0190a0b0-0000-7000-8000-000000000001", "device_name":"Laptop"
                }}),
                "status" => {
                    serde_json::json!({"response":"status", "status":{"account":null,"running":true,"folders":[]}})
                }
                "questions" => serde_json::json!({"response":"questions", "questions":[]}),
                other => panic!("unexpected request {other}"),
            };
            writeln!(stream, "{response}").unwrap();
        }
    });
    let master = rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR).unwrap();
    rustix::pty::grantpt(&master).unwrap();
    rustix::pty::unlockpt(&master).unwrap();
    let name = rustix::pty::ptsname(&master, Vec::new()).unwrap();
    let slave = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(name.to_str().unwrap())
        .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_koi"));
    command
        .args(["setup"])
        .args(args)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("DO_NOT_TRACK")
        .env_remove("KOIDRIVE_TELEMETRY")
        .env_remove("CI")
        .env("KOIDRIVE_TELEMETRY_URL", "http://127.0.0.1:9")
        .stdin(slave.try_clone().unwrap())
        .stderr(slave)
        .stdout(Stdio::piped());
    for (key, value) in vars {
        command.env(key, value);
    }
    let mut child = command.spawn().unwrap();
    let mut terminal = File::from(master);
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut text = String::new();
    reader.read_line(&mut text).unwrap();
    if text.contains("Send anonymous usage reports") {
        terminal.write_all(b"n\n").unwrap();
    }
    terminal.write_all(input.as_bytes()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("setup did not finish");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    reader.read_to_string(&mut text).unwrap();
    let output = Output {
        status: child.wait().unwrap(),
        stdout: text.into_bytes(),
        stderr: Vec::new(),
    };
    stop.send(()).unwrap();
    server.join().unwrap();
    (home, output)
}

#[test]
fn telemetry_is_asked_after_pairing_succeeds() {
    let (home, output) = setup("2\nCODE@files.example.com\nn\n", &[], &[], true);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{text}");
    assert!(
        text.find("This device is set up").unwrap()
            < text.find("Send anonymous usage reports").unwrap(),
        "{text}"
    );
    let settings: serde_json::Value = serde_json::from_slice(
        &fs::read(home.path().join("state/koidrive/telemetry/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(settings["enabled"], false);
}

#[test]
fn failed_setup_does_not_ask_about_telemetry() {
    let (home, output) = setup("2\nnot-a-code\nn\n", &[], &[], false);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!output.status.success(), "{text}");
    assert!(!text.contains("Send anonymous usage reports"), "{text}");
    assert!(!home.path().join("state/koidrive/telemetry").exists());
}

#[test]
fn opting_in_after_setup_enables_future_reports_without_recording_the_setup() {
    let (home, output) = setup("2\nCODE@files.example.com\ny\n", &[], &[], true);
    assert!(output.status.success());
    let settings: serde_json::Value = serde_json::from_slice(
        &fs::read(home.path().join("state/koidrive/telemetry/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(settings["enabled"], true);
    assert!(
        !home
            .path()
            .join("state/koidrive/telemetry/queue.json")
            .exists()
    );
}

#[test]
fn explicit_choice_and_environment_do_not_prompt_after_setup() {
    for (args, vars) in [
        (vec!["--telemetry", "off"], vec![]),
        (vec![], vec![("DO_NOT_TRACK", "1")]),
        (vec![], vec![("KOIDRIVE_TELEMETRY", "0")]),
    ] {
        let (_, output) = setup("2\nCODE@files.example.com\n", &args, &vars, true);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(output.status.success(), "{text}");
        assert!(!text.contains("Send anonymous usage reports"), "{text}");
    }
}
