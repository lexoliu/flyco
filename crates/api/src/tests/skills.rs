//! Coverage of the skills registry.
//!
//! A skill is two things that have to stay in step: a row that names it and
//! an object that holds it. What these tests pin is the pair — that an
//! upload produces both, that re-uploading a name replaces the bundle rather
//! than accumulating a second one, that a delete takes the object with the
//! row, and that one user's skills are unreachable from another's.

use flyco_core::{CurrentUser, Problem, SkillMount, SkillScope, SkillView};
use skyzen_services::{Db, Kv, Storage};
use skyzen_test::TestContext;

use crate::daemon_tokens;
use crate::session;
use crate::testing::{migrated_router, seed_other_user, seed_session, seed_user};

/// The smallest byte string that starts like a zip archive.
const BUNDLE: &[u8] = b"PK\x03\x04a skill bundle";

/// A second bundle, so a replacement is visible by its size.
const BIGGER: &[u8] = b"PK\x03\x04a rather longer skill bundle";

async fn sign_in(kv: &Kv, user: CurrentUser) -> String {
    session::issue(kv, user.id).await.expect("issue a session")
}

fn upload_path(name: &str, scope: &str) -> String {
    format!("/v1/skills?name={name}&scope={scope}")
}

#[skyzen::test]
async fn an_upload_stores_the_bundle_and_indexes_it(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    storage: Storage,
) {
    let router = migrated_router(&db).await;
    let token = sign_in(&kv, seed_user(&db).await).await;
    let client = ctx.client(router);

    let created = client
        .post(&upload_path("release-notes", "claude"))
        .bearer(&token)
        .body(BUNDLE)
        .send()
        .await;
    created.assert_status(201);

    let view: SkillView = created.json();
    assert_eq!(view.name, "release-notes");
    assert_eq!(view.scope, SkillScope::Claude);
    assert_eq!(view.size_bytes, BUNDLE.len() as u64);

    let stored = storage
        .get(&format!("skills/{}.zip", view.id))
        .await
        .expect("read the bundle")
        .expect("the bundle is stored under its id");
    assert_eq!(stored.body, BUNDLE);

    let listed: Vec<SkillView> = client.get("/v1/skills").bearer(&token).send().await.json();
    assert_eq!(listed, vec![view.clone()]);

    let read: SkillView = client
        .get(&format!("/v1/skills/{}", view.id))
        .bearer(&token)
        .send()
        .await
        .json();
    assert_eq!(read, view);
}

#[skyzen::test]
async fn re_uploading_a_name_replaces_the_bundle_it_had(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    storage: Storage,
) {
    let router = migrated_router(&db).await;
    let token = sign_in(&kv, seed_user(&db).await).await;
    let client = ctx.client(router);

    let first: SkillView = client
        .post(&upload_path("changelog", "codex"))
        .bearer(&token)
        .body(BUNDLE)
        .send()
        .await
        .json();

    let second: SkillView = client
        .post(&upload_path("changelog", "codex"))
        .bearer(&token)
        .body(BIGGER)
        .send()
        .await
        .json();

    // The same name keeps the same identity, so a machine that materializes
    // skills by id finds one bundle rather than two.
    assert_eq!(first.id, second.id);
    assert_eq!(second.size_bytes, BIGGER.len() as u64);

    let listed: Vec<SkillView> = client.get("/v1/skills").bearer(&token).send().await.json();
    assert_eq!(listed.len(), 1);

    let stored = storage
        .get(&format!("skills/{}.zip", first.id))
        .await
        .expect("read the bundle")
        .expect("the bundle is stored");
    assert_eq!(stored.body, BIGGER);
}

#[skyzen::test]
async fn one_name_may_exist_once_per_harness(ctx: TestContext, kv: Kv, db: Db, _storage: Storage) {
    let router = migrated_router(&db).await;
    let token = sign_in(&kv, seed_user(&db).await).await;
    let client = ctx.client(router);

    for scope in ["claude", "codex"] {
        client
            .post(&upload_path("shared", scope))
            .bearer(&token)
            .body(BUNDLE)
            .send()
            .await
            .assert_status(201);
    }

    let listed: Vec<SkillView> = client.get("/v1/skills").bearer(&token).send().await.json();
    assert_eq!(listed.len(), 2, "the scopes are separate directories");
}

#[skyzen::test]
async fn deleting_a_skill_takes_its_bundle_with_it(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    storage: Storage,
) {
    let router = migrated_router(&db).await;
    let token = sign_in(&kv, seed_user(&db).await).await;
    let client = ctx.client(router);

    let view: SkillView = client
        .post(&upload_path("obsolete", "claude"))
        .bearer(&token)
        .body(BUNDLE)
        .send()
        .await
        .json();

    let path = format!("/v1/skills/{}", view.id);
    client
        .delete(&path)
        .bearer(&token)
        .send()
        .await
        .assert_status(204);

    client
        .get(&path)
        .bearer(&token)
        .send()
        .await
        .assert_status(404);
    assert!(
        storage
            .get(&format!("skills/{}.zip", view.id))
            .await
            .expect("read the bundle")
            .is_none(),
        "the object must not outlive the row that named it"
    );

    // A second delete is a 404, not a second success.
    client
        .delete(&path)
        .bearer(&token)
        .send()
        .await
        .assert_status(404);
}

