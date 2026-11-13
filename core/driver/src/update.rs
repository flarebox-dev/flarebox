// SPDX-License-Identifier: Apache-2.0

//! Whether a newer Koidrive release is out. The sync host asks GitHub's release list at most once a day and keeps
//! the answer in the profile's `update-check.json`. Every reader turns that file into an [`Update`] for the version
//! it runs, with no request of its own. `koi update` asks again at once with [`check_now`].

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{ACCEPT, HeaderMap, IF_NONE_MATCH};
use reqwest::{StatusCode, redirect};
use semver::Version;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::AbortHandle;
use url::Url;

use crate::config::{Config, UpdateCheck};
use crate::paths::Paths;

pub const VARIABLE: &str = "KOIDRIVE_UPDATE_CHECK";

pub const URL_VARIABLE: &str = "KOIDRIVE_UPDATE_CHECK_URL";

const API: &str = "https://api.github.com/";
const RELEASES: &str = "repos/koidrive/koidrive/releases?per_page=20";
const TIMEOUT: Duration = Duration::from_secs(10);
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

/// Whether a newer Koidrive release is out than the version the reporting process runs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Update {
    Available {
        latest: String,
        url: String,
        checked_at_ms: i64,
    },
    UpToDate {
        checked_at_ms: i64,
    },
    /// No check has had an answer from GitHub yet, or the daemon is too old to say.
    #[default]
    Unknown,
    /// `[updates] check = false` in config.toml or [`VARIABLE`] turned the check off.
    Off,
}

/// The release of the device's track: the newest stable release once one exists, else the newest prerelease. A
/// release counts once it lists the tarball for this target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub version: String,
    /// The release's page on GitHub.
    pub url: String,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CheckError {
    #[error("GitHub cannot be reached")]
    Unreachable,
    #[error("GitHub did not send the list of Koidrive releases; try again later")]
    Refused,
    #[error("{URL_VARIABLE} is not an https URL or an http URL on a loopback address")]
    Url,
}

/// Whether the update check is on for the profile at `paths`: `[updates]` in its config.toml, unless [`VARIABLE`]
/// says otherwise. A config.toml that cannot be read leaves it on.
pub fn is_on(paths: &Paths) -> bool {
    let config = Config::load(&paths.config_file).map(|config| config.updates);
    switched_on(
        &config.unwrap_or_default(),
        std::env::var(VARIABLE).ok().as_deref(),
    )
}

