//! Skills: the zipped bundles installed into every session's global skills
//! directory.
//!
//! The bundle is the request body, not a JSON field: a skill is a zip, and
//! base64 inside a document would double its size to no end. The name and
//! the harness it belongs to therefore ride the query string, which is what
//! keeps the body exactly the bytes that get stored.
//!
//! Agents cannot write either harness's skills directory — it is read-only
//! and a hook refuses the write — so an agent that wants to publish a skill
//! calls the `skill_upload` MCP tool, which lands on this same route.
//!
//! # Where a bundle lives
//!
//! The zip is object storage content and the row is its index: `skills` holds
//! the name, the scope and the size, and the bytes sit at
//! `skills/{id}.zip`. Claude Code and Codex read different global skills
//! directories, so [`SkillScope`] is what decides which one a bundle is
//! installed into.
//!
//! The machine pulls, nothing pushes: a session's daemon reads the owner's
//! mount list from `GET /v1/sessions/{id}/skills` and each bundle from
//! `GET /v1/sessions/{id}/skills/{skill}/bundle`, then unpacks them into
//! the harness's directory before the agent starts (see
//! `crates/daemon/src/skills.rs`). A live session picks a change up the
//! next time its machine starts, exactly as it does for MCP servers — a
//! skills directory is read when the harness launches, and swapping it
//! mid-turn would leave the model holding skills that no longer exist.
//!
//! The object is written before the row, and the row's `id` is reused when a
//! bundle of the same name and scope already exists. D1 has no transactions,
//! so one of the two orders has to be chosen: this one can leave an object no
//! row names, which is invisible; the other would leave a row naming an
//! object that is not there, which every reader would trip over.

use flyco_core::{CurrentUser, SkillId, SkillMount, SkillScope, SkillView, UserId};
use serde::Deserialize;
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Bytes, Json, State};
use skyzen_services::{Db, Storage};

use crate::clock::now_unix;
use crate::error::ApiError;
use crate::extract::path_id;
use crate::problem::Outcome;
use crate::respond::{Created, NoContent};

/// Prefix every skill bundle lives under in object storage.
const ROOT: &str = "skills";

/// Largest bundle flyco stores.
///
/// A skill is instructions and a few scripts. Ten mebibytes is far more than
/// any of them needs and small enough that a machine can materialize every
/// one of a user's skills during provisioning without the download becoming
/// the slow part of a session's start.
pub const MAX_BUNDLE_BYTES: usize = 10 * 1024 * 1024;

/// The four bytes every zip archive starts with (PKZIP local file header).
const ZIP_MAGIC: [u8; 4] = [b'P', b'K', 0x03, 0x04];

/// What an uploaded zip is stored as.
#[derive(Debug, Deserialize, skyzen::ToSchema)]
pub struct UploadSkill {
    /// Directory name the bundle is installed under.
    pub name: String,
    /// Which harness's skills directory it belongs in. Claude Code and Codex
    /// read different ones, so this is not derivable from the bundle.
    pub scope: SkillScope,
}

/// The columns every read on this path projects.
#[derive(Debug, skyzen::FromRow)]
struct SkillRow {
    id: SkillId,
    name: String,
    scope: SkillScope,
    size_bytes: u64,
    uploaded_at_unix: u64,
}

impl From<SkillRow> for SkillView {
    fn from(row: SkillRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            scope: row.scope,
            size_bytes: row.size_bytes,
            uploaded_at_unix: row.uploaded_at_unix,
        }
    }
}

impl From<SkillRow> for SkillMount {
    fn from(row: SkillRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            scope: row.scope,
            size_bytes: row.size_bytes,
        }
    }
}

/// The object key one bundle is stored under.
fn bundle_key(id: SkillId) -> String {
    format!("{ROOT}/{id}.zip")
}

