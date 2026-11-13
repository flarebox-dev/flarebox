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
    let body = serde_json::json!([{
        "tag_name": tag,
        "html_url": format!("https://github.com/koidrive/koidrive/releases/tag/{tag}"),
        "draft": false,
        "assets": [{ "name": asset() }, { "name": "SHA256SUMS" }],
    }])
    .to_string();
    http(body)
}

fn http(body: String) -> GitHub {
    http_before_reply(body, || {})
}

fn http_before_reply(body: String, before_reply: impl Fn() + Send + 'static) -> GitHub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
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
            before_reply();
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

fn development_version() -> bool {
    koidrive_driver::update::is_development_version(env!("CARGO_PKG_VERSION"))
}

fn development_update(output: &Output, home: &Path, github: &GitHub) -> bool {
    if !development_version() {
        return false;
    }
    let envelope = envelope(output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["outcome"], "unchanged");
    assert_eq!(envelope["next_commands"], serde_json::json!([]));
    assert_eq!(
        envelope["data"],
        serde_json::json!({
            "from": env!("CARGO_PKG_VERSION"),
            "to": env!("CARGO_PKG_VERSION"),
            "result": "unchanged",
        })
    );
    assert!(github.requests.lock().unwrap().is_empty());
    assert!(!home.join("brew/bin/brew.calls").exists());
    assert!(!home.join("brew/opt/koidrive/bin/koi.calls").exists());
    true
}

const REPORT: &str = r#"{"schema_version":1,"command":"_reconcile","outcome":"ok","data":{"service":{"manager":"launchd","name":"dev.koidrive.koi","state":"running"}}}"#;

fn copy_koi(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_koi"), path).unwrap();
    path.to_path_buf()
}

fn script_install(home: &Path, version: &str, report: &str) -> (PathBuf, GitHub) {
    let binary = copy_koi(&home.join(".local/bin/koi"));
    let helper = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi {version}' ;;\n_reconcile) echo '{report}' ;;\nesac\n"
    );
    let installer = http(format!(
        "printf '%s|%s\\n' \"$KOIDRIVE_INSTALL_DIR\" \"$KOIDRIVE_VERSION\" >> \"$KOIDRIVE_INSTALL_DIR/installer.calls\"\nprintf '%s' '{}' > \"$KOIDRIVE_INSTALL_DIR/koi.new\"\nchmod +x \"$KOIDRIVE_INSTALL_DIR/koi.new\"\nmv \"$KOIDRIVE_INSTALL_DIR/koi.new\" \"$KOIDRIVE_INSTALL_DIR/koi\"\n",
        helper.replace('\'', "'\\''")
    ));
    (binary, installer)
}

fn homebrew_binary(home: &Path, cask: bool) -> PathBuf {
    let prefix = home.join("brew");
    let binary = if cask {
        let app = home.join("Applications/Koidrive.app");
        let binary = copy_koi(&app.join("Contents/Helpers/koi"));
        let owned = prefix.join("Caskroom/koidrive/0.1.0/Koidrive.app");
        std::fs::create_dir_all(owned.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&app, owned).unwrap();
        binary
    } else {
        let binary = copy_koi(&prefix.join("Cellar/koidrive/0.1.0/bin/koi"));
        let opt = prefix.join("opt/koidrive");
        std::fs::create_dir_all(opt.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(binary.parent().unwrap().parent().unwrap(), opt).unwrap();
        binary
    };
    let link = prefix.join("bin/koi");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(binary, &link).unwrap();
    executable(
        &prefix.join("bin/brew"),
        "#!/bin/sh\necho called >> \"$0.calls\"\nexit 97\n",
    );
    link
}

fn update_log(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Logs/Koidrive/update.log")
    } else {
        home.join("state/koidrive/update.log")
    }
}

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
fn development_update_never_contacts_github_or_runs_an_installer() {
    if !development_version() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    homebrew(home.path(), "0.0.1");
    let script = home.path().join(".local/bin/koi");
    let app = home
        .path()
        .join("Applications/Koidrive.app/Contents/Helpers/koi");
    for path in [&script, &app] {
        executable(
            path,
            "#!/bin/sh\necho called >> \"$0.calls\"\necho 'koi 0.0.1'\n",
        );
    }
    for switch in ["on", "off"] {
        let output = koi(home.path(), &github, &["update", "--json"])
            .env("KOIDRIVE_UPDATE_CHECK", switch)
            .output()
            .unwrap();
        assert!(development_update(&output, home.path(), &github));
        assert!(!script.with_extension("calls").exists());
        assert!(!app.with_extension("calls").exists());
    }
    let output = koi(home.path(), &github, &["update"])
        .env("KOIDRIVE_UPDATE_CHECK_URL", "not a URL")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Development builds do not update to releases.\n"
    );
    assert!(github.requests.lock().unwrap().is_empty());
}

