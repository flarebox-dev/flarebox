// SPDX-License-Identifier: Apache-2.0

//! `koi diagnose`: one text file to attach to a report of a problem. It holds the version, the system, the service,
//! `koi status --json` and the end of the daemon's log. Every path, file name, space and device name, server address
//! and token in it is replaced by a placeholder such as `<folder-1>`, the same one wherever the value appears.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use koidrive_driver::{Credential, Paths};
use serde_json::{Value, json};

use crate::error::CliError;
use crate::report::Done;
use crate::service;

/// Where problems are reported (CONTRIBUTING.md).
const ISSUES: &str = "https://github.com/koidrive/koidrive/issues";

/// How many of the daemon's last log lines the file holds.
const LOG_LINES: usize = 200;

/// The fields of `koi status --json` whose string values name something of the person's, with the placeholder kind
/// each gets.
const NAMED: [(&str, &str); 6] = [
    ("path", "folder"),
    ("folder", "folder"),
    ("server", "server"),
    ("space_name", "space"),
    ("device_name", "device"),
    ("user_name", "user"),
];

/// The log fields whose values stay, redacted like any text: causes, ids and waits. `signal` stays as it is, since the
/// daemon writes only a signal's name there. Any other field, such as a path or a name, is replaced whole.
const KEPT: [&str; 8] = [
    "error",
    "problem",
    "reason",
    "space_id",
    "question_id",
    "restore_id",
    "retry_in",
    "wait",
];

const LEVELS: [&str; 5] = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"];

/// Names of Koidrive's own that look like a file or a host name.
const OURS: [&str; 4] = [
    "config.toml",
    "credential.json",
    "koidrive.service",
    ".koiignore",
];

/// Writes the file into the current directory and returns its path.
pub async fn run(paths: &Paths) -> Result<Done, CliError> {
    let mut redactor = Redactor::default();
    know_profile(&mut redactor, paths);
    let status = match crate::status(paths, false).await {
        Ok(done) => done.data,
        Err(error) => json!({
            "error": error.message,
            "service": service::inspect(paths, false).await,
        }),
    };
    let text = bundle(&mut redactor, &system(), &status, &daemon_log(paths));
    let seconds = (SystemTime::now().duration_since(UNIX_EPOCH)).map_or(0, |since| since.as_secs());
    let dir = std::env::current_dir()
        .map_err(|error| format!("cannot find the current directory: {error}"))?;
    let path = dir.join(format!("koi-diagnostics-{seconds}.txt"));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(Done {
        note: format!("To report a problem, attach this file to an issue at {ISSUES}\n"),
        ..Done::ok(json!({ "path": path }), format!("{}\n", path.display()))
    })
}

/// The values of this profile that are not in its status: the home folder and the user, the host, the profile's own
/// folders and the stored credential.
fn know_profile(redactor: &mut Redactor, paths: &Paths) {
    let home = std::env::var("HOME").unwrap_or_default();
    for path in [
        &paths.state_dir,
        &paths.runtime_dir,
        paths.config_file.parent().unwrap_or(&paths.state_dir),
    ] {
        if !path.starts_with(&home) || home.is_empty() {
            redactor.know("path", &path.to_string_lossy());
        }
    }
    redactor.know("home", &home);
    let user = Path::new(&home)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    for user in [std::env::var("USER").ok(), user].into_iter().flatten() {
        redactor.know("user", &user);
    }
    let host = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    redactor.know("host", &host);
    redactor.know("host", host.split('.').next().unwrap_or_default());
    if let Ok(Some(credential)) = Credential::load(&paths.credential()) {
        redactor.know("token", &credential.refresh_credential);
        redactor.know_server(&credential.server);
        redactor.know("device", &credential.device_name);
        redactor.know("user", credential.user_name.as_deref().unwrap_or_default());
    }
}

fn bundle(
    redactor: &mut Redactor,
    system: &str,
    status: &Value,
    log: &Result<String, String>,
) -> String {
    redactor.register(status);
    let status = redactor.json(status);
    let status = serde_json::to_string_pretty(&status).expect("JSON values serialize");
    let log = match log {
        Ok(log) => {
            let lines: Vec<&str> = log.lines().collect();
            let tail = &lines[lines.len().saturating_sub(LOG_LINES)..];
            let lines: Vec<String> = tail.iter().map(|line| redactor.log_line(line)).collect();
            lines.join("\n")
        }
        Err(problem) => format!("(no log: {})", redactor.text(problem)),
    };
    format!(
        "Koidrive diagnostics\n\nkoi {}\n{system}\n\n## koi status --json\n\n{status}\n\n## The daemon's log, last {LOG_LINES} lines\n\n{log}\n",
        env!("CARGO_PKG_VERSION"),
    )
}

