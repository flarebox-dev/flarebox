// SPDX-License-Identifier: Apache-2.0

//! The sync driver: everything around the engine that a client runs on one device (docs/design/driver.md).
//!
//! - [`Paths`], [`Credential`] and [`Config`]: where a profile keeps its files, the device credential (one file only
//!   its owner can read, outside every linked folder) and the device-local configuration with its `[sync]` rules.
//! - [`Driver`]: the profile's API, shared by the Linux daemon and, through `core/ffi`, the macOS app. It pairs the
//!   device, links and unlinks folders, reports [`Status`], lists [`Question`]s and records answers, and runs one
//!   task per linked folder.
//! - Each folder task checks the root (design §6.2) and claims the folder, observes the disk through an [`Observer`],
//!   and keeps a `core/net` session with the space, applying the server's log to the remote tree in `core/store`.
//! - [`policy`]: what to do after the server refuses a call (H1 of reports/BOX-71).
//! - [`ignore`]: the input of the `.fbignore` matcher and the interface the driver uses it through.
//!
//! The engine is not wired in yet: the store holds the remote tree and the cursor, observations are produced but not
//! acted on, and nothing is uploaded or written to disk. docs/design/driver.md lists what connecting it adds.

mod claim;
mod config;
mod credential;
mod driver;
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
