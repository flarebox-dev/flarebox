// SPDX-License-Identifier: Apache-2.0

use std::fmt::Write;
use std::path::{Path, PathBuf};

#[path = "build/plan.rs"]
mod plan;

fn sql_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut files: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("reading {}: {error}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.is_file() && !path.file_name().unwrap().to_string_lossy().starts_with('.')
        })
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".sql"))
                .unwrap_or_else(|| panic!("{} is not a .sql migration", path.display()))
                .to_owned();
            (name, path)
        })
        .collect();
    files.sort();
    files
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    println!("cargo::rerun-if-changed={}", root.display());
    let legacy = sql_files(&root.join("legacy"));
    let named = sql_files(&root);
    let sql: Vec<String> = named
        .iter()
        .map(|(_, path)| std::fs::read_to_string(path).unwrap())
        .collect();
    let planned = plan::plan(
        &legacy
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        &named
            .iter()
            .zip(&sql)
            .map(|((name, _), sql)| (name.as_str(), sql.as_str()))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|error| panic!("core/store/migrations: {error}"));
    let mut out = String::from("pub const MIGRATIONS: &[Migration] = &[\n");
    for planned in &planned {
        let path = legacy
            .iter()
            .chain(&named)
            .find(|(name, _)| *name == planned.name)
            .map(|(_, path)| path)
            .unwrap();
        writeln!(
            out,
            "    Migration {{ name: {:?}, requires: &{:?}, sql: include_str!({:?}) }},",
            planned.name,
            planned.requires,
            path.to_str().unwrap()
        )
        .unwrap();
    }
    writeln!(out, "];\npub const LEGACY_STEPS: usize = {};", legacy.len()).unwrap();
    let dest = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("migrations.rs");
    std::fs::write(dest, out).unwrap();
}
