// SPDX-License-Identifier: Apache-2.0

use flarebox_proto::v1::{
    AccessToken, ClaimRequest, CreateSpaceRequest, DeviceCredentials, Error as ProtoError,
    PairCompleteRequest, PairStartResponse, RewindRequest, RewindResult, ServerInfo, SpaceInfo,
    SpaceList, TokenRequest,
};
use prost::Message;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::{Method, RequestBuilder};
use url::Url;

use crate::auth::Auth;
use crate::error::{ApiError, ServerError};

const PROTOBUF: &str = "application/x-protobuf";

/// The HTTP endpoints of one Flarebox deployment (spec v0 section 15.1). Bodies are binary protobuf.
#[derive(Clone, Debug)]
pub struct Api {
    http: reqwest::Client,
    base: Url,
}

impl Api {
    /// `base` is the deployment's origin, for example `https://files.example.com`. A path is kept as a prefix.
    pub fn new(base: Url) -> Result<Self, ApiError> {
        if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
            return Err(ApiError::Url(format!("{base} is not an http or https URL")));
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
