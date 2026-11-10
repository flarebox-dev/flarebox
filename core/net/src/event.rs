// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use flarebox_proto::v1::{CommitRecord, CommitResult, Notice, ResyncRequired, Welcome};

use crate::error::StopReason;

/// What a session reports, in the order the driver must handle it.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A connection is up: the server's `Welcome`. It follows every reconnect. The driver compares
    /// `history_id` and `device_seq` with its store (RFC 0002 sections 2 and 6).
    Connected(Welcome),
    /// Commits from the log, contiguous and in seq order, continuing from the subscribed seq.
    Changes(Vec<CommitRecord>),
    /// This device's commit was accepted. It is placed at its seq among `Changes`, so applying events in order
    /// applies the result in seq order (RFC 0002 section 5). It also comes when the commit's record already
    /// arrived by replay, because the result still has to be acknowledged.
    Committed(Committed),
    /// The backlog after a `Resume` is delivered; live pushes follow.
    CaughtUp {
        head_seq: u64,
    },
    /// The server cannot continue from the subscribed point. The stream stops until the driver subscribes again.
    ResyncRequired(ResyncRequired),
    Notice(Notice),
    /// The connection dropped. The session reconnects after `retry_in` and resends what RFC 0002 allows.
    Disconnected {
        reason: DisconnectReason,
        retry_in: Duration,
    },
    /// The session stopped for good; no further events follow.
    Stopped(StopReason),
}

/// An accepted commit of this device.
#[derive(Clone, Debug, PartialEq)]
pub struct Committed {
    pub commit_id: String,
    pub result: CommitResult,
}

/// Why a connection dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisconnectReason {
    /// The connection could not be made or broke.
    Transport(String),
    /// The server refused the WebSocket upgrade with this HTTP status.
    Refused { status: u16 },
    /// The server closed the connection, after a `Goodbye` when it sent one.
    Closed { code: Option<u16>, reason: String },
    /// The server broke the protocol, for example with a message larger than 1 MiB.
    ProtocolViolation(String),
    /// Nothing arrived from the server for two ping intervals, or no `Welcome` in time.
    Timeout,
}
