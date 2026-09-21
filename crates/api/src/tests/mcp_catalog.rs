//! Coverage of the MCP catalog.
//!
//! The picker is a search over the official registry, read one page at a
//! time and kept in the store for an hour; an install names an entry and a
//! kind and lets the control plane fill the config. What these tests pin is
//! that a page lists only what a machine can run, that a repeated query is
//! answered from the store, and that an install registers exactly the
//! config the entry describes — refusing a blank required input, a kind the
//! entry does not offer, and a name already in use.

use std::collections::BTreeMap;

use flyco_core::{
    CatalogInstallKind, CurrentUser, InstallCatalogMcpServer, McpCatalogPage, McpServerConfig,
    McpServerView, Problem,
};
use skyzen_services::{Db, Kv, Queue};
use skyzen_test::TestContext;
use skyzen_test::mock::InMemoryQueue;

use crate::session;
use crate::testing::{
    TestRegistry, migrate, migrated_router, seed_user, test_router_with_registry,
};

const SMITHERY: &str = "ai.smithery/Hint-Services-obsidian-github-mcp";

async fn sign_in(kv: &Kv, user: CurrentUser) -> String {
    session::issue(kv, user.id).await.expect("issue a session")
}

fn install(
    server: &str,
    kind: CatalogInstallKind,
    values: &[(&str, &str)],
) -> InstallCatalogMcpServer {
    InstallCatalogMcpServer {
        server: server.to_owned(),
        kind,
        name: None,
        values: values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>(),
    }
}

#[skyzen::test]
async fn a_page_lists_what_a_machine_can_run_and_is_read_once(ctx: TestContext, kv: Kv, db: Db) {
    migrate(&db).await;
    let registry = TestRegistry::fixtures();
    let router = test_router_with_registry(
        db.clone(),
        Queue::new(InMemoryQueue::new()),
        registry.clone(),
    );
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user).await;
    let client = ctx.client(router);

    let page = client
        .get("/v1/catalog/mcp-servers?search=github")
        .bearer(&token)
        .send()
        .await;
    page.assert_status(200);
    let page = page.json::<McpCatalogPage>();
    assert_eq!(page.servers.len(), 5);
    let smithery = page
        .servers
        .iter()
        .find(|server| server.name == SMITHERY)
        .expect("the remote entry is listed");
    assert_eq!(smithery.installs.len(), 1);
    assert_eq!(smithery.installs[0].kind, CatalogInstallKind::Remote);
    assert_eq!(smithery.installs[0].inputs[0].key, "var:smithery_api_key");
    assert_eq!(registry.calls(), 1);

    // The same query again is the store's answer.
    client
        .get("/v1/catalog/mcp-servers?search=github")
        .bearer(&token)
        .send()
        .await
        .assert_status(200);
    assert_eq!(registry.calls(), 1);

    // A query the registry would refuse never reaches it.
    let long = format!("/v1/catalog/mcp-servers?search={}", "x".repeat(101));
    let refused = client.get(&long).bearer(&token).send().await;
    refused.assert_status(400);
    assert!(
        refused
            .json::<Problem>()
            .kind
            .ends_with("invalid-catalog-query")
    );
    assert_eq!(registry.calls(), 1);
}

