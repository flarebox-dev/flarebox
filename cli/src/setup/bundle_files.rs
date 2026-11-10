// SPDX-License-Identifier: Apache-2.0

//! The files of a server bundle directory, which `build.rs` embeds. It applies the checks of `Bundle::from_dir` at
//! build time, so a release cannot embed a file from outside the directory.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// Every file under `dir` with its path relative to `dir`, sorted. A symbolic link, an entry that is neither a regular
/// file nor a directory, a missing `manifest.json` or a `main_module` that is not a plain file name stops it.
pub fn bundle_files(dir: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let mut files = Vec::new();
    collect(dir, dir, &mut files)?;
    files.sort();
    let (_, manifest) = files
        .iter()
        .find(|(relative, _)| relative == "manifest.json")
        .ok_or("the bundle holds no manifest.json")?;
    let manifest: serde_json::Value = fs::read(manifest)
        .map_err(|error| error.to_string())
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|error| error.to_string()))
        .map_err(|error| format!("manifest.json: {error}"))?;
    let module = manifest["main_module"]
        .as_str()
        .ok_or("manifest.json names no main_module")?;
    let mut components = Path::new(module).components();
    if !matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    ) {
        return Err(format!(
            "the main_module {module:?} is not a plain file name"
        ));
    }
    Ok(files)
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|error| format!("{}: {error}", dir.display()))?
            .path();
        let kind = fs::symlink_metadata(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .file_type();
        if kind.is_dir() {
            collect(root, &path, files)?;
        } else if kind.is_file() {
            let relative = path
                .strip_prefix(root)
                .expect("a directory entry is under the root")
                .to_str()
                .ok_or_else(|| format!("{} is not a UTF-8 path", path.display()))?
                .to_owned();
            files.push((relative, path));
        } else {
            return Err(format!(
                "{} is not a regular file or directory; a bundle holds no links",
                path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(module: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("manifest.json"),
            serde_json::json!({ "main_module": module }).to_string(),
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("worker")).unwrap();
        fs::write(dir.path().join("worker/index.js"), "export default {}").unwrap();
        fs::create_dir_all(dir.path().join("assets/app")).unwrap();
        fs::write(dir.path().join("assets/app/index.html"), "<html>").unwrap();
        dir
    }

    #[test]
    fn every_file_is_listed_by_its_relative_path() {
        let dir = bundle("index.js");
        let files = bundle_files(dir.path()).unwrap();
        let names: Vec<&str> = files
            .iter()
            .map(|(relative, _)| relative.as_str())
            .collect();
        assert_eq!(
            names,
            ["assets/app/index.html", "manifest.json", "worker/index.js"]
        );
        assert!(
            files
                .iter()
                .all(|(relative, path)| *path == dir.path().join(relative))
        );
    }

    #[test]
    fn a_symbolic_link_stops_the_build() {
        let dir = bundle("index.js");
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("assets/app/secret")).unwrap();
        let error = bundle_files(dir.path()).unwrap_err();
        assert!(
            error.contains("assets/app/secret is not a regular file"),
            "{error}"
        );
    }

    #[test]
    fn a_linked_directory_stops_the_build() {
        let dir = bundle("index.js");
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("assets/more")).unwrap();
        assert!(bundle_files(dir.path()).is_err());
    }

    #[test]
    fn a_socket_stops_the_build() {
        let dir = bundle("index.js");
        let _socket =
            std::os::unix::net::UnixListener::bind(dir.path().join("worker/socket")).unwrap();
        let error = bundle_files(dir.path()).unwrap_err();
        assert!(
            error.contains("worker/socket is not a regular file"),
            "{error}"
        );
    }

    #[test]
    fn a_module_outside_worker_stops_the_build() {
        for module in ["../index.js", "/etc/passwd", "worker/index.js", ""] {
            let dir = bundle(module);
            let error = bundle_files(dir.path()).unwrap_err();
            assert!(
                error.contains("is not a plain file name"),
                "{module}: {error}"
            );
        }
    }

    #[test]
    fn a_bundle_without_a_manifest_stops_the_build() {
        let dir = bundle("index.js");
        fs::remove_file(dir.path().join("manifest.json")).unwrap();
        assert_eq!(
            bundle_files(dir.path()).unwrap_err(),
            "the bundle holds no manifest.json"
        );
    }
}
