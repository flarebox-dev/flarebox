// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn setup(
    input: &str,
    args: &[&str],
    vars: &[(&str, &str)],
    paired: bool,
) -> (tempfile::TempDir, Output) {
    session(input, args, vars, paired, false, None)
}

fn session(
    input: &str,
    args: &[&str],
    vars: &[(&str, &str)],
    paired: bool,
    rich: bool,
    interrupt: Option<(usize, rustix::process::Signal)>,
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
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
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
    let mut arguments = vec!["setup", "--interactive", "--no-browser"];
    arguments.extend(args);
    let output = terminal_run(home.path(), input, &arguments, vars, rich, interrupt, false);
    stop.send(()).unwrap();
    server.join().unwrap();
    (home, output)
}

fn terminal_run(
    home: &Path,
    input: &str,
    args: &[&str],
    vars: &[(&str, &str)],
    rich: bool,
    interrupt: Option<(usize, rustix::process::Signal)>,
    queued_enter: bool,
) -> Output {
    let master = rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR).unwrap();
    rustix::pty::grantpt(&master).unwrap();
    rustix::pty::unlockpt(&master).unwrap();
    let name = rustix::pty::ptsname(&master, Vec::new()).unwrap();
    let slave = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(name.to_str().unwrap())
        .unwrap();
    let flags = rustix::fs::fcntl_getfl(&master).unwrap();
    rustix::fs::fcntl_setfl(&master, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    rustix::termios::tcsetwinsize(
        &master,
        rustix::termios::Winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    let original = rustix::termios::tcgetattr(&slave).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_koi"));
    command
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("DO_NOT_TRACK")
        .env_remove("KOIDRIVE_TELEMETRY")
        .env_remove("CI")
        .env_remove("KOI_PLAIN")
        .env("KOIDRIVE_TELEMETRY_URL", "http://127.0.0.1:9")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave)
        .env("TERM", "xterm-256color");
    if !rich {
        command.arg("--plain");
    }
    for (key, value) in vars {
        command.env(key, value);
    }
    let mut terminal = File::from(master);
    if queued_enter {
        terminal.write_all(b"\r").unwrap();
    }
    let mut child = command.spawn().unwrap();
    let mut text = String::new();
    let answers: Vec<&str> = input.lines().collect();
    let prompts = if args.first() == Some(&"status") {
        vec![if rich {
            "Send reports?"
        } else {
            "Send reports? [Y/n] "
        }]
    } else if rich {
        vec![
            "How do you want to set up Koidrive on this machine?",
            "Pairing code",
            "Send reports?",
        ]
    } else {
        vec!["Choose 1 to 2: ", "Pairing code: ", "Send reports? [Y/n] "]
    };
    let mut answered = 0;
    let mut signalled = false;
    let mut helpers = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut bytes = [0; 4096];
        match terminal.read(&mut bytes) {
            Ok(0) => break,
            Ok(count) => {
                text.push_str(&String::from_utf8_lossy(&bytes[..count]));
                let visible = console::strip_ansi_codes(&text);
                if !signalled && answered < prompts.len() && visible.contains(prompts[answered]) {
                    if queued_enter {
                        std::thread::sleep(Duration::from_millis(250));
                        let state = home.join("state/koidrive/telemetry");
                        assert!(!state.join("settings.json").exists(), "{text}");
                        assert!(state.join("prompt.json").exists());
                    }
                    if let Some((stage, signal)) = interrupt.filter(|(stage, _)| *stage == answered)
                    {
                        let children = Command::new("pgrep")
                            .args(["-P", &child.id().to_string()])
                            .output()
                            .unwrap();
                        helpers = String::from_utf8(children.stdout)
                            .unwrap()
                            .lines()
                            .map(|pid| {
                                rustix::process::Pid::from_raw(pid.trim().parse().unwrap()).unwrap()
                            })
                            .collect();
                        assert!(!helpers.is_empty(), "no helper at prompt {stage}: {text}");
                        rustix::process::kill_process(
                            rustix::process::Pid::from_child(&child),
                            signal,
                        )
                        .unwrap();
                        signalled = true;
                    } else if let Some(answer) = answers.get(answered) {
                        let answer = if rich && prompts[answered] == "Send reports?" {
                            answer.to_string()
                        } else {
                            format!("{answer}\n")
                        };
                        terminal.write_all(answer.as_bytes()).unwrap();
                        answered += 1;
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) if error.raw_os_error() == Some(5) => break,
            Err(error) => panic!("terminal read failed: {error}"),
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("setup did not finish: {text}");
        }
    }
    let output = Output {
        status: child.wait().unwrap(),
        stdout: text.into_bytes(),
        stderr: Vec::new(),
    };
    let restored = rustix::termios::tcgetattr(&terminal).unwrap();
    let restored_modes = restored.local_modes;
    let original_modes = original.local_modes;
    #[cfg(target_os = "macos")]
    let (restored_modes, original_modes) = (
        restored_modes & !rustix::termios::LocalModes::PENDIN,
        original_modes & !rustix::termios::LocalModes::PENDIN,
    );
    assert_eq!(restored_modes, original_modes);
    assert_eq!(restored.input_modes, original.input_modes);
    assert_eq!(restored.output_modes, original.output_modes);
    for helper in helpers {
        assert!(
            rustix::process::test_kill_process(helper).is_err(),
            "prompt helper {helper} survived cancellation"
        );
    }
    output
}

#[test]
fn rich_setup_masks_pairing_input() {
    let (_, output) = session(
        "\u{1b}[B\nCODE@files.example.com\nn",
        &[],
        &[],
        true,
        true,
        None,
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{text}");
    assert!(text.contains("This device is set up"), "{text}");
    assert!(!text.contains("CODE@files.example.com"), "{text}");
}

#[test]
fn plain_setup_has_no_terminal_escapes_or_pairing_code_echo() {
    let (_, output) = setup("2\nCODE@files.example.com\nn", &[], &[], true);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{text}");
    assert!(!text.contains('\u{1b}'), "{text}");
    assert!(!text.contains("CODE@files.example.com"), "{text}");
}

#[test]
fn signals_reap_helpers_and_restore_each_prompt() {
    for rich in [false, true] {
        for signal in [rustix::process::Signal::INT, rustix::process::Signal::TERM] {
            for stage in 0..3 {
                let input = if rich {
                    "\u{1b}[B\nCODE@files.example.com\nn"
                } else {
                    "2\nCODE@files.example.com\nn"
                };
                let (_, output) = session(input, &[], &[], true, rich, Some((stage, signal)));
                assert_eq!(
                    output.status.code(),
                    Some(if signal == rustix::process::Signal::INT {
                        130
                    } else {
                        143
                    }),
                    "{}",
                    String::from_utf8_lossy(&output.stdout)
                );
            }
        }
    }
}

#[test]
fn raw_ctrl_c_cancels_rich_selection() {
    let (home, output) = session("\u{3}", &[], &[], false, true, None);
    assert_eq!(
        output.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!home.path().join("state/koidrive/telemetry").exists());
}

#[test]
fn eof_closes_each_plain_and_rich_prompt_without_granting_consent() {
    for rich in [false, true] {
        for stage in 0..3 {
            let route = if rich { "\u{1b}[B" } else { "2" };
            let input = match stage {
                0 => "\u{4}".to_string(),
                1 => format!("{route}\n\u{4}"),
                _ => format!("{route}\nCODE@files.example.com\n\u{4}"),
            };
            let (home, output) = session(&input, &[], &[], true, rich, None);
            let text = String::from_utf8(output.stdout).unwrap();
            assert_eq!(
                output.status.code(),
                Some(if stage == 2 { 0 } else { 1 }),
                "{text}"
            );
            assert_eq!(text.contains("This device is set up"), stage == 2, "{text}");
            let state = home.path().join("state/koidrive/telemetry");
            assert!(!state.join("settings.json").exists());
            assert_eq!(state.join("prompt.json").exists(), stage == 2);
        }
    }
}

#[test]
fn eof_at_the_final_question_keeps_setup_successful() {
    let (home, output) = setup("2\nCODE@files.example.com\n\u{4}", &[], &[], true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        home.path()
            .join("state/koidrive/telemetry/prompt.json")
            .exists()
    );
    assert!(
        !home
            .path()
            .join("state/koidrive/telemetry/settings.json")
            .exists()
    );
}

#[test]
fn raw_ctrl_c_cancels_rich_pairing_and_telemetry() {
    for input in ["\u{1b}[B\n\u{3}", "\u{1b}[B\nCODE@files.example.com\n\u{3}"] {
        let (home, output) = session(input, &[], &[], true, true, None);
        assert_eq!(
            output.status.code(),
            Some(130),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            !home
                .path()
                .join("state/koidrive/telemetry/settings.json")
                .exists()
        );
    }
}

#[test]
fn no_color_overrides_forced_color_without_removing_rich_choices() {
    let (_, output) = session(
        "\u{1b}[B\nCODE@files.example.com\nn",
        &[],
        &[
            ("NO_COLOR", "1"),
            ("CLICOLOR_FORCE", "1"),
            ("FORCE_COLOR", "1"),
        ],
        true,
        true,
        None,
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{text}");
    let sgr = text
        .split("\u{1b}[")
        .skip(1)
        .any(|part| part.chars().find(|c| ('@'..='~').contains(c)) == Some('m'));
    assert!(!sgr, "{text}");
}

#[test]
fn cancelled_reporting_returns_after_the_next_successful_interactive_command() {
    for rich in [false, true] {
        let input = if rich {
            "\u{1b}[B\nCODE@files.example.com\n\u{3}"
        } else {
            "2\nCODE@files.example.com\n\u{4}"
        };
        let (home, output) = session(input, &[], &[], true, rich, None);
        assert_eq!(output.status.code(), Some(if rich { 130 } else { 0 }));
        let state = home.path().join("state/koidrive/telemetry");
        assert!(state.join("prompt.json").exists());
        assert!(!state.join("settings.json").exists());
        for args in [vec!["status", "--json"], vec!["status"]] {
            let output = Command::new(env!("CARGO_BIN_EXE_koi"))
                .args(args)
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", home.path().join("config"))
                .env("XDG_STATE_HOME", home.path().join("state"))
                .env_remove("XDG_RUNTIME_DIR")
                .env("CI", "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(!String::from_utf8_lossy(&output.stderr).contains("Send reports?"));
            assert!(state.join("prompt.json").exists());
        }
        let output = terminal_run(home.path(), "n", &["status"], &[], rich, None, false);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(output.status.success(), "{text}");
        assert!(text.contains("Send reports?"), "{text}");
        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(state.join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["enabled"], false);
        assert!(settings["install_id"].is_null());
        assert!(!state.join("prompt.json").exists());
        let output = terminal_run(home.path(), "", &["status"], &[], rich, None, false);
        assert!(output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("Send reports?"));
    }
}

#[test]
fn rich_questions_use_the_terminal_color_policy_with_a_private_answer_pipe() {
    let (_, output) = session(
        "\u{1b}[B\nCODE@files.example.com\nn",
        &[],
        &[("NO_COLOR", ""), ("CLICOLOR", "1"), ("CLICOLOR_FORCE", "0")],
        true,
        true,
        None,
    );
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let question = text
        .find("How do you want to set up Koidrive on this machine?")
        .unwrap();
    let pair = text.find("On your connected computer").unwrap();
    assert!(text[question..pair].contains("\u{1b}[32m"), "{text}");
}

#[test]
fn failed_status_check_preserves_its_exit_and_defers_consent() {
    for rich in [true, false] {
        let (home, _) = setup("2\nCODE@files.example.com\n\u{4}", &[], &[], true);
        let output = terminal_run(
            home.path(),
            if rich { "\u{3}" } else { "n" },
            &["status", "--check"],
            &[],
            rich,
            None,
            false,
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(4), "{text}");
        assert!(!text.contains("Send reports?"), "{text}");
        let state = home.path().join("state/koidrive/telemetry");
        assert!(state.join("prompt.json").exists());
        assert!(!state.join("settings.json").exists());
    }
}

#[test]
fn queued_enter_cannot_answer_deferred_consent() {
    for rich in [true, false] {
        let (home, _) = setup("2\nCODE@files.example.com\n\u{4}", &[], &[], true);
        let output = terminal_run(home.path(), "\u{4}", &["status"], &[], rich, None, true);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(output.status.success(), "{text}");
        let state = home.path().join("state/koidrive/telemetry");
        assert!(state.join("prompt.json").exists());
        assert!(!state.join("settings.json").exists());
    }
}

#[test]
fn no_color_applies_to_warnings_after_a_cancelled_question() {
    let (home, _) = setup("2\nCODE@files.example.com\n\u{4}", &[], &[], true);
    fs::write(
        home.path().join("state/koidrive/telemetry/prompt.json"),
        "invalid",
    )
    .unwrap();
    let output = terminal_run(
        home.path(),
        "",
        &["status"],
        &[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")],
        true,
        None,
        false,
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{text}");
    assert!(
        text.contains("the unanswered telemetry question cannot be read"),
        "{text}"
    );
    assert!(!text.contains('\u{1b}'), "{text}");
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
