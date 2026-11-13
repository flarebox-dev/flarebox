// SPDX-License-Identifier: Apache-2.0

//! What the driver reports about itself: the signed-in device and each linked folder. The CLI prints it and the
//! macOS app will map it onto its `SessionState`.

use serde::{Deserialize, Serialize};

use crate::update::Update;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The paired device, or none before `pair`.
    pub account: Option<Account>,
    /// Whether the driver was started: the daemon is running, or the app started it. A paused driver syncs nothing all
    /// the same.
    pub running: bool,
    /// Whether sync is paused until a resume, also across restarts.
    #[serde(default)]
    pub paused: bool,
    /// The Koidrive version of the process that reports the status. None from a daemon too old to send it.
    #[serde(default)]
    pub version: Option<String>,
    /// Whether a newer release is out than `version`. Unknown from a daemon too old to send it.
    #[serde(default)]
    pub update: Update,
    pub folders: Vec<FolderStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub server: String,
    pub user_id: String,
    /// The user's name, known on the device that claimed the server. A paired device does not learn it: no server
    /// response carries it yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
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
    /// The call the folder puts off, as soon as it waits: why and until when. `problem` shows a wait only once it is
    /// long or keeps failing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<Wait>,
}

/// A call a folder sends again later, after the server put it off or it kept failing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wait {
    pub kind: WaitKind,
    /// When the call is sent again, in Unix milliseconds.
    pub until_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitKind {
    /// The server refused with `QUOTA_EXCEEDED`: a daily limit is used up, or the space's storage is full.
    Quota,
    /// The call failed for another reason, such as the network or `RETRY_LATER`, and is tried again.
    Retry,
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
    /// Stopped by a problem; `problem` says which. A folder that was moved away or fails its root check syncs again
    /// on its own once it passes the check. Any other problem needs a person, and the folder waits for a restart.
    Halted,
}