#[test]
fn koi_update_installs_a_release_and_hands_the_service_to_the_new_koi() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let (binary, installer) = script_install(home.path(), "99.0.0", REPORT);
    let calls = homebrew(home.path(), "100.0.0");
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(
        envelope["data"],
        serde_json::json!({
            "from": env!("CARGO_PKG_VERSION"),
            "to": "99.0.0",
            "via": "installer",
            "result": "updated",
            "service": { "manager": "launchd", "name": "dev.koidrive.koi", "state": "running" },
        })
    );
    assert_eq!(
        std::fs::read_to_string(binary.parent().unwrap().join("installer.calls")).unwrap(),
        format!(
            "{}|v99.0.0\n",
            std::fs::canonicalize(binary.parent().unwrap())
                .unwrap()
                .display()
        )
    );
    assert!(!calls.exists());
    assert_eq!(installer.requests.lock().unwrap().len(), 1);
    assert!(
        std::fs::read_to_string(binary.with_extension("calls"))
            .unwrap()
            .contains("_reconcile --json\n")
    );
    let requests = github.requests.lock().unwrap();
    assert!(
        requests[0].starts_with("GET /repos/koidrive/koidrive/releases?per_page=20 HTTP/1.1\r\n"),
        "{requests:?}"
    );
}

#[test]
fn koi_update_reconciles_a_concurrently_installed_newest_release_without_an_installer() {
    let home = tempfile::tempdir().unwrap();
    let (binary, installer) = script_install(home.path(), "99.0.0", REPORT);
    let replacement = binary.clone();
    let releases = serde_json::json!([{
        "tag_name": "v99.0.0",
        "html_url": "https://example.com/v99.0.0",
        "assets": [{ "name": asset() }, { "name": "SHA256SUMS" }],
    }]);
    let github = http_before_reply(releases.to_string(), move || {
        let staged = replacement.with_extension("new");
        executable(
            &staged,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi 99.0.0' ;;\n_reconcile) echo '{REPORT}' ;;\nesac\n"
            ),
        );
        std::fs::rename(staged, &replacement).unwrap();
    });
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["outcome"], "unchanged");
    assert_eq!(
        envelope["data"],
        serde_json::json!({
            "from": "99.0.0", "to": "99.0.0", "via": "installer", "result": "unchanged",
            "service": { "manager": "launchd", "name": "dev.koidrive.koi", "state": "running" },
        })
    );
    assert!(installer.requests.lock().unwrap().is_empty());
    assert!(
        std::fs::read_to_string(binary.with_extension("calls"))
            .unwrap()
            .contains("_reconcile --json\n")
    );
}

