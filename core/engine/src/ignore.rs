// SPDX-License-Identifier: Apache-2.0

use koidrive_ignore::RULES_FILE;

use crate::names::name_key;

/// Whether a file of this name is a rules file: `.koiignore`, or a name that [`name_key`] takes for the same
/// name, such as `.KOIIGNORE`.
pub fn is_rules_file(name: &str) -> bool {
    name_key(name) == name_key(RULES_FILE)
}

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
