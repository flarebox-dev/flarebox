// SPDX-License-Identifier: Apache-2.0

#[path = "src/setup/bundle_files.rs"]
mod bundle_files;

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-env-changed=KOIDRIVE_SERVER_BUNDLE");
    let files = match env::var_os("KOIDRIVE_SERVER_BUNDLE") {
        Some(dir) => {
            let dir = fs::canonicalize(&dir).unwrap_or_else(|error| {
                panic!(
                    "KOIDRIVE_SERVER_BUNDLE={}: {error}",
                    PathBuf::from(&dir).display()
                )
            });
            println!("cargo::rerun-if-changed={}", dir.display());
            let manifest: serde_json::Value = serde_json::from_slice(
                &fs::read(dir.join("manifest.json"))
                    .expect("the server bundle needs manifest.json"),
            )
            .expect("the server bundle manifest must be JSON");
            assert_eq!(
                manifest["version"].as_str(),
                Some(env!("CARGO_PKG_VERSION")),
                "the server bundle must match the CLI release version; repack it with server/scripts/pack-bundle.mjs"
            );
            bundle_files::bundle_files(&dir)
                .unwrap_or_else(|error| panic!("KOIDRIVE_SERVER_BUNDLE={}: {error}", dir.display()))
        }
        None => Vec::new(),
    };
    let mut code = String::from(
        "/// The embedded server bundle's files, by path relative to the bundle.\npub const SERVER_BUNDLE: &[(&str, &[u8])] = &[\n",
    );
    for (relative, path) in &files {
        code += &format!("    ({relative:?}, include_bytes!({path:?})),\n");
    }
    code += "];\n";
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    fs::write(out.join("server_bundle.rs"), code).expect("OUT_DIR is writable");
}
