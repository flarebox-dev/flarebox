// SPDX-License-Identifier: Apache-2.0

//! The ignore rules of a linked folder as the driver holds them (design §5.10). The matcher is the engine's
//! ([`flarebox_engine::ignore`]): the built-in exclusions, this device's `[sync]` list and the `.fbignore` files of
//! the folder. [`matcher`] builds the part that is the same for every folder of the profile, and `scan_rules` gives
//! `core/fs` the form its scans and watchers apply, which also reads the `.fbignore` files. The engine's trees take the
//! same list through [`local_rules`] and the same files with each observation, so a scan leaves out exactly the paths
//! the engine excludes.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::Arc;

use flarebox_engine::ignore::{LocalRules, Matcher as Rules, RuleFile};
use flarebox_engine::names::name_key;
use flarebox_fs::{EntryKind, Ignore, Ignored};
use flarebox_ignore::{Kind, LocalRulesError, Reason, Status};

use crate::config::SyncRules;

/// Decides whether a path of a linked folder is excluded on this device, before any `.fbignore` file is read.
pub trait Matcher: Send + Sync {
    /// `path` is relative to the folder's root; `dir` says whether it names a folder.
    fn is_excluded(&self, path: &Path, dir: bool) -> bool;
}

/// Excludes nothing of its own. The built-in exclusions and the `.fbignore` files still apply to a scan.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unfiltered;

impl Matcher for Unfiltered {
    fn is_excluded(&self, _path: &Path, _dir: bool) -> bool {
        false
    }
}

/// The names of a relative path. A name that is not UTF-8 is no name the server can hold, so how it is spelled here
/// does not matter.
fn names(path: &Path) -> Vec<String> {
    path.iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

fn status(rules: &Rules, path: &Path, dir: bool) -> Status {
    let kind = if dir { Kind::Dir } else { Kind::File };
    rules.status(&names(path), kind)
}

/// The built-in exclusions and this device's list.
struct Device(Rules);

impl Matcher for Device {
    fn is_excluded(&self, path: &Path, dir: bool) -> bool {
        status(&self.0, path, dir) != Status::Included
    }
}

/// This device's `[sync]` list in the form the matcher takes, for the engine's trees as well. An include path is
/// read as [`SyncRules::lines`] reads it, with any number of slashes around it.
pub fn local_rules(sync: &SyncRules) -> Result<LocalRules, LocalRulesError> {
    let include: Vec<String> = sync
        .include
        .iter()
        .map(|path| {
            let dir = if path.ends_with('/') { "/" } else { "" };
            format!("{}{dir}", path.trim_matches('/'))
        })
        .collect();
    LocalRules::new(&include, &sync.exclude)
}

/// The matcher of this device's `[sync]` list and the built-in exclusions.
pub fn matcher(sync: &SyncRules) -> Result<Arc<dyn Matcher>, LocalRulesError> {
    Ok(Arc::new(Device(Rules::new(local_rules(sync)?))))
}

/// What `core/fs` asks while it scans: the device's matcher, and the `.fbignore` files the scan has read so far.
struct Scanned {
    device: Arc<dyn Matcher>,
    files: Rules,
}

impl Ignore for Scanned {
    fn set_rules(&mut self, dir: &Path, rules: Option<&[u8]>) {
        let rules = rules.map(RuleFile::parse);
        self.files.set_rules(&names(dir), rules, std::iter::empty());
    }

    fn status(&self, path: &Path, kind: EntryKind) -> Ignored {
        let dir = kind == EntryKind::Dir;
        match status(&self.files, path, dir) {
            Status::Excluded(Reason::BuiltIn) => Ignored::BuiltIn,
            Status::Excluded(_) => Ignored::Rule,
            Status::Included if self.device.is_excluded(path, dir) => Ignored::Rule,
            Status::Included => Ignored::No,
        }
    }

    fn key(&self, name: &OsStr) -> OsString {
        name_key(&name.to_string_lossy()).into()
    }
}

/// The rules a new watcher of a folder scans with: `device`, the built-in exclusions, and the `.fbignore` files, of
/// which it has read none yet. Its first scan reads and reports every one.
pub(crate) fn scan_rules(device: &Arc<dyn Matcher>) -> flarebox_fs::Rules {
    flarebox_fs::Rules::new(Box::new(Scanned {
        device: device.clone(),
        files: Rules::new(LocalRules::default()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync(include: &[&str], exclude: &[&str]) -> SyncRules {
        SyncRules {
            include: include.iter().map(|path| (*path).to_owned()).collect(),
            exclude: exclude.iter().map(|line| (*line).to_owned()).collect(),
        }
    }

    #[test]
    fn the_platform_files_and_the_state_folder_are_excluded_without_any_configuration() {
        let matcher = matcher(&SyncRules::default()).unwrap();
        for name in [
            ".DS_Store",
            "Thumbs.db",
            "desktop.ini",
            "Icon\r",
            "docs/.DS_Store",
        ] {
            assert!(matcher.is_excluded(Path::new(name), false), "{name:?}");
        }
        assert!(matcher.is_excluded(Path::new(".flarebox"), true));
        assert!(!matcher.is_excluded(Path::new("docs/.flarebox"), true));
        assert!(!matcher.is_excluded(Path::new("docs/notes.txt"), false));
    }

    #[test]
    fn an_include_list_leaves_out_everything_else_and_an_exclude_pattern_narrows_it() {
        let matcher = matcher(&sync(&["/Work//", "Notes/todo.txt"], &["*.tmp"])).unwrap();
        assert!(!matcher.is_excluded(Path::new("Work"), true));
        assert!(!matcher.is_excluded(Path::new("Work/plan.txt"), false));
        assert!(matcher.is_excluded(Path::new("Work/plan.tmp"), false));
        assert!(
            matcher.is_excluded(Path::new("Work"), false),
            "the entry names a folder"
        );
        assert!(
            !matcher.is_excluded(Path::new("Notes"), true),
            "a folder on the way stays"
        );
        assert!(!matcher.is_excluded(Path::new("Notes/todo.txt"), false));
        assert!(matcher.is_excluded(Path::new("Notes/other.txt"), false));
        assert!(matcher.is_excluded(Path::new("Photos"), true));
        assert!(!matcher.is_excluded(Path::new("Notes/.fbignore"), false));
    }

    #[test]
    fn a_scan_asks_the_device_list_the_built_in_exclusions_and_the_rules_files_it_read() {
        let device = matcher(&sync(&[], &["/private/"])).unwrap();
        let mut scanned = Scanned {
            device,
            files: Rules::new(LocalRules::default()),
        };
        let status = |scanned: &Scanned, path: &str, kind| scanned.status(Path::new(path), kind);
        assert_eq!(status(&scanned, "private", EntryKind::Dir), Ignored::Rule);
        assert_eq!(
            status(&scanned, "a/Thumbs.db", EntryKind::File),
            Ignored::BuiltIn
        );
        assert_eq!(status(&scanned, "a/b.log", EntryKind::File), Ignored::No);

        scanned.set_rules(Path::new("a"), Some(b"*.log\n"));
        assert_eq!(status(&scanned, "a/b.log", EntryKind::File), Ignored::Rule);
        assert_eq!(status(&scanned, "b.log", EntryKind::File), Ignored::No);
        scanned.set_rules(Path::new("a"), None);
        assert_eq!(status(&scanned, "a/b.log", EntryKind::File), Ignored::No);
    }
}
