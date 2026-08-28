//! End-to-end coverage of the tree memory and the shared `AGENTS.md`.
//!
//! The two documents an agent is given rather than allowed to write, so what
//! these tests are mostly about is scope: one user's memory must be
//! unreachable — and indistinguishable from absent — to another.

use flyco_core::{
    AgentsDocument, CreateMemoryNode, CurrentUser, MemoryNode, MemoryNodeId, Problem,
    UpdateAgentsDocument, UpdateMemoryNode,
};
use skyzen::routing::Router;
use skyzen_services::{Db, Kv};
use skyzen_test::{TestClient, TestContext};

use crate::session;
use crate::testing::{migrated_router, seed_other_user, seed_user};

const REPO: &str = "lexoliu/flyco";

fn problem_kind(slug: &str) -> String {
    let mut kind = String::from("https://flyco.dev/problems/");
    kind.push_str(slug);
    kind
}

async fn sign_in(kv: &Kv, user: CurrentUser) -> String {
    session::issue(kv, user.id).await.expect("issue a session")
}

fn remember(parent: Option<MemoryNodeId>, repo: Option<&str>, title: &str) -> CreateMemoryNode {
    CreateMemoryNode {
        parent,
        repo: repo.map(|repo| repo.parse().expect("a valid repo slug")),
        title: title.to_owned(),
        content: format!("everything worth knowing about {title}"),
    }
}

async fn create(client: &TestClient<Router>, token: &str, body: &CreateMemoryNode) -> MemoryNode {
    let response = client
        .post("/v1/memory")
        .bearer(token)
        .json(body)
        .send()
        .await;
    response.assert_status(201);
    response.json()
}

async fn list(client: &TestClient<Router>, token: &str, query: &str) -> Vec<MemoryNode> {
    let response = client
        .get(&format!("/v1/memory{query}"))
        .bearer(token)
        .send()
        .await;
    response.assert_status(200);
    response.json()
}

// ── The tree ──

#[skyzen::test]
async fn a_node_is_created_read_back_and_edited(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let token = sign_in(&kv, seed_user(&db).await).await;

    let node = create(&client, &token, &remember(None, None, "commit style")).await;
    assert!(node.parent.is_none());
    assert!(node.repo.is_none());
    assert!(node.updated_at_unix > 0);

    let read = client
        .get(&format!("/v1/memory/{}", node.id))
        .bearer(&token)
        .send()
        .await;
    read.assert_status(200);
    assert_eq!(read.json::<MemoryNode>(), node);

    let edited = client
        .patch(&format!("/v1/memory/{}", node.id))
        .bearer(&token)
        .json(&UpdateMemoryNode {
            title: None,
            content: Some("conventional commits, no co-author trailers".to_owned()),
        })
        .send()
        .await;
    edited.assert_status(200);
    let edited: MemoryNode = edited.json();
    assert_eq!(edited.title, node.title, "an absent field is left alone");
    assert_eq!(
        edited.content,
        "conventional commits, no co-author trailers"
    );
}

#[skyzen::test]
async fn a_listing_is_one_level_of_one_scope(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let token = sign_in(&kv, seed_user(&db).await).await;

    let shared = create(&client, &token, &remember(None, None, "shared root")).await;
    let scoped = create(&client, &token, &remember(None, Some(REPO), "repo root")).await;
    let child = create(
        &client,
        &token,
        &remember(Some(shared.id), None, "shared child"),
    )
    .await;

    let roots = list(&client, &token, "").await;
    assert_eq!(
        roots.iter().map(|node| node.id).collect::<Vec<_>>(),
        vec![shared.id],
        "an unfiltered listing is the shared roots, not everything flat"
    );

    let scoped_roots = list(&client, &token, &format!("?repo={REPO}")).await;
    assert_eq!(
        scoped_roots.iter().map(|node| node.id).collect::<Vec<_>>(),
        vec![scoped.id]
    );

    let children = list(&client, &token, &format!("?parent={}", shared.id)).await;
    assert_eq!(
        children.iter().map(|node| node.id).collect::<Vec<_>>(),
        vec![child.id]
    );
}

#[skyzen::test]
async fn forgetting_a_node_forgets_everything_under_it(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let token = sign_in(&kv, seed_user(&db).await).await;

    let root = create(&client, &token, &remember(None, None, "root")).await;
    let child = create(&client, &token, &remember(Some(root.id), None, "child")).await;
    let grandchild = create(
        &client,
        &token,
        &remember(Some(child.id), None, "grandchild"),
    )
    .await;
    let untouched = create(&client, &token, &remember(None, None, "elsewhere")).await;

    client
        .delete(&format!("/v1/memory/{}", root.id))
        .bearer(&token)
        .send()
        .await
        .assert_status(204);

    for gone in [root.id, child.id, grandchild.id] {
        client
            .get(&format!("/v1/memory/{gone}"))
            .bearer(&token)
            .send()
            .await
            .assert_status(404);
    }
    client
        .get(&format!("/v1/memory/{}", untouched.id))
        .bearer(&token)
        .send()
        .await
        .assert_status(200);
}

