// SPDX-License-Identifier: Apache-2.0

use flarebox_proto::v1::{
    AccessToken, ClaimRequest, CreateSpaceRequest, DeviceCredentials, Error as ProtoError,
    PairCompleteRequest, PairStartResponse, RewindRequest, RewindResult, ServerInfo, SpaceInfo,
    SpaceList, TokenRequest,
};
use std::time::Duration;

use prost::Message;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::{Method, RequestBuilder, redirect};
use url::{Host, Url};

use crate::auth::Auth;
use crate::error::{ApiError, ServerError};

const PROTOBUF: &str = "application/x-protobuf";
/// Bounds making a connection, including TLS.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Bounds the wait for any single read of a response.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds a whole call to an HTTP endpoint.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The HTTP endpoints of one Flarebox deployment (spec v0 section 15.1). Bodies are binary protobuf.
///
/// Credentials and tokens travel only over HTTPS, and the WebSocket only over WSS. Plain HTTP is allowed only for
/// a loopback address, and only through [`Api::local_development`]. Redirects are never followed, so a request
/// that carries a token or a refresh credential is never resent to another address or downgraded to HTTP.
#[derive(Clone, Debug)]
pub struct Api {
    http: reqwest::Client,
    base: Url,
}

impl Api {
    /// `base` is the deployment's HTTPS origin, for example `https://files.example.com`. A path is kept as a prefix.
    pub fn new(base: Url) -> Result<Self, ApiError> {
        Api::build(base, false)
    }

    /// Like [`Api::new`], but also accepts `http://` on a loopback address (`localhost`, `127.0.0.0/8`, `::1`),
    /// for a server under `wrangler dev`.
    pub fn local_development(base: Url) -> Result<Self, ApiError> {
        Api::build(base, true)
    }

    fn build(base: Url, local_development: bool) -> Result<Self, ApiError> {
        if base.host_str().is_none() || !permitted(&base, local_development) {
            let rule = if local_development {
                "an https URL, or an http URL on a loopback address"
            } else {
                "an https URL; plain http is only for local development on a loopback address"
            };
            return Err(ApiError::Url(format!("{base} is not {rule}")));
        }
        let mut base = base;
        base.set_query(None);
        base.set_fragment(None);
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("flarebox-net/", env!("CARGO_PKG_VERSION")))
            .https_only(!local_development)
            .redirect(redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(ApiError::Transport)?;
        Ok(Api { http, base })
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    /// Joins a path relative to the base, such as `v1/server`.
    pub(crate) fn url(&self, path: &str) -> Url {
        self.base
            .join(path)
            .expect("a relative path joins onto an http URL")
    }

    /// `GET /v1/server`.
    pub async fn server_info(&self) -> Result<ServerInfo, ApiError> {
        self.send(self.http.get(self.url("v1/server")), "ServerInfo")
            .await
    }

    /// `POST /v1/claim`: registers the first owner with the one-time claim code from the Worker log.
    pub async fn claim(&self, request: &ClaimRequest) -> Result<DeviceCredentials, ApiError> {
        self.send(self.post("v1/claim", request), "DeviceCredentials")
            .await
    }

    /// `POST /v1/pair/complete`, called by the new device with the code an already paired device received.
    pub async fn pair_complete(
        &self,
        request: &PairCompleteRequest,
    ) -> Result<DeviceCredentials, ApiError> {
        self.send(self.post("v1/pair/complete", request), "DeviceCredentials")
            .await
    }

    /// `POST /v1/token`: exchanges the refresh credential for an access token.
    pub async fn token(&self, request: &TokenRequest) -> Result<AccessToken, ApiError> {
        self.send(self.post("v1/token", request), "AccessToken")
            .await
    }

    /// `POST /v1/pair/start`, called by an already paired device.
    pub async fn pair_start(&self, auth: &Auth) -> Result<PairStartResponse, ApiError> {
        self.authed(
            auth,
            Method::POST,
            "v1/pair/start",
            None,
            "PairStartResponse",
        )
        .await
    }

    /// `GET /v1/spaces`.
    pub async fn list_spaces(&self, auth: &Auth) -> Result<SpaceList, ApiError> {
        self.authed(auth, Method::GET, "v1/spaces", None, "SpaceList")
            .await
    }

    /// `POST /v1/spaces`.
    pub async fn create_space(
        &self,
        auth: &Auth,
        request: &CreateSpaceRequest,
    ) -> Result<SpaceInfo, ApiError> {
        self.authed(
            auth,
            Method::POST,
            "v1/spaces",
            Some(request.encode_to_vec()),
            "SpaceInfo",
        )
        .await
    }

    /// `POST /v1/spaces/{space_id}/rewind`, owner only (RFC 0003 section 1). Retry with the same `rewind_id`.
    pub async fn rewind(
        &self,
        auth: &Auth,
        space_id: &str,
        request: &RewindRequest,
    ) -> Result<RewindResult, ApiError> {
        let path = format!("v1/spaces/{space_id}/rewind");
        self.authed(
            auth,
            Method::POST,
            &path,
            Some(request.encode_to_vec()),
            "RewindResult",
        )
        .await
    }

    fn post(&self, path: &str, body: &impl Message) -> RequestBuilder {
        self.http
            .post(self.url(path))
            .header(CONTENT_TYPE, PROTOBUF)
            .body(body.encode_to_vec())
    }

    /// Sends with the current access token. A 401 gets one retry with a fresh token, because the cached one may have
    /// expired on the server's clock.
    async fn authed<M: Message + Default>(
        &self,
        auth: &Auth,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
        what: &'static str,
    ) -> Result<M, ApiError> {
        let mut token = auth.access_token().await?;
        let mut refreshed = false;
        loop {
            let mut request = self
                .http
                .request(method.clone(), self.url(path))
                .bearer_auth(&token.token);
            if let Some(body) = &body {
                request = request.header(CONTENT_TYPE, PROTOBUF).body(body.clone());
            }
            match self.send(request, what).await {
                Err(error) if error.is_unauthenticated() && !refreshed => {
                    auth.reject(&token.token).await;
                    token = auth.access_token().await?;
                    refreshed = true;
                }
                outcome => return outcome,
            }
        }
    }

    async fn send<M: Message + Default>(
        &self,
        request: RequestBuilder,
        what: &'static str,
    ) -> Result<M, ApiError> {
        let response = request
            .header(ACCEPT, PROTOBUF)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(ApiError::Transport)?;
        let status = response.status();
        let is_protobuf = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with(PROTOBUF));
        let body = response.bytes().await.map_err(ApiError::Transport)?;
        if status.is_success() {
            return M::decode(body).map_err(|_| ApiError::Decode(what));
        }
        let status = status.as_u16();
        match is_protobuf.then(|| ProtoError::decode(body)) {
            Some(Ok(error)) => Err(ApiError::Server {
                status,
                error: ServerError(error),
            }),
            _ => Err(ApiError::Status { status }),
        }
    }
}