#[test]
fn koi_update_homebrew_guidance_changes_nothing_for_formula_cask_and_aliases() {
    for cask in [false, true] {
        for alias in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let github = github("v99.0.0");
            let binary = homebrew_binary(home.path(), cask);
            let target = std::fs::canonicalize(&binary).unwrap();
            let before = std::fs::read(&target).unwrap();
            let link_before = std::fs::read_link(&binary).unwrap();
            let app = home
                .path()
                .join("Applications/Koidrive.app/Contents/Helpers/koi");
            if !cask {
                executable(
                    &app,
                    "#!/bin/sh\necho called >> \"$0.calls\"\necho 'koi 100.0.0'\n",
                );
            }
            let app_before = std::fs::read(&app).unwrap();
            let ownership = home
                .path()
                .join("brew/Caskroom/koidrive/0.1.0/Koidrive.app");
            let ownership_before = std::fs::read_link(&ownership).ok();
            let (script, installer) = script_install(home.path(), "100.0.0", REPORT);
            let script_before = std::fs::read(&script).unwrap();
            let invoked = if alias {
                let first = home.path().join("alias-one");
                let second = home.path().join("alias-two");
                std::os::unix::fs::symlink(&binary, &first).unwrap();
                std::os::unix::fs::symlink(first, &second).unwrap();
                second
            } else {
                binary.clone()
            };
            let log = update_log(home.path());
            std::fs::create_dir_all(log.parent().unwrap()).unwrap();
            std::fs::write(&log, "existing update log\n").unwrap();
            let brew_command = if cask {
                vec!["brew", "upgrade", "--cask", "koidrive"]
            } else {
                vec!["brew", "upgrade", "koidrive"]
            };
            for switch in ["on", "off"] {
                let output = koi_at(&invoked, home.path(), &github, &["update", "--json"])
                    .env("KOIDRIVE_UPDATE_CHECK", switch)
                    .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
                    .output()
                    .unwrap();
                if !development_update(&output, home.path(), &github) {
                    let envelope = envelope(&output);
                    assert!(output.status.success(), "{envelope:#}");
                    assert_eq!(envelope["outcome"], "unchanged");
                    assert_eq!(
                        envelope["data"],
                        serde_json::json!({
                            "from": env!("CARGO_PKG_VERSION"),
                            "to": env!("CARGO_PKG_VERSION"),
                            "via": "homebrew",
                            "result": "unchanged",
                        })
                    );
                    assert_eq!(
                        envelope["next_commands"],
                        serde_json::json!([{ "argv": brew_command }])
                    );
                }
            }
            let output = koi_at(&invoked, home.path(), &github, &["update"])
                .env("KOIDRIVE_UPDATE_CHECK_URL", "not a URL")
                .env("KOIDRIVE_INSTALL_SCRIPT_URL", "not a URL")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                if development_version() {
                    "Development builds do not update to releases.\n".to_owned()
                } else {
                    format!(
                        "Koidrive is managed by Homebrew. To update, run: {}\n",
                        brew_command.join(" ")
                    )
                }
            );
            assert!(github.requests.lock().unwrap().is_empty());
            assert!(installer.requests.lock().unwrap().is_empty());
            assert!(!home.path().join("brew/bin/brew.calls").exists());
            assert!(!target.with_extension("calls").exists());
            assert!(!app.with_extension("calls").exists());
            assert!(!script.with_extension("calls").exists());
            assert!(!script.parent().unwrap().join("installer.calls").exists());
            assert_eq!(std::fs::read(&target).unwrap(), before);
            assert_eq!(std::fs::read(&app).unwrap(), app_before);
            assert_eq!(std::fs::read(&script).unwrap(), script_before);
            assert_eq!(std::fs::read_link(&binary).unwrap(), link_before);
            assert_eq!(std::fs::read_link(&ownership).ok(), ownership_before);
            assert_eq!(
                std::fs::read_to_string(log).unwrap(),
                "existing update log\n"
            );
            assert!(
                !home
                    .path()
                    .join("state/koidrive/update-check.json")
                    .exists()
            );
        }
    }
}

#[test]
fn koi_update_rechecks_foreign_and_legacy_caches_but_keeps_same_target_conditionals() {
    for cached_asset in [
        Some("koidrive-foreign-target.tar.gz".to_owned()),
        None,
        Some(asset()),
    ] {
        let home = tempfile::tempdir().unwrap();
        let (binary, installer) = script_install(home.path(), "99.0.0", REPORT);
        let state = home.path().join("state/koidrive");
        std::fs::create_dir_all(&state).unwrap();
        let mut cached = serde_json::json!({
            "checked_at": 1763006400000_i64,
            "successful_at": 1763006400000_i64,
            "etag": "\"cached\"",
            "release": { "version": "99.0.0", "url": "https://example.com/v99.0.0" },
        });
        if let Some(asset) = &cached_asset {
            cached["asset"] = asset.clone().into();
        }
        std::fs::write(state.join("update-check.json"), cached.to_string()).unwrap();
        if development_version() {
            let github = github("v99.0.0");
            let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
                .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
                .output()
                .unwrap();
            assert!(development_update(&output, home.path(), &github));
            continue;
        }
        let same_target = cached_asset.as_deref() == Some(asset().as_str());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let github = GitHub {
            base: format!("http://{}", listener.local_addr().unwrap()),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            let body = serde_json::json!([{
                "tag_name": "v99.0.0",
                "html_url": "https://example.com/v99.0.0",
                "assets": [{ "name": "koidrive-foreign-target.tar.gz" }],
            }])
            .to_string();
            let response = if same_target {
                "HTTP/1.1 304 Not Modified\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    .to_owned()
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            stream.write_all(response.as_bytes()).unwrap();
            request
        });
        let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
            .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
            .output()
            .unwrap();
        let request = server.join().unwrap().to_ascii_lowercase();
        assert_eq!(
            request.contains("if-none-match: \"cached\""),
            same_target,
            "{request}"
        );
        let envelope = envelope(&output);
        assert_eq!(output.status.success(), same_target, "{envelope:#}");
        if same_target {
            assert_eq!(envelope["outcome"], "ok");
            assert_eq!(envelope["data"]["to"], "99.0.0");
            assert_eq!(envelope["data"]["result"], "updated");
        } else {
            assert_eq!(envelope["outcome"], "error");
            assert_eq!(
                envelope["error"]["message"],
                "GitHub lists no Koidrive release for this system"
            );
        }
        assert_eq!(
            installer.requests.lock().unwrap().len(),
            usize::from(same_target)
        );
    }
}

