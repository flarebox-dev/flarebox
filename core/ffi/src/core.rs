// SPDX-License-Identifier: Apache-2.0

//! The interface between the app's session and whatever syncs: the sync driver, or
//! [`FakeCore`](crate::FakeCore) for previews and tests. The app only talks to
//! [`SyncSession`](crate::SyncSession), which forwards to a [`Core`], so either runs the same app code.

use std::sync::{Arc, Weak};

use crate::types::{
    Account, Answer, Device, LinkedFolder, PairingCode, ServerInfo, SessionError, SessionEvent,
    SessionState, Space, SpaceUsage,
};

/// The code part of a pairing code as typed or pasted. `koi device add` prints the code as
/// `<code>@<host>`, with `:<port>` after the host when the server's port isn't the default, so the
/// text is split on its last `@` as `koi join` does. A host and port other than those of `server_url`,
/// the server this device connected to, are refused; host names compare without case.
pub(crate) fn pairing_code<'a>(text: &'a str, server_url: &str) -> Result<&'a str, SessionError> {
    let text = text.trim();
    let Some((code, host)) = text.rsplit_once('@') else {
        return Ok(text);
    };
    let server = url::Url::parse(server_url)
        .ok()
        .and_then(|url| {
            let host = url.host_str()?.to_owned();
            Some(match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        })
        .unwrap_or_else(|| server_url.to_owned());
    let host = host.trim();
    if !host.eq_ignore_ascii_case(&server) {
        return Err(SessionError::CodeForOtherServer {
            host: host.to_owned(),
            server,
        });
    }
    Ok(code.trim())
}

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
    /// Revokes the device on the server, forgets its credentials and unlinks every folder. Files on
    /// disk stay. Unless `confirm_last_device` is set, it changes nothing and returns
    /// [`SessionError::LastDevice`] when this is the account's last active device, or
    /// [`SessionError::NotRevoked`] when the server cannot be reached, so the app can tell the person
    /// first. With it, a sign-out without the server finishes on the device.
    fn sign_out(&self, confirm_last_device: bool) -> Result<(), SessionError>;
    /// The account's devices that are not revoked.
    fn devices(&self) -> Result<Vec<Device>, SessionError>;
    /// Revokes another device of the account on the server. It can no longer sync, and the files already on it stay
    /// there. This device leaves with [`Core::sign_out`], so its own ID returns [`SessionError::ThisDevice`]. An ID
    /// unlike those the server issues (36 lowercase hexadecimal digits and hyphens) returns
    /// [`SessionError::DeviceNotFound`] without a request.
    fn revoke_device(&self, device_id: &str) -> Result<(), SessionError>;
    /// Creates a code that signs in another device.
    fn start_pairing(&self) -> Result<PairingCode, SessionError>;

    /// The account's spaces.
    fn spaces(&self) -> Result<Vec<Space>, SessionError>;
    /// Creates a space.
    fn create_space(&self, name: &str) -> Result<Space, SessionError>;
    /// What a space stores. The server answers only the space's owner.
    fn space_usage(&self, space_id: &str) -> Result<SpaceUsage, SessionError>;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_code_drops_the_host_of_its_server() {
        let server = "https://koidrive.alice.workers.dev/";
        assert_eq!(pairing_code(" K7QM-2XPD ", server), Ok("K7QM-2XPD"));
        assert_eq!(
            pairing_code("K7QM-2XPD@koidrive.alice.workers.dev", server),
            Ok("K7QM-2XPD")
        );
        assert_eq!(
            pairing_code("K7QM-2XPD@KoiDrive.Alice.Workers.Dev ", server),
            Ok("K7QM-2XPD")
        );
        assert_eq!(
            pairing_code(
                "K7QM-2XPD@files.example.com:8443",
                "https://files.example.com:8443"
            ),
            Ok("K7QM-2XPD")
        );
    }

    #[test]
    fn a_code_for_another_server_is_refused() {
        assert_eq!(
            pairing_code(
                "K7QM-2XPD@files.example.com",
                "https://koidrive.alice.workers.dev"
            ),
            Err(SessionError::CodeForOtherServer {
                host: "files.example.com".to_owned(),
                server: "koidrive.alice.workers.dev".to_owned(),
            })
        );
        assert_eq!(
            pairing_code(
                "K7QM-2XPD@files.example.com",
                "https://files.example.com:8443"
            ),
            Err(SessionError::CodeForOtherServer {
                host: "files.example.com".to_owned(),
                server: "files.example.com:8443".to_owned(),
            })
        );
    }
}
