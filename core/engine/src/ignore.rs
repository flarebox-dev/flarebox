// SPDX-License-Identifier: Apache-2.0

//! The ignore matcher as the engine holds it (docs/design/sync-engine.md section 5.10): the
//! matcher of `koidrive-ignore` with names compared by [`name_key`], the way R and S compare
//! them. [`Trees`](crate::classify::Trees) owns one and decides with it which nodes are excluded
//! on this device. `core/fs` is given a matcher of the same type and the same rules, so a scan
//! leaves out exactly the paths the engine excludes.

use koidrive_ignore::RULES_FILE;

use crate::names::name_key;

/// Whether a file of this name is a rules file: `.koiignore`, or a name that [`name_key`] takes for the same
/// name, such as `.KOIIGNORE`. The matcher compares the name this way, so every part that singles out a rules file
/// must as well.
pub fn is_rules_file(name: &str) -> bool {
    name_key(name) == name_key(RULES_FILE)
}

/// Compares names by [`name_key`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key;

impl koidrive_ignore::NameKey for Key {
    fn key(name: &str) -> String {
        name_key(name)
    }
}

/// The rules in force on this device: the built-in exclusions, the device-local list and the
/// `.koiignore` files.
pub type Matcher = koidrive_ignore::Matcher<Key>;
/// The parsed rules of one `.koiignore` file.
pub type RuleFile = koidrive_ignore::RuleFile<Key>;
/// This device's include and exclude lists.
pub type LocalRules = koidrive_ignore::LocalRules<Key>;
