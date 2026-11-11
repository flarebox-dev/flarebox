// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

fn koidrive(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_koi"))
        .args(args)
        .output()
        .expect("run koi")
}

#[test]
fn prints_its_version() {
    let output = koidrive(&["--version"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("koi {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn alone_or_with_a_group_alone_it_shows_help_and_succeeds() {
    for args in [&[][..], &["folder"], &["history"], &["server"]] {
        let output = koidrive(args);
        let help = String::from_utf8(output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(0), "{args:?}: {help}");
        assert!(help.contains("Usage: koi"), "{args:?}: {help}");
    }
    let folder = String::from_utf8(koidrive(&["folder"]).stdout).unwrap();
    assert!(folder.contains("Usage: koi folder"), "{folder}");
    let help = String::from_utf8(koidrive(&[]).stdout).unwrap();
    assert!(help.contains("Koidrive sync client"), "{help}");
    assert!(!help.contains("_daemon"), "{help}");
}
