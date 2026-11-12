// SPDX-License-Identifier: Apache-2.0

//! How much this process synced since it started: commits sent and received, and content bytes uploaded and
//! downloaded. The counts only grow. Usage reports read them and keep each day's total as a magnitude.

use std::sync::atomic::{AtomicU64, Ordering};

static COMMITS_SENT: AtomicU64 = AtomicU64::new(0);
static COMMITS_RECEIVED: AtomicU64 = AtomicU64::new(0);
static BYTES_UPLOADED: AtomicU64 = AtomicU64::new(0);
static BYTES_DOWNLOADED: AtomicU64 = AtomicU64::new(0);

/// The totals of this process, as [`volume`] reads them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Volume {
    /// Commits of this device the server accepted.
    pub commits_sent: u64,
    /// Commits of the server's log that reached this device.
    pub commits_received: u64,
    /// Chunk bytes read for upload: the content the server did not hold yet.
    pub bytes_uploaded: u64,
    /// Chunk bytes downloaded and checked against their hash.
    pub bytes_downloaded: u64,
}

pub fn volume() -> Volume {
    Volume {
        commits_sent: COMMITS_SENT.load(Ordering::Relaxed),
        commits_received: COMMITS_RECEIVED.load(Ordering::Relaxed),
        bytes_uploaded: BYTES_UPLOADED.load(Ordering::Relaxed),
        bytes_downloaded: BYTES_DOWNLOADED.load(Ordering::Relaxed),
    }
}

pub(crate) fn commit_sent() {
    COMMITS_SENT.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn commits_received(count: usize) {
    COMMITS_RECEIVED.fetch_add(count as u64, Ordering::Relaxed);
}

pub(crate) fn uploaded(bytes: usize) {
    BYTES_UPLOADED.fetch_add(bytes as u64, Ordering::Relaxed);
}

pub(crate) fn downloaded(bytes: usize) {
    BYTES_DOWNLOADED.fetch_add(bytes as u64, Ordering::Relaxed);
}