#[test]
fn koi_update_preserves_disabled_login_items_and_only_fails_for_a_real_problem() {
    for problem in [None, Some("the service could not be registered")] {
        let home = tempfile::tempdir().unwrap();
        let github = github("v99.0.0");
        let mut report: Value = serde_json::from_str(REPORT).unwrap();
        report["data"]["service"]["state"] = Value::from("switched_off");
        if let Some(problem) = problem {
            report["data"]["service"]["problem"] = Value::from(problem);
        }
        let (binary, installer) = script_install(home.path(), "99.0.0", &report.to_string());
        let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
            .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
            .output()
            .unwrap();
        if development_update(&output, home.path(), &github) {
            continue;
        }
        let envelope = envelope(&output);
        assert_eq!(output.status.success(), problem.is_none(), "{envelope:#}");
        assert_eq!(envelope["data"]["result"], "updated");
        assert_eq!(envelope["data"]["service"]["state"], "switched_off");
        copy_koi(&binary);
        let output = koi_at(&binary, home.path(), &github, &["update"])
            .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
            .output()
            .unwrap();
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
    let (binary, installer) = script_install(home.path(), "99.0.0", REPORT);
    let output = koi_at(&binary, home.path(), &github, &["update"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{text}");
    if development_version() {
        assert_eq!(text, "Development builds do not update to releases.\n");
        assert!(github.requests.lock().unwrap().is_empty());
        assert!(!home.path().join("brew/bin/brew.calls").exists());
        assert!(!home.path().join("brew/opt/koidrive/bin/koi.calls").exists());
        return;
    }
    assert!(
        text.contains(&format!(
            "Updated Koidrive from {} to 99.0.0.",
            env!("CARGO_PKG_VERSION")
        )),
        "{text}"
    );
    assert!(!text.contains("schema_version"), "{text}");
    let log = update_log(home.path());
    assert!(std::fs::read_to_string(log).unwrap().contains(REPORT));
}

#[test]
fn koi_update_rejects_a_successful_installer_that_leaves_the_old_binary_installed() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(&home.path().join(".local/bin/koi"));
    let installer = http("exit 0\n".to_owned());
    let started = Instant::now();
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(!output.status.success(), "{envelope:#}");
    assert_eq!(envelope["outcome"], "error");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains(env!("CARGO_PKG_VERSION"))
    );
    assert_ne!(envelope["data"]["result"], "updated");
    assert!(!binary.with_extension("calls").exists());
    assert_eq!(installer.requests.lock().unwrap().len(), 1);
}

#[test]
fn koi_update_reports_the_version_actually_installed_when_it_is_above_the_target() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let (binary, installer) = script_install(home.path(), "100.0.0", REPORT);
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["data"]["to"], "100.0.0");
    assert_eq!(envelope["data"]["result"], "updated");
}

#[test]
fn koi_update_through_a_script_alias_installs_into_the_canonical_directory() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let (original, installer) = script_install(home.path(), "99.0.0", REPORT);
    let directory = home.path().join("standalone install/bin");
    std::fs::create_dir_all(directory.parent().unwrap()).unwrap();
    std::fs::rename(original.parent().unwrap(), &directory).unwrap();
    let binary = directory.join("koi");
    let first = home.path().join("first-alias");
    let alias = home.path().join("second-alias");
    std::os::unix::fs::symlink(&binary, &first).unwrap();
    std::os::unix::fs::symlink(&first, &alias).unwrap();
    let calls = homebrew(home.path(), "100.0.0");
    let output = koi_at(&alias, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["data"]["via"], "installer");
    assert_eq!(envelope["data"]["result"], "updated");
    assert_eq!(envelope["data"]["to"], "99.0.0");
    assert_eq!(
        std::fs::read_to_string(directory.join("installer.calls")).unwrap(),
        format!(
            "{}|v99.0.0\n",
            std::fs::canonicalize(&directory).unwrap().display()
        )
    );
    assert!(
        std::fs::read_to_string(binary.with_extension("calls"))
            .unwrap()
            .contains("_reconcile --json\n")
    );
    assert_eq!(std::fs::read_link(&alias).unwrap(), first);
    assert_eq!(std::fs::read_link(&first).unwrap(), binary);
    assert!(!calls.exists());
    assert!(!home.path().join("koi").exists());
    assert!(!original.exists());
}

