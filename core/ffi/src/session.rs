// SPDX-License-Identifier: Apache-2.0

//! The session object the app holds.

use std::sync::Arc;

use crate::core::{Core, Events, SessionListener};
use crate::fake::FakeCore;
use crate::types::{
    Account, Answer, DeviceList, LinkedFolder, PairingCode, ServerInfo, SessionError, SessionState,
    Space,
};

/// A sync session: the one object the app talks to. Each method forwards to the session's
/// [`Core`]; see there for what it does. Methods that talk to the server block, so call them
/// off the main thread.
#[derive(uniffi::Object)]
pub struct SyncSession {
    core: Arc<dyn Core>,
    fake: Option<Arc<FakeCore>>,
    _listener: Arc<Box<dyn SessionListener>>,
}

#[uniffi::export]
impl SyncSession {
    /// Creates a stopped session backed by [`FakeCore`]. With `state_dir`, the fake keeps
    /// its state in that directory, so questions, folders and the account survive a relaunch as
    /// they will with the engine; without it, the state lives in memory.
    #[uniffi::constructor]
    pub fn fake(listener: Box<dyn SessionListener>, state_dir: Option<String>) -> Arc<Self> {
        let listener = Arc::new(listener);
        let fake = Arc::new(FakeCore::new(
            Events::new(&listener),
            state_dir.map(Into::into),
        ));
        Arc::new(Self {
            core: fake.clone(),
            fake: Some(fake),
            _listener: listener,
        })
    }

    /// The fake behind this session, for scripting its state, or none for a real core.
    pub fn fake_core(&self) -> Option<Arc<FakeCore>> {
        self.fake.clone()
    }

    pub fn state(&self) -> SessionState {
        self.core.state()
    }

    pub fn start(&self) {
        self.core.start();
    }

    pub fn stop(&self) {
        self.core.stop();
    }

    pub fn pause(&self) -> Result<(), SessionError> {
        self.core.pause()
    }

    pub fn resume(&self) -> Result<(), SessionError> {
        self.core.resume()
    }

    pub fn connect(&self, url: String) -> Result<ServerInfo, SessionError> {
        self.core.connect(&url)
    }

    pub fn claim(
        &self,
        code: String,
        user_name: String,
        device_name: String,
    ) -> Result<Account, SessionError> {
        self.core.claim(&code, &user_name, &device_name)
    }

    pub fn pair(&self, code: String, device_name: String) -> Result<Account, SessionError> {
        self.core.pair(&code, &device_name)
    }

    pub fn sign_out(&self) -> Result<(), SessionError> {
        self.core.sign_out()
    }

    pub fn devices(&self) -> Result<DeviceList, SessionError> {
        self.core.devices()
    }

    pub fn start_pairing(&self) -> Result<PairingCode, SessionError> {
        self.core.start_pairing()
    }

    pub fn spaces(&self) -> Result<Vec<Space>, SessionError> {
        self.core.spaces()
    }

    pub fn create_space(&self, name: String) -> Result<Space, SessionError> {
        self.core.create_space(&name)
    }

    pub fn link_folder(
        &self,
        space_id: String,
        path: String,
    ) -> Result<LinkedFolder, SessionError> {
        self.core.link_folder(&space_id, &path)
    }

    pub fn unlink_folder(&self, space_id: String) -> Result<(), SessionError> {
        self.core.unlink_folder(&space_id)
    }

    pub fn answer(&self, question_id: String, answer: Answer) -> Result<(), SessionError> {
        self.core.answer(&question_id, answer)
    }

    pub fn dismiss_notice(&self, notice_id: String) -> Result<(), SessionError> {
        self.core.dismiss_notice(&notice_id)
    }
}
