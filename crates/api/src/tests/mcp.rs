//! Coverage of the MCP registry.
//!
//! Agents may not configure MCP for themselves, so this registry is the only
//! path a server takes to a session. What matters here is that a name is
//! unique per user — it is what the harness announces the server under, and
//! two servers answering to one name collide on the machine — and that one
//! user's registry is unreachable from another's.

use flyco_core::{CurrentUser, McpServerConfig, McpServerView, Problem, UpsertMcpServer};
use skyzen_services::{Db, Kv};
use skyzen_test::TestContext;

use crate::session;
use crate::testing::{migrated_router, seed_other_user, seed_user};

fn stdio(name: &str) -> UpsertMcpServer {
    UpsertMcpServer {
        name: name.to_owned(),
        config: McpServerConfig::Stdio {
            command: "uvx".to_owned(),
            args: vec!["mcp-server-git".to_owned()],
            env: Vec::new(),
        },
        enabled: true,
    }
}

async fn sign_in(kv: &Kv, user: CurrentUser) -> String {
    session::issue(kv, user.id).await.expect("issue a session")
}

#[skyzen::test]
async fn a_name_may_be_registered_once_per_user(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let other = seed_other_user(&db).await;
    let token = sign_in(&kv, user).await;
    let other_token = sign_in(&kv, other).await;
    let client = ctx.client(router);

    client
        .post("/v1/mcp-servers")
        .bearer(&token)
        .json(&stdio("git"))
        .send()
        .await
        .assert_status(201);

    let clash = client
        .post("/v1/mcp-servers")
        .bearer(&token)
        .json(&stdio("git"))
        .send()
        .await;
    clash.assert_status(409);
    assert!(
        clash
            .json::<Problem>()
            .kind
            .ends_with("mcp-server-name-taken"),
        "a duplicate name must say so rather than failing generically"
    );

    // The name is only taken within one user's own registry.
    client
        .post("/v1/mcp-servers")
        .bearer(&other_token)
        .json(&stdio("git"))
        .send()
        .await
        .assert_status(201);
}

#[skyzen::test]
async fn another_users_server_is_indistinguishable_from_absent(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let owner = seed_user(&db).await;
    let stranger = seed_other_user(&db).await;
    let owner_token = sign_in(&kv, owner).await;
    let stranger_token = sign_in(&kv, stranger).await;
    let client = ctx.client(router);

    let registered: McpServerView = client
        .post("/v1/mcp-servers")
        .bearer(&owner_token)
        .json(&stdio("playwright"))
        .send()
        .await
        .json();

    let path = format!("/v1/mcp-servers/{}", registered.id);
    for status in [
        client.get(&path).bearer(&stranger_token).send().await,
        client.delete(&path).bearer(&stranger_token).send().await,
    ] {
        status.assert_status(404);
    }

    client
        .get(&path)
        .bearer(&owner_token)
        .send()
        .await
        .assert_status(200);
}

#[skyzen::test]
async fn a_name_the_harness_could_not_announce_is_refused(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user).await;

    // The name becomes part of `mcp__<server>__<tool>`, so a space would
    // produce tools the model cannot address.
    let response = ctx
        .client(router)
        .post("/v1/mcp-servers")
        .bearer(&token)
        .json(&stdio("my server"))
        .send()
        .await;

    response.assert_status(422);
}
