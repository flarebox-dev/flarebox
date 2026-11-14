// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

use url::{Host, Url};

pub(super) struct Config {
    pub base: Url,
    pub bundle: PathBuf,
}

pub(super) fn config() -> Result<Option<Config>, String> {
    let base = std::env::var_os("KOIDRIVE_TEST_SETUP_URL");
    let bundle = std::env::var_os("KOIDRIVE_TEST_SETUP_BUNDLE");
    let (base, bundle) = match (base, bundle) {
        (None, None) => return Ok(None),
        (Some(base), Some(bundle)) => (base, PathBuf::from(bundle)),
        _ => return Err("test setup needs both its loopback URL and bundle".into()),
    };
    let base = base
        .to_str()
        .ok_or("the test setup URL is not UTF-8")?
        .parse::<Url>()
        .map_err(|error| format!("invalid test setup URL: {error}"))?;
    let loopback = match base.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if !loopback
        || base.scheme() != "http"
        || base.port().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.path() != "/"
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err("test setup requires an HTTP loopback IP URL with an explicit port and no other components".into());
    }
    if !bundle.is_absolute() || !bundle.is_dir() {
        return Err("the test setup bundle must be an absolute directory".into());
    }
    Ok(Some(Config { base, bundle }))
}
