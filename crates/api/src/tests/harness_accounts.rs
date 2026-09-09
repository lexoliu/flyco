//! Harness accounts are addressed by account id, like every other resource.
//!
//! They were addressed by harness *kind* once, which quietly made "one
//! account per harness" a property of the API rather than of the table: the
//! list response has always carried a per-account id, and there was no way
//! to name the second account with it.

use flyco_core::{
    HarnessAccountView, HarnessKind, ModelOption, Problem, SessionState, builtin_models,
};
use skyzen_services::{Db, Kv};
use skyzen_test::TestContext;

use crate::testing::{
    migrated_router, seed_harness_account, seed_other_user, seed_session, seed_user,
};
use crate::{harness_accounts, session, sessions};

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
async fn an_account_a_session_runs_on_is_not_unlinked_out_from_under_it(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let client = ctx.client(router);

    let claude = seed_harness_account(&db, user.id, HarnessKind::ClaudeCode).await;
    let codex = seed_harness_account(&db, user.id, HarnessKind::Codex).await;
    // Seeded sessions run Claude Code, so this one is on `claude` and on
    // nothing else.
    let running = seed_session(&db, &user).await;

    let refused = client
        .delete(&format!("/v1/harness-accounts/{claude}"))
        .bearer(&token)
        .send()
        .await;
    refused.assert_status(409);
    let problem = refused.json::<Problem>();
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/harness-account-in-use"
    );
    // The number the confirmation states is a member of the document, not a
    // word in a sentence the browser would have to parse (RFC 9457 §3.2).
    assert_eq!(problem.extensions.active_sessions, Some(1));

    // The refusal is about the harness that is busy, not about the user:
    // nothing runs on Codex, so that account unlinks.
    client
        .delete(&format!("/v1/harness-accounts/{codex}"))
        .bearer(&token)
        .send()
        .await
        .assert_status(204);

    // And it lifts the moment the session lets go of the account. Archived
    // is the only state that does: an interrupted or failed session is one
    // the user can still resume onto this harness.
    for state in [SessionState::Active, SessionState::Interrupted] {
        sessions::transition(&db, user.id, running, state)
            .await
            .expect("move the session");
        client
            .delete(&format!("/v1/harness-accounts/{claude}"))
            .bearer(&token)
            .send()
            .await
            .assert_status(409);
    }

    sessions::transition(&db, user.id, running, SessionState::Archived)
        .await
        .expect("archive the session");
    client
        .delete(&format!("/v1/harness-accounts/{claude}"))
        .bearer(&token)
        .send()
        .await
        .assert_status(204);
}

#[skyzen::test]
async fn somebody_elses_sessions_do_not_hold_this_account_open(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let account = seed_harness_account(&db, user.id, HarnessKind::ClaudeCode).await;

    // A stranger running Claude Code is not a reason this user cannot
    // unlink their own credential: the count is scoped to the owner, like
    // every other read of the sessions table.
    let stranger = seed_other_user(&db).await;
    seed_session(&db, &stranger).await;

    ctx.client(router)
        .delete(&format!("/v1/harness-accounts/{account}"))
        .bearer(&token)
        .send()
        .await
        .assert_status(204);
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

// ── The model list a linked account offers ──

#[skyzen::test]
async fn an_account_that_has_never_run_a_session_offers_the_built_in_list(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    // The picker has to show something the first time an account is linked,
    // and the built-in list is the honest answer: nothing has told flyco
    // what this account's harness build accepts.
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let client = ctx.client(router);
    seed_harness_account(&db, user.id, HarnessKind::ClaudeCode).await;

    let listed = client
        .get("/v1/harness-accounts")
        .bearer(&token)
        .send()
        .await;
    listed.assert_status(200);
    let accounts: Vec<HarnessAccountView> = listed.json();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].models, builtin_models(HarnessKind::ClaudeCode));
}

#[skyzen::test]
async fn a_reported_list_replaces_the_built_in_one_for_that_account(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let client = ctx.client(router);
    seed_harness_account(&db, user.id, HarnessKind::ClaudeCode).await;
    seed_harness_account(&db, user.id, HarnessKind::Codex).await;

    let reported = vec![ModelOption {
        id: "claude-opus-6".to_owned(),
        label: "Opus 6".to_owned(),
        description: "A model this build of the CLI knows about and flyco did not.".to_owned(),
        is_default: true,
        efforts: vec!["low".to_owned(), "max".to_owned()],
        default_effort: None,
    }];
    harness_accounts::record_models(&db, user.id, HarnessKind::ClaudeCode, &reported)
        .await
        .expect("record what the harness offers");

    let listed = client
        .get("/v1/harness-accounts")
        .bearer(&token)
        .send()
        .await;
    listed.assert_status(200);
    let accounts: Vec<HarnessAccountView> = listed.json();
    let claude = accounts
        .iter()
        .find(|account| account.harness == HarnessKind::ClaudeCode)
        .expect("the Claude account");
    assert_eq!(claude.models, reported);

    // Recorded against one account and not the other: the list is a fact
    // about the harness build that account's machines run.
    let codex = accounts
        .iter()
        .find(|account| account.harness == HarnessKind::Codex)
        .expect("the Codex account");
    assert_eq!(codex.models, builtin_models(HarnessKind::Codex));
}

#[skyzen::test]
async fn a_harness_that_lists_nothing_is_refused_rather_than_stored(db: Db) {
    // An empty list is a daemon bug, and storing it would leave the account
    // with a picker that can never be opened and no way back.
    crate::testing::migrate(&db).await;
    let user = seed_user(&db).await;
    seed_harness_account(&db, user.id, HarnessKind::Codex).await;

    let refusal = harness_accounts::record_models(&db, user.id, HarnessKind::Codex, &[])
        .await
        .expect_err("an empty model list is refused");
    assert_eq!(refusal.slug(), "internal");
    let _ = &refusal;
    assert_eq!(
        harness_accounts::models(&db, user.id, HarnessKind::Codex)
            .await
            .expect("read the list back"),
        builtin_models(HarnessKind::Codex),
        "the refusal leaves the stored list alone"
    );
}

#[skyzen::test]
async fn a_report_for_a_harness_the_user_has_no_account_for_is_refused(db: Db) {
    // A write that matched nothing would leave the picker quietly on the
    // built-in list and report success, so it is a refusal instead.
    crate::testing::migrate(&db).await;
    let user = seed_user(&db).await;

    let refusal = harness_accounts::record_models(
        &db,
        user.id,
        HarnessKind::Codex,
        &builtin_models(HarnessKind::Codex),
    )
    .await
    .expect_err("there is no Codex account to record against");
    assert_eq!(refusal.slug(), "harness-account-not-found");
}
