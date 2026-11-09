// SPDX-License-Identifier: Apache-2.0

//! UniFFI bridge between the Rust core and the native desktop apps.
//!
//! The API is a placeholder for the macOS shell: a session only moves between
//! stopped and idle. The sync engine replaces the stub behind it later.

use std::sync::{Arc, Mutex};

uniffi::setup_scaffolding!();

/// Returns the version of the Rust core.
#[uniffi::export]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// What a sync session is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncStatus {
    /// The session is not running.
    Stopped,
    /// The session is running and has nothing to do.
    Idle,
}

/// Receives every status change of a [`SyncSession`], in order.
///
/// The session calls the listener on the thread that changed the status,
/// while holding its lock, so the listener must not call back into the
/// session before returning.
#[uniffi::export(callback_interface)]
pub trait StatusListener: Send + Sync {
    fn on_status(&self, status: SyncStatus);
}

/// A placeholder sync session that reports its status to a listener.
#[derive(uniffi::Object)]
pub struct SyncSession {
    listener: Box<dyn StatusListener>,
    status: Mutex<SyncStatus>,
}

#[uniffi::export]
impl SyncSession {
    /// Creates a stopped session.
    #[uniffi::constructor]
    pub fn new(listener: Box<dyn StatusListener>) -> Arc<Self> {
        Arc::new(Self {
            listener,
            status: Mutex::new(SyncStatus::Stopped),
        })
    }

    pub fn start(&self) {
        self.set_status(SyncStatus::Idle);
    }

    pub fn stop(&self) {
        self.set_status(SyncStatus::Stopped);
    }

    pub fn status(&self) -> SyncStatus {
        *self.status.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl SyncSession {
    fn set_status(&self, next: SyncStatus) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if *status != next {
            *status = next;
            self.listener.on_status(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Recorder(Arc<Mutex<Vec<SyncStatus>>>);

    impl StatusListener for Recorder {
        fn on_status(&self, status: SyncStatus) {
            self.0.lock().unwrap().push(status);
        }
    }

    #[test]
    fn session_reports_each_status_change_once() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let session = SyncSession::new(Box::new(Recorder(seen.clone())));
        assert_eq!(session.status(), SyncStatus::Stopped);

        session.start();
        session.start();
        assert_eq!(session.status(), SyncStatus::Idle);

        session.stop();
        session.stop();
        assert_eq!(session.status(), SyncStatus::Stopped);

        assert_eq!(
            *seen.lock().unwrap(),
            [SyncStatus::Idle, SyncStatus::Stopped]
        );
    }
}