#[skyzen::test]
async fn one_users_memory_is_invisible_to_another(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let owner = sign_in(&kv, seed_user(&db).await).await;
    let stranger = sign_in(&kv, seed_other_user(&db).await).await;

    let node = create(&client, &owner, &remember(None, None, "private")).await;

    let read = client
        .get(&format!("/v1/memory/{}", node.id))
        .bearer(&stranger)
        .send()
        .await;
    read.assert_status(404);
    assert_eq!(
        read.json::<Problem>().kind,
        problem_kind("memory-node-not-found")
    );

    client
        .patch(&format!("/v1/memory/{}", node.id))
        .bearer(&stranger)
        .json(&UpdateMemoryNode {
            title: Some("mine now".to_owned()),
            content: None,
        })
        .send()
        .await
        .assert_status(404);
    client
        .delete(&format!("/v1/memory/{}", node.id))
        .bearer(&stranger)
        .send()
        .await
        .assert_status(404);
    assert_eq!(list(&client, &stranger, "").await, Vec::new());

    // …and the owner's node is untouched by any of it.
    let still_there = client
        .get(&format!("/v1/memory/{}", node.id))
        .bearer(&owner)
        .send()
        .await;
    still_there.assert_status(200);
    assert_eq!(still_there.json::<MemoryNode>().title, "private");
}

#[skyzen::test]
async fn a_node_cannot_be_hung_off_another_users_parent(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let owner = sign_in(&kv, seed_user(&db).await).await;
    let stranger = sign_in(&kv, seed_other_user(&db).await).await;

    let parent = create(&client, &owner, &remember(None, None, "root")).await;

    let refused = client
        .post("/v1/memory")
        .bearer(&stranger)
        .json(&remember(Some(parent.id), None, "smuggled"))
        .send()
        .await;
    refused.assert_status(404);
    assert_eq!(
        list(&client, &owner, &format!("?parent={}", parent.id)).await,
        Vec::new(),
        "nothing was written under the owner's node"
    );
}

#[skyzen::test]
async fn a_parent_that_does_not_exist_is_refused(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let token = sign_in(&kv, seed_user(&db).await).await;

    client
        .post("/v1/memory")
        .bearer(&token)
        .json(&remember(Some(MemoryNodeId::generate()), None, "orphan"))
        .send()
        .await
        .assert_status(404);
}

// ── The shared AGENTS.md ──

#[skyzen::test]
async fn an_unwritten_agents_md_is_empty_rather_than_missing(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let token = sign_in(&kv, seed_user(&db).await).await;

    let response = client.get("/v1/agents-md").bearer(&token).send().await;
    response.assert_status(200);
    let document: AgentsDocument = response.json();
    assert_eq!(document.content, "");
    assert_eq!(document.updated_at_unix, 0);
}

#[skyzen::test]
async fn writing_the_agents_md_replaces_it_and_stamps_it(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let token = sign_in(&kv, seed_user(&db).await).await;

    let written = client
        .put("/v1/agents-md")
        .bearer(&token)
        .json(&UpdateAgentsDocument {
            content: "# Rules\n\nNo co-author trailers.\n".to_owned(),
        })
        .send()
        .await;
    written.assert_status(200);
    let written: AgentsDocument = written.json();
    assert!(written.content.starts_with("# Rules"));
    assert!(written.updated_at_unix > 0);

    let replaced = client
        .put("/v1/agents-md")
        .bearer(&token)
        .json(&UpdateAgentsDocument {
            content: "# Rules\n\nConventional commits.\n".to_owned(),
        })
        .send()
        .await;
    replaced.assert_status(200);
    assert_eq!(
        replaced.json::<AgentsDocument>().content,
        "# Rules\n\nConventional commits.\n",
        "a write replaces the document rather than appending to it"
    );

    let read = client.get("/v1/agents-md").bearer(&token).send().await;
    read.assert_status(200);
    assert_eq!(
        read.json::<AgentsDocument>().content,
        "# Rules\n\nConventional commits.\n"
    );
}

#[skyzen::test]
async fn one_users_agents_md_is_not_anothers(ctx: TestContext, kv: Kv, db: Db) {
    let client = ctx.client(migrated_router(&db).await);
    let owner = sign_in(&kv, seed_user(&db).await).await;
    let stranger = sign_in(&kv, seed_other_user(&db).await).await;

    client
        .put("/v1/agents-md")
        .bearer(&owner)
        .json(&UpdateAgentsDocument {
            content: "mine".to_owned(),
        })
        .send()
        .await
        .assert_status(200);

    let theirs = client.get("/v1/agents-md").bearer(&stranger).send().await;
    theirs.assert_status(200);
    assert_eq!(theirs.json::<AgentsDocument>().content, "");
}