/// Rejects a name that could not be a directory on a machine.
///
/// The name becomes a directory inside the harness's global skills
/// directory, so anything outside this set is either unaddressable or an
/// escape from the prefix it is meant to live in.
pub(crate) fn checked_name(name: &str) -> Result<String, ApiError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ApiError::InvalidSkill("a name is required"));
    }
    if trimmed.len() > 64 {
        return Err(ApiError::InvalidSkill("a name may hold at most 64 bytes"));
    }
    if !trimmed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ApiError::InvalidSkill(
            "a name may hold only letters, digits, dashes and underscores",
        ));
    }
    Ok(trimmed.to_owned())
}

/// Refuses a body that is not a bundle this control plane will store.
///
/// The archive is not unpacked here. A Worker is the wrong place to walk a
/// zip — the machine that installs it is the one that has to survive a
/// malformed entry, and it unpacks into a directory it owns — so this checks
/// the two things a store *can* answer for: that the body is a zip at all,
/// and that it is within the size a session's start can afford.
fn checked_bundle(body: &[u8]) -> Result<(), ApiError> {
    if body.len() > MAX_BUNDLE_BYTES {
        return Err(ApiError::InvalidSkill("the bundle is larger than 10 MiB"));
    }
    if !body.starts_with(&ZIP_MAGIC) {
        return Err(ApiError::InvalidSkill("the bundle is not a zip archive"));
    }
    Ok(())
}

/// Lists the caller's skills.
#[skyzen::openapi]
async fn list_skills(State(user): State<CurrentUser>, db: Db) -> Outcome<Json<Vec<SkillView>>> {
    list(&db, user.id).await.map(Json).into()
}

async fn list(db: &Db, user: UserId) -> Result<Vec<SkillView>, ApiError> {
    let rows: Vec<SkillRow> = sql!(
        db,
        "SELECT id, name, scope, size_bytes, uploaded_at_unix FROM skills \
         WHERE user_id = {user} ORDER BY uploaded_at_unix DESC, id"
    )
    .fetch_all()
    .await?;

    Ok(rows.into_iter().map(Into::into).collect())
}

/// Uploads a skill bundle, replacing any bundle of the same name and scope.
#[skyzen::openapi]
async fn upload_skill(
    State(user): State<CurrentUser>,
    Query(upload): Query<UploadSkill>,
    storage: Storage,
    db: Db,
    body: Bytes,
) -> Outcome<Created<Json<SkillView>>> {
    store(&db, &storage, user.id, &upload.name, upload.scope, &body)
        .await
        .map(|view| Created(Json(view)))
        .into()
}

/// Stores a bundle and upserts its index row.
///
/// Re-uploading a name keeps the id it already had, so the bundle it
/// replaces is overwritten rather than left behind: a machine materializes
/// skills by id, and two objects for one name would be two answers to the
/// same question.
pub(crate) async fn store(
    db: &Db,
    storage: &Storage,
    user: UserId,
    name: &str,
    scope: SkillScope,
    body: &[u8],
) -> Result<SkillView, ApiError> {
    let name = checked_name(name)?;
    checked_bundle(body)?;

    let existing: Option<SkillId> = sql!(
        db,
        "SELECT id FROM skills WHERE user_id = {user} AND scope = {scope} AND name = {name.as_str()}"
    )
    .fetch_scalar_optional()
    .await?;
    let id = existing.unwrap_or_else(SkillId::generate);

    storage.put(&bundle_key(id), body.to_vec()).await?;

    let size = body.len() as u64;
    let row: SkillRow = sql!(
        db,
        "INSERT INTO skills (id, user_id, name, scope, size_bytes, uploaded_at_unix) \
         VALUES ({id}, {user}, {name}, {scope}, {size}, {now_unix()}) \
         ON CONFLICT (user_id, scope, name) DO UPDATE SET \
         size_bytes = excluded.size_bytes, uploaded_at_unix = excluded.uploaded_at_unix \
         RETURNING id, name, scope, size_bytes, uploaded_at_unix"
    )
    .fetch_one()
    .await?;

    tracing::info!(skill = %row.id, name = %row.name, ?scope, bytes = size, "stored a skill bundle");
    Ok(row.into())
}