/// The operating system, its version and the processor.
fn system() -> String {
    let arch = std::env::consts::ARCH;
    if cfg!(target_os = "macos") {
        let version = Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
        return format!("macOS {} ({arch})", version.unwrap_or_default());
    }
    let name = (fs::read_to_string("/etc/os-release").ok())
        .and_then(|text| {
            (text.lines())
                .find_map(|line| line.strip_prefix("PRETTY_NAME="))
                .map(|name| name.trim_matches('"').to_owned())
        })
        .unwrap_or_else(|| std::env::consts::OS.to_owned());
    let kernel = rustix::system::uname()
        .release()
        .to_string_lossy()
        .into_owned();
    format!("{name} ({arch}), kernel {kernel}")
}

/// The end of the service's log: the launch agent's log file on macOS, the unit's journal elsewhere.
fn daemon_log(paths: &Paths) -> Result<String, String> {
    let service = service::this_service(paths)?;
    if cfg!(target_os = "macos") {
        let path = service.log_file();
        let log = fs::read(&path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => "the service has not written a log".to_owned(),
            _ => format!("cannot read {}: {error}", path.display()),
        })?;
        return Ok(String::from_utf8_lossy(&log).into_owned());
    }
    let lines = LOG_LINES.to_string();
    let unit = service.unit_name();
    let args = [
        "--user",
        "--unit",
        &unit,
        "--lines",
        &lines,
        "--output",
        "cat",
        "--no-pager",
        "--quiet",
    ];
    let output = Command::new("journalctl")
        .args(args)
        .output()
        .map_err(|error| format!("cannot run journalctl: {error}"))?;
    match output.status.success() {
        true => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        false => Err(format!(
            "journalctl failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Replaces what names something of the person's with placeholders, numbered by kind in the order first seen.
#[derive(Default)]
struct Redactor {
    placeholders: BTreeMap<String, String>,
    counts: BTreeMap<&'static str, usize>,
}

impl Redactor {
    fn name(&mut self, kind: &'static str, value: &str) -> String {
        if let Some(placeholder) = self.placeholders.get(value) {
            return placeholder.clone();
        }
        let count = self.counts.entry(kind).or_default();
        *count += 1;
        let placeholder = format!("<{kind}-{count}>");
        self.placeholders
            .insert(value.to_owned(), placeholder.clone());
        placeholder
    }

    /// Notes a value to find in any text.
    fn know(&mut self, kind: &'static str, value: &str) {
        if !value.trim().is_empty() {
            self.name(kind, value);
        }
    }

    fn know_server(&mut self, server: &str) {
        self.know("server", server);
        if let Some(host) = url::Url::parse(server)
            .ok()
            .as_ref()
            .and_then(url::Url::host_str)
        {
            self.know("host", host);
        }
    }

    /// Notes the values of the status's named fields.
    fn register(&mut self, value: &Value) {
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    let kind = NAMED
                        .iter()
                        .find(|(name, _)| name == key)
                        .map(|(_, kind)| *kind);
                    match (kind, value.as_str()) {
                        (Some("server"), Some(text)) => self.know_server(text),
                        (Some(kind), Some(text)) => self.know(kind, text),
                        _ => self.register(value),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|item| self.register(item)),
            _ => {}
        }
    }

    fn json(&mut self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(text)),
            Value::Array(items) => items.iter().map(|item| self.json(item)).collect(),
            Value::Object(fields) => (fields.iter())
                .map(|(key, value)| (key.clone(), self.json(value)))
                .collect(),
            other => other.clone(),
        }
    }

    /// One line of the daemon's log, `<time> <LEVEL> <target>: <message> <field>=<value>...`. A line in another shape,
    /// such as a panic's, is left out.
    fn log_line(&mut self, line: &str) -> String {
        let Some((time, level, target, rest)) = log_parts(line) else {
            return "<redacted>".to_owned();
        };
        let starts = field_starts(rest);
        let message = &rest[..starts.first().map_or(rest.len(), |start| start - 1)];
        let mut out = format!("{time} {level} {target}: {}", self.text(message));
        for (index, start) in starts.iter().enumerate() {
            let end = starts.get(index + 1).map_or(rest.len(), |next| next - 1);
            let (key, value) = rest[*start..end].split_once('=').unwrap_or_default();
            let value = match key {
                "signal" => value.to_owned(),
                _ if KEPT.contains(&key) => self.text(value),
                _ => "<redacted>".to_owned(),
            };
            out.push_str(&format!(" {key}={value}"));
        }
        out
    }

    /// Replaces the known values, quoted text, paths, URLs, addresses, file and host names, and long tokens.
    fn text(&mut self, text: &str) -> String {
        let mut known: Vec<(String, String)> = (self.placeholders.iter())
            .map(|(value, placeholder)| (value.clone(), placeholder.clone()))
            .collect();
        known.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        let mut out = String::new();
        let mut rest = text;
        'scan: while let Some(c) = rest.chars().next() {
            let starts_word = !out.chars().next_back().is_some_and(char::is_alphanumeric);
            for (value, placeholder) in &known {
                let ends_word =
                    |after: &str| !after.chars().next().is_some_and(char::is_alphanumeric);
                if starts_word
                    && rest.starts_with(value.as_str())
                    && ends_word(&rest[value.len()..])
                {
                    out.push_str(placeholder);
                    rest = &rest[value.len()..];
                    continue 'scan;
                }
            }
            if c == '"'
                && let Some(end) = closing_quote(rest)
            {
                out.push('"');
                out.push_str(&self.name("name", &rest[1..end]));
                out.push('"');
                rest = &rest[end + 1..];
                continue;
            }
            if delimiter(c) {
                out.push(c);
                rest = &rest[c.len_utf8()..];
                continue;
            }
            let word = &rest[..rest.find(delimiter).unwrap_or(rest.len())];
            let len = match word.contains('/') && !word.contains("://") {
                true => path_len(rest),
                false => word.trim_end_matches(['.', ':', '!', '?']).len(),
            };
            let core = &rest[..len.max(c.len_utf8())];
            match kind_of(core) {
                Some(kind) => out.push_str(&self.name(kind, core)),
                None => out.push_str(core),
            }
            rest = &rest[core.len()..];
        }
        out
    }
}

