//! The catalog cache, end to end through the router and the queue.
//!
//! What is worth pinning here is the difference this whole mechanism exists
//! to make: "no machines" and "no machines *yet*" are different answers, and
//! the request path never computes a cloud catalog. What a provider's
//! catalog *contains* is pinned where it can be — against recorded
//! exchanges, in `flyco_provider::azure::tests` — so nothing here reaches a
//! cloud.

use flyco_core::{
    CloudProviderKind, MachineCapacity, MachineCatalog, MachineCatalogEntry, MachineDefault,
    MachinePricing, OsFamily, Problem, ProviderAccountId, Runtime, StoragePricing, Usd, UserId,
};
use skyzen::routing::Router;
use skyzen_services::queue::{QueueBatch, QueueMessage, ReceiveOptions};
use skyzen_services::{Db, Kv, Queue};
use skyzen_test::mock::InMemoryQueue;
use skyzen_test::{TestClient, TestContext};

use crate::catalog::{self, RegionCatalog, RegionOutcome};
use crate::provisioning::CloudProvisioner;
use crate::provisioning_queue::{self, ProvisioningJob};
use crate::session;
use crate::testing::{
    TestGithub, migrated_router_on, seed_azure_account, seed_provider_account, seed_user,
    test_config, test_host_rooms, test_rooms, test_vendors,
};

const CATALOG: &str = "/v1/machines/catalog";
const DEFAULT: &str = "/v1/machines/default";
const REGION: &str = "eastus";
const MACHINE_TYPE: &str = "Standard_D4als_v6";

/// A signed-in caller and the router they call, producing to `queue`.
struct Caller {
    client: TestClient<Router>,
    token: String,
    user: UserId,
}

async fn signed_in(ctx: &TestContext, kv: &Kv, db: &Db, queue: Queue) -> Caller {
    let router = migrated_router_on(db, queue).await;
    let user = seed_user(db).await;
    let token = session::issue(kv, user.id).await.expect("issue a session");
    Caller {
        client: ctx.client(router),
        token,
        user: user.id,
    }
}

/// One machine big enough for flyco to pick on its own.
fn entry(account: ProviderAccountId) -> MachineCatalogEntry {
    MachineCatalogEntry {
        account: Some(account),
        provider: CloudProviderKind::Azure,
        region: REGION.to_owned(),
        machine_type: MACHINE_TYPE.to_owned(),
        runtime: Runtime::Vm,
        free_grant: None,
        os: OsFamily::Linux,
        capacity: Some(MachineCapacity {
            vcpus: 4,
            memory_mib: 16 * 1024,
        }),
        lineage: None,
        pricing: MachinePricing::Metered {
            on_demand_hourly: Usd::from_micros(160_000),
            spot_hourly: Some(Usd::from_micros(30_000)),
            minimum: None,
            storage: StoragePricing::CapacityTiers { tiers: Vec::new() },
        },
    }
}

/// Puts one region into the cache, exactly as the consumer does.
async fn cache_region(kv: &Kv, account: ProviderAccountId, outcome: RegionOutcome) {
    catalog::record_region(
        kv,
        account,
        RegionCatalog {
            region: REGION.to_owned(),
            read_at_unix: crate::clock::now_unix(),
            outcome,
        },
    )
    .await
    .expect("record a region");
}

/// Every job the queue holds.
fn queued(backend: &InMemoryQueue) -> Vec<ProvisioningJob> {
    backend
        .messages()
        .iter()
        .map(|body| serde_json::from_slice(body).expect("a queued provisioning job"))
        .collect()
}

/// Takes everything the queue holds and hands it to the real consumer.
async fn run_queue(db: &Db, kv: &Kv, queue: &Queue) {
    let taken = queue
        .receive_json::<ProvisioningJob>(ReceiveOptions::new().with_max_messages(16))
        .await
        .expect("read the provisioning queue");
    let mut messages = Vec::with_capacity(taken.len());
    for message in taken {
        queue.ack(&message.receipt).await.expect("settle a message");
        messages.push(QueueMessage {
            id: message.id.unwrap_or_default(),
            timestamp_ms: 0,
            body: message.body,
        });
    }

    let github = TestGithub::default();
    let vendors = test_vendors();
    let mut provisioner = CloudProvisioner::new(test_host_rooms());
    provisioning_queue::consume(
        db,
        &test_config(),
        kv,
        queue,
        &test_rooms(),
        &mut provisioning_queue::Clients {
            provisioner: &mut provisioner,
            vendors: &vendors,
            github: &github,
        },
        QueueBatch {
            queue: "provisioning".to_owned(),
            messages,
        },
    )
    .await;
}

