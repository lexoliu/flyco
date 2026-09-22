//! Coverage of marketplaces and the skill catalog.
//!
//! The catalog's contract is that a marketplace is *read* somewhere other
//! than the request that wants it: a first read answers `pending` and
//! enqueues the job, and the job's document is what the next request
//! serves. What these tests pin is that pair, that the built-in
//! marketplace is there for everyone and cannot be removed, that one user's
//! marketplaces are unreachable from another's, and that an install turns a
//! directory in a repository into the same zipped bundle an upload would
//! have produced.

use flyco_core::{
    AddMarketplace, BUILT_IN_MARKETPLACE, CurrentUser, InstallCatalogSkill, MarketplaceView,
    Problem, SkillCatalog, SkillView,
};
use skyzen_services::queue::ReceiveOptions;
use skyzen_services::{Db, Kv, Queue, Storage};
use skyzen_test::mock::InMemoryQueue;
use skyzen_test::{TestClient, TestContext};

use crate::provisioning_queue::ProvisioningJob;
use crate::session;
use crate::testing::{
    FakeRepoFiles, TestGithub, migrate, migrated_router_on, seed_other_user, seed_user,
    test_config, test_router_with_github,
};

/// A marketplace with two plugins: one naming its skills, one relying on
/// the default layout, and one pointing at another repository.
const MANIFEST: &[u8] = br#"{
  "name": "example-marketplace",
  "owner": { "name": "Example" },
  "plugins": [
    { "name": "document-skills", "source": "./", "skills": ["./skills/xlsx"] },
    { "name": "tooling", "source": "./plugins/tooling" },
    { "name": "elsewhere", "source": { "source": "github", "repo": "other/repo" } }
  ]
}"#;

const XLSX_SKILL: &[u8] =
    b"---\nname: xlsx\ndescription: Read and write spreadsheets.\n---\n\n# Spreadsheets\n";
const XLSX_SCRIPT: &[u8] = b"print('hello')\n";
const LINT_SKILL: &[u8] = b"---\nname: lint\ndescription: Lint a checkout.\n---\n";
const REVIEW_SKILL: &[u8] = b"---\nname: review\ndescription: Review a diff.\n---\n";

const MARKET_FILES: FakeRepoFiles = &[
    (".claude-plugin/marketplace.json", MANIFEST),
    ("skills/xlsx/SKILL.md", XLSX_SKILL),
    ("skills/xlsx/scripts/convert.py", XLSX_SCRIPT),
    ("plugins/tooling/skills/lint/SKILL.md", LINT_SKILL),
    // The `elsewhere` plugin's files are in ELSEWHERE_FILES, below.
];

/// The repository the `elsewhere` plugin points at: its skills are the
/// marketplace's too, read one repository further out.
const ELSEWHERE_FILES: FakeRepoFiles = &[("skills/review/SKILL.md", REVIEW_SKILL)];

const MARKET: &str = "example/marketplace";
const ELSEWHERE: &str = "other/repo";

const REPOS: &[(&str, FakeRepoFiles)] = &[(MARKET, MARKET_FILES), (ELSEWHERE, ELSEWHERE_FILES)];

fn github() -> TestGithub {
    TestGithub::default().with_files(REPOS)
}

async fn sign_in(kv: &Kv, user: CurrentUser) -> String {
    session::issue(kv, user.id).await.expect("issue a session")
}

/// Runs the refresh the queue would have run, for the job that was queued.
async fn run_queued_refresh(db: &Db, kv: &Kv, queue: &Queue, jobs: usize) {
    let batch = queue
        .receive_json::<ProvisioningJob>(ReceiveOptions::new().with_max_messages(16))
        .await
        .expect("read the queue");
    assert_eq!(
        batch.len(),
        jobs,
        "one refresh per marketplace that has not been read"
    );
    for message in batch {
        let ProvisioningJob::RefreshMarketplace {
            user,
            repo,
            git_ref,
        } = message.body
        else {
            panic!("the catalog enqueues marketplace refreshes and nothing else");
        };
        let marketplace = crate::marketplaces::Marketplace {
            repo: repo.parse().expect("a slug"),
            git_ref,
        };
        crate::skill_catalog::refresh(db, &test_config(), kv, &github(), user, &marketplace)
            .await
            .expect("read the marketplace");
    }
}

