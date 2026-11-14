// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)]

pub mod proxy;

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use koidrive_net::{Api, Auth, Credentials, Event, Events, Session, SessionConfig};
use koidrive_proto::v1::{
    ClaimRequest, Commit, CreateSpaceRequest, DeviceCredentials, Op, PairCompleteRequest, SpaceInfo,
};
use tokio::sync::OnceCell;
use url::Url;

pub const WAIT: Duration = Duration::from_secs(15);

pub fn server_url() -> Url {
    let url = std::env::var("KOIDRIVE_TEST_SERVER")
        .expect("KOIDRIVE_TEST_SERVER is not set; run core/net/scripts/integration-test.sh, which starts the Worker");
    url.parse().expect("KOIDRIVE_TEST_SERVER is a URL")
}

pub fn api() -> Api {
    Api::local_development(server_url()).unwrap()
}

static OWNER: OnceCell<DeviceCredentials> = OnceCell::const_new();

pub async fn owner() -> Credentials {
    let owner = OWNER.get_or_init(claim).await;
    Credentials::from(owner)
}

async fn claim() -> DeviceCredentials {
    let log =
        std::env::var("KOIDRIVE_TEST_SERVER_LOG").expect("KOIDRIVE_TEST_SERVER_LOG is not set");
    let setup = reqwest::get(server_url().join("setup").unwrap())
        .await
        .unwrap();
    assert!(
        setup.status().is_success(),
        "GET /setup answered {}",
        setup.status()
    );
    let started = Instant::now();
    let code = loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if let Some(code) = text.lines().rev().find_map(claim_code) {
            break code;
        }
        assert!(started.elapsed() < WAIT, "no claim code in {log}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let request = ClaimRequest {
        code,
        user_name: "Owner".into(),
        device_name: "owner".into(),
        platform: "test".into(),
        setup_attempt: String::new(),
        request_id: String::new(),
    };
    api().claim(&request).await.expect("the claim succeeds")
}

fn claim_code(line: &str) -> Option<String> {
    if !line.to_ascii_lowercase().contains("claim code") {
        return None;
    }
    line.split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        .rfind(|word| {
            word.contains('-')
                && word.len() >= 8
                && word.split('-').all(|group| {
                    !group.is_empty() && group.chars().all(|c| c.is_ascii_alphanumeric())
                })
        })
        .map(str::to_owned)
}

pub fn auth(credentials: Credentials) -> Arc<Auth> {
    Arc::new(Auth::new(api(), credentials))
}

pub async fn new_device(name: &str) -> Credentials {
    let owner = auth(owner().await);
    let started = owner.api().pair_start(&owner).await.unwrap();
    let request = PairCompleteRequest {
        code: started.code,
        device_name: name.into(),
        platform: "test".into(),
        request_id: String::new(),
    };
    Credentials::from(&api().pair_complete(&request).await.unwrap())
}

pub async fn new_space() -> SpaceInfo {
    let owner = auth(owner().await);
    owner
        .api()
        .create_space(
            &owner,
            &CreateSpaceRequest {
                name: format!("test {}", uuidv7()),
                request_id: String::new(),
            },
        )
        .await
        .unwrap()
}

pub fn config(space: &SpaceInfo) -> SessionConfig {
    let mut config = SessionConfig::new(
        &space.space_id,
        "koidrive-net tests",
        env!("CARGO_PKG_VERSION"),
        1,
    );
    config.backoff = koidrive_net::Backoff {
        initial: Duration::from_millis(50),
        max: Duration::from_millis(500),
    };
    config
}

pub fn start(auth: Arc<Auth>, space: &SpaceInfo) -> (Session, Events) {
    Session::start(auth, config(space)).unwrap()
}

pub async fn until<T>(events: &mut Events, mut pick: impl FnMut(&Event) -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let event = tokio::time::timeout_at(deadline, events.next())
            .await
            .expect("the expected event did not come in time")
            .expect("the session ended");
        if let Some(value) = pick(&event) {
            return value;
        }
    }
}

pub async fn connected(events: &mut Events) -> koidrive_proto::v1::Welcome {
    until(events, |event| match event {
        Event::Connected(welcome) => Some(welcome.clone()),
        _ => None,
    })
    .await
}

pub fn uuidv7() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let random = fastrand::u128(..);
    let bits = (ms << 80) | (random & ((1 << 80) - 1));
    let bits = (bits & !(0xf << 76)) | (0x7 << 76);
    let bits = (bits & !(0x3 << 62)) | (0x2 << 62);
    let hex = format!("{bits:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

pub fn commit(history_id: &str, base_seq: u64, device_seq: u64, ops: Vec<Op>) -> Commit {
    Commit {
        commit_id: uuidv7(),
        base_seq,
        ops,
        history_id: history_id.into(),
        device_seq,
        ..Default::default()
    }
}