/// Where the quoted string that starts `text` ends, skipping the quotes a backslash escapes, as `{:?}` writes them.
fn closing_quote(text: &str) -> Option<usize> {
    let mut chars = text.char_indices().skip(1);
    while let Some((index, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => return Some(index),
            _ => {}
        }
    }
    None
}

/// Splits a log line into its time, level, target and the rest.
fn log_parts(line: &str) -> Option<(&str, &str, &str, &str)> {
    let (time, rest) = line.split_once(' ')?;
    let (level, rest) = rest.trim_start().split_once(' ')?;
    let (target, rest) = rest.trim_start().split_once(": ")?;
    let target_shape =
        |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == ':';
    (LEVELS.contains(&level) && !target.is_empty() && target.chars().all(target_shape))
        .then_some((time, level, target, rest))
}

/// Where each ` <field>=` of a log line's fields starts, after its space.
fn field_starts(text: &str) -> Vec<usize> {
    (text.match_indices(' '))
        .map(|(index, _)| index + 1)
        .filter(|&start| {
            let key = &text[start..];
            let len = key
                .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(key.len());
            len > 0 && !key.starts_with(|c: char| c.is_ascii_digit()) && key[len..].starts_with('=')
        })
        .collect()
}

fn delimiter(c: char) -> bool {
    c.is_whitespace() || "\"'<>()[]{},;=".contains(c)
}

/// How far a path that starts `text` goes. File and folder names hold spaces, commas, quotes, parentheses and `: `, so
/// nothing in the text can end one, except an operating system error that ends the line, as in
/// `<path>: Permission denied (os error 13)`. Any other path runs to the end of the line, hiding what follows it.
fn path_len(text: &str) -> usize {
    let line = &text[..text.find('\n').unwrap_or(text.len())];
    let end = os_error_start(line).unwrap_or(line.len());
    line[..end].trim_end_matches(['.', ':']).len()
}

