// SPDX-License-Identifier: Apache-2.0

//! Claiming a server with a one-time `FLAREBOX_RECOVERY_KEY` (docs/design/cli-setup.md sections 4.4 to 4.6): set the
//! secret, claim with the setup attempt id, save the credential, then delete the secret. Each step is recorded in the
//! state file first, so a rerun after a crash or a lost response converges on one device.

use std::time::{Duration, Instant};

use flarebox_cloudflare::Secret;
use flarebox_driver::{Credential, Paths};
use flarebox_net::{Api, ApiError, Auth};
use flarebox_proto::v1::ClaimRequest;
use flarebox_proto::v1::error::Code;

use super::state::{SetupState, StateFile};
use super::{Cloud, Found, Session, report_paired};
use crate::ipc::{self, Request, Response};
use url::Url;

pub const KEY_NAME: &str = "FLAREBOX_RECOVERY_KEY";
/// A new key at most this often per run; more spent keys mean something else is wrong.
const MAX_KEYS: usize = 5;

/// The server's refusals while it cannot see the key yet: a new secret still propagating, or an Account DO that still
/// runs code without the key claim. Both pass on a later try.
const NOT_YET: [&str; 2] = [
    "no valid claim code; open /setup to issue one",
    "this deployment already has an owner",
];
/// The server's refusal of a key that a claim already used. Retrying it never succeeds.
const SPENT: &str = "this recovery key was already used; set a new one";

pub struct Claim<'a> {
    session: &'a mut Session,
    paths: &'a Paths,
    server: &'a Found,
    device_name: &'a str,
    state: SetupState,
    /// The key the deploy uploaded with the Worker, if this run set one.
    key: Option<Secret>,
}

enum Outcome {
    Claimed(Credential),
    Spent,
}

impl<'a> Claim<'a> {
    pub fn new(
        session: &'a mut Session,
        paths: &'a Paths,
        server: &'a Found,
        device_name: &'a str,
        state: SetupState,
        key: Option<Secret>,
    ) -> Self {
        Claim {
            session,
            paths,
            server,
            device_name,
            state,
            key,
        }
    }

    /// Claims within `window` per key: 60 seconds for a server that already runs the key claim, up to 330 when an
    /// Account DO may still be moving to new code.
    pub async fn run(mut self, window: Duration) -> Result<(), String> {
        let file = StateFile::new(self.paths);
        if self.state.setup_attempt.is_none() {
            self.state.setup_attempt = Some(Secret::random(16).expose().to_owned());
        }
        file.save(&self.state)?;
        if saved_device(self.paths, &self.server.url).await? {
            println!(
                "This device is already set up with {}, so it was not claimed again.",
                self.server.script.id
            );
            if self.state.secret_set {
                self.delete_key().await?;
            }
            return file.remove();
        }
        let mut key = match self.key.take() {
            Some(key) => key,
            None => self.set_key(&file, true).await?,
        };
        let mut credential = None;
        for _ in 0..MAX_KEYS {
            match self.claim(&key, window).await? {
                Outcome::Claimed(issued) => {
                    credential = Some(issued);
                    break;
                }
                Outcome::Spent => key = self.set_key(&file, false).await?,
            }
        }
        let credential = credential.ok_or_else(|| {
            format!("the server refused {MAX_KEYS} keys as already used; run the command again")
        })?;
        let saved = ipc::run(self.paths, Request::SaveCredential { credential }).await?;
        self.state.setup_attempt = None;
        file.save(&self.state)?;
        report_paired(self.paths, saved)?;
        self.delete_key().await?;
        file.remove()
    }