/// Describes one of the caller's skills.
#[skyzen::openapi]
async fn get_skill(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<Json<SkillView>> {
    read(&db, user.id, &params).await.map(Json).into()
}

async fn read(db: &Db, user: UserId, params: &Params) -> Result<SkillView, ApiError> {
    let id: SkillId = path_id(params, "id")?;
    Ok(load(db, user, id).await?.into())
}

/// Removes one of the caller's skills.
#[skyzen::openapi]
async fn delete_skill(
    State(user): State<CurrentUser>,
    params: Params,
    storage: Storage,
    db: Db,
) -> Outcome<NoContent> {
    remove(&db, &storage, user.id, &params).await.into()
}

/// Deletes the row, then the object it indexed.
///
/// This order is the reverse of the upload's, and for the same reason: the
/// state a failure can leave is an object nothing names, never a row naming
/// an object that is gone.
async fn remove(
    db: &Db,
    storage: &Storage,
    user: UserId,
    params: &Params,
) -> Result<NoContent, ApiError> {
    let id: SkillId = path_id(params, "id")?;
    let row = load(db, user, id).await?;

    sql!(
        db,
        "DELETE FROM skills WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;
    storage.delete(&bundle_key(id)).await?;

    tracing::info!(skill = %id, name = %row.name, "removed a skill bundle");
    Ok(NoContent)
}

/// Loads one of the caller's skills.
///
/// Scoped by user in the `WHERE` clause, so somebody else's skill is
/// indistinguishable from one that does not exist.
async fn load(db: &Db, user: UserId, id: SkillId) -> Result<SkillRow, ApiError> {
    sql!(
        db,
        "SELECT id, name, scope, size_bytes, uploaded_at_unix FROM skills \
         WHERE id = {id} AND user_id = {user}"
    )
    .fetch_optional()
    .await?
    .ok_or(ApiError::SkillNotFound)
}

/// The mount list a session's daemon installs, in the owner's name.
///
/// `GET /v1/sessions/{id}/skills` resolves the session to its owner and
/// calls this — the daemon's `fd_` token names a session, never a user, so
/// the owner is derived here rather than taken from the caller.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails.
pub async fn mounts(db: &Db, user: UserId) -> Result<Vec<SkillMount>, ApiError> {
    let rows: Vec<SkillRow> = sql!(
        db,
        "SELECT id, name, scope, size_bytes, uploaded_at_unix FROM skills \
         WHERE user_id = {user} ORDER BY name, id"
    )
    .fetch_all()
    .await?;

    Ok(rows.into_iter().map(Into::into).collect())
}

/// One bundle's bytes for a session's daemon, in the owner's name.
///
/// The row is read scoped to `user`, so a skill id from somebody else's
/// registry is a 404 like every other owner-scoped read — and so is a row
/// whose object is gone, which the store's write order (object first, then
/// row) can never produce.
///
/// # Errors
///
/// Returns [`ApiError::SkillNotFound`] if `id` is not one of `user`'s
/// skills, or [`ApiError`] if the database or the bucket fails.
pub async fn bundle(
    db: &Db,
    storage: &Storage,
    user: UserId,
    id: SkillId,
) -> Result<Vec<u8>, ApiError> {
    load(db, user, id).await?;
    let object = storage
        .get(&bundle_key(id))
        .await?
        .ok_or(ApiError::CorruptRecord(
            "a skills row names a bundle that is not stored",
        ))?;
    Ok(object.body)
}

/// The user-scoped skill routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/skills".at(list_skills).post(upload_skill),
        "/v1/skills/{id}".at(get_skill).delete(delete_skill),
    ))
    .into_route_nodes()
}