/// Where `: <description> (os error <code>)` starts at the end of `line`, as Rust shows an operating system error.
/// The descriptions the system gives hold no `: `.
fn os_error_start(line: &str) -> Option<usize> {
    let (head, code) = (line.trim_end().strip_suffix(')')?).rsplit_once(" (os error ")?;
    code.parse::<i32>().ok()?;
    head.rfind(": ")
}

/// What a word is, when it names something: a URL, an address, a path, a file or host name, or a token.
fn kind_of(word: &str) -> Option<&'static str> {
    if word.contains("://") {
        return Some("url");
    }
    if word.contains('@') || ip_address(word) {
        return Some("address");
    }
    if word.contains('/') {
        return Some("path");
    }
    if word.starts_with("dev.koidrive.") || OURS.contains(&word) {
        return None;
    }
    if let Some((head, tail)) = word.rsplit_once('.')
        && !head.is_empty()
        && head.chars().any(char::is_alphabetic)
        && (1..=12).contains(&tail.len())
        && tail.starts_with(|c: char| c.is_ascii_alphabetic())
        && tail.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return Some("name");
    }
    let token = |c: char| c.is_ascii_alphanumeric() || "_-+/=.".contains(c);
    (word.len() >= 24
        && word.chars().all(token)
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_alphabetic()))
    .then_some("token")
}

