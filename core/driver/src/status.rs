// SPDX-License-Identifier: Apache-2.0

//! What the driver reports about itself: the signed-in device and each linked folder. The CLI prints it and the
//! macOS app will map it onto its `SessionState`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The paired device, or none before `pair`.
    pub account: Option<Account>,
    /// Whether the folders are syncing: the daemon is running, or the app started the driver.
    pub running: bool,
    pub folders: Vec<FolderStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub server: String,
    pub user_id: String,
    pub device_id: String,
    pub device_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderStatus {
    pub space_id: String,
    pub space_name: String,
    /// The folder's absolute path.
    pub path: String,
    pub state: FolderState,
    /// The server history and the last seq whose changes are in the store.
    pub history_id: Option<String>,
    pub last_seq: Option<u64>,
    /// The server's head, as of the last `Welcome` or `CaughtUp`.
    pub head_seq: Option<u64>,
    /// Nodes of the remote tree in the store, live and trashed.
    pub remote_nodes: u64,
    /// Files and folders the last observation of the disk found, while running.
    pub local_entries: Option<u64>,
    /// Questions waiting for an answer.
    pub questions: u64,
    /// Why the folder is halted, or what the last transient failure was.
    pub problem: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderState {
    /// Linked, but the driver is not running.
    Stopped,
    /// Opening a session with the server.
    Connecting,
    /// Reading the whole remote tree, for a first link or after the server's log no longer reaches the store.
    Bootstrapping,
    /// Applying the server's log up to its head.
    CatchingUp,
    /// The store holds the server's head, and changes arrive as they are committed.
    UpToDate,
    /// The server cannot be reached; the session keeps retrying.
    Offline,
    /// Stopped by a problem only a person can fix; `problem` says which. The driver does not retry it until it is
    /// started again.
    Halted,
}
