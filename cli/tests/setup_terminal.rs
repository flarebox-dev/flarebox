// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use koidrive_driver::{Credential, FolderState, FolderStatus, Paths};
use koidrive_ipc::{Code, Response};
use koidrive_proto::v1::{AccessToken, CreateSpaceRequest, SpaceInfo, SpaceList};
use prost::Message;

mod schema;

const ROUTE: &str = "How do you want to set up Koidrive on this machine?";
const CODE: &str = "Pairing code";
const SPACE: &str = "Which shared folder should sync here?";
const NAME: &str = "Name of the new shared folder";
const PATH: &str = "Press Enter to use this folder, or type another path";
const TELEMETRY: &str = "Send reports?";

struct Step {
    wait: Vec<&'static str>,
    input: String,
    before_answer: Option<Box<dyn Fn()>>,
}

fn step(wait: &'static str, input: impl Into<String>) -> Step {
    Step {
        wait: vec![wait],
        input: input.into(),
        before_answer: None,
    }
}

fn after(wait: &[&'static str], input: impl Into<String>) -> Step {
    Step {
        wait: wait.to_vec(),
        input: input.into(),
        before_answer: None,
    }
}

fn line(text: &str) -> String {
    format!("{text}\n")
}

fn choose(rich: bool, index: usize) -> String {
    match rich {
        true => format!("{}\n", "\u{1b}[B".repeat(index)),
        false => format!("{}\n", index + 1),
    }
}

fn yes(rich: bool, answer: bool) -> String {
    match (rich, answer) {
        (true, true) => "y".into(),
        (true, false) => "n".into(),
        (false, true) => "y\n".into(),
        (false, false) => "n\n".into(),
    }
}

fn pairing(rich: bool) -> Vec<Step> {
    vec![
        step(ROUTE, choose(rich, 1)),
        step(CODE, line("CODE@files.example.com")),
    ]
}

fn default_folder(rich: bool) -> Vec<Step> {
    vec![
        step(PATH, "\n"),
        step("and its shared folder named Koidrive?", yes(rich, true)),
    ]
}

fn full(rich: bool, telemetry: &str) -> Vec<Step> {
    let mut steps = pairing(rich);
    steps.extend(default_folder(rich));
    steps.push(step(TELEMETRY, telemetry));
    steps
}

fn no(rich: bool) -> String {
    yes(rich, false)
}

struct Home {
    dir: tempfile::TempDir,
    server: String,
    spaces: Arc<Mutex<Vec<SpaceInfo>>>,
    linked: Arc<Mutex<Vec<FolderStatus>>>,
    requests: Arc<Mutex<Vec<String>>>,
    lose_create: Arc<AtomicBool>,
    request_hook: RequestHook,
}

type RequestHook = Arc<Mutex<Option<Box<dyn FnMut(&str) + Send>>>>;

impl Home {
    fn new(spaces: &[&str]) -> Home {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let state = dir.path().join("state/koidrive");
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(dir.path().join("config/koidrive")).unwrap();
        fs::write(
            dir.path().join("config/koidrive/config.toml"),
            "[service]\nbackground = false\n",
        )
        .unwrap();
        let spaces = (spaces.iter().enumerate())
            .map(|(index, name)| space(index, name))
            .collect();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let home = Home {
            server: format!("http://{}/", listener.local_addr().unwrap()),
            dir,
            spaces: Arc::new(Mutex::new(spaces)),
            linked: Arc::new(Mutex::new(Vec::new())),
            requests: Arc::new(Mutex::new(Vec::new())),
            lose_create: Arc::new(AtomicBool::new(false)),
            request_hook: Arc::new(Mutex::new(None)),
        };
        let (spaces, requests, lose) = (
            home.spaces.clone(),
            home.requests.clone(),
            home.lose_create.clone(),
        );
        let hook = home.request_hook.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                serve(stream.unwrap(), &spaces, &requests, &lose, &hook);
            }
        });
        home
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn paths(&self) -> Paths {
        let home = self.path().to_owned();
        Paths::from_vars(|name| match name {
            "HOME" => Some(OsString::from(&home)),
            "XDG_CONFIG_HOME" => Some(home.join("config").into()),
            "XDG_STATE_HOME" => Some(home.join("state").into()),
            _ => None,
        })
        .unwrap()
    }

    fn connect(&self) {
        credential(&self.server)
            .save_new(&self.paths().credential())
            .unwrap();
    }

    fn connected(&self) -> bool {
        self.paths().credential().exists()
    }

    fn disconnect(&self) {
        fs::remove_file(self.paths().credential()).unwrap();
    }

    fn link(&self, folder: &Path, space_id: &str) {
        let space = (self.spaces.lock().unwrap().iter())
            .find(|space| space.space_id == space_id)
            .cloned()
            .unwrap();
        self.linked
            .lock()
            .unwrap()
            .push(folder_status(folder, &space));
    }

    fn space_id(&self, name: &str) -> String {
        (self.spaces.lock().unwrap().iter())
            .find(|space| space.name == name)
            .unwrap()
            .space_id
            .clone()
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn links(&self) -> Vec<String> {
        (self.requests().into_iter())
            .filter(|request| request.starts_with("link "))
            .collect()
    }

    fn daemon(&self, paired: bool) -> Daemon {
        let socket = self.path().join("state/koidrive/daemon.sock");
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (stop, stopped) = std::sync::mpsc::channel();
        let (paths, server) = (self.paths(), self.server.clone());
        let (spaces, linked, requests) = (
            self.spaces.clone(),
            self.linked.clone(),
            self.requests.clone(),
        );
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
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
                let kind = request["request"].as_str().unwrap();
                let response = match kind {
                    "pair" if paired => {
                        requests.lock().unwrap().push("pair".into());
                        let _ = credential(&server).save_new(&paths.credential());
                        serde_json::json!({"response":"paired", "account":{
                            "server":"https://files.example.com/", "user_id":"owner",
                            "device_id":"0190a0b0-0000-7000-8000-000000000001", "device_name":"Laptop"
                        }})
                    }
                    "status" => {
                        let folders = linked.lock().unwrap().clone();
                        serde_json::json!({"response":"status", "status":{"account":null,"running":true,"folders":folders}})
                    }
                    "questions" => serde_json::json!({"response":"questions", "questions":[]}),
                    "link" | "link_approved" => {
                        let folder = request["folder"].as_str().unwrap();
                        let wanted = request["space"].as_str().unwrap();
                        requests
                            .lock()
                            .unwrap()
                            .push(format!("link {folder} {wanted}"));
                        let space = (spaces.lock().unwrap().iter())
                            .find(|space| space.space_id == wanted || space.name == wanted)
                            .cloned();
                        let response = match space {
                            Some(space) => {
                                let status = folder_status(Path::new(folder), &space);
                                let mut linked = linked.lock().unwrap();
                                match linked.contains(&status) {
                                    true => Response::AlreadyLinked { folder: status },
                                    false => {
                                        linked.push(status.clone());
                                        Response::Linked { folder: status }
                                    }
                                }
                            }
                            None => Response::Error {
                                message: format!("no space is named {wanted}"),
                                code: Code::Usage,
                                next: Vec::new(),
                            },
                        };
                        serde_json::to_value(response).unwrap()
                    }
                    other => panic!("unexpected request {other}"),
                };
                writeln!(stream, "{response}").unwrap();
            }
        });
        Daemon {
            stop,
            thread: Some(thread),
        }
    }

    fn setup(
        &self,
        steps: &[Step],
        args: &[&str],
        vars: &[(&str, &str)],
        paired: bool,
        rich: bool,
        interrupt: Option<(usize, rustix::process::Signal)>,
    ) -> Output {
        let daemon = self.daemon(paired);
        let mut arguments = vec!["setup", "--interactive", "--no-browser"];
        arguments.extend(args);
        let output = terminal_run(self.path(), steps, &arguments, vars, rich, interrupt, false);
        daemon.finish();
        output
    }

    fn scripted(&self, stdin: &str, args: &[&str]) -> (i32, serde_json::Value) {
        self.script(stdin, args, true)
    }

    fn script(&self, stdin: &str, args: &[&str], daemon: bool) -> (i32, serde_json::Value) {
        let daemon = daemon.then(|| self.daemon(true));
        let mut child = koi(self.path())
            .arg("setup")
            .args(args)
            .arg("--json")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
        let output = child.wait_with_output().unwrap();
        if let Some(daemon) = daemon {
            daemon.finish();
        }
        let envelope: serde_json::Value =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
                panic!(
                    "{args:?} printed no JSON envelope ({error}): {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
            });
        schema::check(&envelope);
        (output.status.code().unwrap(), envelope)
    }
}

