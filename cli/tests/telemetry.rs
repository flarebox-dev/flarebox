// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::process::{Command, Output};

#[test]
fn preview_round_trips_every_timing_field_and_drops_private_fixture_fields() {
    use koidrive_telemetry::{
        Recorder, Store,
        schema::{Counter, Measure},
    };
    use serde_json::{Value, json};

    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    let recorder = Recorder::on();
    for measure in Measure::all() {
        recorder.measure(measure, 1234);
        if matches!(
            measure,
            Measure::FirstSync(_) | Measure::RemoteApplyMilliseconds | Measure::SyncingMilliseconds
        ) {
            for millis in [60_000, 600_000, 5_280_000] {
                recorder.measure(measure, millis);
            }
        }
    }
    recorder.count(Counter::CpuMilliseconds, 4567);
    recorder.count(Counter::UnfinishedSyncing, 2);
    let pending = recorder.take();
    let expected = serde_json::to_value(&pending.days[&koidrive_telemetry::today()]).unwrap();
    let store = Store::new(&home.path().join("state/koidrive"));
    store.absorb(pending).unwrap();
    let queue = home.path().join("state/koidrive/telemetry/queue.json");
    let mut value: Value = serde_json::from_slice(&std::fs::read(&queue).unwrap()).unwrap();
    let day = &mut value["days"][koidrive_telemetry::today().to_string()];
    let secrets = [
        home.path().display().to_string(),
        "private-name.txt".into(),
        "private-space-id".into(),
        "1794527123456".into(),
    ];
    day["path"] = json!(secrets[0]);
    day["name"] = json!(secrets[1]);
    day["id"] = json!(secrets[2]);
    day["committed_at_ms"] = json!(1794527123456u64);
    day["counters"]["private-space-id"] = json!({ "private-name.txt": 1794527123456u64 });
    day["histograms"]["first_sync_ms"]["private-space-id"] =
        json!({"shift": 10, "buckets": vec![0;16], "count": 1, "sum": 1794527123456u64});
    day["histograms"]["syncing_ms"]["active_age"] =
        json!({"shift": 10, "buckets": vec![0;16], "count": 1, "sum": 1794527123456u64});
    std::fs::write(&queue, serde_json::to_vec(&value).unwrap()).unwrap();
    let preview = stdout(&koi(home.path(), &["telemetry", "preview"], &[]));
    let body: Value = serde_json::from_str(&preview).unwrap();
    let properties = &body["batch"][0]["properties"];
    assert_eq!(properties["c.process.cpu_ms"], 4567);
    assert_eq!(properties["c.sync.unfinished"], 2);
    assert!(
        !properties
            .as_object()
            .unwrap()
            .contains_key("h.syncing_ms.active_age")
    );
    for metric in [
        "h.first_sync_ms.0",
        "h.remote_apply_ms.effect",
        "h.syncing_ms.completed",
    ] {
        let histogram = &properties[metric];
        assert_eq!(histogram["shift"], 10);
        for bucket in [1, 6, 10, 13] {
            assert_eq!(histogram["buckets"][bucket], 1);
        }
        assert_eq!(histogram["count"], 4);
    }
    for (metric, labels) in expected["histograms"].as_object().unwrap() {
        for (label, histogram) in labels.as_object().unwrap() {
            assert_eq!(&properties[format!("h.{metric}.{label}")], histogram);
        }
    }
    fn private_free(value: &Value, secrets: &[String]) {
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    assert!(secrets.iter().all(|secret| !key.contains(secret)));
                    private_free(value, secrets);
                }
            }
            Value::Array(values) => {
                for value in values {
                    private_free(value, secrets);
                }
            }
            Value::String(text) => assert!(secrets.iter().all(|secret| !text.contains(secret))),
            Value::Number(number) => assert_ne!(number.as_u64(), Some(1794527123456)),
            _ => {}
        }
    }
    private_free(&body, &secrets);
}