async fn read_catalog(caller: &Caller) -> MachineCatalog {
    let response = caller
        .client
        .get(CATALOG)
        .bearer(&caller.token)
        .send()
        .await;
    response.assert_status(200);
    response.json()
}

#[skyzen::test]
async fn an_unread_account_answers_pending_rather_than_empty(ctx: TestContext, kv: Kv, db: Db) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let caller = signed_in(&ctx, &kv, &db, queue.clone()).await;
    let account = seed_azure_account(&db, caller.user).await;

    let catalog = read_catalog(&caller).await;
    assert!(catalog.entries.is_empty());
    assert_eq!(
        catalog.pending_accounts,
        vec![account],
        "an account nothing has read yet is named as such: the answer is \
         'not yet', not 'nothing'"
    );

    assert!(
        queued(&backend).contains(&ProvisioningJob::RefreshCatalog {
            user: caller.user,
            account,
        }),
        "and the read the answer is waiting on was asked for: {:?}",
        queued(&backend)
    );
}

#[skyzen::test]
async fn polling_while_a_refresh_is_in_flight_asks_for_it_once(ctx: TestContext, kv: Kv, db: Db) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let caller = signed_in(&ctx, &kv, &db, queue.clone()).await;
    seed_azure_account(&db, caller.user).await;

    // The composer polls every five seconds while an account is pending.
    for _ in 0..4 {
        read_catalog(&caller).await;
    }

    assert_eq!(
        queued(&backend).len(),
        1,
        "one unread account is one refresh, however often it is asked about"
    );
}

#[skyzen::test]
async fn a_cached_region_is_what_the_next_request_serves(ctx: TestContext, kv: Kv, db: Db) {
    let queue = Queue::new(InMemoryQueue::new());
    let caller = signed_in(&ctx, &kv, &db, queue).await;
    let account = seed_azure_account(&db, caller.user).await;

    cache_region(
        &kv,
        account,
        RegionOutcome::Offered {
            entries: vec![entry(account)],
        },
    )
    .await;

    let catalog = read_catalog(&caller).await;
    assert!(
        catalog.pending_accounts.is_empty(),
        "an account that has been read is not pending"
    );
    assert_eq!(
        catalog
            .entries
            .iter()
            .map(|entry| entry.machine_type.as_str())
            .collect::<Vec<_>>(),
        vec![MACHINE_TYPE],
        "and what it can deploy comes out of the cache rather than out of a \
         provider call this request made"
    );
}

#[skyzen::test]
async fn a_consumed_refresh_settles_the_account_it_named(ctx: TestContext, kv: Kv, db: Db) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let caller = signed_in(&ctx, &kv, &db, queue.clone()).await;
    let account = seed_azure_account(&db, caller.user).await;

    assert_eq!(read_catalog(&caller).await.pending_accounts, vec![account]);

    // The real consumer, on the job the request queued. This account has no
    // resource group, so the read refuses before it reaches Azure — and a
    // refusal is still an answer, which is the whole point: the account
    // stops being pending and stops being asked about every five seconds.
    run_queue(&db, &kv, &queue).await;

    let document = catalog::read(&kv, account)
        .await
        .expect("read the document")
        .expect("the consumer wrote one");
    assert!(
        document.failure.is_some(),
        "what the provider said is recorded rather than dropped"
    );

    let catalog = read_catalog(&caller).await;
    assert!(
        catalog.pending_accounts.is_empty(),
        "read and refused is not pending"
    );
    assert!(catalog.entries.is_empty());
}

#[skyzen::test]
async fn a_refresh_for_an_unlinked_account_is_dropped(ctx: TestContext, kv: Kv, db: Db) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let caller = signed_in(&ctx, &kv, &db, queue.clone()).await;
    let account = seed_azure_account(&db, caller.user).await;
    read_catalog(&caller).await;

    caller
        .client
        .delete(&format!("/v1/providers/{account}"))
        .bearer(&caller.token)
        .send()
        .await
        .assert_status(204);

    run_queue(&db, &kv, &queue).await;

    assert!(
        catalog::read(&kv, account)
            .await
            .expect("read the document")
            .is_none(),
        "an account the user withdrew is not read, and nothing is written \
         for it"
    );
}

#[skyzen::test]
async fn the_default_machine_is_not_ready_while_an_account_is_pending(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let queue = Queue::new(InMemoryQueue::new());
    let caller = signed_in(&ctx, &kv, &db, queue).await;
    seed_azure_account(&db, caller.user).await;

    let response = caller
        .client
        .get(DEFAULT)
        .bearer(&caller.token)
        .send()
        .await;
    response.assert_status(409);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/catalog-not-ready",
        "a catalog that is still being read is a different refusal from one \
         that was read and offers nothing"
    );
}

