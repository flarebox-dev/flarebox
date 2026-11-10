// SPDX-License-Identifier: Apache-2.0

//! The interface between the app's session and whatever syncs: the sync driver, or
//! [`FakeCore`](crate::FakeCore) for previews and tests. The app only talks to
//! [`SyncSession`](crate::SyncSession), which forwards to a [`Core`], so either runs the same app code.

use std::sync::{Arc, Weak};

use crate::types::{
    Account, Answer, DeviceList, LinkedFolder, PairingCode, ServerInfo, SessionError, SessionEvent,
    SessionState, Space,
};

/// Receives every [`SessionEvent`] of a session, in order.
///
/// The core calls the listener on the thread that changed the state, while holding its lock,
/// which keeps the events in order. A listener that calls back into the session before
/// returning deadlocks.
///
/// The session drops the listener when the session itself is dropped, so a listener can treat
/// its own drop as the end of the stream.
#[uniffi::export(callback_interface)]
pub trait SessionListener: Send + Sync {
    fn on_event(&self, event: SessionEvent);
}

/// Where a core sends its events. It holds the session's listener weakly, so events sent after
/// the session is gone are dropped.
#[derive(Clone)]
pub struct Events(Weak<Box<dyn SessionListener>>);

impl Events {
    pub(crate) fn new(listener: &Arc<Box<dyn SessionListener>>) -> Self {
        Self(Arc::downgrade(listener))
    }

    /// Sends an event to the listener, if the session still exists.
    pub fn send(&self, event: SessionEvent) {
        if let Some(listener) = self.0.upgrade() {
            listener.on_event(event);
        }
    }
}

/// What the app needs from the sync core. It maps onto the engine's inputs and outputs (design
/// §1): the core turns the person's actions into engine inputs (`UserDecision` for answers)
/// and session setup, and reports the engine's `AskUser` and `Problem` outputs, with sync
/// status, progress and recent changes, as [`SessionEvent`]s.
///
/// Calls that talk to the server block until it answers, so the app makes them off its main
/// thread. Every change a call makes to [`SessionState`] is also sent as an event before the
/// call returns.
///
/// A call that changes what this device keeps (its credentials, folders, answers, notices or
/// the pause) saves the change before it reports success, as the engine persists each input
/// before acting on it (design §1). If saving fails, the call changes nothing and returns
/// [`SessionError::NotSaved`], and [`ProblemKind::StateNotWritable`](crate::ProblemKind::StateNotWritable)
/// stays listed until a save succeeds.
pub trait Core: Send + Sync {
    /// The current state. Events sent before this call returns are already included.
    fn state(&self) -> SessionState;

    /// Starts syncing every linked folder. The core then asks every unanswered question again by
    /// sending [`SessionEvent::Questions`], as the engine does after a restart (design §2.8).
    /// Starting a running session does nothing.
    fn start(&self);
    /// Stops syncing. State on disk and in the store is kept. Stopping a stopped session does
    /// nothing.
    fn stop(&self);
    /// Pauses a running session until [`Core::resume`]. The pause outlives a restart. Pausing
    /// a stopped session does nothing.
    fn pause(&self) -> Result<(), SessionError>;
    /// Resumes a paused session.
    fn resume(&self) -> Result<(), SessionError>;

    /// Looks up the server at `url` and remembers it for signing in.
    fn connect(&self, url: &str) -> Result<ServerInfo, SessionError>;
    /// Claims an unclaimed server as its owner, with the one-time code from its Worker log, and
    /// signs this device in.
    fn claim(
        &self,
        code: &str,
        user_name: &str,
        device_name: &str,
    ) -> Result<Account, SessionError>;
    /// Signs this device in to a claimed server with a pairing code from a device already
    /// signed in.
    fn pair(&self, code: &str, device_name: &str) -> Result<Account, SessionError>;
    /// Forgets this device's credentials and unlinks every folder. Files on disk stay.
    fn sign_out(&self) -> Result<(), SessionError>;
    /// The account's devices, or [`DeviceList::Unavailable`] when the server cannot list them.
    fn devices(&self) -> Result<DeviceList, SessionError>;
    /// Creates a code that signs in another device.
    fn start_pairing(&self) -> Result<PairingCode, SessionError>;

    /// The account's spaces.
    fn spaces(&self) -> Result<Vec<Space>, SessionError>;
    /// Creates a space.
    fn create_space(&self, name: &str) -> Result<Space, SessionError>;
    /// Links the folder at absolute `path` to a space and starts syncing it.
    fn link_folder(&self, space_id: &str, path: &str) -> Result<LinkedFolder, SessionError>;
    /// Stops syncing a space on this device. Files on disk stay.
    fn unlink_folder(&self, space_id: &str) -> Result<(), SessionError>;

    /// Answers a question. The answer must be one of the question's
    /// [`answers`](crate::Question::answers).
    fn answer(&self, question_id: &str, answer: Answer) -> Result<(), SessionError>;
    /// Dismisses a notice.
    fn dismiss_notice(&self, notice_id: &str) -> Result<(), SessionError>;

    /// Stops syncing and releases what the core holds outside memory, such as the lock on this
    /// device's sync state, so that the state can be opened again at once. The session calls it
    /// when it is dropped; no other call follows.
    fn close(&self) {}
}
