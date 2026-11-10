// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::process::Command;

/// Logs written to anything but a terminal, such as the journal of the systemd unit, carry no ANSI escapes.
#[test]
fn logs_to_a_pipe_are_plain_text() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    fs::create_dir_all(config.join("flarebox")).unwrap();
    fs::write(
        config.join("flarebox/config.toml"),
        "[sync]\nexclude = [\"*.tmp\"]\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_flarebox"))
        .arg("status")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("NO_COLOR")
        .env_remove("FLAREBOX_LOG")
        .output()
        .expect("run flarebox");
    let log = String::from_utf8(output.stderr).unwrap();
    assert!(log.contains("no matcher applies them yet"), "{log}");
    assert!(!log.contains('\u{1b}'), "{log:?}");
}
