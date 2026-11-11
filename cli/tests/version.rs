// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

#[test]
fn prints_its_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_koi"))
        .output()
        .expect("run koi");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("koi {}\n", env!("CARGO_PKG_VERSION"))
    );
}
