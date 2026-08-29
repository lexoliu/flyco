//! Harness accounts are addressed by account id, like every other resource.
//!
//! They were addressed by harness *kind* once, which quietly made "one
//! account per harness" a property of the API rather than of the table: the
//! list response has always carried a per-account id, and there was no way
//! to name the second account with it.

use flyco_core::{HarnessAccountView, HarnessKind, Problem};
use skyzen_services::{Db, Kv};
use skyzen_test::TestContext;

use crate::session;
use crate::testing::{migrated_router, seed_harness_account, seed_other_user, seed_user};

#[skyzen::test]
async fn each_linked_account_is_unlinked_by_its_own_id(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let client = ctx.client(router);

    let claude = seed_harness_account(&db, user.id, HarnessKind::ClaudeCode).await;
    let codex = seed_harness_account(&db, user.id, HarnessKind::Codex).await;

    let listed = client
        .get("/v1/harness-accounts")
        .bearer(&token)
        .send()
        .await;
    listed.assert_status(200);
    assert_eq!(listed.json::<Vec<HarnessAccountView>>().len(), 2);

    client
        .delete(&format!("/v1/harness-accounts/{claude}"))
        .bearer(&token)
        .send()
        .await
        .assert_status(204);

    let left = client
        .get("/v1/harness-accounts")
        .bearer(&token)
        .send()
        .await;
    let left: Vec<HarnessAccountView> = left.json();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, codex, "only the account that was named is gone");

    let again = client
        .delete(&format!("/v1/harness-accounts/{claude}"))
        .bearer(&token)
        .send()
        .await;
    again.assert_status(404);
    assert_eq!(
        again.json::<Problem>().kind,
        "https://flyco.dev/problems/harness-account-not-found"
    );
}

#[skyzen::test]
async fn a_stranger_cannot_unlink_somebody_elses_account(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let owner = seed_user(&db).await;
    let account = seed_harness_account(&db, owner.id, HarnessKind::ClaudeCode).await;

    let stranger = seed_other_user(&db).await;
    let stranger = session::issue(&kv, stranger.id)
        .await
        .expect("issue a session");

    ctx.client(router)
        .delete(&format!("/v1/harness-accounts/{account}"))
        .bearer(&stranger)
        .send()
        .await
        .assert_status(404);
}
