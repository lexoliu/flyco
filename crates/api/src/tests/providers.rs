//! Linking compute, and the two documents the wizards read before they do.
//!
//! What is worth pinning here is that both documents are *derived*. The AWS
//! policy is rendered from the driver's own call sites, so the wizard cannot
//! show a permission set the driver has outgrown; the machine catalog is
//! curated, so the slider's detents are the same short list the agent's
//! resize sees. Both would be easy to hardcode and quietly wrong afterwards.

use flyco_core::{
    AwsIamPolicy, HostId, LinkProvider, MachineCatalogEntry, MachinePricing, Problem,
    ProviderCredentials,
};
use skyzen_services::{Db, Kv};
use skyzen_test::TestContext;

use crate::session;
use crate::testing::{SSH_HOST, host_facts, migrated_router, seed_provider_account, seed_user};

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
    seed_provider_account(&db, user.id).await;

    let response = client
        .get("/v1/machines/catalog")
        .bearer(&token)
        .send()
        .await;
    response.assert_status(200);
    let catalog: Vec<MachineCatalogEntry> = response.json();

    // No price to rank it by, and it survives anyway: curation removes
    // machines that are known to be worse, and a machine flyco does not
    // meter is not a known-worse. Its size and its architecture are the
    // machine's own report rather than anything flyco invented.
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0].machine_type, SSH_HOST);
    assert_eq!(catalog[0].pricing, MachinePricing::UserOwned);
    assert_eq!(catalog[0].capacity, Some(host_facts().capacity()));
    assert_eq!(catalog[0].lineage, Some(host_facts().lineage()));
}

#[skyzen::test]
async fn a_machine_you_own_cannot_be_linked_as_a_credential(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");

    // Naming a host id here would create an account pointing at a machine
    // nobody enrolled, which could never answer. A host is linked by
    // enrolling it, and by nothing else.
    let response = client
        .post("/v1/providers")
        .bearer(&token)
        .json(&LinkProvider {
            label: "the build host".to_owned(),
            credentials: ProviderCredentials::Host {
                host: HostId::generate(),
            },
        })
        .send()
        .await;

    response.assert_status(422);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/host-not-linkable"
    );
}