#[skyzen::test]
async fn the_built_in_marketplace_is_listed_for_everybody_and_cannot_be_added(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    let router = migrated_router_on(&db, Queue::new(InMemoryQueue::new())).await;
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user).await;
    let client = ctx.client(router);

    let listed = client
        .get("/v1/marketplaces")
        .bearer(&token)
        .send()
        .await
        .json::<Vec<MarketplaceView>>();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].repo, BUILT_IN_MARKETPLACE);
    assert!(listed[0].built_in);
    assert_eq!(listed[0].id, None);

    let again = client
        .post("/v1/marketplaces")
        .bearer(&token)
        .json(&AddMarketplace {
            repo: BUILT_IN_MARKETPLACE.to_owned(),
            git_ref: None,
        })
        .send()
        .await;
    again.assert_status(422);
    assert!(
        again
            .json::<Problem>()
            .kind
            .ends_with("invalid-marketplace")
    );
}

#[skyzen::test]
async fn a_marketplace_is_added_once_and_is_the_adder_s_alone(ctx: TestContext, kv: Kv, db: Db) {
    let router = migrated_router_on(&db, Queue::new(InMemoryQueue::new())).await;
    let user = seed_user(&db).await;
    let other = seed_other_user(&db).await;
    let token = sign_in(&kv, user).await;
    let other_token = sign_in(&kv, other).await;
    let client = ctx.client(router);

    let added = client
        .post("/v1/marketplaces")
        .bearer(&token)
        .json(&AddMarketplace {
            repo: MARKET.to_owned(),
            git_ref: Some("main".to_owned()),
        })
        .send()
        .await;
    added.assert_status(201);
    let added = added.json::<MarketplaceView>();
    assert_eq!(added.repo, MARKET);
    assert_eq!(added.git_ref.as_deref(), Some("main"));
    assert!(!added.built_in);

    let clash = client
        .post("/v1/marketplaces")
        .bearer(&token)
        .json(&AddMarketplace {
            repo: MARKET.to_owned(),
            git_ref: None,
        })
        .send()
        .await;
    clash.assert_status(409);
    assert!(
        clash
            .json::<Problem>()
            .kind
            .ends_with("marketplace-already-added")
    );

    // Somebody else's list has only the built-in one, and their delete
    // cannot reach this row.
    let theirs = client
        .get("/v1/marketplaces")
        .bearer(&other_token)
        .send()
        .await
        .json::<Vec<MarketplaceView>>();
    assert_eq!(theirs.len(), 1);
    assert!(theirs[0].built_in);

    let id = added.id.expect("an added marketplace has an id");
    client
        .delete(&format!("/v1/marketplaces/{id}"))
        .bearer(&other_token)
        .send()
        .await
        .assert_status(404);

    client
        .delete(&format!("/v1/marketplaces/{id}"))
        .bearer(&token)
        .send()
        .await
        .assert_status(204);
    assert_eq!(
        client
            .get("/v1/marketplaces")
            .bearer(&token)
            .send()
            .await
            .json::<Vec<MarketplaceView>>()
            .len(),
        1
    );
}