fn permitted(url: &Url, local_development: bool) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => local_development && loopback(url),
        _ => false,
    }
}

fn loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn plain_http_is_refused_outside_local_development_on_loopback() {
        let url = |text: &str| text.parse::<Url>().unwrap();
        for remote in [
            "http://203.0.113.1",
            "http://files.example.com",
            "ws://127.0.0.1",
            "ftp://127.0.0.1",
        ] {
            assert!(Api::new(url(remote)).is_err(), "{remote}");
            assert!(Api::local_development(url(remote)).is_err(), "{remote}");
        }
        for local in [
            "http://127.0.0.1:8787",
            "http://127.1.2.3",
            "http://[::1]:8787",
            "http://localhost:8787",
        ] {
            assert!(Api::new(url(local)).is_err(), "{local} needs the opt-in");
            assert!(Api::local_development(url(local)).is_ok(), "{local}");
        }
        assert!(Api::new(url("https://files.example.com/fb")).is_ok());
        assert!(Api::local_development(url("https://files.example.com")).is_ok());
    }

    #[tokio::test]
    async fn redirects_are_not_followed_with_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base: Url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let n = socket.read(&mut request).await.unwrap();
            let location = "http://203.0.113.1/v1/token";
            let reply = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nlocation: {location}\r\ncontent-length: 0\r\n\r\n"
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            let mut rest = Vec::new();
            let _ = socket.read_to_end(&mut rest).await;
            String::from_utf8_lossy(&request[..n]).into_owned()
        });
        let api = Api::local_development(base).unwrap();
        let request = TokenRequest {
            device_id: "d".into(),
            refresh_credential: "secret".into(),
        };
        let outcome = api.token(&request).await;
        assert!(
            matches!(outcome, Err(ApiError::Status { status: 307 })),
            "{outcome:?}"
        );
        drop(api);
        assert!(
            server.await.unwrap().starts_with("POST /v1/token"),
            "one request, not followed"
        );
    }
}