/// Whether a word is an IP address, with or without a port.
fn ip_address(word: &str) -> bool {
    let host = (word.rsplit_once(':'))
        .filter(|(_, port)| port.parse::<u16>().is_ok())
        .map_or(word, |(host, _)| host);
    host.parse::<std::net::IpAddr>().is_ok() || word.parse::<std::net::IpAddr>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLANTED: [&str; 9] = [
        "/Users/ada",
        "Secret Plans",
        "budget 2026",
        "Tax Returns",
        "Ada's MacBook",
        "koi-ada-7f3k",
        "workers.dev",
        "kdr_9f8e7d6c5b4a39281706f5e4d3c2b1a0",
        "ledger",
    ];

    fn status() -> Value {
        json!({
            "account": {
                "server": "https://koi-ada-7f3k.workers.dev",
                "user_id": "u1",
                "user_name": "ada",
                "device_id": "d1",
                "device_name": "Ada's MacBook",
            },
            "running": true,
            "folders": [{
                "path": "/Users/ada/Secret Plans",
                "space_id": "s1",
                "space_name": "Tax Returns",
                "state": "halted",
                "problem": "cannot read /Users/ada/Secret Plans/budget 2026.xlsx: Permission denied (os error 13)",
            }],
            "conditions": [{"condition": "halted", "folder": "/Users/ada/Secret Plans", "problem": null}],
        })
    }

    const LOG: &str = "\
2026-11-12T06:29:22.152628Z  INFO koidrive_cli: the daemon is running
2026-11-12T06:29:23.000001Z  INFO koidrive_driver::folder: linked a folder space_id=s1 path=/Users/ada/Secret Plans
2026-11-12T06:29:24.000001Z  WARN koidrive_driver::folder: a local trash entry written after it entered was recovered space_id=s1 name=ledger
2026-11-12T06:29:25.000001Z DEBUG koidrive_net::session: koidrive session stopped reason=the server https://koi-ada-7f3k.workers.dev/v1/sync refused the token kdr_9f8e7d6c5b4a39281706f5e4d3c2b1a0
2026-11-12T06:29:26.000001Z  WARN koidrive_driver::folder: sync of a folder halted space_id=s1 reason=no space is called \"Tax Returns\"
thread 'tokio-runtime-worker' panicked at /Users/ada/Secret Plans/ledger.rs:1:1:
";

    fn redacted() -> String {
        let mut redactor = Redactor::default();
        redactor.know("home", "/Users/ada");
        bundle(
            &mut redactor,
            "macOS 26.1 (aarch64)",
            &status(),
            &Ok(LOG.to_owned()),
        )
    }

    #[test]
    fn no_planted_path_name_url_or_token_reaches_the_file() {
        let text = redacted();
        for planted in PLANTED {
            assert!(!text.contains(planted), "{planted} in:\n{text}");
        }
    }

    #[test]
    fn the_file_keeps_what_explains_the_problem() {
        let text = redacted();
        for kept in [
            "the daemon is running",
            "linked a folder space_id=s1 path=<redacted>",
            "Permission denied (os error 13)",
            "\"state\": \"halted\"",
            "sync of a folder halted",
            "macOS 26.1 (aarch64)",
        ] {
            assert!(text.contains(kept), "{kept} not in:\n{text}");
        }
    }

    #[test]
    fn a_value_gets_the_same_placeholder_everywhere() {
        let text = redacted();
        assert!(text.contains("\"path\": \"<folder-1>\""), "{text}");
        assert!(text.contains("\"folder\": \"<folder-1>\""), "{text}");
        assert!(text.contains("\"space_name\": \"<space-1>\""), "{text}");
        assert!(text.contains("no space is called \"<space-1>\""), "{text}");
        assert!(
            text.contains("cannot read <folder-1><path-1>: Permission denied (os error 13)"),
            "{text}"
        );
    }

    #[test]
    fn a_word_that_holds_a_known_value_keeps_its_other_letters() {
        let mut redactor = Redactor::default();
        redactor.know("user", "ada");
        assert_eq!(redactor.text("ada and adam"), "<user-1> and adam");
    }

    /// The daemon logs a file it cannot read as `reason=the folder: <path>: <error>`, and file names hold spaces,
    /// commas, quotes and parentheses.
    #[test]
    fn a_path_is_redacted_to_the_error_after_it_whatever_its_name_holds() {
        let mut redactor = Redactor::default();
        redactor.know("home", "/Users/ada");
        let line = |reason: &str| {
            format!(
                "2026-11-12T06:29:23.000001Z  WARN koidrive_driver::folder: sync of a folder halted space_id=s1 reason=the folder: {reason}"
            )
        };
        for (reason, error, leaks) in [
            (
                "/Volumes/외장 SSD/가족 사진/여권 사본 (김건우 900101).jpg: Permission denied (os error 13)",
                ": Permission denied (os error 13)",
                ["김건우", "900101", "jpg"],
            ),
            (
                "/Users/ada/Library/Mobile Documents/com~apple~CloudDocs/병원/Kim, Gunwoo - HIV 검사 결과.pdf: Operation not permitted (os error 1)",
                ": Operation not permitted (os error 1)",
                ["Gunwoo", "HIV", "검사"],
            ),
            (
                "/Users/ada/Documents/O'Brien Family/taxes: Permission denied (os error 13)",
                ": Permission denied (os error 13)",
                ["Brien", "Family", "taxes"],
            ),
            (
                "/home/ada/Sync/Re: 김건우 연봉계약서.pdf: Permission denied (os error 13)",
                ": Permission denied (os error 13)",
                ["김건우", "연봉계약서", "pdf"],
            ),
        ] {
            let out = redactor.log_line(&line(reason));
            assert!(out.contains("reason=the folder: <"), "{out}");
            assert!(out.ends_with(error), "{out}");
            for leak in leaks {
                assert!(!out.contains(leak), "{leak} in {out}");
            }
        }
    }

    /// Paths built from random name parts, in the lines the daemon writes about a file or folder: no word of a name
    /// reaches the file.
    #[test]
    fn no_word_of_a_random_path_survives_in_the_lines_the_daemon_writes() {
        const WORDS: [&str; 8] = [
            "김건우",
            "연봉계약서",
            "여권",
            "Jiyeon",
            "Passport",
            "OBrien",
            "Interview",
            "x7",
        ];
        const GLUE: [&str; 10] = [" ", ": ", ", ", " (", ")", "'", "\"", ".", "-", ""];
        const EXTENSIONS: [&str; 4] = ["", ".pdf", ".mp4", ".tar.gz"];
        let relative_shapes = [
            "2026-11-12T06:29:23.000001Z  WARN koidrive_driver::folder: sync of a folder halted space_id=s1 reason=the folder: {}: Permission denied (os error 13)",
            "2026-11-12T06:29:23.000001Z  WARN koidrive_driver::folder: sync of a folder halted space_id=s1 reason=the folder: {} is missing",
            "2026-11-12T06:29:23.000001Z  WARN koidrive_driver::folder: sync of a folder halted space_id=s1 reason=the folder: {} is not a folder",
        ];
        let absolute_shapes = [
            "2026-11-12T06:29:23.000001Z  WARN koidrive_driver::folder: the folder cannot be observed space_id=s1 error=cannot read {}: No such file or directory (os error 2)",
            "2026-11-12T06:29:23.000001Z  INFO koidrive_driver::driver: linked a folder space_id=s1 path={}",
            "2026-11-12T06:29:23.000001Z  WARN koidrive_driver::folder: the folder fails its root check space_id=s1 problem={} is not a folder",
        ];
        let mut rng = fastrand::Rng::with_seed(262);
        for _ in 0..2000 {
            let mut path = String::new();
            for _ in 0..rng.usize(1..5) {
                path.push('/');
                for _ in 0..rng.usize(1..5) {
                    path.push_str(WORDS[rng.usize(..WORDS.len())]);
                    path.push_str(GLUE[rng.usize(..GLUE.len())]);
                }
                path.push_str(WORDS[rng.usize(..WORDS.len())]);
            }
            path.push_str(EXTENSIONS[rng.usize(..EXTENSIONS.len())]);
            let relative = rng.bool();
            if relative {
                path = format!("{:?}", Path::new(&path[1..]));
            }
            let shape = match relative || rng.bool() {
                true => relative_shapes[rng.usize(..relative_shapes.len())],
                false => absolute_shapes[rng.usize(..absolute_shapes.len())],
            };
            let mut redactor = Redactor::default();
            let out = redactor.log_line(&shape.replace("{}", &path));
            for word in WORDS {
                assert!(!out.contains(word), "{word} in {out}\nfrom {path}");
            }
            let problem = match relative {
                true => format!("the folder: {path} is not a folder"),
                false => format!("cannot read {path}: Permission denied (os error 13)"),
            };
            let problem = redactor.json(&json!({ "problem": problem }));
            assert!(
                WORDS.iter().all(|word| !problem.to_string().contains(word)),
                "{problem}"
            );
        }
    }

    /// The driver quotes a path relative to the folder, as in `the folder: "Secret Plans/budget.xlsx" is missing`.
    #[test]
    fn a_quoted_path_relative_to_the_folder_is_redacted_whole() {
        let mut redactor = Redactor::default();
        let quoted = |path: &str| format!("{:?}", Path::new(path));
        assert_eq!(
            redactor.text(&format!(
                "the folder: {} is missing",
                quoted("Secret Plans/budget 2026.xlsx")
            )),
            "the folder: \"<name-1>\" is missing"
        );
        assert_eq!(
            redactor.text(&format!(
                "the folder: {}: Permission denied (os error 13)",
                quoted("Jiyeon Passport/scan")
            )),
            "the folder: \"<name-2>\": Permission denied (os error 13)"
        );
        let out = redactor.text(&format!(
            "the folder: {} is not a folder",
            quoted("Kim \"Private\" Ledger")
        ));
        assert_eq!(out, "the folder: \"<name-3>\" is not a folder");
    }

    #[test]
    fn a_signal_keeps_its_name() {
        let mut redactor = Redactor::default();
        let line = "2026-11-12T06:29:23.000001Z  INFO koidrive_cli: stopping the daemon signal=\"SIGTERM\"";
        assert_eq!(redactor.log_line(line), line.replace("  INFO", " INFO"));
    }

    #[test]
    fn a_path_with_a_colon_in_a_name_runs_to_the_last_colon_before_the_error() {
        let mut redactor = Redactor::default();
        let out =
            redactor.text("cannot read /x/Meeting: Jiyeon/notes: Permission denied (os error 13)");
        assert_eq!(out, "cannot read <path-1>: Permission denied (os error 13)");
    }

    #[test]
    fn a_bare_file_name_and_an_ip_address_are_redacted() {
        let mut redactor = Redactor::default();
        let out =
            redactor.text("cannot upload 여권스캔.mp4 to 192.168.0.12:8787 or [fe80::1]:8787");
        for leak in ["여권스캔", "mp4", "192.168", "fe80"] {
            assert!(!out.contains(leak), "{leak} in {out}");
        }
        assert_eq!(
            redactor.text("retry in 1.5s, version 0.1.0"),
            "retry in 1.5s, version 0.1.0"
        );
    }

    #[test]
    fn koidrives_own_names_stay() {
        let mut redactor = Redactor::default();
        let text = "running (dev.koidrive.koi.1a2b3c4d) and koidrive.service, see config.toml, version 0.1.0";
        assert_eq!(redactor.text(text), text);
    }
}