struct Daemon {
    stop: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Daemon {
    fn finish(mut self) {
        let _ = self.stop.send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

fn credential(server: &str) -> Credential {
    Credential {
        server: server.into(),
        user_id: "owner".into(),
        user_name: None,
        device_id: "0190a0b0-0000-7000-8000-000000000001".into(),
        device_name: "Laptop".into(),
        refresh_credential: "secret".into(),
    }
}

fn space(index: usize, name: &str) -> SpaceInfo {
    SpaceInfo {
        space_id: format!("0190a0b0-0000-7000-8000-0000000{:05}", index + 10),
        name: name.into(),
        ..Default::default()
    }
}

fn folder_status(folder: &Path, space: &SpaceInfo) -> FolderStatus {
    FolderStatus {
        space_id: space.space_id.clone(),
        space_name: space.name.clone(),
        path: folder.display().to_string(),
        state: FolderState::Connecting,
        history_id: None,
        last_seq: None,
        head_seq: None,
        remote_nodes: 0,
        local_entries: None,
        questions: 0,
        problem: None,
        wait: None,
    }
}

fn serve(
    mut stream: TcpStream,
    spaces: &Mutex<Vec<SpaceInfo>>,
    requests: &Mutex<Vec<String>>,
    lose_create: &AtomicBool,
    hook: &RequestHook,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let mut words = first.split(' ');
    let (method, path) = (words.next().unwrap(), words.next().unwrap());
    requests.lock().unwrap().push(format!("{method} {path}"));
    if let Some(hook) = hook.lock().unwrap().as_mut() {
        hook(&format!("{method} {path}"));
    }
    let answer = match (method, path) {
        ("POST", "/v1/token") => AccessToken {
            token: "token".into(),
            expires_at_ms: i64::MAX,
            ..Default::default()
        }
        .encode_to_vec(),
        ("GET", "/v1/spaces") => SpaceList {
            spaces: spaces.lock().unwrap().clone(),
            ..Default::default()
        }
        .encode_to_vec(),
        ("POST", "/v1/spaces") => {
            let request = CreateSpaceRequest::decode(body.as_slice()).unwrap();
            let mut spaces = spaces.lock().unwrap();
            let created = space(spaces.len(), &request.name);
            spaces.push(created.clone());
            if lose_create.swap(false, Ordering::Relaxed) {
                return;
            }
            created.encode_to_vec()
        }
        other => panic!("unexpected server request {other:?}"),
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        answer.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(&answer).unwrap();
}

fn setup(steps: &[Step], args: &[&str], vars: &[(&str, &str)], paired: bool) -> (Home, Output) {
    session(steps, args, vars, paired, false, None)
}

fn session(
    steps: &[Step],
    args: &[&str],
    vars: &[(&str, &str)],
    paired: bool,
    rich: bool,
    interrupt: Option<(usize, rustix::process::Signal)>,
) -> (Home, Output) {
    let home = Home::new(&[]);
    let output = home.setup(steps, args, vars, paired, rich, interrupt);
    (home, output)
}

fn koi(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_koi"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("DO_NOT_TRACK")
        .env_remove("KOIDRIVE_TELEMETRY")
        .env_remove("CI")
        .env_remove("KOI_PLAIN")
        .env("KOIDRIVE_TELEMETRY_URL", "http://127.0.0.1:9");
    command
}

fn terminal_run(
    home: &Path,
    steps: &[Step],
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
    let mut command = koi(home);
    command
        .args(args)
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
    let mut raw = Vec::new();
    let mut answered = 0;
    let mut seen = 0;
    let mut signalled = false;
    let mut helpers = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut bytes = [0; 4096];
        match terminal.read(&mut bytes) {
            Ok(0) => break,
            Ok(count) => {
                raw.extend_from_slice(&bytes[..count]);
                text = String::from_utf8_lossy(&raw).into_owned();
                let visible = console::strip_ansi_codes(&text);
                let reached = (!signalled)
                    .then(|| steps.get(answered))
                    .flatten()
                    .and_then(|step| {
                        step.wait.iter().try_fold(seen, |from, wait| {
                            let at = visible.get(from..)?.find(wait)?;
                            Some(from + at + wait.len())
                        })
                    });
                if let Some(end) = reached {
                    seen = end;
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
                    } else {
                        if let Some(before_answer) = &steps[answered].before_answer {
                            before_answer();
                        }
                        terminal
                            .write_all(steps[answered].input.as_bytes())
                            .unwrap();
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
        stdout: raw,
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

fn shown(output: Output) -> String {
    console::strip_ansi_codes(&String::from_utf8(output.stdout).unwrap()).into_owned()
}

#[test]
fn rich_setup_masks_pairing_input() {
    let (_, output) = session(&full(true, "n"), &[], &[], true, true, None);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{text}");
    assert!(text.contains("This device is set up"), "{text}");
    assert!(!text.contains("CODE@files.example.com"), "{text}");
}

#[test]
fn plain_setup_has_no_terminal_escapes_or_pairing_code_echo() {
    let (_, output) = setup(&full(false, "n\n"), &[], &[], true);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{text}");
    assert!(!text.contains('\u{1b}'), "{text}");
    assert!(!text.contains("CODE@files.example.com"), "{text}");
}

#[test]
fn signals_reap_helpers_and_restore_each_prompt() {
    for rich in [false, true] {
        for signal in [rustix::process::Signal::INT, rustix::process::Signal::TERM] {
            for stage in 0..5 {
                let steps = full(rich, if rich { "n" } else { "n\n" });
                let (home, output) = session(&steps, &[], &[], true, rich, Some((stage, signal)));
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
                assert_eq!(home.connected(), stage >= 2);
                assert_eq!(home.links().len(), usize::from(stage == 4));
            }
        }
    }
}

#[test]
fn raw_ctrl_c_cancels_rich_selection() {
    let (home, output) = session(&[step(ROUTE, "\u{3}")], &[], &[], false, true, None);
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
        for stage in 0..5 {
            let mut steps = full(rich, "");
            steps.truncate(stage + 1);
            steps[stage].input = "\u{4}".into();
            let (home, output) = session(&steps, &[], &[], true, rich, None);
            let text = String::from_utf8(output.stdout).unwrap();
            assert_eq!(
                output.status.code(),
                Some(if stage == 4 { 0 } else { 1 }),
                "{text}"
            );
            assert_eq!(text.contains("This device is set up"), stage >= 2, "{text}");
            assert_eq!(home.connected(), stage >= 2);
            assert_eq!(home.links().len(), usize::from(stage == 4));
            let state = home.path().join("state/koidrive/telemetry");
            assert!(!state.join("settings.json").exists());
            assert_eq!(state.join("prompt.json").exists(), stage == 4);
        }
    }
}

#[test]
fn eof_at_the_final_question_keeps_setup_successful() {
    let (home, output) = setup(&full(false, "\u{4}"), &[], &[], true);
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
    let mut code = pairing(true);
    code[1].input = "\u{3}".into();
    for steps in [code, full(true, "\u{3}")] {
        let (home, output) = session(&steps, &[], &[], true, true, None);
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
        &full(true, "n"),
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
        let steps = full(rich, if rich { "\u{3}" } else { "\u{4}" });
        let (home, output) = session(&steps, &[], &[], true, rich, None);
        assert_eq!(output.status.code(), Some(if rich { 130 } else { 0 }));
        home.disconnect();
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
        let output = terminal_run(
            home.path(),
            &[step(TELEMETRY, yes(rich, false))],
            &["status"],
            &[],
            rich,
            None,
            false,
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(output.status.success(), "{text}");
        assert!(text.contains("Send reports?"), "{text}");
        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(state.join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["enabled"], false);
        assert!(settings["install_id"].is_null());
        assert!(!state.join("prompt.json").exists());
        let output = terminal_run(home.path(), &[], &["status"], &[], rich, None, false);
        assert!(output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("Send reports?"));
    }
}

#[test]
fn rich_questions_use_the_terminal_color_policy_with_a_private_answer_pipe() {
    let (_, output) = session(
        &full(true, "n"),
        &[],
        &[("NO_COLOR", ""), ("CLICOLOR", "1"), ("CLICOLOR_FORCE", "0")],
        true,
        true,
        None,
    );
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let question = text.find(ROUTE).unwrap();
    let pair = text.find("On your connected computer").unwrap();
    assert!(text[question..pair].contains("\u{1b}[32m"), "{text}");
}

#[test]
fn failed_status_check_preserves_its_exit_and_defers_consent() {
    for rich in [true, false] {
        let (home, _) = setup(&full(false, "\u{4}"), &[], &[], true);
        home.disconnect();
        let output = terminal_run(
            home.path(),
            &[step(TELEMETRY, if rich { "\u{3}" } else { "n\n" })],
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
        let (home, _) = setup(&full(false, "\u{4}"), &[], &[], true);
        home.disconnect();
        let output = terminal_run(
            home.path(),
            &[step(TELEMETRY, "\u{4}")],
            &["status"],
            &[],
            rich,
            None,
            true,
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(output.status.success(), "{text}");
        let state = home.path().join("state/koidrive/telemetry");
        assert!(state.join("prompt.json").exists());
        assert!(!state.join("settings.json").exists());
    }
}

#[test]
fn no_color_applies_to_warnings_after_a_cancelled_question() {
    let (home, _) = setup(&full(false, "\u{4}"), &[], &[], true);
    home.disconnect();
    fs::write(
        home.path().join("state/koidrive/telemetry/prompt.json"),
        "invalid",
    )
    .unwrap();
    let output = terminal_run(
        home.path(),
        &[],
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
    let (home, output) = setup(&full(false, "n\n"), &[], &[], true);
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
    let steps = [
        step(ROUTE, "2\n"),
        step(CODE, line("not-a-code")),
        step(TELEMETRY, "n\n"),
    ];
    let (home, output) = setup(&steps, &[], &[], false);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!output.status.success(), "{text}");
    assert!(!text.contains("Send anonymous usage reports"), "{text}");
    assert!(!home.path().join("state/koidrive/telemetry").exists());
}

#[test]
fn opting_in_after_setup_enables_future_reports_without_recording_the_setup() {
    let (home, output) = setup(&full(false, "y\n"), &[], &[], true);
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
        let mut steps = full(false, "");
        steps.pop();
        let (_, output) = setup(&steps, &args, &vars, true);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(output.status.success(), "{text}");
        assert!(!text.contains("Send anonymous usage reports"), "{text}");
    }
}

#[test]
fn pairing_with_a_new_server_proposes_and_links_a_koidrive_folder() {
    for rich in [false, true] {
        let home = Home::new(&[]);
        let output = home.setup(&full(rich, &no(rich)), &[], &[], true, rich, None);
        let folder = home.path().join("Koidrive");
        let text = shown(output.clone());
        assert!(output.status.success(), "{text}");
        assert!(
            text.contains("Where should Koidrive sync your files?"),
            "{text}"
        );
        assert!(
            text.contains(&format!("Folder: {}", folder.display())),
            "{text}"
        );
        assert!(folder.is_dir());
        let created = (home.requests().iter())
            .filter(|request| *request == "POST /v1/spaces")
            .count();
        assert_eq!(created, 1);
        let space = home.space_id("Koidrive");
        assert_eq!(home.links(), [format!("link {} {space}", folder.display())]);
        let ending = text
            .find(&format!("Add files to {} to sync them.", folder.display()))
            .unwrap();
        assert!(text[ending..].contains("Check sync: koi status"), "{text}");
        assert!(ending < text.find("Send anonymous usage reports").unwrap());
        assert!(!text.contains("koi space list"), "{text}");
    }
}

#[test]
fn pairing_with_an_existing_server_selects_a_shared_folder() {
    for rich in [false, true] {
        let home = Home::new(&["Koidrive", "Work"]);
        let mut steps = pairing(rich);
        steps.extend([
            step(SPACE, choose(rich, 1)),
            step(PATH, "\n"),
            after(&["Create ", "Koidrive?"], yes(rich, true)),
            step(TELEMETRY, no(rich)),
        ]);
        let output = home.setup(&steps, &[], &[], true, rich, None);
        let text = shown(output.clone());
        assert!(output.status.success(), "{text}");
        assert!(text.contains("Create a shared folder"), "{text}");
        let folder = home.path().join("Koidrive");
        assert_eq!(
            home.links(),
            [format!(
                "link {} {}",
                folder.display(),
                home.space_id("Work")
            )]
        );
        assert!(!home.requests().contains(&"POST /v1/spaces".to_owned()));
        assert!(
            text.contains(&format!("Linked {} to Work.", folder.display())),
            "{text}"
        );
    }
}

#[test]
fn shared_folders_with_one_name_show_a_short_id() {
    let home = Home::new(&["Work", "Work"]);
    home.connect();
    let steps = [
        step(SPACE, choose(false, 1)),
        step(PATH, "\n"),
        step("Create ", yes(false, true)),
        step(TELEMETRY, no(false)),
    ];
    let output = home.setup(&steps, &[], &[], true, false, None);
    let text = shown(output.clone());
    assert!(output.status.success(), "{text}");
    let spaces = home.spaces.lock().unwrap().clone();
    for space in &spaces {
        let short = &space.space_id[space.space_id.len() - 8..];
        assert!(text.contains(&format!("Work ({short})")), "{text}");
    }
    let folder = home.path().join("Koidrive");
    assert_eq!(
        home.links(),
        [format!("link {} {}", folder.display(), spaces[1].space_id)]
    );
}

#[test]
fn a_new_shared_folder_takes_the_name_typed() {
    for rich in [false, true] {
        let home = Home::new(&["Koidrive"]);
        let mut steps = pairing(rich);
        steps.extend([
            step(SPACE, choose(rich, 1)),
            step(NAME, line("Photos")),
            step(PATH, "\n"),
            step("and its shared folder named Photos?", yes(rich, true)),
            step(TELEMETRY, no(rich)),
        ]);
        let output = home.setup(&steps, &[], &[], true, rich, None);
        let text = shown(output.clone());
        assert!(output.status.success(), "{text}");
        let folder = home.path().join("Koidrive");
        assert_eq!(
            home.links(),
            [format!(
                "link {} {}",
                folder.display(),
                home.space_id("Photos")
            )]
        );
        assert!(text.contains("Created the shared folder Photos."), "{text}");
    }
}

#[test]
fn a_lost_create_answer_links_the_shared_folder_it_made_once() {
    let home = Home::new(&[]);
    home.lose_create.store(true, Ordering::Relaxed);
    let output = home.setup(&full(false, "n\n"), &[], &[], true, false, None);
    let text = shown(output.clone());
    assert!(output.status.success(), "{text}");
    let created = (home.requests().iter())
        .filter(|request| *request == "POST /v1/spaces")
        .count();
    assert_eq!(created, 1);
    assert_eq!(home.spaces.lock().unwrap().len(), 1);
    assert_eq!(home.links().len(), 1);
    assert!(home.links()[0].ends_with(&home.space_id("Koidrive")));
}

#[test]
fn a_folder_with_files_links_only_after_its_owner_agrees() {
    for rich in [false, true] {
        for accept in [true, false] {
            let home = Home::new(&["Koidrive"]);
            home.connect();
            let files = home.path().join("Files");
            fs::create_dir(&files).unwrap();
            fs::write(files.join("report.docx"), "draft").unwrap();
            let other = home.path().join("My Files é");
            let mut steps = vec![
                step(SPACE, choose(rich, 0)),
                step(PATH, line("~/Files")),
                step(
                    "will upload to Koidrive, and files in Koidrive will download here.",
                    no(rich),
                ),
            ];
            if accept {
                steps[2].input = yes(rich, true);
            } else {
                steps.extend([
                    after(&["Folder: ", PATH], line("~/My Files é")),
                    after(&["Create ", "My Files é?"], yes(rich, true)),
                ]);
            }
            steps.push(step(TELEMETRY, no(rich)));
            let output = home.setup(&steps, &[], &[], true, rich, None);
            let text = shown(output.clone());
            assert!(output.status.success(), "{text}");
            let linked = if accept { &files } else { &other };
            assert_eq!(
                home.links(),
                [format!(
                    "link {} {}",
                    linked.display(),
                    home.space_id("Koidrive")
                )]
            );
            assert_eq!(fs::read(files.join("report.docx")).unwrap(), b"draft");
            assert_eq!(other.is_dir(), !accept);
            assert!(!home.requests().contains(&"pair".to_owned()));
        }
    }
}

#[test]
fn a_file_is_refused_and_the_folder_asked_again() {
    for rich in [false, true] {
        let home = Home::new(&["Koidrive"]);
        home.connect();
        fs::write(home.path().join("notes.txt"), "notes").unwrap();
        let steps = [
            step(SPACE, choose(rich, 0)),
            step(PATH, line("~/notes.txt")),
            after(&["is a file, not a folder", PATH], "\n"),
            after(&["Create ", "Koidrive?"], yes(rich, true)),
            step(TELEMETRY, no(rich)),
        ];
        let output = home.setup(&steps, &[], &[], true, rich, None);
        let text = shown(output.clone());
        assert!(output.status.success(), "{text}");
        assert_eq!(fs::read(home.path().join("notes.txt")).unwrap(), b"notes");
        let folder = home.path().join("Koidrive");
        assert_eq!(
            home.links(),
            [format!(
                "link {} {}",
                folder.display(),
                home.space_id("Koidrive")
            )]
        );
    }
}

#[test]
fn a_rerun_with_a_linked_folder_changes_nothing() {
    for rich in [false, true] {
        let home = Home::new(&["Koidrive"]);
        home.connect();
        let folder = home.path().join("Koidrive");
        fs::create_dir(&folder).unwrap();
        home.link(&folder, &home.space_id("Koidrive"));
        let output = home.setup(&[step(TELEMETRY, no(rich))], &[], &[], true, rich, None);
        let text = shown(output.clone());
        assert!(output.status.success(), "{text}");
        assert!(
            text.contains("This computer is already connected to Koidrive."),
            "{text}"
        );
        assert!(
            text.contains(&format!("{} syncs with Koidrive.", folder.display())),
            "{text}"
        );
        assert!(!text.contains(ROUTE), "{text}");
        assert!(!text.contains(PATH), "{text}");
        let requests = home.requests();
        assert!(
            !requests
                .iter()
                .any(|request| request == "pair" || request.starts_with("link ")),
            "{requests:?}"
        );
    }
}

#[test]
fn a_connected_device_without_a_folder_goes_to_the_folder_step() {
    for rich in [false, true] {
        let home = Home::new(&["Koidrive"]);
        home.connect();
        let steps = [
            step(SPACE, choose(rich, 0)),
            step(PATH, "\n"),
            after(&["Create ", "Koidrive?"], yes(rich, true)),
            step(TELEMETRY, no(rich)),
        ];
        let output = home.setup(&steps, &[], &[], true, rich, None);
        let text = shown(output.clone());
        assert!(output.status.success(), "{text}");
        assert!(
            text.contains("This computer is already connected to Koidrive."),
            "{text}"
        );
        assert!(!text.contains(ROUTE), "{text}");
        assert!(!home.requests().contains(&"pair".to_owned()));
        assert_eq!(home.links().len(), 1);
    }
}

#[test]
fn cancelling_a_folder_question_keeps_the_device_and_links_nothing() {
    for rich in [false, true] {
        for key in ["\u{3}", "\u{4}"] {
            if key == "\u{3}" && !rich {
                continue;
            }
            for stage in 0..4 {
                let home = Home::new(&["Koidrive"]);
                home.connect();
                let mut steps = vec![
                    step(SPACE, choose(rich, 1)),
                    step(NAME, line("Photos")),
                    step(PATH, "\n"),
                    step("and its shared folder named Photos?", yes(rich, true)),
                ];
                steps.truncate(stage + 1);
                steps[stage].input = key.into();
                let output = home.setup(&steps, &[], &[], true, rich, None);
                let text = shown(output.clone());
                assert_eq!(
                    output.status.code(),
                    Some(if key == "\u{3}" { 130 } else { 1 }),
                    "{text}"
                );
                assert!(home.connected());
                assert!(home.links().is_empty(), "{:?}", home.requests());
                assert!(!home.requests().contains(&"POST /v1/spaces".to_owned()));
                assert!(!home.path().join("Koidrive").exists());
            }
        }
    }
}

#[test]
fn scripted_pairing_links_the_folder_without_a_terminal() {
    let home = Home::new(&["Work"]);
    let folder = home.path().join("Sync Folder é");
    let (exit, envelope) = home.scripted(
        "CODE@files.example.com\n",
        &[
            "--pairing-code-stdin",
            "--folder",
            folder.to_str().unwrap(),
            "--space",
            "Work",
            "--telemetry",
            "off",
        ],
    );
    assert_eq!(exit, 0, "{envelope:#}");
    assert_eq!(envelope["outcome"], "ok");
    let data = &envelope["data"];
    assert_eq!(data["folder"]["path"], folder.display().to_string());
    assert_eq!(data["folder"]["space_id"], home.space_id("Work"));
    assert_eq!(data["folder"]["space_name"], "Work");
    assert_eq!(
        data["account"]["device_id"],
        "0190a0b0-0000-7000-8000-000000000001"
    );
    assert!(data["service"].is_object(), "{envelope:#}");
    assert!(folder.is_dir());
    let requests: Vec<_> = (home.requests().into_iter())
        .filter(|request| request == "pair" || request.starts_with("link "))
        .collect();
    assert_eq!(
        requests,
        ["pair".to_owned(), format!("link {} Work", folder.display())]
    );
}

#[test]
fn files_appearing_during_creation_confirmation_are_not_linked_without_consent() {
    for rich in [false, true] {
        let home = Home::new(&["Work"]);
        home.connect();
        let folder = home.path().join("Koidrive");
        let changed = folder.clone();
        let steps = [
            step(SPACE, choose(rich, 0)),
            step(PATH, "\n"),
            Step {
                before_answer: Some(Box::new(move || {
                    fs::create_dir(&changed).unwrap();
                    fs::write(changed.join("private.docx"), "private document").unwrap();
                })),
                ..after(&["Create ", "Koidrive?"], yes(rich, true))
            },
        ];
        let output = terminal_run(
            home.path(),
            &steps,
            &["setup", "--telemetry", "off"],
            &[],
            rich,
            None,
            false,
        );
        let text = shown(output.clone());
        assert!(!output.status.success(), "{text}");
        let driver = koidrive_driver::Driver::open(home.paths()).unwrap();
        assert!(driver.status().folders.is_empty(), "{text}");
        assert_eq!(
            fs::read(folder.join("private.docx")).unwrap(),
            b"private document"
        );
    }
}

#[test]
fn files_appearing_during_the_link_server_call_still_need_consent() {
    for rich in [false, true] {
        let home = Home::new(&["Work"]);
        home.connect();
        let folder = home.path().join("Koidrive");
        fs::create_dir(&folder).unwrap();
        let changed = folder.clone();
        let mut lists = 0;
        *home.request_hook.lock().unwrap() = Some(Box::new(move |request| {
            if request == "GET /v1/spaces" {
                lists += 1;
                if lists == 2 {
                    fs::write(changed.join("private.docx"), "private document").unwrap();
                }
            }
        }));
        let steps = [step(SPACE, choose(rich, 0)), step(PATH, "\n")];
        let output = terminal_run(
            home.path(),
            &steps,
            &["setup", "--telemetry", "off"],
            &[],
            rich,
            None,
            false,
        );
        let text = shown(output.clone());
        assert!(!output.status.success(), "{text}");
        let driver = koidrive_driver::Driver::open(home.paths()).unwrap();
        assert!(driver.status().folders.is_empty(), "{text}");
        assert_eq!(
            fs::read(folder.join("private.docx")).unwrap(),
            b"private document"
        );
    }
}

#[test]
fn creating_a_shared_folder_does_not_choose_between_duplicate_names() {
    for rich in [false, true] {
        let home = Home::new(&["Work", "Work"]);
        home.connect();
        let steps = [
            step(SPACE, choose(rich, 2)),
            step(NAME, line("Work")),
            step(PATH, "\n"),
            step("Create ", yes(rich, true)),
        ];
        let output = home.setup(&steps, &["--telemetry", "off"], &[], true, rich, None);
        let text = shown(output.clone());
        assert!(!output.status.success(), "{text}");
        assert!(home.links().is_empty(), "{text}");
    }
}

#[test]
fn replacing_an_approved_directory_during_the_link_server_call_is_refused() {
    for rich in [false, true] {
        let home = Home::new(&["Work"]);
        home.connect();
        let folder = home.path().join("Koidrive");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("approved.docx"), "approved document").unwrap();
        let changed = folder.clone();
        let saved = home.path().join("Saved");
        let old = saved.clone();
        let mut lists = 0;
        *home.request_hook.lock().unwrap() = Some(Box::new(move |request| {
            if request == "GET /v1/spaces" {
                lists += 1;
                if lists == 2 {
                    fs::rename(&changed, &old).unwrap();
                    fs::create_dir(&changed).unwrap();
                    fs::write(changed.join("private.docx"), "private document").unwrap();
                }
            }
        }));
        let steps = [
            step(SPACE, choose(rich, 0)),
            step(PATH, "\n"),
            step("Link this folder?", yes(rich, true)),
        ];
        let output = terminal_run(
            home.path(),
            &steps,
            &["setup", "--telemetry", "off"],
            &[],
            rich,
            None,
            false,
        );
        let text = shown(output.clone());
        assert!(!output.status.success(), "{text}");
        let driver = koidrive_driver::Driver::open(home.paths()).unwrap();
        assert!(driver.status().folders.is_empty(), "{text}");
        assert_eq!(
            fs::read(folder.join("private.docx")).unwrap(),
            b"private document"
        );
        assert_eq!(
            fs::read(saved.join("approved.docx")).unwrap(),
            b"approved document"
        );
    }
}

#[test]
fn a_lost_create_response_does_not_choose_between_duplicate_names() {
    for rich in [false, true] {
        let home = Home::new(&[]);
        home.connect();
        home.lose_create.store(true, Ordering::Relaxed);
        let spaces = home.spaces.clone();
        *home.request_hook.lock().unwrap() = Some(Box::new(move |request| {
            if request == "POST /v1/spaces" {
                spaces.lock().unwrap().push(space(0, "Koidrive"));
            }
        }));
        let output = home.setup(
            &default_folder(rich),
            &["--telemetry", "off"],
            &[],
            true,
            rich,
            None,
        );
        let text = shown(output.clone());
        assert!(!output.status.success(), "{text}");
        assert!(text.contains("several spaces are called"), "{text}");
        assert!(home.links().is_empty());
        assert_eq!(home.spaces.lock().unwrap().len(), 2);
    }
}

#[test]
fn a_dangling_path_is_refused_and_the_folder_asked_again() {
    for rich in [false, true] {
        let home = Home::new(&["Work"]);
        home.connect();
        let dangling = home.path().join("Dangling");
        let missing = home.path().join("Missing");
        std::os::unix::fs::symlink(&missing, &dangling).unwrap();
        let steps = [
            step(SPACE, choose(rich, 0)),
            step(PATH, line(dangling.to_str().unwrap())),
            after(&["cannot be read", PATH], "\n"),
            step("Create ", yes(rich, true)),
        ];
        let output = home.setup(&steps, &["--telemetry", "off"], &[], true, rich, None);
        let text = shown(output.clone());
        assert!(output.status.success(), "{text}");
        assert!(text.contains("cannot be read"), "{text}");
        assert!(!missing.exists());
        assert!(
            fs::symlink_metadata(&dangling)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!home.requests().contains(&"pair".to_owned()));
        assert_eq!(home.links().len(), 1);
    }
}

#[test]
fn scripted_setup_checks_its_input_before_redeeming_the_code() {
    let home = Home::new(&["Work"]);
    let file = home.path().join("notes.txt");
    fs::write(&file, "notes").unwrap();
    let full = home.path().join("Full");
    fs::create_dir(&full).unwrap();
    fs::write(full.join("a.txt"), "a").unwrap();
    let under_file = file.join("Koidrive");
    let empty = home.path().join("Empty");
    let dangling = home.path().join("Dangling");
    std::os::unix::fs::symlink(home.path().join("Missing"), &dangling).unwrap();
    let under_dangling = dangling.join("Child");
    let readonly = home.path().join("Readonly");
    fs::create_dir(&readonly).unwrap();
    fs::set_permissions(&readonly, fs::Permissions::from_mode(0o555)).unwrap();
    let under_readonly = readonly.join("Child");
    let cases: Vec<(&str, Vec<&str>)> = vec![
        (
            "CODE@files.example.com\n",
            vec!["--folder", readonly.to_str().unwrap(), "--space", "Work"],
        ),
        (
            "CODE@files.example.com\n",
            vec![
                "--folder",
                under_readonly.to_str().unwrap(),
                "--space",
                "Work",
            ],
        ),
        (
            "CODE@files.example.com\n",
            vec!["--folder", dangling.to_str().unwrap(), "--space", "Work"],
        ),
        (
            "CODE@files.example.com\n",
            vec![
                "--folder",
                under_dangling.to_str().unwrap(),
                "--space",
                "Work",
            ],
        ),
        (
            "CODE@files.example.com\n",
            vec!["--folder", file.to_str().unwrap(), "--space", "Work"],
        ),
        (
            "CODE@files.example.com\n",
            vec!["--folder", full.to_str().unwrap(), "--space", "Work"],
        ),
        (
            "CODE@files.example.com\n",
            vec!["--folder", under_file.to_str().unwrap(), "--space", "Work"],
        ),
        (
            "not-a-code\n",
            vec!["--folder", empty.to_str().unwrap(), "--space", "Work"],
        ),
        (
            "",
            vec!["--folder", empty.to_str().unwrap(), "--space", "Work"],
        ),
        (
            "CODE@files.example.com\n",
            vec!["--folder", empty.to_str().unwrap(), "--space", " "],
        ),
        (
            "CODE@files.example.com\n",
            vec!["--folder", empty.to_str().unwrap()],
        ),
        ("CODE@files.example.com\n", vec!["--space", "Work"]),
        ("CODE@files.example.com\n", vec!["--accept-existing-files"]),
        (
            "CODE@files.example.com\n",
            vec![
                "--folder",
                empty.to_str().unwrap(),
                "--space",
                "Work",
                "--interactive",
            ],
        ),
    ];
    for (stdin, args) in cases {
        let mut args = args;
        args.insert(0, "--pairing-code-stdin");
        let (exit, envelope) = home.scripted(stdin, &args);
        assert_eq!(exit, 2, "{args:?}: {envelope:#}");
        assert_eq!(envelope["error"]["code"], "usage", "{envelope:#}");
        assert!(
            home.requests().is_empty(),
            "{args:?}: {:?}",
            home.requests()
        );
        assert!(!home.connected());
        assert!(!empty.exists());
        assert!(!under_file.exists());
    }
}

#[test]
fn scripted_setup_links_a_folder_with_files_only_when_asked() {
    let home = Home::new(&["Work"]);
    let folder = home.path().join("Files");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("deck.key"), "slides").unwrap();
    let (exit, envelope) = home.scripted(
        "CODE@files.example.com\n",
        &[
            "--pairing-code-stdin",
            "--folder",
            "~/Files",
            "--space",
            &home.space_id("Work"),
            "--accept-existing-files",
        ],
    );
    assert_eq!(exit, 0, "{envelope:#}");
    assert_eq!(
        envelope["data"]["folder"]["path"],
        folder.display().to_string()
    );
    assert_eq!(fs::read(folder.join("deck.key")).unwrap(), b"slides");
}

#[test]
fn scripted_setup_on_a_connected_device_links_without_pairing() {
    let home = Home::new(&["Work"]);
    credential("https://files.example.com/")
        .save_new(&home.paths().credential())
        .unwrap();
    let folder = home.path().join("Work");
    let args = ["--folder", folder.to_str().unwrap(), "--space", "Work"];
    let (exit, envelope) = home.scripted("", &args);
    assert_eq!(exit, 0, "{envelope:#}");
    assert_eq!(envelope["outcome"], "ok");
    let (exit, envelope) = home.scripted(
        "CODE@files.example.com\n",
        &[&["--pairing-code-stdin"], &args[..]].concat(),
    );
    assert_eq!(exit, 0, "{envelope:#}");
    assert_eq!(envelope["outcome"], "unchanged");
    assert!(!home.requests().contains(&"pair".to_owned()));
    assert_eq!(home.links().len(), 2);
    let (exit, envelope) = home.scripted(
        "CODE@other.example.com\n",
        &[&["--pairing-code-stdin"], &args[..]].concat(),
    );
    assert_eq!(exit, 2, "{envelope:#}");
    assert_eq!(home.links().len(), 2);
}

#[test]
fn scripted_setup_replays_a_linked_folder_after_files_arrive() {
    let home = Home::new(&["Work"]);
    home.connect();
    let folder = home.path().join("Work");
    let args = ["--folder", folder.to_str().unwrap(), "--space", "Work"];
    let (exit, envelope) = home.script("", &args, false);
    assert_eq!(exit, 0, "{envelope:#}");
    assert!(folder.join(".koidrive").is_dir());
    fs::write(folder.join("report.docx"), "downloaded report").unwrap();
    let before = fs::read(folder.join(".koidrive/space.json")).unwrap();
    let (exit, envelope) = home.script("", &args, false);
    assert_eq!(exit, 0, "{envelope:#}");
    assert_eq!(envelope["outcome"], "unchanged");
    assert_eq!(
        fs::read(folder.join(".koidrive/space.json")).unwrap(),
        before
    );
    let driver = koidrive_driver::Driver::open(home.paths()).unwrap();
    let folders = driver.status().folders;
    assert_eq!(folders.len(), 1);
    assert_eq!(
        Path::new(&folders[0].path),
        fs::canonicalize(&folder).unwrap()
    );
    assert_eq!(
        fs::read(folder.join("report.docx")).unwrap(),
        b"downloaded report"
    );
    assert!(!home.requests().contains(&"pair".to_owned()));
}

#[test]
fn scripted_setup_replays_canonical_roots_but_requires_consent_after_an_alias_moves() {
    let home = Home::new(&["Work"]);
    home.connect();
    let folder = home.path().join("Work");
    let space = home.space_id("Work");
    let (exit, envelope) = home.script(
        "",
        &["--folder", folder.to_str().unwrap(), "--space", &space],
        false,
    );
    assert_eq!(exit, 0, "{envelope:#}");
    fs::write(folder.join("report.docx"), "downloaded report").unwrap();
    let alias = home.path().join("Alias");
    std::os::unix::fs::symlink(&folder, &alias).unwrap();
    let root = fs::canonicalize(&folder).unwrap();
    for path in [&folder, &root, &alias] {
        let (exit, envelope) = home.script(
            "",
            &["--folder", path.to_str().unwrap(), "--space", &space],
            false,
        );
        assert_eq!(exit, 0, "{envelope:#}");
        assert_eq!(envelope["outcome"], "unchanged");
        assert_eq!(
            envelope["data"]["folder"]["path"],
            root.display().to_string()
        );
    }
    let unlinked = home.path().join("Unlinked");
    fs::create_dir(&unlinked).unwrap();
    fs::create_dir(unlinked.join(".koidrive")).unwrap();
    fs::write(unlinked.join("report.docx"), "unlinked report").unwrap();
    fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&unlinked, &alias).unwrap();
    let requests = home.requests();
    let (exit, envelope) = home.script(
        "CODE@files.example.com\n",
        &[
            "--pairing-code-stdin",
            "--folder",
            alias.to_str().unwrap(),
            "--space",
            &space,
        ],
        false,
    );
    assert_eq!(exit, 2, "{envelope:#}");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--accept-existing-files"),
        "{envelope:#}"
    );
    assert_eq!(home.requests(), requests);
    assert_eq!(
        fs::read(unlinked.join("report.docx")).unwrap(),
        b"unlinked report"
    );
}

#[test]
fn scripted_setup_replay_keeps_driver_space_validation() {
    let home = Home::new(&["Work", "Other"]);
    home.connect();
    let folder = home.path().join("Work");
    let space_id = home.space_id("Work");
    let args = ["--folder", folder.to_str().unwrap(), "--space", &space_id];
    let (exit, envelope) = home.script("", &args, false);
    assert_eq!(exit, 0, "{envelope:#}");
    fs::write(folder.join("report.docx"), "downloaded report").unwrap();
    for space in ["Other", &home.space_id("Other")] {
        let (exit, envelope) = home.script(
            "",
            &["--folder", folder.to_str().unwrap(), "--space", space],
            false,
        );
        assert_eq!(exit, 2, "{envelope:#}");
        assert!(
            envelope["error"]["message"]
                .as_str()
                .unwrap()
                .contains("overlaps the linked folder"),
            "{envelope:#}"
        );
    }
    let duplicate = space(2, "Work");
    home.spaces.lock().unwrap().push(duplicate.clone());
    let (exit, envelope) = home.script(
        "",
        &["--folder", folder.to_str().unwrap(), "--space", "Work"],
        false,
    );
    assert_eq!(exit, 2, "{envelope:#}");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("several spaces are called"),
        "{envelope:#}"
    );
    assert!(message.contains(&space_id), "{envelope:#}");
    assert!(message.contains(&duplicate.space_id), "{envelope:#}");
    let (exit, envelope) = home.script("", &args, false);
    assert_eq!(exit, 0, "{envelope:#}");
    assert_eq!(envelope["outcome"], "unchanged");
    let driver = koidrive_driver::Driver::open(home.paths()).unwrap();
    let folders = driver.status().folders;
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].space_id, space_id);
    assert_eq!(
        fs::read(folder.join("report.docx")).unwrap(),
        b"downloaded report"
    );
}

#[test]
fn scripted_setup_on_a_new_device_needs_a_pairing_code() {
    let home = Home::new(&["Work"]);
    let folder = home.path().join("Work");
    let (exit, envelope) = home.scripted(
        "",
        &["--folder", folder.to_str().unwrap(), "--space", "Work"],
    );
    assert_eq!(exit, 4, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "auth_required");
    assert!(home.requests().is_empty());
    assert!(!folder.exists());
}

#[test]
fn setup_without_script_flags_still_refuses_a_connected_device() {
    let home = Home::new(&[]);
    home.connect();
    let (exit, envelope) = home.scripted("", &[]);
    assert_eq!(exit, 1, "{envelope:#}");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already set up"),
        "{envelope:#}"
    );
    assert!(home.requests().is_empty());
}