#[test]
fn koi_update_keeps_a_direct_app_helper_on_the_manual_download_path() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(
        &home
            .path()
            .join("Applications/Koidrive.app/Contents/Helpers/koi"),
    );
    let before = std::fs::read(&binary).unwrap();
    let (script, installer) = script_install(home.path(), "99.0.0", REPORT);
    let script_before = std::fs::read(&script).unwrap();
    let calls = homebrew(home.path(), "100.0.0");
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let response = envelope(&output);
    assert!(!output.status.success(), "{response:#}");
    assert_eq!(response["outcome"], "error");
    assert_eq!(
        response["error"]["message"],
        "Download Koidrive 99.0.0 from https://github.com/koidrive/koidrive/releases/tag/v99.0.0"
    );
    assert_eq!(response["next_commands"], serde_json::json!([]));
    assert_eq!(github.requests.lock().unwrap().len(), 1);
    assert!(installer.requests.lock().unwrap().is_empty());
    assert!(!calls.exists());
    assert!(!script.with_extension("calls").exists());
    assert!(!binary.with_extension("calls").exists());
    assert_eq!(std::fs::read(&binary).unwrap(), before);
    assert_eq!(std::fs::read(&script).unwrap(), script_before);
    let state = home.path().join("state/koidrive");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("update-check.json"),
        serde_json::json!({
            "asset": asset(),
            "checked_at": 1763006400000_i64,
            "release": {
                "version": "99.0.0",
                "url": "https://github.com/koidrive/koidrive/releases/tag/v99.0.0",
            },
        })
        .to_string(),
    )
    .unwrap();
    let output = koi_at(&binary, home.path(), &github, &["status", "--json"])
        .output()
        .unwrap();
    let update = envelope(&output)["data"]["update"].clone();
    assert_eq!(update["state"], "available");
    assert!(update.get("command").is_none(), "{update:#}");
    let output = koi_at(&binary, home.path(), &github, &["status"])
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("https://github.com/koidrive/koidrive/releases/tag/v99.0.0"),
        "{text}"
    );
    assert!(!text.contains("Run: koi update"), "{text}");
    assert_eq!(github.requests.lock().unwrap().len(), 1);
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
    if development_update(&output, home.path(), &github) {
        assert!(!brew.with_extension("calls").exists());
        return;
    }
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["outcome"], "unchanged");
    assert_eq!(
        envelope["data"],
        serde_json::json!({
            "from": env!("CARGO_PKG_VERSION"), "to": env!("CARGO_PKG_VERSION"),
            "via": "homebrew", "result": "unchanged",
        })
    );
    assert_eq!(
        envelope["next_commands"],
        serde_json::json!([{ "argv": ["brew", "upgrade", "koidrive"] }])
    );
    assert!(!brew.with_extension("calls").exists());
    assert!(github.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_daemon_checks_for_a_release_and_koi_status_names_it() {
    use tokio::io::AsyncBufReadExt;

    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(&home.path().join(".local/bin/koi"));
    let mut daemon = tokio::process::Command::from(koi_at(
        &binary,
        home.path(),
        &github,
        &["_daemon", "--json"],
    ))
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    let mut events = tokio::io::BufReader::new(daemon.stdout.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(30), async {
        let started = events.next_line().await.unwrap().unwrap();
        let started: Value = serde_json::from_str(&started).unwrap();
        schema::check(&started);
        assert_eq!(started["event"], "started", "{started}");
        let mut changes = koidrive_ipc::watch(
            Path::new(started["socket"].as_str().unwrap()),
            rustix::process::getuid().as_raw(),
        )
        .await
        .unwrap()
        .unwrap();
        loop {
            let response = changes.next().await.unwrap().unwrap();
            let koidrive_ipc::Response::State { status, .. } = response else {
                panic!("{response:?}");
            };
            if matches!(
                (status.update, development_version()),
                (koidrive_driver::Update::Off, true)
                    | (koidrive_driver::Update::Available { .. }, false)
            ) {
                break;
            }
        }
    })
    .await
    .expect("the daemon starts and publishes its release check");
    let output = koi_at(&binary, home.path(), &github, &["status", "--json"])
        .output()
        .unwrap();
    let status = envelope(&output);
    let text = koi_at(&binary, home.path(), &github, &["status"])
        .output()
        .unwrap();
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
    assert!(output.status.success(), "{status:#}");
    assert_eq!(status["data"]["running"], true, "{status:#}");
    let update = &status["data"]["update"];
    if development_version() {
        assert_eq!(
            update,
            &serde_json::json!({ "state": "off", "version": env!("CARGO_PKG_VERSION") })
        );
        assert!(!String::from_utf8(text.stdout).unwrap().contains("Update:"));
        assert!(github.requests.lock().unwrap().is_empty());
        return;
    }
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
    let binary = copy_koi(&home.path().join(".local/bin/koi"));
    let state = home.path().join("state/koidrive");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("update-check.json"),
        serde_json::json!({
            "asset": asset(),
            "checked_at": 1763006400000_i64,
            "etag": "\"e1\"",
            "release": { "version": "99.0.0", "url": "https://example.com/v99.0.0" },
        })
        .to_string(),
    )
    .unwrap();
    let output = koi_at(&binary, home.path(), &github, &["status", "--json"])
        .output()
        .unwrap();
    let update = envelope(&output)["data"]["update"].clone();
    if development_version() {
        assert_eq!(
            update,
            serde_json::json!({ "state": "off", "version": env!("CARGO_PKG_VERSION") })
        );
    } else {
        assert_eq!(update["state"], "available");
        assert_eq!(update["checked_at"], "2025-11-13T04:00:00Z");
    }

    let output = koi_at(&binary, home.path(), &github, &["status", "--json"])
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

#[test]
fn cached_status_uses_the_current_homebrew_formula_or_cask_upgrade_command() {
    for cask in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let github = github("v99.0.0");
        let binary = homebrew_binary(home.path(), cask);
        let script = home.path().join(".local/bin/koi");
        executable(
            &script,
            "#!/bin/sh\necho called >> \"$0.calls\"\necho 'koi 100.0.0'\n",
        );
        let alias = home.path().join("status-alias");
        std::os::unix::fs::symlink(&binary, &alias).unwrap();
        let state = home.path().join("state/koidrive");
        std::fs::create_dir_all(&state).unwrap();
        let cached = serde_json::json!({
            "asset": asset(),
            "checked_at": 1763006400000_i64,
            "etag": "\"e1\"",
            "release": { "version": "99.0.0", "url": "https://example.com/v99.0.0" },
        })
        .to_string();
        std::fs::write(state.join("update-check.json"), &cached).unwrap();
        let output = koi_at(&alias, home.path(), &github, &["status", "--json"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let update = envelope(&output)["data"]["update"].clone();
        let output = koi_at(&alias, home.path(), &github, &["status"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        if development_version() {
            assert_eq!(
                update,
                serde_json::json!({ "state": "off", "version": env!("CARGO_PKG_VERSION") })
            );
            assert!(!text.contains("Update:"), "{text}");
        } else {
            let command = if cask {
                "brew upgrade --cask koidrive"
            } else {
                "brew upgrade koidrive"
            };
            assert_eq!(
                update,
                serde_json::json!({
                    "state": "available", "version": env!("CARGO_PKG_VERSION"),
                    "latest": "99.0.0", "url": "https://example.com/v99.0.0",
                    "checked_at": "2025-11-13T04:00:00Z", "command": command,
                })
            );
            assert!(
                text.contains(&format!(
                    "Update: Koidrive 99.0.0 is out (this device runs {}). Run: {command}\n",
                    env!("CARGO_PKG_VERSION")
                )),
                "{text}"
            );
            assert!(!text.contains("Run: koi update"), "{text}");
        }
        assert!(github.requests.lock().unwrap().is_empty());
        assert!(!script.with_extension("calls").exists());
        assert!(!home.path().join("brew/bin/brew.calls").exists());
        assert_eq!(
            std::fs::read_to_string(state.join("update-check.json")).unwrap(),
            cached
        );
    }
}
