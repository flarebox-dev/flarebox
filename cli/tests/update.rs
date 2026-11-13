// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

mod schema;

fn asset() -> String {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        _ => "unknown-linux-musl",
    };
    format!("koidrive-{}-{os}.tar.gz", std::env::consts::ARCH)
}

struct GitHub {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

fn github(tag: &str) -> GitHub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let body = serde_json::json!([{
        "tag_name": tag,
        "html_url": format!("https://github.com/koidrive/koidrive/releases/tag/{tag}"),
        "draft": false,
        "assets": [{ "name": asset() }, { "name": "SHA256SUMS" }],
    }])
    .to_string();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            seen.lock().unwrap().push(request);
            let response = format!(
                "HTTP/1.1 200 OK\r\netag: \"e1\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    GitHub { base, requests }
}

fn executable(path: &Path, script: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn koi(home: &Path, github: &GitHub, args: &[&str]) -> Command {
    koi_at(Path::new(env!("CARGO_BIN_EXE_koi")), home, github, args)
}

fn koi_at(binary: &Path, home: &Path, github: &GitHub, args: &[&str]) -> Command {
    let mut command = Command::new(binary);
    command
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env("HOMEBREW_PREFIX", home.join("brew"))
        .env("KOIDRIVE_TEST_INSTALL_INVENTORY", home)
        .env("KOIDRIVE_UPDATE_CHECK_URL", &github.base)
        .env_remove("KOIDRIVE_UPDATE_CHECK")
        .env_remove("KOIDRIVE_LOG");
    command
}

fn envelope(output: &Output) -> Value {
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    schema::check(&envelope);
    envelope
}

const REPORT: &str = r#"{"schema_version":1,"command":"_reconcile","outcome":"ok","data":{"service":{"manager":"launchd","name":"dev.koidrive.koi","state":"running"}}}"#;

fn homebrew(home: &Path, version: &str) -> PathBuf {
    let opt = home.join("brew/opt/koidrive/bin/koi");
    let koi = |version: &str| {
        format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) echo 'koi {version}' ;;\n_reconcile) echo reconciled >> \"$0.calls\"; echo '{REPORT}' ;;\nesac\n"
        )
    };
    executable(&opt, &koi(version));
    let upgraded = koi("99.0.0").replace('\'', "'\\''");
    executable(
        &home.join("brew/bin/brew"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\n[ \"$1\" = upgrade ] && printf '%s' '{upgraded}' > '{}'\nexit 0\n",
            opt.display()
        ),
    );
    home.join("brew/bin/brew.calls")
}

#[test]
fn koi_update_upgrades_the_homebrew_formula_and_hands_the_service_to_the_new_koi() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let calls = homebrew(home.path(), "0.0.1");
    let output = koi(home.path(), &github, &["update", "--json"])
        .output()
        .unwrap();
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(
        envelope["data"],
        serde_json::json!({
            "from": "0.0.1",
            "to": "99.0.0",
            "via": "homebrew",
            "result": "updated",
            "service": { "manager": "launchd", "name": "dev.koidrive.koi", "state": "running" },
        })
    );
    assert_eq!(
        std::fs::read_to_string(calls).unwrap(),
        "update\nupgrade koidrive/tap/koidrive\n"
    );
    let requests = github.requests.lock().unwrap();
    assert!(
        requests[0].starts_with("GET /repos/koidrive/koidrive/releases?per_page=20 HTTP/1.1\r\n"),
        "{requests:?}"
    );
}

#[test]
fn koi_update_on_the_newest_release_changes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let calls = homebrew(home.path(), "99.0.0");
    let output = koi(home.path(), &github, &["update", "--json"])
        .output()
        .unwrap();
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["outcome"], "unchanged");
    assert_eq!(
        envelope["data"],
        serde_json::json!({ "from": "99.0.0", "to": "99.0.0", "via": "homebrew", "result": "unchanged", "service": { "manager": "launchd", "name": "dev.koidrive.koi", "state": "running" } })
    );
    assert!(!calls.exists(), "brew did not run");
    assert_eq!(
        std::fs::read_to_string(home.path().join("brew/opt/koidrive/bin/koi.calls")).unwrap(),
        "reconciled\n"
    );
}

