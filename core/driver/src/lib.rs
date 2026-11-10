// SPDX-License-Identifier: Apache-2.0

//! The sync driver: everything around the engine that a client runs on one device (docs/design/driver.md).
//!
//! - [`Paths`], [`Credential`] and [`Config`]: where a profile keeps its files, the device credential (one file only
//!   its owner can read, outside every linked folder) and the device-local configuration with its `[sync]` rules.
//! - [`Driver`]: the profile's API, shared by the Linux daemon and, through `core/ffi`, the macOS app. It pairs the
//!   device, links and unlinks folders, reports [`Status`], lists [`Question`]s and records answers, and runs one
//!   task per linked folder.
//! - Each folder task checks the root (design §6.2) and claims the folder, observes the disk through an [`Observer`],
//!   and keeps a `core/net` session with the space. It runs the sync engine between them: the server's log goes
//!   into the remote tree in `core/store`, observations go into the local tree, the engine's effects are applied to
//!   the folder, and its commits are sent to the server.
//! - [`policy`]: what to do after the server refuses a call (H1 of reports/BOX-71).
//! - [`ignore`]: the input of the `.fbignore` matcher and the interface the driver uses it through.
//!
//! New files and folders sync in both directions. Edits, renames, deletes and conflicts go through the same loop and
//! are not proved end to end yet. docs/design/driver.md describes the loop and lists what is still to wire.

mod claim;
mod config;
mod credential;
mod disk;
mod driver;
mod engine;
mod folder;
pub mod ignore;
mod observe;
mod paths;
pub mod policy;
mod question;
mod status;

pub use config::{Config, ConfigError, SyncRules};
pub use credential::{Credential, CredentialError, api_for};
pub use driver::{Driver, DriverError};
pub use folder::MAX_FORMAT;
pub use observe::{FsObserver, Observation, Observed, Observer};
pub use paths::{Paths, PathsError};
pub use question::{Answer, HeldItem, Question, QuestionKind, RecoveryGroup};
pub use status::{Account, FolderState, FolderStatus, Status};
