// SPDX-License-Identifier: Apache-2.0

//! The ignore matcher as the engine holds it (docs/design/sync-engine.md section 5.10): the
//! matcher of `flarebox-ignore` with names compared by [`name_key`], the way R and S compare
//! them. [`Trees`](crate::classify::Trees) owns one and decides with it which nodes are excluded
//! on this device. `core/fs` is given a matcher of the same type and the same rules, so a scan
//! leaves out exactly the paths the engine excludes.

use crate::names::name_key;

/// Compares names by [`name_key`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key;

impl flarebox_ignore::NameKey for Key {
    fn key(name: &str) -> String {
        name_key(name)
    }
}

/// The rules in force on this device: the built-in exclusions, the device-local list and the
/// `.fbignore` files.
pub type Matcher = flarebox_ignore::Matcher<Key>;
/// The parsed rules of one `.fbignore` file.
pub type RuleFile = flarebox_ignore::RuleFile<Key>;
/// This device's include and exclude lists.
pub type LocalRules = flarebox_ignore::LocalRules<Key>;