    /// Sets a new one-time key on the Worker. Before the first one it checks that the owner has not set a key of
    /// their own for recovery, which this run must not replace.
    async fn set_key(&mut self, file: &StateFile, first: bool) -> Result<Secret, String> {
        let account = self.session.account().to_owned();
        let script = self.state.script.clone();
        let api = self.session.fresh().await?;
        if first && !self.state.secret_set {
            let secrets = api
                .secrets(&account, &script)
                .await
                .map_err(|error| format!("cannot list the secrets of {script}: {error}"))?;
            if secrets.iter().any(|name| name == KEY_NAME) {
                return Err(format!(
                    "{script} already has a {KEY_NAME} secret, which its owner set for recovery. Use that key, or delete the secret in the dashboard, then run this command again."
                ));
            }
        }
        self.state.secret_set = true;
        file.save(&self.state)?;
        let key = Secret::random(32);
        api.put_secret(&account, &script, KEY_NAME, &key)
            .await
            .map_err(|error| format!("cannot set the one-time key on {script}: {error}"))?;
        Ok(key)
    }

    async fn claim(&self, key: &Secret, window: Duration) -> Result<Outcome, String> {
        let api = Api::new(self.server.url.clone()).map_err(|error| error.to_string())?;
        let request = ClaimRequest {
            code: key.expose().to_owned(),
            user_name: user_name(),
            device_name: self.device_name.to_owned(),
            platform: std::env::consts::OS.to_owned(),
            setup_attempt: self.state.setup_attempt.clone().unwrap_or_default(),
        };
        let started = Instant::now();
        println!("Claiming {} with this device...", self.server.script.id);
        loop {
            let wait = match api.claim(&request).await {
                Ok(issued) => {
                    return Ok(Outcome::Claimed(Credential {
                        server: api.base().to_string(),
                        user_id: issued.user_id,
                        user_name: Some(issued.user_name).filter(|name| !name.is_empty()),
                        device_id: issued.device_id,
                        device_name: self.device_name.to_owned(),
                        refresh_credential: issued.refresh_credential,
                    }));
                }
                Err(ApiError::Server { status: 403, error }) if error.message() == SPENT => {
                    return Ok(Outcome::Spent);
                }
                Err(ApiError::Server { status: 403, error })
                    if NOT_YET.contains(&error.message()) =>
                {
                    Duration::from_secs(3)
                }
                Err(ApiError::Server { error, .. }) if error.code() == Code::RetryLater => {
                    error.retry_after().unwrap_or(Duration::from_secs(10))
                }
                Err(
                    ApiError::Transport(_)
                    | ApiError::TooLarge { .. }
                    | ApiError::Unreadable { .. },
                ) => Duration::from_secs(3),
                Err(ApiError::Status { status }) if status >= 500 => Duration::from_secs(3),
                Err(error) => return Err(format!("the server refused the claim: {error}")),
            };
            if started.elapsed() + wait > window {
                return Err(format!(
                    "the server did not accept the claim within {} seconds; run the command again to continue",
                    window.as_secs()
                ));
            }
            tokio::time::sleep(wait).await;
        }
    }

    /// Deletes the one-time key and checks that it is gone. Other secrets stay.
    async fn delete_key(&mut self) -> Result<(), String> {
        let account = self.session.account().to_owned();
        let script = self.state.script.clone();
        let api = self.session.fresh().await?;
        api.delete_secret(&account, &script, KEY_NAME)
            .await
            .map_err(|error| {
                format!(
                    "cannot delete the one-time key from {script}: {error}; run the command again"
                )
            })?;
        let secrets = api
            .secrets(&account, &script)
            .await
            .map_err(|error| format!("cannot list the secrets of {script}: {error}"))?;
        if secrets.iter().any(|name| name == KEY_NAME) {
            return Err(format!(
                "{KEY_NAME} is still set on {script}; run the command again"
            ));
        }
        self.state.secret_set = false;
        Ok(())
    }
}

