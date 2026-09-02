//! Linking compute, and the two documents the wizards read before they do.
//!
//! What is worth pinning here is that both documents are *derived*. The AWS
//! policy is rendered from the driver's own call sites, so the wizard cannot
//! show a permission set the driver has outgrown; the machine catalog is
//! curated, so the slider's detents are the same short list the agent's
//! resize sees. Both would be easy to hardcode and quietly wrong afterwards.

use flyco_core::{
    AwsIamPolicy, LinkProvider, MachineCatalogEntry, MachinePricing, ProviderCredentials,
};
use skyzen::routing::Router;
use skyzen_services::{Db, Kv};
use skyzen_test::{TestClient, TestContext};

use crate::session;
use crate::testing::{migrated_router, seed_user};

/// Registers the SSH host flyco develops against — a real machine the user
/// owns, which is the one catalog entry that carries neither a price nor a
/// size.
async fn link_host(client: &TestClient<Router>, token: &str) {
    client
        .post("/v1/providers")
        .bearer(token)
        .json(&LinkProvider {
            label: "the build host".to_owned(),
            credentials: ProviderCredentials::ByoSsh {
                host: "build.lexo.cool".to_owned(),
                port: 22,
                user: "flyco".to_owned(),
                private_key: "-----BEGIN OPENSSH PRIVATE KEY-----".to_owned(),
                host_fingerprint: "SHA256:qWyVLPxNBRr7Nnkm1xTQKMDcXwHFsSFRnLW6iNfPmcQ".to_owned(),
            },
        })
        .send()
        .await
        .assert_status(201);
}

#[skyzen::test]
async fn the_iam_policy_is_not_public(ctx: TestContext, db: Db) {
    ctx.client(migrated_router(&db).await)
        .get("/v1/providers/aws/iam-policy")
        .send()
        .await
        .assert_status(401);
}

#[skyzen::test]
async fn the_iam_policy_grants_exactly_what_the_driver_calls(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");

    let response = client
        .get("/v1/providers/aws/iam-policy")
        .bearer(&token)
        .send()
        .await;
    response.assert_status(200);
    let policy: AwsIamPolicy = response.json();

    // The actions are the driver's, not a list maintained beside it.
    assert_eq!(policy.actions, flyco_provider::aws::iam::actions());

    // And the document is real IAM JSON carrying exactly those actions,
    // rather than a string that merely looks like one.
    let document: serde_json::Value =
        serde_json::from_str(&policy.document).expect("the policy is valid JSON");
    assert_eq!(document["Version"], "2012-10-17");
    let granted = document["Statement"][0]["Action"]
        .as_array()
        .expect("a statement lists its actions")
        .iter()
        .map(|action| action.as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(granted, policy.actions);
    assert!(granted.contains(&"ec2:RunInstances".to_owned()));
    assert!(granted.contains(&"sts:GetCallerIdentity".to_owned()));
}

#[skyzen::test]
async fn curation_never_drops_the_machine_the_user_already_owns(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    link_host(&client, &token).await;

    let response = client
        .get("/v1/machines/catalog")
        .bearer(&token)
        .send()
        .await;
    response.assert_status(200);
    let catalog: Vec<MachineCatalogEntry> = response.json();

    // Neither a price nor a size to rank it by, and it survives anyway:
    // curation removes machines that are known to be worse, and an unknown
    // is not a known-worse.
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].machine_type, "build.lexo.cool");
    assert_eq!(catalog[0].pricing, MachinePricing::UserOwned);
    assert!(catalog[0].capacity.is_none());
    assert!(catalog[0].lineage.is_none());
}
