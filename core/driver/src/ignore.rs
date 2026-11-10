// SPDX-License-Identifier: Apache-2.0

//! The input of the ignore matcher (design §5.10) and the interface the driver uses it through. The matcher itself
//! is built separately (GUN-270); until it lands, [`matcher`] returns [`Unfiltered`], which excludes nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where a set of rules comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleSource {
    /// A `.fbignore` file in the folder. It is synced, so every device reads the same rules.
    Fbignore,
    /// This device's `[sync]` configuration (H13). It is never synced.
    Device,
}

/// One file's worth of rules in `.fbignore` syntax, relative to `dir`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleFile {
    /// The folder the rules are relative to, relative to the linked folder's root; empty for the root.
    pub dir: PathBuf,
    pub source: RuleSource,
    pub lines: Vec<String>,
}

/// Everything that decides what one linked folder leaves out. Files are read in order, a later file's rules taking
/// precedence: `.fbignore` files from the root down, then this device's rules, so the device can always narrow what
/// it syncs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IgnoreInput {
    pub files: Vec<RuleFile>,
}

/// Decides whether a path of a linked folder is excluded on this device.
pub trait Matcher: Send + Sync {
    /// `path` is relative to the folder's root; `dir` says whether it names a folder.
    fn is_excluded(&self, path: &Path, dir: bool) -> bool;
}

/// Excludes nothing. Stands in for the matcher of GUN-270.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unfiltered;

impl Matcher for Unfiltered {
    fn is_excluded(&self, _path: &Path, _dir: bool) -> bool {
        false
    }
}

/// The matcher for `input`. The `.fbignore` matcher of GUN-270 replaces this body; nothing else changes.
pub fn matcher(_input: &IgnoreInput) -> Arc<dyn Matcher> {
    Arc::new(Unfiltered)
}