#[skyzen::test]
async fn a_catalog_reports_pending_until_the_queue_has_read_the_marketplace(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    storage: Storage,
) {
    let _ = storage;
    migrate(&db).await;
    let queue = Queue::new(InMemoryQueue::new());
    let router = test_router_with_github(db.clone(), queue.clone(), github());
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user).await;
    let client = ctx.client(router);

    client
        .post("/v1/marketplaces")
        .bearer(&token)
        .json(&AddMarketplace {
            repo: MARKET.to_owned(),
            git_ref: Some("main".to_owned()),
        })
        .send()
        .await
        .assert_status(201);

    // Nothing has been read yet: both marketplaces are pending and neither
    // reads as "offers nothing".
    let pending = client
        .get("/v1/catalog/skills")
        .bearer(&token)
        .send()
        .await
        .json::<SkillCatalog>();
    assert!(pending.skills.is_empty());
    assert_eq!(pending.pending.len(), 2);
    assert!(pending.pending.contains(&MARKET.to_owned()));
    assert!(pending.pending.contains(&BUILT_IN_MARKETPLACE.to_owned()));

    // Adding asked for one read and the first catalog request asked for the
    // other; a second request within the claim asks for neither.
    run_queued_refresh(&db, &kv, &queue, 2).await;

    let read = client
        .get("/v1/catalog/skills")
        .bearer(&token)
        .send()
        .await
        .json::<SkillCatalog>();
    assert!(read.pending.is_empty());
    // The built-in marketplace is not in the fake GitHub, so it is a
    // failure rather than an empty success.
    assert_eq!(read.failed.len(), 1);
    assert_eq!(read.failed[0].marketplace, BUILT_IN_MARKETPLACE);

    let names: Vec<&str> = read
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect();
    // `review` comes from the repository the `elsewhere` plugin points at,
    // and is listed as one of the marketplace's own.
    assert_eq!(names, vec!["lint", "review", "xlsx"], "{names:?}");
    let xlsx = read
        .skills
        .iter()
        .find(|skill| skill.name == "xlsx")
        .expect("listed");
    assert_eq!(xlsx.description, "Read and write spreadsheets.");
    assert_eq!(xlsx.plugin, "document-skills");
}

/// Adds the fixture marketplace, which every install test starts from.
async fn add_market<E: skyzen::Endpoint + Clone>(client: &TestClient<E>, token: &str) {
    client
        .post("/v1/marketplaces")
        .bearer(token)
        .json(&AddMarketplace {
            repo: MARKET.to_owned(),
            git_ref: Some("main".to_owned()),
        })
        .send()
        .await
        .assert_status(201);
}

/// Asks for one skill, whatever the request is.
fn install(plugin: &str, name: &str) -> InstallCatalogSkill {
    InstallCatalogSkill {
        marketplace: MARKET.to_owned(),
        plugin: plugin.to_owned(),
        name: name.to_owned(),
    }
}

#[skyzen::test]
async fn installing_a_skill_stores_its_directory_as_one_row(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    storage: Storage,
) {
    migrate(&db).await;
    let queue = Queue::new(InMemoryQueue::new());
    let router = test_router_with_github(db.clone(), queue.clone(), github());
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user).await;
    let client = ctx.client(router);

    add_market(&client, &token).await;
    run_queued_refresh(&db, &kv, &queue, 1).await;

    let installed = client
        .post("/v1/catalog/skills")
        .bearer(&token)
        .json(&install("document-skills", "xlsx"))
        .send()
        .await;
    installed.assert_status(201);
    let installed = installed.json::<SkillView>();
    assert_eq!(installed.name, "xlsx");

    // It is now an ordinary skill — one row, because the daemon mounts it
    // into every harness's directory rather than one per harness.
    let listed = client
        .get("/v1/skills")
        .bearer(&token)
        .send()
        .await
        .json::<Vec<SkillView>>();
    assert_eq!(listed.len(), 1);

    // And installing the same skill again replaces that row rather than
    // adding to it: the name keeps the id it already had.
    let again = client
        .post("/v1/catalog/skills")
        .bearer(&token)
        .json(&install("document-skills", "xlsx"))
        .send()
        .await
        .json::<SkillView>();
    assert_eq!(again.id, installed.id);
    let listed = client
        .get("/v1/skills")
        .bearer(&token)
        .send()
        .await
        .json::<Vec<SkillView>>();
    assert_eq!(listed.len(), 1);

    let bundle = storage
        .get(&format!("skills/{}.zip", installed.id))
        .await
        .expect("read the object")
        .expect("the bundle is stored");
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bundle.body)).expect("a readable archive");
    let mut names: Vec<String> = archive.file_names().map(ToOwned::to_owned).collect();
    names.sort();
    assert_eq!(names, vec!["SKILL.md", "scripts/convert.py"]);
    let mut entry = archive.by_name("SKILL.md").expect("the skill file");
    let mut body = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut body).expect("read it back");
    assert_eq!(body, XLSX_SKILL);
}

