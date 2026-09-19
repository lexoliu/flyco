//! The per-principal request budgets, driven through the router (issue
//! #342).
//!
//! [`crate::request_budget`]'s own tests pin the ledger arithmetic; these
//! pin what a caller sees: the per-minute refusal before any credential
//! is resolved, the daily refusal once a principal's ceiling is reached,
//! the `Retry-After` both carry, and that one principal's day is nobody
//! else's.

use flyco_core::{CurrentUser, Problem};
use skyzen::routing::Router;
use skyzen_services::{Db, Kv, Queue};
use skyzen_test::TestContext;
use skyzen_test::mock::InMemoryQueue;

use crate::request_budget::{Limit, Limits, Principal, WINDOW_SECONDS, recorded};
use crate::testing::{migrate, seed_user, test_router_budgeted};
use crate::{clock, session};

/// Ceilings a test reaches in a handful of requests.
///
/// The per-minute limits stay far above the per-day ones except where a
/// test lowers one on purpose, so each test exercises one bound.
fn limits() -> Limits {
    let wide = Limit {
        per_minute: 1_000,
        per_day: 1_000,
    };
    Limits {
        user: Limit {
            per_minute: 1_000,
            per_day: 3,
        },
        daemon: wide,
        host: wide,
        public: Limit {
            per_minute: 1_000,
            per_day: 2,
        },
    }
}

async fn router_with(db: &Db, limits: Limits) -> Router {
    migrate(db).await;
    test_router_budgeted(db.clone(), Queue::new(InMemoryQueue::new()), limits)
}

async fn signed_in(kv: &Kv, db: &Db) -> (CurrentUser, String) {
    let user = seed_user(db).await;
    let token = session::issue(kv, user.id).await.expect("issue a session");
    (user, token)
}

#[skyzen::test]
async fn a_user_at_its_daily_ceiling_is_refused_until_midnight(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(router_with(&db, limits()).await);
    let (user, token) = signed_in(&kv, &db).await;

    for _ in 0..3 {
        client
            .get("/v1/me")
            .bearer(&token)
            .send()
            .await
            .assert_status(200);
    }

    let refused = client.get("/v1/me").bearer(&token).send().await;
    refused.assert_status(429);
    refused.assert_header("content-type", "application/problem+json");
    let problem: Problem = refused.json();
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/request-budget-exhausted"
    );
    // The wait is the time to UTC midnight, which only the clock knows;
    // that it is named at all is the contract.
    refused.assert_header_exists("retry-after");

    // The day's total reached D1: the first request and the one that hit
    // the ceiling both flushed.
    let now = clock::now_unix();
    assert_eq!(
        recorded(&db, &Principal::User(user.id), now)
            .await
            .expect("read the ledger"),
        3
    );
}

#[skyzen::test]
async fn one_principals_day_is_not_anothers(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(router_with(&db, limits()).await);
    let (_, first) = signed_in(&kv, &db).await;
    let second_user = crate::testing::seed_other_user(&db).await;
    let second = session::issue(&kv, second_user.id)
        .await
        .expect("issue a session");

    for _ in 0..3 {
        client
            .get("/v1/me")
            .bearer(&first)
            .send()
            .await
            .assert_status(200);
    }
    client
        .get("/v1/me")
        .bearer(&first)
        .send()
        .await
        .assert_status(429);

    client
        .get("/v1/me")
        .bearer(&second)
        .send()
        .await
        .assert_status(200);
}

#[skyzen::test]
async fn a_spent_user_is_refused_on_every_credential_it_holds(ctx: TestContext, kv: Kv, db: Db) {
    // The budget is the account's: a second browser session of the same
    // user runs into the same ceiling. It costs one credential lookup —
    // the pre-auth block list is keyed by credential — and is then blocked
    // like the first.
    let client = ctx.client(router_with(&db, limits()).await);
    let (user, browser) = signed_in(&kv, &db).await;
    let other_tab = session::issue(&kv, user.id).await.expect("issue a session");

    for _ in 0..3 {
        client
            .get("/v1/me")
            .bearer(&browser)
            .send()
            .await
            .assert_status(200);
    }
    // The ceiling was reached on the third request; the ledger knows.
    let refused = client.get("/v1/me").bearer(&other_tab).send().await;
    refused.assert_status(200);
    client
        .get("/v1/me")
        .bearer(&other_tab)
        .send()
        .await
        .assert_status(429);
}

#[skyzen::test]
async fn the_per_minute_bound_refuses_before_any_credential_is_resolved(
    ctx: TestContext,
    _kv: Kv,
    db: Db,
) {
    let mut limits = limits();
    limits.user.per_minute = 2;
    limits.user.per_day = 1_000;
    // The refused sign-ins are charged to the address; keep that day open
    // so the per-minute bound is the one that answers.
    limits.public.per_day = 1_000;
    let client = ctx.client(router_with(&db, limits).await);

    // A credential nobody minted: resolving it would be a KV read and a
    // 401. The third request is refused before either.
    for _ in 0..2 {
        client
            .get("/v1/me")
            .bearer("fs_never-minted")
            .send()
            .await
            .assert_status(401);
    }
    let refused = client.get("/v1/me").bearer("fs_never-minted").send().await;
    refused.assert_status(429);
    let problem: Problem = refused.json();
    assert_eq!(problem.kind, "https://flyco.dev/problems/rate-limited");
    refused.assert_header("retry-after", &WINDOW_SECONDS.to_string());
}

#[skyzen::test]
async fn anonymous_traffic_is_charged_to_its_address(ctx: TestContext, _kv: Kv, db: Db) {
    let client = ctx.client(router_with(&db, limits()).await);

    for _ in 0..2 {
        client.get("/v1/healthz").send().await.assert_status(200);
    }
    let refused = client.get("/v1/healthz").send().await;
    refused.assert_status(429);
    let problem: Problem = refused.json();
    assert_eq!(
        problem.kind,
        "https://flyco.dev/problems/request-budget-exhausted"
    );

    // Two different addresses are two principals.
    client
        .get("/v1/healthz")
        .header("cf-connecting-ip", "203.0.113.7")
        .send()
        .await
        .assert_status(200);
}

#[skyzen::test]
async fn a_credential_that_does_not_resolve_spends_the_address_not_a_user(
    ctx: TestContext,
    _kv: Kv,
    db: Db,
) {
    let client = ctx.client(router_with(&db, limits()).await);

    // Two refused sign-ins spend the address's day (public ceiling: 2);
    // the third is refused before the credential is even looked up.
    for _ in 0..2 {
        client
            .get("/v1/me")
            .bearer("fk_never-minted")
            .send()
            .await
            .assert_status(401);
    }
    client
        .get("/v1/me")
        .bearer("fk_never-minted")
        .send()
        .await
        .assert_status(429);
    let now = clock::now_unix();
    assert_eq!(
        recorded(&db, &Principal::Address("local".to_owned()), now)
            .await
            .expect("read the ledger"),
        2
    );
}