/// Whether this profile already holds a working credential for `server`, which a run that stopped after saving its
/// claim's credential leaves behind (section 4.6). The server decides with `POST /v1/token`: a credential it refuses
/// as unknown or revoked is removed, so the claim can go on. Any other answer stops the run and changes nothing.
pub async fn saved_device(paths: &Paths, server: &Url) -> Result<bool, String> {
    let Some(credential) =
        Credential::load(&paths.credential()).map_err(|error| error.to_string())?
    else {
        return Ok(false);
    };
    let api = credential.api().map_err(|error| error.to_string())?;
    if api.base().as_str().trim_end_matches('/') != server.as_str().trim_end_matches('/') {
        return Err(format!(
            "this device is already set up with {}; its credential is {}",
            credential.server,
            paths.credential().display()
        ));
    }
    match Auth::new(api, credential.net()).access_token().await {
        Ok(_) => Ok(true),
        Err(error) if error.is_unauthenticated() => {
            println!(
                "The server no longer accepts this device's saved credential, so it was removed."
            );
            match ipc::run(paths, Request::SignOut).await? {
                Response::SignedOut => Ok(false),
                Response::Error { message } => Err(message),
                other => Err(format!("the daemon answered {other:?}")),
            }
        }
        Err(error) => Err(format!(
            "cannot check this device's saved credential with {}: {error}. Nothing was changed; run the command again",
            credential.server
        )),
    }
}

/// The owner's name for a new server: the login name, which the apps show and the owner can change later.
fn user_name() -> String {
    std::env::var("USER")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Owner".into())
}

#[cfg(test)]
mod tests {
    use flarebox_proto::v1::{AccessToken, Error};
    use prost::Message;

    use super::*;
    use crate::setup::fake::{Fake, Reply};

    fn token(status: u16, body: Vec<u8>) -> Reply {
        Reply {
            method: "POST",
            path: "/v1/token".into(),
            status,
            content_type: "application/x-protobuf",
            body,
        }
    }

    fn refusal(code: Code) -> Vec<u8> {
        Error {
            code: code as i32,
            message: "unknown device or refresh credential".into(),
            ..Error::default()
        }
        .encode_to_vec()
    }

    /// A profile whose credential names `server`.
    fn profile(server: &Url) -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        Credential {
            server: server.to_string(),
            user_id: "user".into(),
            user_name: Some("owner".into()),
            device_id: "0194f8a2-0000-7000-8000-000000000001".into(),
            device_name: "laptop".into(),
            refresh_credential: "refresh".into(),
        }
        .save_new(&paths.credential())
        .unwrap();
        (dir, paths)
    }

    #[tokio::test]
    async fn without_a_saved_credential_the_claim_goes_on() {
        let fake = Fake::start(vec![]).await;
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !saved_device(&Paths::under(dir.path()), &fake.base)
                .await
                .unwrap()
        );
        assert!(fake.seen().is_empty());
    }

    #[tokio::test]
    async fn a_credential_the_server_accepts_skips_the_claim() {
        let answer = AccessToken {
            token: "access".into(),
            expires_at_ms: i64::MAX,
        };
        let fake = Fake::start(vec![token(200, answer.encode_to_vec())]).await;
        let (_dir, paths) = profile(&fake.base);

        assert!(saved_device(&paths, &fake.base).await.unwrap());
        assert!(paths.credential().exists());
    }

    #[tokio::test]
    async fn a_credential_the_server_no_longer_knows_is_removed() {
        let fake = Fake::start(vec![token(401, refusal(Code::Unauthenticated))]).await;
        let (_dir, paths) = profile(&fake.base);

        assert!(!saved_device(&paths, &fake.base).await.unwrap());
        assert!(!paths.credential().exists());
    }

    #[tokio::test]
    async fn any_other_answer_stops_the_run_and_keeps_the_credential() {
        let answers = [
            token(503, b"unavailable".to_vec()),
            token(429, refusal(Code::RetryLater)),
            token(401, refusal(Code::RetryLater)),
        ];
        for answer in answers {
            let fake = Fake::start(vec![answer]).await;
            let (_dir, paths) = profile(&fake.base);

            let error = saved_device(&paths, &fake.base).await.unwrap_err();

            assert!(error.contains("Nothing was changed"), "{error}");
            assert!(paths.credential().exists());
        }
    }

    #[tokio::test]
    async fn a_credential_for_another_server_stops_the_run() {
        let fake = Fake::start(vec![]).await;
        let (_dir, paths) = profile(&Url::parse("https://files.example.com/").unwrap());

        let error = saved_device(&paths, &fake.base).await.unwrap_err();

        assert!(
            error.contains("already set up with https://files.example.com/"),
            "{error}"
        );
        assert!(fake.seen().is_empty());
        assert!(paths.credential().exists());
    }
}