pub(crate) fn switched_on(config: &UpdateCheck, variable: Option<&str>) -> bool {
    match variable
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("0" | "off" | "false") => false,
        Some("1" | "on" | "true") => true,
        _ => config.check,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Checked {
    #[serde(default)]
    pub checked_at: Option<i64>,
    #[serde(default)]
    pub successful_at: Option<i64>,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub release: Option<Release>,
    #[serde(default)]
    pub retry_after: Option<i64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    Releases {
        etag: Option<String>,
        release: Option<Release>,
    },
    NotModified,
    Refused {
        until: Option<i64>,
    },
    Unreachable,
}

impl Checked {
    pub fn due(&self) -> i64 {
        let day = (self.checked_at).map_or(i64::MIN, |at| at.saturating_add(DAY_MS));
        day.max(self.retry_after.unwrap_or(i64::MIN))
    }

    pub fn after(self, answer: Answer, now: i64) -> Checked {
        let successful_at = self
            .successful_at
            .or_else(|| self.release.as_ref().and(self.checked_at));
        match answer {
            Answer::Releases { etag, release } => Checked {
                checked_at: Some(now),
                successful_at: Some(now),
                etag,
                release,
                retry_after: None,
            },
            Answer::NotModified => Checked {
                checked_at: Some(now),
                successful_at: successful_at.map(|_| now),
                retry_after: None,
                ..self
            },
            Answer::Refused { until } => Checked {
                checked_at: Some(now),
                successful_at,
                retry_after: until.filter(|until| *until > now.saturating_add(DAY_MS)),
                ..self
            },
            Answer::Unreachable => Checked {
                successful_at,
                retry_after: Some(now.saturating_add(HOUR_MS)),
                ..self
            },
        }
    }

    pub fn notice(&self, runs: &Version) -> Update {
        let Some(checked_at_ms) = self
            .successful_at
            .or_else(|| self.release.as_ref().and(self.checked_at))
        else {
            return Update::Unknown;
        };
        let newer = (self.release.as_ref())
            .and_then(|release| Some((Version::parse(&release.version).ok()?, release)))
            .filter(|(latest, _)| latest.cmp_precedence(runs).is_gt());
        match newer {
            Some((latest, release)) => Update::Available {
                latest: latest.to_string(),
                url: release.url.clone(),
                checked_at_ms,
            },
            None => Update::UpToDate { checked_at_ms },
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct Listed {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
}

pub(crate) fn track(listed: &[Listed], asset: &str) -> Option<Release> {
    let published: Vec<(Version, &Listed)> = (listed.iter())
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let version = Version::parse(release.tag_name.strip_prefix('v')?).ok()?;
            Some((version, release))
        })
        .collect();
    let stable = published.iter().any(|(version, _)| version.pre.is_empty());
    published
        .into_iter()
        .filter(|(version, _)| !stable || version.pre.is_empty())
        .filter(|(_, release)| release.assets.iter().any(|listed| listed.name == asset))
        .max_by(|(a, _), (b, _)| a.cmp_precedence(b))
        .map(|(version, release)| Release {
            version: version.to_string(),
            url: release.html_url.clone(),
        })
}

pub(crate) fn this_asset() -> String {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        _ => "unknown-linux-musl",
    };
    format!("koidrive-{}-{os}.tar.gz", std::env::consts::ARCH)
}

pub(crate) fn answer(
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
    asset: &str,
    now: i64,
) -> Answer {
    let header = |name: &str| {
        (headers.get(name))
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    let seconds = |name: &str| header(name).and_then(|value| value.parse::<i64>().ok());
    match status.as_u16() {
        200 => match serde_json::from_slice::<Vec<Listed>>(body) {
            Ok(listed) => Answer::Releases {
                etag: header("etag").map(str::to_owned),
                release: track(&listed, asset),
            },
            Err(error) => {
                tracing::warn!(%error, "GitHub's list of Koidrive releases cannot be read");
                Answer::Refused { until: None }
            }
        },
        304 => Answer::NotModified,
        403 | 429 => {
            let retry =
                seconds("retry-after").map(|after| now.saturating_add(after.saturating_mul(1000)));
            let reset = (header("x-ratelimit-remaining") == Some("0"))
                .then(|| seconds("x-ratelimit-reset"))
                .flatten()
                .map(|reset| reset.saturating_mul(1000));
            Answer::Refused {
                until: retry.max(reset),
            }
        }
        _ => Answer::Refused { until: None },
    }
}

pub(crate) fn releases_url(base: Option<&str>) -> Option<(Url, bool)> {
    let mut base = Url::parse(base.unwrap_or(API)).ok()?;
    if !base.path().ends_with('/') {
        base.path_segments_mut().ok()?.push("");
    }
    let loopback = matches!(base.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    let https = match base.scheme() {
        "https" => true,
        "http" if loopback => false,
        _ => return None,
    };
    Some((base.join(RELEASES).ok()?, https))
}

pub(crate) async fn ask(
    http: &reqwest::Client,
    url: Url,
    etag: Option<&str>,
    asset: &str,
) -> Answer {
    let mut request = (http.get(url))
        .header(ACCEPT, "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28");
    if let Some(etag) = etag {
        request = request.header(IF_NONE_MATCH, etag);
    }
    let Ok(response) = request.send().await else {
        return Answer::Unreachable;
    };
    let (status, headers) = (response.status(), response.headers().clone());
    let body = match status {
        StatusCode::OK => match response.bytes().await {
            Ok(body) => body,
            Err(_) => return Answer::Refused { until: None },
        },
        _ => Default::default(),
    };
    answer(status, &headers, &body, asset, now_ms())
}

pub(crate) fn client(https_only: bool) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("koidrive")
        .https_only(https_only)
        .redirect(redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
}

fn endpoint() -> Result<(reqwest::Client, Url), CheckError> {
    let base = std::env::var(URL_VARIABLE).ok();
    let (url, https) = releases_url(base.as_deref()).ok_or(CheckError::Url)?;
    let http = client(https).map_err(|_| CheckError::Unreachable)?;
    Ok((http, url))
}

/// Asks GitHub for the track's release now, whatever the time of the last check, with the request of the daily check.
/// A list that GitHub answers is unchanged since the profile's last check gives that check's release. Nothing is
/// saved.
pub async fn check_now(paths: &Paths) -> Result<Option<Release>, CheckError> {
    let (http, url) = endpoint()?;
    let checked = load(&paths.update_check()).unwrap_or_default();
    match ask(&http, url, checked.etag.as_deref(), &this_asset()).await {
        Answer::Releases { release, .. } => Ok(release),
        Answer::NotModified => Ok(checked.release),
        Answer::Refused { .. } => Err(CheckError::Refused),
        Answer::Unreachable => Err(CheckError::Unreachable),
    }
}

pub(crate) fn load(path: &Path) -> Option<Checked> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn save(path: &Path, checked: &Checked) -> io::Result<()> {
    let temp = path.with_extension("json.new");
    let mut file = File::create(&temp)?;
    file.write_all(&serde_json::to_vec(checked)?)?;
    file.sync_all()?;
    fs::rename(&temp, path)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

fn this_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate's version is SemVer")
}

pub(crate) struct Updates {
    on: bool,
    path: PathBuf,
    checked: Mutex<Option<Checked>>,
    task: Mutex<Option<AbortHandle>>,
}

impl Updates {
    pub fn open(on: bool, path: PathBuf) -> Updates {
        let checked = on.then(|| load(&path)).flatten();
        Updates {
            on,
            path,
            checked: Mutex::new(checked),
            task: Mutex::new(None),
        }
    }

    pub fn notice(&self) -> Update {
        match (self.on, self.checked.lock().unwrap().as_ref()) {
            (false, _) => Update::Off,
            (true, None) => Update::Unknown,
            (true, Some(checked)) => checked.notice(&this_version()),
        }
    }

    pub fn start(self: &Arc<Self>, changed: Arc<watch::Sender<()>>) {
        if !self.on {
            match fs::remove_file(&self.path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    tracing::warn!(%error, "the update check's file cannot be removed");
                }
                _ => {}
            }
            return;
        }
        let mut task = self.task.lock().unwrap();
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        let (http, url) = match endpoint() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                tracing::warn!(%error, "the update check cannot start");
                return;
            }
        };
        let updates = self.clone();
        *task = Some(tokio::spawn(updates.keep_checking(http, url, changed)).abort_handle());
    }

    pub fn stop(&self) {
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }

    async fn keep_checking(
        self: Arc<Self>,
        http: reqwest::Client,
        url: Url,
        changed: Arc<watch::Sender<()>>,
    ) {
        let asset = this_asset();
        loop {
            let checked = self.checked.lock().unwrap().clone().unwrap_or_default();
            let wait = checked.due().saturating_sub(now_ms());
            if wait > 0 {
                tokio::time::sleep(Duration::from_millis(wait.min(HOUR_MS) as u64)).await;
                continue;
            }
            let answer = ask(&http, url.clone(), checked.etag.as_deref(), &asset).await;
            let checked = checked.after(answer, now_ms());
            if let Err(error) = save(&self.path, &checked) {
                tracing::warn!(%error, "the update check's answer cannot be saved");
            }
            *self.checked.lock().unwrap() = Some(checked);
            changed.send_replace(());
        }
    }
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    const ASSET: &str = "koidrive-aarch64-apple-darwin.tar.gz";

    fn listed(tag: &str, draft: bool, assets: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "html_url": format!("https://github.com/koidrive/koidrive/releases/tag/{tag}"),
            "draft": draft,
            "prerelease": tag.contains('-'),
            "assets": assets.iter().map(|name| serde_json::json!({ "name": name })).collect::<Vec<_>>(),
        })
    }

    fn picked(releases: &[serde_json::Value]) -> Option<String> {
        let listed: Vec<Listed> =
            serde_json::from_value(serde_json::Value::from(releases.to_vec())).unwrap();
        track(&listed, ASSET).map(|release| release.version)
    }

    #[test]
    fn the_track_is_the_newest_stable_release_once_one_exists_else_the_newest_prerelease() {
        let full = [ASSET, "SHA256SUMS"];
        let alphas = [
            listed("v0.1.0-alpha.2", false, &full),
            listed("v0.1.0-alpha.10", false, &full),
            listed("v0.1.0-alpha.9", false, &full),
        ];
        assert_eq!(picked(&alphas).as_deref(), Some("0.1.0-alpha.10"));

        let mixed = [
            listed("v0.2.0-alpha.1", false, &full),
            listed("v0.1.0", false, &full),
            listed("v0.1.0-rc.1", false, &full),
        ];
        assert_eq!(picked(&mixed).as_deref(), Some("0.1.0"));

        let backport = [
            listed("v0.1.3", false, &full),
            listed("v0.2.0", false, &full),
            listed("v0.1.2", false, &full),
        ];
        assert_eq!(picked(&backport).as_deref(), Some("0.2.0"));
    }

    #[test]
    fn drafts_tags_that_do_not_parse_and_releases_without_this_targets_tarball_do_not_count() {
        let full = [ASSET];
        assert_eq!(
            picked(&[
                listed("v0.3.0", true, &full),
                listed("0.4.0", false, &full),
                listed("v0.5", false, &full),
                listed("nightly", false, &full),
                listed(
                    "v0.2.0",
                    false,
                    &["koidrive-x86_64-unknown-linux-musl.tar.gz"]
                ),
                listed("v0.1.1", false, &full),
            ])
            .as_deref(),
            Some("0.1.1")
        );
        assert_eq!(
            picked(&[
                listed("v0.1.0", false, &["SHA256SUMS"]),
                listed("v0.1.0-alpha.3", false, &full),
            ]),
            None,
            "a stable release whose build is missing does not send the device back to prereleases"
        );
        assert_eq!(picked(&[listed("v0.1.0", true, &full)]), None);
        assert_eq!(picked(&[]), None);
    }

    fn release(version: &str) -> Option<Release> {
        Some(Release {
            version: version.to_owned(),
            url: format!("https://github.com/koidrive/koidrive/releases/tag/v{version}"),
        })
    }

    #[test]
    fn a_device_is_told_only_about_a_release_above_the_version_it_runs() {
        let checked = |version: &str| Checked {
            checked_at: Some(1_000),
            release: release(version),
            ..Checked::default()
        };
        let runs = |version: &str| Version::parse(version).unwrap();
        let notice = |cached: &str, running: &str| checked(cached).notice(&runs(running));
        assert_eq!(
            notice("0.1.2", "0.1.0"),
            Update::Available {
                latest: "0.1.2".into(),
                url: "https://github.com/koidrive/koidrive/releases/tag/v0.1.2".into(),
                checked_at_ms: 1_000,
            }
        );
        assert_eq!(
            notice("0.1.0", "0.1.0"),
            Update::UpToDate {
                checked_at_ms: 1_000
            }
        );
        assert_eq!(
            notice("0.1.0", "0.2.0-alpha.1"),
            Update::UpToDate {
                checked_at_ms: 1_000
            }
        );
        assert!(matches!(
            notice("0.1.0", "0.1.0-alpha.4"),
            Update::Available { .. }
        ));
        assert!(matches!(
            notice("0.2.0-alpha.2", "0.2.0-alpha.1"),
            Update::Available { .. }
        ));
        assert_eq!(Checked::default().notice(&runs("0.1.0")), Update::Unknown);
        let unanswered = Checked::default().after(Answer::Unreachable, 5);
        assert_eq!(unanswered.notice(&runs("0.1.0")), Update::Unknown);
    }

    #[test]
    fn a_check_is_due_a_day_after_githubs_last_answer_an_hour_after_no_answer_and_not_before_a_rate_limit_resets()
     {
        let now = 10 * DAY_MS;
        let first = Checked::default();
        assert!(
            first.due() <= now,
            "a profile that never checked checks at once"
        );

        let answered = first.clone().after(
            Answer::Releases {
                etag: Some("\"e1\"".into()),
                release: release("0.1.2"),
            },
            now,
        );
        assert_eq!(answered.due(), now + DAY_MS);
        assert_eq!(answered.etag.as_deref(), Some("\"e1\""));

        let later = now + DAY_MS;
        let same = answered.clone().after(Answer::NotModified, later);
        assert_eq!(
            (same.checked_at, same.release.clone(), same.etag.clone()),
            (Some(later), release("0.1.2"), Some("\"e1\"".into()))
        );
        assert_eq!(same.due(), later + DAY_MS);

        let offline = answered.clone().after(Answer::Unreachable, later);
        assert_eq!(offline.checked_at, Some(now), "no answer is no day's check");
        assert_eq!(
            offline.release,
            release("0.1.2"),
            "the last good answer stays"
        );
        assert_eq!(offline.due(), later + HOUR_MS);
        let back = offline.after(Answer::NotModified, later + HOUR_MS);
        assert_eq!(back.due(), later + HOUR_MS + DAY_MS);

        let refused = answered
            .clone()
            .after(Answer::Refused { until: None }, later);
        assert_eq!(
            (refused.due(), refused.release.clone()),
            (later + DAY_MS, release("0.1.2"))
        );

        let soon = answered.clone().after(
            Answer::Refused {
                until: Some(later + HOUR_MS),
            },
            later,
        );
        assert_eq!(
            soon.due(),
            later + DAY_MS,
            "a rate limit waits at least a day"
        );
        let reset = later + DAY_MS + 5 * HOUR_MS;
        let limited = answered.after(Answer::Refused { until: Some(reset) }, later);
        assert_eq!(limited.due(), reset);
        assert_eq!(
            limited.after(Answer::NotModified, reset).due(),
            reset + DAY_MS
        );
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn failed_checks_back_off_without_claiming_a_successful_check() {
        let runs = Version::parse("0.1.0").unwrap();
        for (status, body) in [(500, b"".as_slice()), (403, b""), (200, b"not json")] {
            let failure = || {
                answer(
                    StatusCode::from_u16(status).unwrap(),
                    &HeaderMap::new(),
                    body,
                    ASSET,
                    100,
                )
            };
            let failed = Checked::default().after(failure(), 100);
            assert_eq!(failed.notice(&runs), Update::Unknown, "{status}");
            assert_eq!(failed.due(), 100 + DAY_MS);
            for (version, expected) in [
                (None, Update::UpToDate { checked_at_ms: 200 }),
                (release("0.1.0"), Update::UpToDate { checked_at_ms: 200 }),
                (
                    release("0.1.2"),
                    Update::Available {
                        latest: "0.1.2".into(),
                        url: "https://github.com/koidrive/koidrive/releases/tag/v0.1.2".into(),
                        checked_at_ms: 200,
                    },
                ),
            ] {
                let success = failed.clone().after(
                    Answer::Releases {
                        etag: Some("e1".into()),
                        release: version,
                    },
                    200,
                );
                assert_eq!(success.notice(&runs), expected);
                let failed_again = success.after(failure(), 300);
                assert_eq!(failed_again.notice(&runs), expected);
                assert_eq!(failed_again.due(), 300 + DAY_MS);
            }
        }
    }

    #[test]
    fn not_modified_without_a_successful_cache_does_not_claim_up_to_date() {
        let runs = Version::parse("0.1.0").unwrap();
        let checked = Checked::default().after(Answer::NotModified, 100);
        assert_eq!(checked.notice(&runs), Update::Unknown);
        assert_eq!(checked.due(), 100 + DAY_MS);
    }

    #[test]
    fn legacy_caches_with_a_release_keep_their_success_time_after_failure() {
        let runs = Version::parse("0.1.0").unwrap();
        let legacy: Checked = serde_json::from_str(
            r#"{"checked_at":100,"release":{"version":"0.1.0","url":"https://example.com"}}"#,
        )
        .unwrap();
        let failed = legacy.after(Answer::Refused { until: None }, 200);
        assert_eq!(
            failed.notice(&runs),
            Update::UpToDate { checked_at_ms: 100 }
        );
        let restored: Checked =
            serde_json::from_slice(&serde_json::to_vec(&failed).unwrap()).unwrap();
        assert_eq!(
            restored.notice(&runs),
            Update::UpToDate { checked_at_ms: 100 }
        );
        let same = restored.after(Answer::NotModified, 300);
        assert_eq!(same.notice(&runs), Update::UpToDate { checked_at_ms: 300 });
        let empty: Checked = serde_json::from_str(r#"{"checked_at":100,"release":null}"#).unwrap();
        assert_eq!(empty.notice(&runs), Update::Unknown);
        let answered = empty.after(
            Answer::Releases {
                etag: None,
                release: None,
            },
            400,
        );
        let restored: Checked =
            serde_json::from_slice(&serde_json::to_vec(&answered).unwrap()).unwrap();
        assert_eq!(
            restored.after(Answer::NotModified, 500).notice(&runs),
            Update::UpToDate { checked_at_ms: 500 }
        );
    }

    #[test]
    fn every_answer_from_github_is_the_days_check_and_a_rate_limit_waits_for_its_reset() {
        let now = 1_000_000;
        let body = serde_json::to_vec(&[listed("v0.1.2", false, &[ASSET])]).unwrap();
        let answered = |status: u16, pairs: &[(&'static str, &str)], body: &[u8]| {
            answer(
                StatusCode::from_u16(status).unwrap(),
                &headers(pairs),
                body,
                ASSET,
                now,
            )
        };
        assert_eq!(
            answered(200, &[("etag", "W/\"abc\"")], &body),
            Answer::Releases {
                etag: Some("W/\"abc\"".into()),
                release: release("0.1.2"),
            }
        );
        assert_eq!(answered(304, &[], b""), Answer::NotModified);
        assert_eq!(
            answered(200, &[], b"<html>"),
            Answer::Refused { until: None }
        );
        assert_eq!(
            answered(
                403,
                &[
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", "5000")
                ],
                b""
            ),
            Answer::Refused {
                until: Some(5_000_000)
            }
        );
        assert_eq!(
            answered(
                403,
                &[
                    ("x-ratelimit-remaining", "12"),
                    ("x-ratelimit-reset", "5000")
                ],
                b""
            ),
            Answer::Refused { until: None },
            "a 403 with requests left is not a rate limit"
        );
        assert_eq!(
            answered(429, &[("retry-after", "60")], b""),
            Answer::Refused {
                until: Some(now + 60_000)
            }
        );
        for status in [301, 404, 500, 502] {
            assert_eq!(
                answered(status, &[], b""),
                Answer::Refused { until: None },
                "{status}"
            );
        }
    }

    #[test]
    fn only_zero_off_or_false_in_the_variable_turns_the_check_off_and_it_wins_over_the_file() {
        let on = UpdateCheck { check: true };
        let off = UpdateCheck { check: false };
        assert!(switched_on(&on, None));
        assert!(!switched_on(&off, None));
        for value in ["0", "off", "OFF", "false", " 0 "] {
            assert!(!switched_on(&on, Some(value)), "{value}");
        }
        for value in ["1", "on", "true"] {
            assert!(switched_on(&off, Some(value)), "{value}");
        }
        assert!(switched_on(&on, Some("")));
        assert!(!switched_on(&off, Some("yes")));
    }

    #[test]
    fn the_api_base_is_https_or_http_on_a_loopback_address() {
        let (url, https) = releases_url(None).unwrap();
        assert_eq!(
            url.as_str(),
            "https://api.github.com/repos/koidrive/koidrive/releases?per_page=20"
        );
        assert!(https);
        let (url, https) = releases_url(Some("http://127.0.0.1:8080/api")).unwrap();
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:8080/api/repos/koidrive/koidrive/releases?per_page=20"
        );
        assert!(!https);
        assert_eq!(releases_url(Some("http://example.com")), None);
        assert_eq!(releases_url(Some("file:///tmp")), None);
    }

    async fn served(listener: &TcpListener, response: &str) -> String {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        while !request.ends_with(b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0, "the request ended early");
            request.extend_from_slice(&buffer[..read]);
        }
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        String::from_utf8(request).unwrap()
    }

    fn header_lines(request: &str) -> Vec<String> {
        let mut lines: Vec<String> = (request.lines().skip(1))
            .filter(|line| !line.is_empty())
            .map(|line| line.to_ascii_lowercase())
            .collect();
        lines.sort();
        lines
    }

    #[tokio::test]
    async fn the_request_carries_only_the_headers_github_needs_and_the_etag_of_the_last_answer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let (url, https) = releases_url(Some(&base)).unwrap();
        let http = client(https).unwrap();
        let body = serde_json::to_string(&[listed("v0.1.2", false, &[ASSET])]).unwrap();
        let ok = format!(
            "HTTP/1.1 200 OK\r\netag: \"e1\"\r\nset-cookie: id=1\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        let (request, answered) =
            tokio::join!(served(&listener, &ok), ask(&http, url.clone(), None, ASSET));
        assert!(
            request.starts_with("GET /repos/koidrive/koidrive/releases?per_page=20 HTTP/1.1\r\n"),
            "{request}"
        );
        let host = format!("host: {}", listener.local_addr().unwrap());
        assert_eq!(
            header_lines(&request),
            [
                "accept: application/vnd.github+json".to_owned(),
                host.clone(),
                "user-agent: koidrive".to_owned(),
                "x-github-api-version: 2022-11-28".to_owned(),
            ]
        );
        assert_eq!(
            answered,
            Answer::Releases {
                etag: Some("\"e1\"".into()),
                release: Some(Release {
                    version: "0.1.2".into(),
                    url: "https://github.com/koidrive/koidrive/releases/tag/v0.1.2".into(),
                }),
            }
        );

        let not_modified = "HTTP/1.1 304 Not Modified\r\ncontent-length: 0\r\n\r\n";
        let (request, answered) = tokio::join!(
            served(&listener, not_modified),
            ask(&http, url.clone(), Some("\"e1\""), ASSET)
        );
        assert_eq!(answered, Answer::NotModified);
        assert_eq!(
            header_lines(&request),
            [
                "accept: application/vnd.github+json".to_owned(),
                host,
                "if-none-match: \"e1\"".to_owned(),
                "user-agent: koidrive".to_owned(),
                "x-github-api-version: 2022-11-28".to_owned(),
            ],
            "the cookie of the first answer is not sent back"
        );

        let elsewhere =
            "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:1/\r\ncontent-length: 0\r\n\r\n";
        let (_, answered) =
            tokio::join!(served(&listener, elsewhere), ask(&http, url, None, ASSET));
        assert_eq!(
            answered,
            Answer::Refused { until: None },
            "a redirect is not followed"
        );

        drop(listener);
        let (closed, _) = releases_url(Some(&base)).unwrap();
        assert_eq!(ask(&http, closed, None, ASSET).await, Answer::Unreachable);
    }

    #[tokio::test]
    async fn a_check_switched_off_removes_the_last_answer_and_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        let answered = Checked {
            checked_at: Some(1),
            release: release("9.0.0"),
            ..Checked::default()
        };
        save(&path, &answered).unwrap();
        assert_eq!(load(&path), Some(answered));

        let off = Arc::new(Updates::open(false, path.clone()));
        assert_eq!(off.notice(), Update::Off);
        off.start(Arc::new(watch::Sender::new(())));
        assert!(!path.exists());
        assert!(off.task.lock().unwrap().is_none());

        save(&path, &Checked::default()).unwrap();
        let on = Updates::open(true, path.clone());
        assert_eq!(on.notice(), Update::Unknown);
    }

    #[test]
    fn this_targets_tarball_is_named_like_the_release_assets() {
        let asset = this_asset();
        assert!(
            [
                "koidrive-aarch64-apple-darwin.tar.gz",
                "koidrive-x86_64-apple-darwin.tar.gz",
                "koidrive-aarch64-unknown-linux-musl.tar.gz",
                "koidrive-x86_64-unknown-linux-musl.tar.gz",
            ]
            .contains(&asset.as_str()),
            "{asset}"
        );
    }
}
