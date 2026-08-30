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

#[skyzen::test]
async fn the_feature_matrix_is_the_verified_table(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");

    let response = ctx
        .client(router)
        .get("/v1/harness-features")
        .bearer(&token)
        .send()
        .await;
    response.assert_status(200);
    let rows: Vec<flyco_core::HarnessFeature> = response.json();
    assert_eq!(rows, flyco_core::matrix());
}

/// Linking is deployment configuration, and its absence must say so.
mod linking {
    use flyco_core::{AuthorizeUrl, HarnessKind, Problem};
    use skyzen_services::{Db, Kv};
    use skyzen_test::TestContext;
    use url::Url;

    use crate::config::HarnessOauthClient;
    use crate::session;
    use crate::testing::{migrated_router_with_config, seed_user, test_config};

    fn client() -> HarnessOauthClient {
        HarnessOauthClient {
            authorize_url: Url::parse("https://auth.example.invalid/authorize")
                .expect("a valid URL"),
            token_url: Url::parse("https://auth.example.invalid/token").expect("a valid URL"),
            client_id: "flyco-test-client".to_owned(),
            client_secret: "flyco-test-secret".to_owned(),
            scope: "inference".to_owned(),
        }
    }

    #[skyzen::test]
    async fn an_unconfigured_deployment_refuses_rather_than_guessing(
        ctx: TestContext,
        kv: Kv,
        db: Db,
    ) {
        let router = migrated_router_with_config(&db, test_config()).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");

        let response = ctx
            .client(router)
            .post("/v1/harness-accounts/claude_code/link/start")
            .bearer(&token)
            .send()
            .await;

        response.assert_status(501);
        assert!(
            response
                .json::<Problem>()
                .kind
                .ends_with("harness-link-unconfigured"),
            "the refusal must name the missing configuration"
        );
    }

    #[skyzen::test]
    async fn a_configured_deployment_sends_the_browser_to_the_vendor(
        ctx: TestContext,
        kv: Kv,
        db: Db,
    ) {
        let config = test_config().with_harness_oauth(HarnessKind::ClaudeCode, client());
        let router = migrated_router_with_config(&db, config).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");

        let response = ctx
            .client(router)
            .post("/v1/harness-accounts/claude_code/link/start")
            .bearer(&token)
            .send()
            .await;
        response.assert_status(200);

        let url = Url::parse(&response.json::<AuthorizeUrl>().authorize_url).expect("a valid URL");
        assert_eq!(url.host_str(), Some("auth.example.invalid"));

        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query.get("response_type").map(AsRef::as_ref), Some("code"));
        assert_eq!(
            query.get("client_id").map(AsRef::as_ref),
            Some("flyco-test-client")
        );
        // PKCE, so an intercepted code is useless without the verifier that
        // never left this control plane.
        assert_eq!(
            query.get("code_challenge_method").map(AsRef::as_ref),
            Some("S256")
        );
        assert!(query.contains_key("code_challenge"));
        assert!(query.contains_key("state"));
        assert!(
            !url.as_str().contains("flyco-test-secret"),
            "the client secret must never reach the browser"
        );
    }

    #[skyzen::test]
    async fn a_callback_carrying_an_unknown_state_is_refused(ctx: TestContext, _kv: Kv, db: Db) {
        let config = test_config().with_harness_oauth(HarnessKind::ClaudeCode, client());
        let router = migrated_router_with_config(&db, config).await;

        ctx.client(router)
            .get("/v1/harness-accounts/claude_code/link/callback?code=abc&state=never-minted")
            .send()
            .await
            .assert_status(400);
    }
}
