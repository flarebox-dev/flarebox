// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use flarebox_proto::v1::{AccessToken, DeviceCredentials, TokenRequest};
use tokio::sync::Mutex;

use crate::api::Api;
use crate::error::ApiError;

/// How long before its expiry an access token is replaced. Tokens live 15 minutes (spec v0 section 15.2).
pub const DEFAULT_REFRESH_MARGIN: Duration = Duration::from_secs(120);

/// A device's long-lived credentials from `POST /v1/claim` or `POST /v1/pair/complete`. The caller stores them.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub user_id: String,
    pub device_id: String,
    pub refresh_credential: String,
}

impl From<&DeviceCredentials> for Credentials {
    fn from(credentials: &DeviceCredentials) -> Self {
        Credentials {
            user_id: credentials.user_id.clone(),
            device_id: credentials.device_id.clone(),
            refresh_credential: credentials.refresh_credential.clone(),
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// Hands out access tokens for one device, refreshing them through `POST /v1/token` before they expire.
pub struct Auth {
    api: Api,
    credentials: Credentials,
    token: Mutex<Option<AccessToken>>,
    margin: Duration,
}

impl Auth {
    pub fn new(api: Api, credentials: Credentials) -> Self {
        Auth {
            api,
            credentials,
            token: Mutex::new(None),
            margin: DEFAULT_REFRESH_MARGIN,
        }
    }

    /// Starts with a token the caller already holds, such as the one in `DeviceCredentials`.
    pub fn with_token(self, token: AccessToken) -> Self {
        Auth {
            token: Mutex::new(Some(token)),
            ..self
        }
    }

    /// Replaces tokens this long before they expire. A session also sends `Reauth` at that point.
    pub fn with_refresh_margin(self, margin: Duration) -> Self {
        Auth { margin, ..self }
    }

    pub fn credentials(&self) -> &Credentials {
        &self.credentials
    }

    pub fn api(&self) -> &Api {
        &self.api
    }

    /// A token valid for at least the refresh margin, fetching a new one when needed.
    pub async fn access_token(&self) -> Result<AccessToken, ApiError> {
        let mut cached = self.token.lock().await;
        if let Some(token) = cached.as_ref()
            && !self.due(token)
        {
            return Ok(token.clone());
        }
        let request = TokenRequest {
            device_id: self.credentials.device_id.clone(),
            refresh_credential: self.credentials.refresh_credential.clone(),
            ..Default::default()
        };
        let token = self.api.token(&request).await?;
        *cached = Some(token.clone());
        Ok(token)
    }

    /// When the token should be replaced: the refresh margin before it expires.
    pub(crate) fn refresh_at(&self, token: &AccessToken) -> SystemTime {
        UNIX_EPOCH
            .checked_add(Duration::from_millis(token.expires_at_ms.max(0) as u64))
            .and_then(|expires| expires.checked_sub(self.margin))
            .unwrap_or(UNIX_EPOCH)
    }

    fn due(&self, token: &AccessToken) -> bool {
        SystemTime::now() >= self.refresh_at(token)
    }

    /// Drops the cached token if it is the one the server refused, so the next call fetches a new one.
    pub(crate) async fn reject(&self, token: &str) {
        let mut cached = self.token.lock().await;
        if cached
            .as_ref()
            .is_some_and(|current| current.token == token)
        {
            *cached = None;
        }
    }
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("credentials", &self.credentials)
            .field("margin", &self.margin)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(margin: Duration) -> Auth {
        let api = Api::local_development("http://127.0.0.1:9".parse().unwrap()).unwrap();
        let credentials = Credentials {
            user_id: "u".into(),
            device_id: "d".into(),
            refresh_credential: "secret".into(),
        };
        Auth::new(api, credentials).with_refresh_margin(margin)
    }

    fn token_expiring_in(from_now: Duration) -> AccessToken {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        AccessToken {
            token: "t".into(),
            expires_at_ms: (now + from_now).as_millis() as i64,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_token_outside_the_margin_is_reused_without_a_request() {
        let auth =
            auth(Duration::from_secs(120)).with_token(token_expiring_in(Duration::from_secs(600)));
        assert_eq!(auth.access_token().await.unwrap().token, "t");
    }

    #[tokio::test]
    async fn a_token_inside_the_margin_is_refreshed() {
        let auth =
            auth(Duration::from_secs(120)).with_token(token_expiring_in(Duration::from_secs(60)));
        assert!(
            matches!(auth.access_token().await, Err(ApiError::Transport(_))),
            "port 9 refuses, so a refresh was attempted"
        );
    }

    #[tokio::test]
    async fn reject_drops_only_the_refused_token() {
        let auth = auth(Duration::ZERO).with_token(token_expiring_in(Duration::from_secs(600)));
        auth.reject("other").await;
        assert_eq!(auth.access_token().await.unwrap().token, "t");
        auth.reject("t").await;
        assert!(auth.access_token().await.is_err());
    }

    #[test]
    fn debug_output_leaves_out_the_secrets() {
        let auth = auth(Duration::ZERO);
        let text = format!("{auth:?} {:?}", auth.credentials());
        assert!(!text.contains("secret"), "{text}");
    }
}
