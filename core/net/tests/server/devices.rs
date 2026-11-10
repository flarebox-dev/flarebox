// SPDX-License-Identifier: Apache-2.0

//! The device routes against the real Worker (RFC 0006 section 7, RFC 0009).

use flarebox_net::Auth;
use flarebox_proto::v1::{DeviceInfo, TokenRequest, error::Code};

use crate::support::*;

async fn devices(auth: &Auth) -> Vec<DeviceInfo> {
    let mut devices = Vec::new();
    let mut cursor = String::new();
    loop {
        let page = auth.api().list_devices(auth, &cursor).await.unwrap();
        devices.extend(page.devices);
        if page.next_cursor.is_empty() {
            return devices;
        }
        cursor = page.next_cursor;
    }
}

#[tokio::test]
#[ignore = "needs a local Worker; run core/net/scripts/integration-test.sh"]
async fn the_owner_revokes_a_lost_device_from_another_but_never_its_own() {
    let owner = auth(owner().await);
    let lost = new_device("lost laptop").await;
    let thief = auth(lost.clone());
    thief.api().list_spaces(&thief).await.unwrap();

    let listed = devices(&owner).await;
    let current: Vec<&str> = listed
        .iter()
        .filter(|device| device.current)
        .map(|device| device.device_id.as_str())
        .collect();
    assert_eq!(current, [owner.credentials().device_id.as_str()]);
    let entry = listed
        .iter()
        .find(|device| device.device_id == lost.device_id)
        .expect("the new device is listed");
    assert_eq!(
        (entry.name.as_str(), entry.revoked_at_ms),
        ("lost laptop", 0)
    );

    owner
        .api()
        .revoke_device(&owner, &lost.device_id)
        .await
        .unwrap();
    owner
        .api()
        .revoke_device(&owner, &lost.device_id)
        .await
        .expect("revoking a revoked device succeeds");

    let refused = thief.api().list_spaces(&thief).await.unwrap_err();
    assert!(refused.is_unauthenticated(), "{refused:?}");
    let refresh = api()
        .token(&TokenRequest {
            device_id: lost.device_id.clone(),
            refresh_credential: lost.refresh_credential.clone(),
        })
        .await
        .unwrap_err();
    assert!(refresh.is_unauthenticated(), "{refresh:?}");
    let revoked = devices(&owner)
        .await
        .into_iter()
        .find(|device| device.device_id == lost.device_id)
        .unwrap();
    assert!(revoked.revoked_at_ms > 0);

    let own = owner
        .api()
        .revoke_device(&owner, &owner.credentials().device_id)
        .await
        .unwrap_err();
    assert_eq!(
        own.server().map(|error| error.code()),
        Some(Code::Forbidden),
        "{own:?}"
    );
    owner.api().list_spaces(&owner).await.unwrap();
}
