// SPDX-License-Identifier: Apache-2.0

//! OAuth 2.0 Authorization Code with PKCE against Cloudflare's dashboard, for a public client without a secret
//! (docs/design/cli-setup.md section 2). Every token stays in memory; the caller revokes the grant when its command
//! ends (section 7).

use std::time::{Duration, Instant};

use url::Url;

use crate::secret::Secret;

/// The private development client, until the public client on flarebox.dev exists (cli-setup.md section 11,
/// decision 9). Change it here only.
pub const CLIENT_ID: &str = "ba83f6f3a70d657a488265284052a549";

/// The scopes `flarebox setup` needs (cli-setup.md section 3).
pub const SCOPES: [&str; 4] = [
    "account-settings.read",
    "workers-scripts.admin",
    "workers-r2.write",
    "offline_access",
];

/// The dashboard's OAuth endpoints. Tests point them at a local server.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub authorize: Url,
    pub token: Url,
    pub revoke: Url,
}

impl Endpoints {
    pub fn cloudflare() -> Self {
        todo!()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OauthError {}

/// One login: a PKCE verifier and a state value for exactly one code exchange.
pub struct Login {}

impl Login {
    /// A login whose redirect is `http://127.0.0.1:<port>/oauth/callback`.
    pub fn new(endpoints: Endpoints, client_id: &str, port: u16) -> Self {
        let _ = (endpoints, client_id, port);
        todo!()
    }

    pub fn authorize_url(&self) -> Url {
        todo!()
    }

    /// The code in a redirect URL, from the loopback listener or pasted by the user, after checking its state.
    pub fn code_from(&self, redirect: &Url) -> Result<AuthorizationCode, OauthError> {
        let _ = redirect;
        todo!()
    }

    /// Exchanges the code at the token endpoint. It consumes the login, so a code is never sent twice: a replayed code
    /// revokes the whole grant (section 2).
    pub async fn exchange(self, http: &reqwest::Client, code: AuthorizationCode) -> Result<Grant, OauthError> {
        let _ = (http, code);
        todo!()
    }
}

pub struct AuthorizationCode(Secret);

/// Listens on 127.0.0.1 for the browser's redirect.
pub struct Loopback {}

impl Loopback {
    /// Binds 127.0.0.1 on `port`, or on a free port.
    pub async fn bind(port: Option<u16>) -> Result<Self, OauthError> {
        let _ = port;
        todo!()
    }

    pub fn port(&self) -> u16 {
        todo!()
    }

    /// Answers requests until one reaches the callback path, and returns its code.
    pub async fn callback(&self, login: &Login) -> Result<AuthorizationCode, OauthError> {
        let _ = login;
        todo!()
    }
}

/// The tokens of one grant, in memory only.
pub struct Grant {}

impl Grant {
    pub fn access_token(&self) -> &Secret {
        todo!()
    }

    pub fn expires_within(&self, margin: Duration) -> bool {
        let _ = (margin, Instant::now());
        todo!()
    }

    /// Refreshes once and forgets the old refresh token. After a lost response the caller starts a new login instead
    /// of retrying, because a late reuse of a rotated refresh token revokes the grant (section 2).
    pub async fn refresh(&mut self, http: &reqwest::Client) -> Result<(), OauthError> {
        let _ = http;
        todo!()
    }

    /// Revokes the refresh token, then the access token.
    pub async fn revoke(self, http: &reqwest::Client) -> Result<(), OauthError> {
        let _ = http;
        todo!()
    }
}