#[test]
fn koi_update_preserves_disabled_login_items_and_only_fails_for_a_real_problem() {
    for problem in [None, Some("the service could not be registered")] {
        let home = tempfile::tempdir().unwrap();
        let github = github("v99.0.0");
        homebrew(home.path(), "0.0.1");
        let mut report: Value = serde_json::from_str(REPORT).unwrap();
        report["data"]["service"]["state"] = Value::from("switched_off");
        if let Some(problem) = problem {
            report["data"]["service"]["problem"] = Value::from(problem);
        }
        let brew = home.path().join("brew/bin/brew");
        let script = std::fs::read_to_string(&brew)
            .unwrap()
            .replace(REPORT, &report.to_string());
        executable(&brew, &script);
        let output = koi(home.path(), &github, &["update", "--json"])
            .output()
            .unwrap();
        let envelope = envelope(&output);
        assert_eq!(output.status.success(), problem.is_none(), "{envelope:#}");
        assert_eq!(envelope["data"]["result"], "updated");
        assert_eq!(envelope["data"]["service"]["state"], "switched_off");
        let output = koi(home.path(), &github, &["update"]).output().unwrap();
        assert_eq!(output.status.success(), problem.is_none());
        let text = String::from_utf8(output.stdout).unwrap();
        match problem {
            None => assert!(text.contains("Login Items"), "{text}"),
            Some(problem) => assert!(text.contains(problem), "{text}"),
        }
    }
}

#[test]
fn schema_one_status_without_an_update_notice_is_still_valid() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let output = koi(home.path(), &github, &["status", "--json"])
        .output()
        .unwrap();
    let mut envelope = envelope(&output);
    assert!(
        envelope["data"]
            .as_object_mut()
            .unwrap()
            .remove("update")
            .is_some()
    );
    schema::check(&envelope);
}

#[test]
fn koi_update_text_hides_the_reconcile_envelope_but_keeps_it_in_the_log() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    homebrew(home.path(), "99.0.0");
    let output = koi(home.path(), &github, &["update"]).output().unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{text}");
    assert!(
        text.contains("Koidrive 99.0.0 is the newest release."),
        "{text}"
    );
    assert!(!text.contains("schema_version"), "{text}");
    let log = if cfg!(target_os = "macos") {
        home.path().join("Library/Logs/Koidrive/update.log")
    } else {
        home.path().join("state/koidrive/update.log")
    };
    assert!(std::fs::read_to_string(log).unwrap().contains(REPORT));
}

#[test]
fn koi_update_rejects_a_successful_brew_that_leaves_the_old_binary_installed() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    homebrew(home.path(), "0.0.1");
    executable(&home.path().join("brew/bin/brew"), "#!/bin/sh\nexit 0\n");
    let started = Instant::now();
    let output = koi(home.path(), &github, &["update", "--json"])
        .output()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    let envelope = envelope(&output);
    assert!(!output.status.success(), "{envelope:#}");
    assert_eq!(envelope["outcome"], "error");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("0.0.1")
    );
    assert_ne!(envelope["data"]["result"], "updated");
    assert!(!home.path().join("brew/opt/koidrive/bin/koi.calls").exists());
}

#[test]
fn koi_update_reports_the_version_actually_installed_when_it_is_above_the_target() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    homebrew(home.path(), "0.0.1");
    let brew = home.path().join("brew/bin/brew");
    let script = std::fs::read_to_string(&brew)
        .unwrap()
        .replace("99.0.0", "100.0.0");
    executable(&brew, &script);
    let output = koi(home.path(), &github, &["update", "--json"])
        .output()
        .unwrap();
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["data"]["to"], "100.0.0");
    assert_eq!(envelope["data"]["result"], "updated");
}