#[skyzen::test]
async fn an_install_registers_the_entry_filled_with_the_answers(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user).await;
    let client = ctx.client(router);

    let missing = client
        .post("/v1/catalog/mcp-servers")
        .bearer(&token)
        .json(&install(SMITHERY, CatalogInstallKind::Remote, &[]))
        .send()
        .await;
    missing.assert_status(422);
    assert!(
        missing
            .json::<Problem>()
            .kind
            .ends_with("catalog-input-missing")
    );

    let added = client
        .post("/v1/catalog/mcp-servers")
        .bearer(&token)
        .json(&install(
            SMITHERY,
            CatalogInstallKind::Remote,
            &[("var:smithery_api_key", "sk-test")],
        ))
        .send()
        .await;
    added.assert_status(201);
    let view = added.json::<McpServerView>();
    assert_eq!(view.name, "Hint-Services-obsidian-github-mcp");
    assert!(view.enabled);
    match &view.config {
        McpServerConfig::Http { url, headers } => {
            assert_eq!(
                url,
                "https://server.smithery.ai/@Hint-Services/obsidian-github-mcp/mcp"
            );
            assert_eq!(headers.len(), 1);
            assert_eq!(headers[0].value, "Bearer sk-test");
        }
        other @ McpServerConfig::Stdio { .. } => panic!("expected an http config, got {other:?}"),
    }

    // It is now an ordinary registered server.
    let listed = client
        .get("/v1/mcp-servers")
        .bearer(&token)
        .send()
        .await
        .json::<Vec<McpServerView>>();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, view.id);

    // Adding it again collides on the name, like any other registration.
    let again = client
        .post("/v1/catalog/mcp-servers")
        .bearer(&token)
        .json(&install(
            SMITHERY,
            CatalogInstallKind::Remote,
            &[("var:smithery_api_key", "sk-test")],
        ))
        .send()
        .await;
    again.assert_status(409);
    assert!(
        again
            .json::<Problem>()
            .kind
            .ends_with("mcp-server-name-taken")
    );

    // ... unless registered under another name.
    let mut renamed = install(
        SMITHERY,
        CatalogInstallKind::Remote,
        &[("var:smithery_api_key", "sk-test")],
    );
    renamed.name = Some("obsidian".to_owned());
    let renamed = client
        .post("/v1/catalog/mcp-servers")
        .bearer(&token)
        .json(&renamed)
        .send()
        .await;
    renamed.assert_status(201);
    assert_eq!(renamed.json::<McpServerView>().name, "obsidian");
}

#[skyzen::test]
async fn an_install_refuses_a_kind_the_entry_does_not_offer(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user).await;
    let client = ctx.client(router);

    let wrong_kind = client
        .post("/v1/catalog/mcp-servers")
        .bearer(&token)
        .json(&install(SMITHERY, CatalogInstallKind::Npm, &[]))
        .send()
        .await;
    wrong_kind.assert_status(422);
    assert!(
        wrong_kind
            .json::<Problem>()
            .kind
            .ends_with("catalog-install-unavailable")
    );

    let unknown = client
        .post("/v1/catalog/mcp-servers")
        .bearer(&token)
        .json(&install(
            "io.example/nobody",
            CatalogInstallKind::Remote,
            &[],
        ))
        .send()
        .await;
    unknown.assert_status(404);
    assert!(
        unknown
            .json::<Problem>()
            .kind
            .ends_with("catalog-server-not-found")
    );
}

#[skyzen::test]
async fn a_package_install_runs_through_npx(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user).await;
    let client = ctx.client(router);

    let added = client
        .post("/v1/catalog/mcp-servers")
        .bearer(&token)
        .json(&install(
            "io.github.bytedance/mcp-server-filesystem",
            CatalogInstallKind::Npm,
            &[("arg:allowed-directories", "/workspace")],
        ))
        .send()
        .await;
    added.assert_status(201);
    let view = added.json::<McpServerView>();
    assert_eq!(view.name, "mcp-server-filesystem");
    match &view.config {
        McpServerConfig::Stdio { command, args, env } => {
            assert_eq!(command, "npx");
            assert_eq!(
                args,
                &[
                    "-y",
                    "@agent-infra/mcp-server-filesystem",
                    "--allowed-directories",
                    "/workspace"
                ]
            );
            assert!(env.is_empty());
        }
        other @ McpServerConfig::Http { .. } => panic!("expected a stdio config, got {other:?}"),
    }
}

#[skyzen::test]
async fn the_catalog_needs_a_signed_in_user(ctx: TestContext, db: Db) {
    let router = migrated_router(&db).await;
    let client = ctx.client(router);
    client
        .get("/v1/catalog/mcp-servers")
        .send()
        .await
        .assert_status(401);
}