fn koi(home: &Path, args: &[&str], vars: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_koi"));
    command
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env_remove("DO_NOT_TRACK")
        .env_remove("KOIDRIVE_TELEMETRY")
        .env("KOIDRIVE_TELEMETRY_URL", "http://127.0.0.1:9")
        .env("KOIDRIVE_UPDATE_CHECK", "0")
        .env_remove("KOIDRIVE_TELEMETRY_TOKEN")
        .env_remove("CI");
    for (name, value) in vars {
        command.env(name, value);
    }
    command.output().expect("run koi")
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn reports_are_off_until_someone_turns_them_on() {
    let home = tempfile::tempdir().unwrap();
    let status = stdout(&koi(home.path(), &["telemetry", "status"], &[]));
    assert!(
        status.contains("Usage reports: off (not asked yet)"),
        "{status}"
    );
    assert!(!home.path().join("state/koidrive/telemetry").exists());
}

#[test]
fn noninteractive_setup_never_asks_about_reports() {
    for (args, vars) in [
        (vec!["setup"], vec![]),
        (vec!["setup"], vec![("CI", "1")]),
        (vec!["setup", "--json"], vec![]),
    ] {
        let home = tempfile::tempdir().unwrap();
        let text = stdout(&koi(home.path(), &args, &vars));
        assert!(!text.contains("Send anonymous usage reports"), "{text}");
        assert!(!home.path().join("state/koidrive/telemetry").exists());
    }
}

#[test]
fn a_command_run_with_reports_on_is_counted_by_name_only() {
    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    stdout(&koi(home.path(), &["telemetry", "status"], &[]));
    let preview = stdout(&koi(home.path(), &["telemetry", "preview"], &[]));
    let body: serde_json::Value = serde_json::from_str(&preview).unwrap();
    let report = &body["batch"][0];
    assert_eq!(report["event"], "daily_report");
    assert_eq!(report["properties"]["c.command.telemetry"], 1);
    assert!(
        !preview.contains(&home.path().display().to_string()),
        "{preview}"
    );
}

#[test]
fn a_daemon_that_cannot_start_is_counted_by_why() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    let runtime = home.path().join("run/koidrive");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
    let daemon = koi(home.path(), &["_daemon"], &[]);
    assert!(!daemon.status.success());
    let preview = stdout(&koi(home.path(), &["telemetry", "preview"], &[]));
    let body: serde_json::Value = serde_json::from_str(&preview).unwrap();
    let report = &body["batch"][0]["properties"];
    assert_eq!(report["c.daemon_start_failed.socket"], 1, "{preview}");
    assert!(!preview.contains("run/koidrive"), "{preview}");
}

#[test]
fn off_deletes_the_id_and_what_waits_and_stays_off() {
    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    stdout(&koi(home.path(), &["telemetry", "status"], &[]));
    stdout(&koi(home.path(), &["telemetry", "off"], &[]));
    let status = stdout(&koi(home.path(), &["telemetry", "status"], &[]));
    assert!(
        status.contains("Usage reports: off (your choice)"),
        "{status}"
    );
    assert!(!status.contains("Install id"), "{status}");
    assert_eq!(
        stdout(&koi(home.path(), &["telemetry", "preview"], &[])).trim(),
        "Nothing waits to be sent."
    );
    assert!(
        !home
            .path()
            .join("state/koidrive/telemetry/queue.json")
            .exists()
    );
}

#[test]
fn do_not_track_and_the_variable_override_the_choice() {
    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    let status = stdout(&koi(
        home.path(),
        &["telemetry", "status"],
        &[("DO_NOT_TRACK", "1")],
    ));
    assert!(
        status.contains("Usage reports: off (DO_NOT_TRACK is set)"),
        "{status}"
    );
    let status = stdout(&koi(
        home.path(),
        &["telemetry", "status"],
        &[("KOIDRIVE_TELEMETRY", "0")],
    ));
    assert!(
        status.contains("Usage reports: off (KOIDRIVE_TELEMETRY is set)"),
        "{status}"
    );
}

#[test]
fn the_telemetry_flag_of_a_command_that_never_asks_stores_the_choice() {
    let home = tempfile::tempdir().unwrap();
    let joined = koi(
        home.path(),
        &["join", "not-a-code", "--telemetry", "on"],
        &[],
    );
    assert!(!joined.status.success());
    let status = stdout(&koi(home.path(), &["telemetry", "status"], &[]));
    assert!(
        status.contains("Usage reports: on (your choice)"),
        "{status}"
    );
    let preview = stdout(&koi(home.path(), &["telemetry", "preview"], &[]));
    assert!(
        preview.contains("\"event\": \"pair\"") || preview.contains("\"step\": \"pair\""),
        "{preview}"
    );
}

#[test]
fn the_variable_turns_reports_on_without_a_stored_choice() {
    let home = tempfile::tempdir().unwrap();
    let on = [("KOIDRIVE_TELEMETRY", "1")];
    stdout(&koi(home.path(), &["telemetry", "status"], &on));
    let preview = stdout(&koi(home.path(), &["telemetry", "preview"], &on));
    assert!(preview.contains("\"c.command.telemetry\": 1"), "{preview}");
}

#[test]
fn reset_draws_a_new_id() {
    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    let id = |home: &Path| {
        let status = stdout(&koi(home, &["telemetry", "status"], &[]));
        status
            .lines()
            .find_map(|line| line.strip_prefix("Install id: ").map(str::to_owned))
            .unwrap()
    };
    let before = id(home.path());
    assert_eq!(before.len(), 32);
    stdout(&koi(home.path(), &["telemetry", "reset"], &[]));
    assert_ne!(id(home.path()), before);
}
