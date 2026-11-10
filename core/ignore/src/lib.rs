// SPDX-License-Identifier: Apache-2.0

//! Which paths of a space this device leaves out of sync: `.fbignore` files, the built-in
//! exclusions and the device-local list (design §5.1, §5.10).
//!
//! # Rules files
//!
//! A `.fbignore` file can be in any directory and uses `.gitignore` syntax ([`RuleFile::parse`]).
//! Its patterns are relative to its directory and apply to that directory's subtree. For a path,
//! the deepest rules file with a matching pattern decides, and within that file the last matching
//! pattern does; a `!` pattern includes. A path under an excluded directory is excluded whatever
//! the patterns say, and a rules file inside an excluded directory is not read. A `.fbignore` file
//! in an included directory is always included, so the rules cannot hide themselves.
//!
//! # Built-in exclusions
//!
//! `.flarebox` at the root and the platform files `.DS_Store`, `Thumbs.db`, `desktop.ini` and
//! `Icon\r` in any directory are always excluded, as files or directories. No pattern and no local
//! include can bring them back.
//!
//! # The device-local list
//!
//! [`LocalRules`] holds this device's include and exclude lists from the client configuration.
//! They are never synced and only narrow what this device syncs:
//!
//! - The include list names paths relative to the root (`Projects/`, `Work/Notes.txt`). When it is
//!   not empty, a path is synced only if it is an included path, lies under one, or is a
//!   directory on the way to one. Everything else is excluded, except `.fbignore` files in
//!   included directories, which still apply.
//! - The exclude list holds patterns in `.fbignore` syntax, relative to the root.
//!
//! A path is excluded when a built-in exclusion, the local list or the `.fbignore` files exclude
//! it. So a `!` pattern in a `.fbignore` file cannot include a path the local list excludes, and
//! a `!` pattern in the local exclude list cannot include a path a `.fbignore` file excludes.
//!
//! # Names
//!
//! Patterns and names are compared in the form the engine compares names in (design §5.5): the
//! name key of [`NameKey::key`] (`name_key` in production), then NFC. A pattern therefore means
//! the same thing on case-sensitive and case-insensitive volumes and for NFC and NFD spellings.
//! `?`, `*` and bracket expressions match characters of that form, so `ß` is matched as `ss`.
//! A bracket range matches a character, or the character's uppercase form when that has the same
//! key, which is git's `core.ignorecase` rule for ASCII. The POSIX classes are ASCII-only, as in git.
//!
//! # Changes
//!
//! [`Matcher::set_rules`] and [`Matcher::set_local`] return the entries that became included or
//! excluded, and evaluate only the subtree the change can affect. [`Change::Excluded`] means this
//! device stops syncing the path. It is never a deletion, on either side.
//!
//! # Differences from git
//!
//! Matching follows git's `dir.c` and `wildmatch.c`, and `tests/git_oracle.rs` compares it with
//! `git check-ignore` on random trees and rules files. It differs on purpose here:
//!
//! - Case folding is Unicode full case folding through the name key, and it covers the members of
//!   bracket expressions. Git with `core.ignorecase` folds ASCII only and leaves bracket members
//!   and escaped letters as written, so in git `[A]` and `\A` match no letter at all.
//! - Wildcards match characters; git matches bytes, so its `?` matches one byte of a UTF-8 name.
//! - A `.fbignore` file in an included directory is never excluded; git can ignore a `.gitignore`.
//! - Lines git keeps as patterns that can never match are reported as invalid ([`LineError`]): an
//!   unclosed `[`, an unknown `[:class:]`, a trailing `\`. So are lines that are not UTF-8 or
//!   hold a NUL byte; git would cut the pattern at the NUL.

mod matcher;
mod pattern;

pub use matcher::{Change, Kind, LocalRules, LocalRulesError, Matcher, Reason, Scope, Status};
pub use pattern::{InvalidLine, LineError, RuleFile};

/// The name of a rules file, compared by key.
pub const RULES_FILE: &str = ".fbignore";

/// How names are compared. Production uses the engine's `name_key` (NFD, Unicode full case
/// folding, NFD); the matcher applies NFC on top, so wildcards match composed characters.
pub trait NameKey {
    fn key(name: &str) -> String;
}
