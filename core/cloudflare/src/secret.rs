// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// An OAuth token, a PKCE verifier or a one-time key. It is kept in memory only: `Debug` never shows it, and nothing in
/// this crate writes it to disk or to a log.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Secret(value)
    }

    /// `bytes` random bytes from the operating system, base64url-encoded without padding.
    pub fn random(bytes: usize) -> Self {
        let mut buffer = vec![0u8; bytes];
        getrandom::fill(&mut buffer).expect("the operating system provides random bytes");
        Secret(URL_SAFE_NO_PAD.encode(buffer))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}
