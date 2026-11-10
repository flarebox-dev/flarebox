// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The profile's configuration file (`config.toml`). It is device-local: never synced, and it holds no secrets.
///
/// ```toml
/// [sync]
/// include = ["Projects/", "Notes/2026"]
/// exclude = ["*.iso", "build/"]
///
/// [trash]
/// max_size_mb = 2048
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub sync: SyncRules,
    #[serde(default)]
    pub trash: LocalTrash,
}

/// The local trash of every folder the profile links (design §5.2), which holds what sync effects displaced. An
/// entry leaves it after [`flarebox_fs::TRASH_RETENTION`], or earlier, oldest first, while the folder's trash holds
/// more than `max_size_mb` megabytes (2048 when unset).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalTrash {
    pub max_size_mb: u64,
}

impl Default for LocalTrash {
    fn default() -> Self {
        LocalTrash { max_size_mb: 2048 }
    }
}

impl LocalTrash {
    /// The cap in bytes.
    pub fn cap(&self) -> u64 {
        self.max_size_mb.saturating_mul(1_000_000)
    }
}

/// This device's own selective sync (H13 of reports/BOX-71), applied to every folder the profile links. It saves
/// bandwidth and disk, not access: the server still sends this device every name in the space, and its credential
/// can still read everything.
///
/// - `include`: paths relative to the folder's root. When the list is not empty, only these paths and what is under
///   them are synced.
/// - `exclude`: patterns in `.fbignore` syntax (`.gitignore` syntax, design §5.10), relative to the folder's root.
///
/// Both turn into the same matcher input as a `.fbignore` file (see [`SyncRules::lines`]), so they follow the
/// same rule: becoming ignored never deletes anything, on either side.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRules {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path} is not valid: {reason}")]
    Invalid { path: PathBuf, reason: String },
}

impl Config {
    /// Reads the file, or returns the default configuration when there is none. Unknown keys and rules that cannot
    /// be turned into matcher input are refused, so a typo never silently syncs more than intended.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let invalid = |reason: String| ConfigError::Invalid {
            path: path.to_path_buf(),
            reason,
        };
        let config: Config = toml::from_str(&text).map_err(|err| invalid(err.to_string()))?;
        config.sync.lines().map_err(invalid)?;
        Ok(config)
    }
}

/// One folder of the include list, with the folders it holds that are included one by one.
#[derive(Default)]
struct Included {
    whole: bool,
    children: BTreeMap<String, Included>,
}

impl SyncRules {
    /// The rules as lines of a root `.fbignore` file. An include list becomes `/*` followed by negations that
    /// re-include each listed path and the folders leading to it, since a path cannot be re-included once one of
    /// its parents is excluded. The exclude patterns follow, so they win over the include list.
    pub fn lines(&self) -> Result<Vec<String>, String> {
        let mut lines = Vec::new();
        if !self.include.is_empty() {
            let mut root = Included::default();
            for path in &self.include {
                let mut node = &mut root;
                for part in include_parts(path)? {
                    node = node.children.entry(part.to_owned()).or_default();
                }
                node.whole = true;
            }
            lines.push("/*".to_owned());
            reinclude(&root, "", &mut lines);
        }
        for pattern in &self.exclude {
            if pattern.trim().is_empty() || pattern.contains(['\n', '\r']) {
                return Err(format!(
                    "exclude pattern {pattern:?} is empty or spans lines"
                ));
            }
            if pattern.starts_with('!') {
                return Err(format!(
                    "exclude pattern {pattern:?} starts with '!'; list the path under include instead"
                ));
            }
            if pattern.starts_with('#') {
                return Err(format!(
                    "exclude pattern {pattern:?} starts with '#', which makes it a comment; write \\# for a literal '#'"
                ));
            }
            lines.push(pattern.clone());
        }
        Ok(lines)
    }
}

fn include_parts(path: &str) -> Result<Vec<&str>, String> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.iter().any(|part| matches!(*part, "" | "." | "..")) || path.contains(['\n', '\r']) {
        return Err(format!(
            "include path {path:?} must be a path inside the folder, such as \"Projects/\""
        ));
    }
    Ok(parts)
}

fn reinclude(node: &Included, prefix: &str, lines: &mut Vec<String>) {
    for (name, child) in &node.children {
        let path = format!("{prefix}/{}", escape(name));
        if child.whole {
            lines.push(format!("!{path}"));
        } else {
            lines.push(format!("!{path}/"));
            lines.push(format!("{path}/*"));
            reinclude(child, &path, lines);
        }
    }
}

/// Escapes a name so that `.gitignore` syntax reads it literally.
fn escape(name: &str) -> String {
    let mut escaped = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '*' | '?' | '[' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    if escaped.ends_with(' ') {
        escaped.insert(escaped.len() - 1, '\\');
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(include: &[&str], exclude: &[&str]) -> SyncRules {
        SyncRules {
            include: include.iter().map(|s| s.to_string()).collect(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn no_rules_mean_no_lines() {
        assert!(SyncRules::default().lines().unwrap().is_empty());
    }

    #[test]
    fn includes_reinclude_each_path_and_its_parents() {
        let lines = rules(
            &["Projects/", "Docs/a/b", "/notes.txt", "Docs/a/b/c"],
            &["*.iso"],
        )
        .lines()
        .unwrap();
        assert_eq!(
            lines,
            [
                "/*",
                "!/Docs/",
                "/Docs/*",
                "!/Docs/a/",
                "/Docs/a/*",
                "!/Docs/a/b",
                "!/Projects",
                "!/notes.txt",
                "*.iso",
            ]
        );
    }

    #[test]
    fn a_whole_folder_covers_paths_listed_inside_it() {
        assert_eq!(rules(&["a/b", "a"], &[]).lines().unwrap(), ["/*", "!/a"]);
    }

    #[test]
    fn included_names_are_matched_literally() {
        assert_eq!(
            rules(&["[draft] *?\\x "], &[]).lines().unwrap(),
            ["/*", "!/\\[draft] \\*\\?\\\\x\\ "]
        );
    }

    #[test]
    fn paths_outside_the_folder_and_negated_excludes_are_refused() {
        for bad in ["", "/", "../x", "a/../b", "a//b", "./a"] {
            assert!(rules(&[bad], &[]).lines().is_err(), "{bad:?}");
        }
        for bad in ["", "  ", "!keep", "#backup", "a\nb"] {
            assert!(rules(&[], &[bad]).lines().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_file_is_parsed_strictly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());

        std::fs::write(
            &path,
            "[sync]\ninclude = [\"Projects/\"]\nexclude = [\"*.tmp\"]\n",
        )
        .unwrap();
        assert_eq!(
            Config::load(&path).unwrap().sync,
            rules(&["Projects/"], &["*.tmp"])
        );

        std::fs::write(&path, "[sync]\ninclude = [\"Projects/\"]\nexlude = []\n").unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Invalid { .. })
        ));

        std::fs::write(&path, "[sync]\ninclude = [\"../outside\"]\n").unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Invalid { .. })
        ));

        assert_eq!(Config::default().trash.cap(), 2_048_000_000);
        std::fs::write(&path, "[trash]\nmax_size_mb = 10\n").unwrap();
        assert_eq!(Config::load(&path).unwrap().trash.cap(), 10_000_000);
        std::fs::write(&path, "[trash]\nmax_size_gb = 10\n").unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Invalid { .. })
        ));
    }
}
