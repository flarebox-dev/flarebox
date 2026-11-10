// SPDX-License-Identifier: Apache-2.0

//! The Cloudflare API v4 calls that deploy, find, upgrade and remove a Flarebox server (docs/design/cli-setup.md
//! sections 4 to 6). Each call answers Cloudflare's envelope `{success, errors, result}`.

use url::Url;

use crate::bundle::{Asset, UploadPlan};
use crate::secret::Secret;

/// A message from Cloudflare's `errors` list, such as `10042` when R2 is not enabled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudflareMessage {
    pub code: u64,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub name: String,
}

/// A Worker script as `GET /workers/scripts` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Script {
    pub id: String,
    pub tags: Vec<String>,
    pub migration_tag: Option<String>,
    pub has_assets: bool,
}

/// `GET /workers/scripts/<name>/settings`.
#[derive(Clone, Debug)]
pub struct ScriptSettings {}

/// One page of `GET /r2/buckets/<bucket>/objects`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectPage {
    pub keys: Vec<String>,
    pub cursor: Option<String>,
}

/// A client for one access token. `base` is `https://api.cloudflare.com/client/v4/`, or a local server in tests.
pub struct Cloudflare {}

impl Cloudflare {
    pub fn new(http: reqwest::Client, base: Url, token: Secret) -> Self {
        let _ = (http, base, token);
        todo!()
    }

    pub async fn accounts(&self) -> Result<Vec<Account>, ApiError> {
        todo!()
    }

    pub async fn upload_assets(&self, account: &str, script: &str, assets: &[Asset]) -> Result<Secret, ApiError> {
        let _ = (account, script, assets);
        todo!()
    }

    pub async fn upload_script(&self, account: &str, script: &str, plan: &UploadPlan) -> Result<Script, ApiError> {
        let _ = (account, script, plan);
        todo!()
    }
}