#[skyzen::test]
async fn the_default_machine_is_refused_for_good_once_the_catalog_has_been_read(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let queue = Queue::new(InMemoryQueue::new());
    let caller = signed_in(&ctx, &kv, &db, queue).await;
    let account = seed_azure_account(&db, caller.user).await;

    // Read, and the region offers nothing at all.
    cache_region(
        &kv,
        account,
        RegionOutcome::Offered {
            entries: Vec::new(),
        },
    )
    .await;

    let response = caller
        .client
        .get(DEFAULT)
        .bearer(&caller.token)
        .send()
        .await;
    response.assert_status(422);
    assert_eq!(
        response.json::<Problem>().kind,
        "https://flyco.dev/problems/no-deployable-linux-machine",
        "now it is a fact the user has to act on"
    );
}

#[skyzen::test]
async fn a_cached_machine_is_the_one_flyco_picks(ctx: TestContext, kv: Kv, db: Db) {
    let queue = Queue::new(InMemoryQueue::new());
    let caller = signed_in(&ctx, &kv, &db, queue).await;
    let account = seed_azure_account(&db, caller.user).await;
    cache_region(
        &kv,
        account,
        RegionOutcome::Offered {
            entries: vec![entry(account)],
        },
    )
    .await;

    let response = caller
        .client
        .get(DEFAULT)
        .bearer(&caller.token)
        .send()
        .await;
    response.assert_status(200);
    let chosen: MachineDefault = response.json();
    assert_eq!(chosen.choice.machine_type, MACHINE_TYPE);
    assert_eq!(chosen.choice.provider_account, account);
    assert!(
        chosen.pending_accounts.is_empty(),
        "nothing is still being read, so the choice is final"
    );
}

#[skyzen::test]
async fn a_machine_the_user_owns_is_never_pending_and_never_queued(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    let caller = signed_in(&ctx, &kv, &db, queue).await;
    seed_provider_account(&db, caller.user).await;

    let catalog = read_catalog(&caller).await;
    assert!(
        catalog.pending_accounts.is_empty(),
        "a host's catalog is the row this request already read; there is \
         nothing to wait for"
    );
    assert_eq!(catalog.entries.len(), 1);
    assert!(
        queued(&backend).is_empty(),
        "and nothing to ask the queue for: {:?}",
        queued(&backend)
    );
}

#[skyzen::test]
async fn the_scheduled_sweep_asks_for_an_account_nothing_has_read(kv: Kv, db: Db) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    crate::testing::migrate(&db).await;
    let user = seed_user(&db).await;
    let account = seed_azure_account(&db, user.id).await;
    // A machine the user owns is excluded in SQL: its catalog is never
    // cached, so a refresh would have nothing to write.
    seed_provider_account(&db, user.id).await;

    catalog::refresh_stale(&db, &kv, &queue, crate::clock::now_unix())
        .await
        .expect("sweep");

    assert_eq!(
        queued(&backend),
        vec![ProvisioningJob::RefreshCatalog {
            user: user.id,
            account,
        }],
        "the sweep is what keeps a catalog current for a user who is not \
         looking at it"
    );
}

#[skyzen::test]
async fn the_scheduled_sweep_leaves_a_fresh_document_alone(kv: Kv, db: Db) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    crate::testing::migrate(&db).await;
    let user = seed_user(&db).await;
    let account = seed_azure_account(&db, user.id).await;
    cache_region(
        &kv,
        account,
        RegionOutcome::Offered {
            entries: vec![entry(account)],
        },
    )
    .await;

    catalog::refresh_stale(&db, &kv, &queue, crate::clock::now_unix())
        .await
        .expect("sweep");

    assert!(
        queued(&backend).is_empty(),
        "a document read minutes ago is not re-read every minute"
    );
}

#[skyzen::test]
async fn the_scheduled_sweep_asks_again_once_a_document_is_stale(kv: Kv, db: Db) {
    let backend = InMemoryQueue::new();
    let queue = Queue::new(backend.clone());
    crate::testing::migrate(&db).await;
    let user = seed_user(&db).await;
    let account = seed_azure_account(&db, user.id).await;
    cache_region(
        &kv,
        account,
        RegionOutcome::Offered {
            entries: vec![entry(account)],
        },
    )
    .await;

    // Six hours on: spot meters are republished monthly and a region's
    // availability moves, so the answer is due to be read again.
    catalog::refresh_stale(
        &db,
        &kv,
        &queue,
        crate::clock::now_unix() + catalog::TTL_SECONDS,
    )
    .await
    .expect("sweep");

    assert_eq!(queued(&backend).len(), 1);
}
