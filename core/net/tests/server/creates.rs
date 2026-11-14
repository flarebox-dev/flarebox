// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use koidrive_net::{Api, Auth, request_id, resend_lost};
use koidrive_proto::v1::{CreateShareLinkRequest, CreateSpaceRequest, PairCompleteRequest};

use crate::support::proxy::{Fault, Proxy};
use crate::support::*;

async fn owner_through(proxy: &Proxy) -> Arc<Auth> {
    let owner = Arc::new(Auth::new(
        Api::local_development(proxy.url.clone()).unwrap(),
        owner().await,
    ));
    owner.api().list_spaces(&owner).await.unwrap();
    owner
}

#[tokio::test]
#[ignore = "needs a local Worker; run core/net/scripts/integration-test.sh"]
async fn a_space_create_whose_answer_was_lost_makes_one_space() {
    let proxy = Proxy::start(&server_url()).await;
    let owner = owner_through(&proxy).await;
    let name = format!("Photos {}", uuidv7());
    let request = CreateSpaceRequest {
        name: name.clone(),
        request_id: request_id(),
    };
    proxy.arm(Fault::DropOnServerData);
    let created = resend_lost(|| owner.api().create_space(&owner, &request))
        .await
        .unwrap();
    assert!(proxy.fired(), "the first answer was dropped");
    let named: Vec<String> = (owner.api().list_spaces(&owner).await.unwrap().spaces)
        .into_iter()
        .filter(|space| space.name == name)
        .map(|space| space.space_id)
        .collect();
    assert_eq!(named, [created.space_id]);
}

#[tokio::test]
#[ignore = "needs a local Worker; run core/net/scripts/integration-test.sh"]
async fn a_share_link_create_whose_answer_was_lost_makes_one_link() {
    let space = new_space().await;
    let proxy = Proxy::start(&server_url()).await;
    let owner = owner_through(&proxy).await;
    let request = CreateShareLinkRequest {
        node_id: space.space_id.clone(),
        request_id: request_id(),
        ..Default::default()
    };
    proxy.arm(Fault::DropOnServerData);
    let link = resend_lost(|| {
        owner
            .api()
            .create_share_link(&owner, &space.space_id, &request)
    })
    .await
    .unwrap();
    assert!(proxy.fired(), "the first answer was dropped");
    let links = (owner.api())
        .list_share_links(&owner, &space.space_id, "")
        .await
        .unwrap()
        .links;
    let ids: Vec<String> = links.into_iter().map(|link| link.link_id).collect();
    assert_eq!(ids, [link.link_id]);
}

#[tokio::test]
#[ignore = "needs a local Worker; run core/net/scripts/integration-test.sh"]
async fn a_pairing_whose_answer_was_lost_makes_one_working_device() {
    let owner = auth(owner().await);
    let started = owner.api().pair_start(&owner).await.unwrap();
    let proxy = Proxy::start(&server_url()).await;
    let through_proxy = Api::local_development(proxy.url.clone()).unwrap();
    let name = format!("phone {}", uuidv7());
    let request = PairCompleteRequest {
        code: started.code,
        device_name: name.clone(),
        platform: "test".into(),
        request_id: request_id(),
    };
    proxy.arm(Fault::DropOnServerData);
    let paired = resend_lost(|| through_proxy.pair_complete(&request))
        .await
        .unwrap();
    assert!(proxy.fired(), "the first answer was dropped");
    let device = auth(koidrive_net::Credentials::from(&paired));
    device.api().list_spaces(&device).await.unwrap();
    let mut named = Vec::new();
    let mut cursor = String::new();
    loop {
        let page = owner.api().list_devices(&owner, &cursor).await.unwrap();
        named.extend(
            (page.devices.into_iter())
                .filter(|listed| listed.name == name)
                .map(|listed| listed.device_id),
        );
        if page.next_cursor.is_empty() {
            break;
        }
        cursor = page.next_cursor;
    }
    assert_eq!(named, [paired.device_id]);
}
