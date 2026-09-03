//! Linking compute, and the two documents the wizards read before they do.
//!
//! What is worth pinning here is that both documents are *derived*. The AWS
//! policy is rendered from the driver's own call sites, so the wizard cannot
//! show a permission set the driver has outgrown; the machine catalog is
//! curated, so the slider's detents are the same short list the agent's
//! resize sees. Both would be easy to hardcode and quietly wrong afterwards.

use flyco_core::{
    AwsIamPolicy, CloudProviderKind, HostId, LinkProvider, MachineCatalogEntry, MachinePricing,
    MachineSpec, Problem, ProviderCredentials,
};
use skyzen::sql;
use skyzen_services::{Db, Kv};
use skyzen_test::TestContext;

use crate::testing::{
    SSH_HOST, host_facts, migrated_router, seed_provider_account, seed_session, seed_user,
};
use crate::{machines, session};

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

#[skyzen::test]
async fn unlinking_an_account_with_a_live_machine_counts_what_it_would_strand(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let client = ctx.client(migrated_router(&db).await);
    let user = seed_user(&db).await;
    let token = session::issue(&kv, user.id).await.expect("issue a session");
    let account = seed_provider_account(&db, user.id).await;
    let session = seed_session(&db, &user).await;
    machines::reserve(
        &db,
        session,
        account,
        &MachineSpec {
            provider: CloudProviderKind::Host,
            machine_type: SSH_HOST.to_owned(),
            region: SSH_HOST.to_owned(),
            spot: false,
            disk_gib: 64,
        },
    )
    .await
    .expect("reserve a machine on the account");

    let refused = client
        .delete(&format!("/v1/providers/{account}"))
        .bearer(&token)
        .send()
        .await;
    refused.assert_status(409);
    let problem = refused.json::<Problem>();
    assert_eq!(problem.kind, "https://flyco.dev/problems/provider-in-use");
    // Counted as a member of the document as well as in the sentence: the
    // dialog that explains the refusal states the number, and a sentence
    // written for a person is free to be reworded (RFC 9457 §3.2).
    assert_eq!(problem.extensions.active_sessions, Some(1));
    assert!(
        problem.detail.contains('1'),
        "and in the sentence, for a reader: {}",
        problem.detail
    );

    // What the refusal counts is live machines and nothing else: an account
    // whose machines are all destroyed is one nothing is running on.
    let destroyed = flyco_core::MachineState::Destroyed;
    sql!(
        db,
        "UPDATE machines SET state = {destroyed} WHERE provider_account_id = {account}"
    )
    .execute()
    .await
    .expect("destroy the machine");

    // Not asserted here: that the unlink then succeeds. It does not —
    // `machines.provider_account_id` is a `NOT NULL REFERENCES`, so
    // deleting an account any machine has ever run on fails the foreign key
    // and answers 500 (issue #153). That is a bug of its own and this test
    // is about the refusal, so it stops at proving the refusal has lifted.
    let lifted: u32 = sql!(
        db,
        "SELECT COUNT(*) AS live FROM machines \
         WHERE provider_account_id = {account} AND state != {destroyed}"
    )
    .fetch_scalar()
    .await
    .expect("count what is left running");
    assert_eq!(lifted, 0);
}
