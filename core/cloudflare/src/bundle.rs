// SPDX-License-Identifier: Apache-2.0

//! The server bundle the CLI embeds, and the upload it turns into (docs/design/cli-setup.md sections 4.2 and 4.3).

use serde::Deserialize;

use crate::secret::Secret;

/// `manifest.json` of a server bundle, written by `server/scripts/pack-bundle.mjs` from `server/wrangler.jsonc`.
#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {}

/// One static asset, with its path under the assets root such as `/viewer/index.html`.
#[derive(Clone, Debug)]
pub struct Asset {
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Bundle {
    pub manifest: Manifest,
    pub module: Vec<u8>,
    pub assets: Vec<Asset>,
}

/// The metadata and module of one script upload.
pub struct UploadPlan {}

impl UploadPlan {
    /// `deployed_tag` is the script's current migration tag, if it exists. `secret` is a one-time
    /// `FLAREBOX_RECOVERY_KEY` sent in the same upload. `assets_jwt` is the completion token of the asset upload.
    pub fn new(
        bundle: &Bundle,
        bucket: &str,
        deployed_tag: Option<&str>,
        secret: Option<&Secret>,
        assets_jwt: Option<&Secret>,
    ) -> Result<Self, String> {
        let _ = (bundle, bucket, deployed_tag, secret, assets_jwt);
        todo!()
    }
}
