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

/// Linking accepts only the supported credential modes and never returns a secret.
mod linking {
    use flyco_core::{
        HarnessAccountView, HarnessCredentialInput, HarnessKind, LinkHarnessAccount, Problem,
    };
    use flyco_provider::{CodexCredential, HarnessCredential};
    use skyzen_services::{Db, Kv};
    use skyzen_test::TestContext;

    use crate::session;
    use crate::testing::{migrated_router, seed_user, test_config, test_vendors};

    fn request(credential: HarnessCredentialInput) -> LinkHarnessAccount {
        LinkHarnessAccount {
            label: "Personal".to_owned(),
            credential,
        }
    }

    #[skyzen::test]
    async fn each_supported_credential_is_sealed_and_listed_without_its_secret(
        ctx: TestContext,
        kv: Kv,
        db: Db,
    ) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");
        let client = ctx.client(router);

        for (credential, expected_harness, secret) in [
            (
                HarnessCredentialInput::ClaudeSetupToken {
                    token: "sk-ant-oat01-test".to_owned(),
                },
                HarnessKind::ClaudeCode,
                "sk-ant-oat01-test",
            ),
            (
                HarnessCredentialInput::ClaudeApiKey {
                    key: "sk-ant-api03-test".to_owned(),
                },
                HarnessKind::ClaudeCode,
                "sk-ant-api03-test",
            ),
            (
                HarnessCredentialInput::CodexApiKey {
                    key: "sk-openai-test".to_owned(),
                },
                HarnessKind::Codex,
                "sk-openai-test",
            ),
        ] {
            let response = client
                .post("/v1/harness-accounts")
                .bearer(&token)
                .json(&request(credential))
                .send()
                .await;
            response.assert_status(201);
            let view: HarnessAccountView = response.json();
            assert_eq!(view.harness, expected_harness);
            let account = view.id;

            let sealed: String = skyzen::sql!(
                db,
                "SELECT credential_enc FROM harness_accounts WHERE id = {account}"
            )
            .fetch_scalar()
            .await
            .expect("read the sealed credential");
            assert!(!sealed.contains(secret));
            let encoded = test_config()
                .token_cipher()
                .open(&sealed)
                .expect("open credential");
            assert_ne!(encoded, "");
        }

        let listed = client
            .get("/v1/harness-accounts")
            .bearer(&token)
            .send()
            .await;
        listed.assert_status(200);
        let body = serde_json::to_string(&listed.json::<Vec<HarnessAccountView>>())
            .expect("encode the account list");
        assert!(!body.contains("sk-ant"));
        assert!(!body.contains("sk-openai"));
    }

    #[skyzen::test]
    async fn linking_again_replaces_the_harness_credential(ctx: TestContext, kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");
        let client = ctx.client(router);

        for key in ["first-key", "second-key"] {
            client
                .post("/v1/harness-accounts")
                .bearer(&token)
                .json(&request(HarnessCredentialInput::CodexApiKey {
                    key: key.to_owned(),
                }))
                .send()
                .await
                .assert_status(201);
        }

        let user_id = user.id;
        let harness = HarnessKind::Codex;
        let count: u64 = skyzen::sql!(
            db,
            "SELECT COUNT(*) FROM harness_accounts WHERE user_id = {user_id} AND harness = {harness}"
        )
        .fetch_scalar()
        .await
        .expect("count harness accounts");
        assert_eq!(count, 1);

        let stored = crate::harness_accounts::credential(
            &db,
            &test_config(),
            &test_vendors(),
            user.id,
            HarnessKind::Codex,
        )
        .await
        .expect("read the replacement");
        assert_eq!(
            stored,
            HarnessCredential::Codex(CodexCredential::ApiKey {
                key: "second-key".to_owned()
            })
        );
    }

    #[skyzen::test]
    async fn empty_fields_are_rejected(ctx: TestContext, kv: Kv, db: Db) {
        let router = migrated_router(&db).await;
        let user = seed_user(&db).await;
        let token = session::issue(&kv, user.id).await.expect("issue a session");
        let client = ctx.client(router);

        for invalid in [
            LinkHarnessAccount {
                label: " ".to_owned(),
                credential: HarnessCredentialInput::CodexApiKey {
                    key: "present".to_owned(),
                },
            },
            LinkHarnessAccount {
                label: "Personal".to_owned(),
                credential: HarnessCredentialInput::ClaudeSetupToken {
                    token: " ".to_owned(),
                },
            },
        ] {
            let response = client
                .post("/v1/harness-accounts")
                .bearer(&token)
                .json(&invalid)
                .send()
                .await;
            response.assert_status(422);
            assert!(
                response
                    .json::<Problem>()
                    .kind
                    .ends_with("invalid-harness-credential")
            );
        }
    }
}
