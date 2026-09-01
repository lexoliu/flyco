//! Coverage of the web-push routes.
//!
//! The behaviour worth pinning is the upsert: a browser re-subscribes on
//! every service-worker update, and a second row for the same endpoint means
//! every notification arrives twice.

use flyco_core::{PushKeys, PushSubscription, PushSubscriptionView};
use skyzen_services::{Db, Kv};
use skyzen_test::TestContext;

use crate::session;
use crate::testing::{migrated_router, seed_other_user, seed_user};

const ENDPOINT: &str = "https://web.push.apple.com/QQAA-flyco-test-endpoint";

fn subscription() -> PushSubscription {
    PushSubscription {
        endpoint: ENDPOINT.to_owned(),
        expiration_time: None,
        keys: PushKeys {
            p256dh: "BH1HTeKM7-NwaLGHEqxeu2IamQaVVLkcsFHPIHmsCnqxcBHPQBprF41bEMOr3O1hUQ2jU1opNEm1F_lZV_sxMP8".to_owned(),
            auth: "sBXU5_tIYz-5w7G2B25BEw".to_owned(),
        },
    }
}

#[skyzen::test]
async fn re_subscribing_the_same_browser_keeps_one_row(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let client = ctx.client(router);

    let first = client
        .post("/v1/push/subscriptions")
        .bearer(&token)
        .json(&subscription())
        .send()
        .await;
    first.assert_status(201);
    let first: PushSubscriptionView = first.json();

    // The service worker updated and the browser subscribed again.
    let second = client
        .post("/v1/push/subscriptions")
        .bearer(&token)
        .json(&subscription())
        .send()
        .await;
    second.assert_status(201);
    let second: PushSubscriptionView = second.json();

    assert_eq!(
        first.id, second.id,
        "the same endpoint must land on the row it already has"
    );
}

#[skyzen::test]
async fn a_subscription_belongs_to_the_browser_that_registered_it(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let owner = seed_user(&db).await;
    let stranger = seed_other_user(&db).await;
    let owner_token = session::issue(&kv, owner.id)
        .await
        .expect("issue a session");
    let stranger_token = session::issue(&kv, stranger.id)
        .await
        .expect("issue a session");
    let client = ctx.client(router);

    let registered: PushSubscriptionView = client
        .post("/v1/push/subscriptions")
        .bearer(&owner_token)
        .json(&subscription())
        .send()
        .await
        .json();

    let path = format!("/v1/push/subscriptions/{}", registered.id);
    client
        .delete(&path)
        .bearer(&stranger_token)
        .send()
        .await
        .assert_status(404);

    client
        .delete(&path)
        .bearer(&owner_token)
        .send()
        .await
        .assert_status(204);
}

#[skyzen::test]
async fn push_always_reports_the_public_half_of_its_required_identity(ctx: TestContext, db: Db) {
    let response = ctx
        .client(migrated_router(&db).await)
        .get("/v1/push/vapid-public-key")
        .send()
        .await;

    response.assert_status(200);
}