#[skyzen::test]
async fn another_users_skill_is_indistinguishable_from_absent(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    _storage: Storage,
) {
    let router = migrated_router(&db).await;
    let owner_token = sign_in(&kv, seed_user(&db).await).await;
    let stranger_token = sign_in(&kv, seed_other_user(&db).await).await;
    let client = ctx.client(router);

    let view: SkillView = client
        .post(&upload_path("private", "claude"))
        .bearer(&owner_token)
        .body(BUNDLE)
        .send()
        .await
        .json();

    let path = format!("/v1/skills/{}", view.id);
    for response in [
        client.get(&path).bearer(&stranger_token).send().await,
        client.delete(&path).bearer(&stranger_token).send().await,
    ] {
        response.assert_status(404);
        assert!(response.json::<Problem>().kind.ends_with("skill-not-found"));
    }

    let listed: Vec<SkillView> = client
        .get("/v1/skills")
        .bearer(&stranger_token)
        .send()
        .await
        .json();
    assert_eq!(listed, Vec::new());

    client
        .get(&path)
        .bearer(&owner_token)
        .send()
        .await
        .assert_status(200);
}

#[skyzen::test]
async fn a_body_that_is_not_a_zip_is_refused(ctx: TestContext, kv: Kv, db: Db, _storage: Storage) {
    let router = migrated_router(&db).await;
    let token = sign_in(&kv, seed_user(&db).await).await;
    let client = ctx.client(router);

    let response = client
        .post(&upload_path("not-a-zip", "claude"))
        .bearer(&token)
        .body("just some text")
        .send()
        .await;
    response.assert_status(422);
    assert!(response.json::<Problem>().kind.ends_with("invalid-skill"));
}

#[skyzen::test]
async fn a_name_no_directory_could_hold_is_refused(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    _storage: Storage,
) {
    let router = migrated_router(&db).await;
    let token = sign_in(&kv, seed_user(&db).await).await;
    let client = ctx.client(router);

    // The name becomes a directory under the harness's skills directory, so
    // a path separator would escape the prefix it is meant to live in.
    let response = client
        .post(&upload_path("..%2Fescape", "claude"))
        .bearer(&token)
        .body(BUNDLE)
        .send()
        .await;
    response.assert_status(422);
}

#[skyzen::test]
async fn a_sessions_daemon_sees_exactly_its_owners_skills(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    storage: Storage,
) {
    let router = migrated_router(&db).await;
    let user = seed_user(&db).await;
    let owner_token = sign_in(&kv, user.clone()).await;
    let stranger_token = sign_in(&kv, seed_other_user(&db).await).await;
    let session = seed_session(&db, &user).await;
    let daemon = daemon_tokens::issue(&db, user.id, session)
        .await
        .expect("mint a daemon token")
        .token;
    let client = ctx.client(router);

    // Two of the owner's, one of them a Codex bundle, and one of somebody
    // else's: the mount list carries the owner's whole registry because the
    // daemon, not the control plane, knows which harness it will run.
    for (name, scope) in [("release-notes", "claude"), ("triage", "codex")] {
        client
            .post(&upload_path(name, scope))
            .bearer(&owner_token)
            .body(BUNDLE)
            .send()
            .await
            .assert_status(201);
    }
    client
        .post(&upload_path("not-yours", "claude"))
        .bearer(&stranger_token)
        .body(BUNDLE)
        .send()
        .await
        .assert_status(201);

    let mounts: Vec<SkillMount> = client
        .get(&format!("/v1/sessions/{session}/skills"))
        .bearer(&daemon)
        .send()
        .await
        .json();

    let mut names: Vec<&str> = mounts.iter().map(|mount| mount.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["release-notes", "triage"]);
    assert_eq!(
        mounts.iter().map(|mount| mount.scope).collect::<Vec<_>>(),
        [SkillScope::Claude, SkillScope::Codex],
        "ordered by name"
    );
    assert!(
        mounts
            .iter()
            .all(|mount| mount.size_bytes == BUNDLE.len() as u64)
    );

    // The bundle route streams back the bytes that were uploaded.
    let release_notes = mounts
        .iter()
        .find(|mount| mount.name == "release-notes")
        .expect("the skill is mounted");
    let bundle = client
        .get(&format!(
            "/v1/sessions/{session}/skills/{}/bundle",
            release_notes.id
        ))
        .bearer(&daemon)
        .send()
        .await;
    bundle.assert_status(200);
    assert_eq!(bundle.body_bytes(), BUNDLE);

    // And it is stored under the id the mount list handed out.
    assert!(
        storage
            .get(&format!("skills/{}.zip", release_notes.id))
            .await
            .expect("read the bundle")
            .is_some()
    );
}

#[skyzen::test]
async fn a_daemon_cannot_read_another_users_bundle(
    ctx: TestContext,
    kv: Kv,
    db: Db,
    _storage: Storage,
) {
    let router = migrated_router(&db).await;
    let owner = seed_other_user(&db).await;
    let owner_token = sign_in(&kv, owner.clone()).await;
    let caller = seed_user(&db).await;
    let session = seed_session(&db, &caller).await;
    let daemon = daemon_tokens::issue(&db, caller.id, session)
        .await
        .expect("mint a daemon token")
        .token;
    let client = ctx.client(router);

    let theirs: SkillView = client
        .post(&upload_path("private", "claude"))
        .bearer(&owner_token)
        .body(BUNDLE)
        .send()
        .await
        .json();

    let refused = client
        .get(&format!(
            "/v1/sessions/{session}/skills/{}/bundle",
            theirs.id
        ))
        .bearer(&daemon)
        .send()
        .await;
    refused.assert_status(404);
    assert!(refused.json::<Problem>().kind.ends_with("skill-not-found"));

    // The session's own list does not leak it either.
    let mounts: Vec<SkillMount> = client
        .get(&format!("/v1/sessions/{session}/skills"))
        .bearer(&daemon)
        .send()
        .await
        .json();
    assert_eq!(mounts, Vec::new());
}
