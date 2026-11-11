// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

/// Logs written to anything but a terminal, such as the journal of the systemd unit, carry no ANSI escapes.
#[test]
fn logs_to_a_pipe_are_plain_text() {
    let home = tempfile::tempdir().unwrap();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_koi"))
        .arg("daemon")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("XDG_RUNTIME_DIR", home.path().join("run"))
        .env_remove("NO_COLOR")
        .env_remove("KOIDRIVE_LOG")
        .stderr(Stdio::piped())
        .spawn()
        .expect("run koi");
    let mut log = String::new();
    BufReader::new(daemon.stderr.take().unwrap())
        .read_line(&mut log)
        .unwrap();
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    assert!(log.contains("the daemon is running"), "{log}");
    assert!(!log.contains('\u{1b}'), "{log:?}");
}
