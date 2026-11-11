// SPDX-License-Identifier: Apache-2.0

//! Embeds the server bundle in the `KOIDRIVE_SERVER_BUNDLE` directory, which `server/scripts/pack-bundle.mjs` writes,
//! so that `koi setup` deploys the server of its own release (docs/design/cli-setup.md section 4.2). Without the
//! variable the binary embeds nothing, and deploy and upgrade say so.

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