#[test]
fn koi_update_uses_the_custom_homebrew_of_the_running_koi_without_a_prefix_variable() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let prefix = home.path().join("custombrew");
    let keg = prefix.join("Cellar/koidrive/0.1.0/bin/koi");
    std::fs::create_dir_all(keg.parent().unwrap()).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_koi"), &keg).unwrap();
    let opt = prefix.join("opt/koidrive");
    std::fs::create_dir_all(opt.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(keg.parent().unwrap().parent().unwrap(), &opt).unwrap();
    let brew = prefix.join("bin/brew");
    executable(&brew, "#!/bin/sh\necho \"$*\" >> \"$0.calls\"\nexit 0\n");
    let output = koi_at(&keg, home.path(), &github, &["update", "--json"])
        .env_remove("HOMEBREW_PREFIX")
        .output()
        .unwrap();
    let envelope = envelope(&output);
    assert!(!output.status.success());
    assert!(
        !envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Download Koidrive"),
        "{envelope:#}"
    );
    assert_eq!(
        std::fs::read_to_string(brew.with_extension("calls")).unwrap(),
        "update\nupgrade koidrive/tap/koidrive\n"
    );
}

#[test]
fn the_daemon_checks_for_a_release_and_koi_status_names_it() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let mut daemon = koi(home.path(), &github, &["_daemon"])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        let output = koi(home.path(), &github, &["status", "--json"])
            .output()
            .unwrap();
        let envelope = envelope(&output);
        if envelope["data"]["update"]["state"] == "available" || Instant::now() > deadline {
            break envelope;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let text = koi(home.path(), &github, &["status"]).output().unwrap();
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    let update = &status["data"]["update"];
    assert_eq!(update["state"], "available", "{status:#}");
    assert_eq!(update["latest"], "99.0.0");
    assert_eq!(update["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        update["url"],
        "https://github.com/koidrive/koidrive/releases/tag/v99.0.0"
    );
    assert_eq!(update["command"], "koi update");
    assert!(
        update["checked_at"].as_str().unwrap().ends_with('Z'),
        "{update}"
    );
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(
        text.contains(&format!(
            "Update: Koidrive 99.0.0 is out (this device runs {}). Run: koi update\n",
            env!("CARGO_PKG_VERSION")
        )),
        "{text}"
    );
    let requests = github.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "one check a day: {requests:?}");
    let mut headers: Vec<String> = (requests[0].lines().skip(1))
        .map(|line| line.split(':').next().unwrap().to_ascii_lowercase())
        .collect();
    headers.sort();
    assert_eq!(
        headers,
        ["accept", "host", "user-agent", "x-github-api-version"]
    );
}

#[test]
fn without_a_daemon_status_reads_the_last_check_and_the_switch_turns_it_off() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let state = home.path().join("state/koidrive");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("update-check.json"),
        r#"{"checked_at":1763006400000,"etag":"\"e1\"","release":{"version":"99.0.0","url":"https://example.com/v99.0.0"}}"#,
    )
    .unwrap();
    let output = koi(home.path(), &github, &["status", "--json"])
        .output()
        .unwrap();
    let update = envelope(&output)["data"]["update"].clone();
    assert_eq!(update["state"], "available");
    assert_eq!(update["checked_at"], "2025-11-13T04:00:00Z");

    let output = koi(home.path(), &github, &["status", "--json"])
        .env("KOIDRIVE_UPDATE_CHECK", "off")
        .output()
        .unwrap();
    let update = envelope(&output)["data"]["update"].clone();
    assert_eq!(
        update,
        serde_json::json!({ "state": "off", "version": env!("CARGO_PKG_VERSION") })
    );
    assert!(
        github.requests.lock().unwrap().is_empty(),
        "status asks nothing"
    );
}