#[skyzen::test]
async fn an_install_that_cannot_be_served_says_which_of_the_three_it_is(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    migrate(&db).await;
    let queue = Queue::new(InMemoryQueue::new());
    let router = test_router_with_github(db.clone(), queue.clone(), github());
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user).await;
    let client = ctx.client(router);

    add_market(&client, &token).await;

    // Before the read, an install has nothing to install from and says so
    // rather than inventing an answer.
    let early = client
        .post("/v1/catalog/skills")
        .bearer(&token)
        .json(&install("document-skills", "xlsx"))
        .send()
        .await;
    early.assert_status(409);
    assert!(
        early
            .json::<Problem>()
            .kind
            .ends_with("skill-catalog-not-ready")
    );

    run_queued_refresh(&db, &kv, &queue, 1).await;

    // A skill the marketplace does not offer is a 404.
    let unknown = client
        .post("/v1/catalog/skills")
        .bearer(&token)
        .json(&install("document-skills", "nope"))
        .send()
        .await;
    unknown.assert_status(404);
    assert!(
        unknown
            .json::<Problem>()
            .kind
            .ends_with("catalog-skill-not-found")
    );
}

#[skyzen::test]
async fn a_skill_of_a_plugin_in_another_repository_installs_from_that_repository(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    storage: Storage,
) {
    migrate(&db).await;
    let queue = Queue::new(InMemoryQueue::new());
    let router = test_router_with_github(db.clone(), queue.clone(), github());
    let user = seed_user(&db).await;
    let token = sign_in(&kv, user).await;
    let client = ctx.client(router);

    add_market(&client, &token).await;
    run_queued_refresh(&db, &kv, &queue, 1).await;

    let installed = client
        .post("/v1/catalog/skills")
        .bearer(&token)
        .json(&install("elsewhere", "review"))
        .send()
        .await;
    installed.assert_status(201);
    let installed = installed.json::<SkillView>();
    assert_eq!(installed.name, "review");

    let bundle = storage
        .get(&format!("skills/{}.zip", installed.id))
        .await
        .expect("read the object")
        .expect("the bundle is stored");
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bundle.body)).expect("a readable archive");
    let names: Vec<String> = archive.file_names().map(ToOwned::to_owned).collect();
    assert_eq!(names, vec!["SKILL.md"]);
    let mut entry = archive.by_name("SKILL.md").expect("the skill file");
    let mut body = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut body).expect("read it back");
    assert_eq!(body, REVIEW_SKILL);
}

#[skyzen::test]
async fn a_marketplace_somebody_else_added_cannot_be_installed_from(
    ctx: TestContext,
    kv: Kv,
    db: Db,
) {
    migrate(&db).await;
    let queue = Queue::new(InMemoryQueue::new());
    let router = test_router_with_github(db.clone(), queue.clone(), github());
    let user = seed_user(&db).await;
    let other = seed_other_user(&db).await;
    let token = sign_in(&kv, user).await;
    let other_token = sign_in(&kv, other).await;
    let client = ctx.client(router);

    client
        .post("/v1/marketplaces")
        .bearer(&token)
        .json(&AddMarketplace {
            repo: MARKET.to_owned(),
            git_ref: Some("main".to_owned()),
        })
        .send()
        .await
        .assert_status(201);
    run_queued_refresh(&db, &kv, &queue, 1).await;

    let refused = client
        .post("/v1/catalog/skills")
        .bearer(&other_token)
        .json(&install("document-skills", "xlsx"))
        .send()
        .await;
    refused.assert_status(404);
    assert!(
        refused
            .json::<Problem>()
            .kind
            .ends_with("marketplace-not-found")
    );
}

#[skyzen::test]
async fn the_catalog_needs_a_signed_in_user(ctx: TestContext, db: Db) {
    let router = migrated_router_on(&db, Queue::new(InMemoryQueue::new())).await;
    let client = ctx.client(router);
    client
        .get("/v1/catalog/skills")
        .send()
        .await
        .assert_status(401);
    client
        .get("/v1/marketplaces")
        .send()
        .await
        .assert_status(401);
}
