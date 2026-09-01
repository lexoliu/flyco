//! Claude Code and Codex accounts, plus observed LLM usage.
//!
//! Credentials enter through an authenticated JSON request, are encoded with
//! their authentication mode, and are sealed before D1 sees them. No response
//! type contains the credential. A user has one credential per harness;
//! linking that harness again replaces it through the database's
//! `(user_id, harness)` uniqueness invariant.

use flyco_core::{
    CurrentUser, HarnessAccountId, HarnessAccountView, HarnessCredentialInput, HarnessKind,
    LinkHarnessAccount, LlmUsageView, UserId,
};
use flyco_provider::ClaudeCredential;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::extract::path_id;
use crate::observations;
use crate::problem::Outcome;
use crate::respond::{Created, NoContent};

/// Browser-visible account columns. The sealed credential is never selected.
#[derive(Debug, skyzen::FromRow)]
struct HarnessAccountRow {
    id: HarnessAccountId,
    harness: HarnessKind,
    label: String,
    linked_at_unix: u64,
    expires_at_unix: Option<u64>,
}

impl From<HarnessAccountRow> for HarnessAccountView {
    fn from(row: HarnessAccountRow) -> Self {
        Self {
            id: row.id,
            harness: row.harness,
            label: row.label,
            linked_at_unix: row.linked_at_unix,
            expires_at_unix: row.expires_at_unix,
        }
    }
}

/// Lists the caller's linked harness accounts.
#[skyzen::openapi]
async fn list_harness_accounts(
    State(user): State<CurrentUser>,
    db: Db,
) -> Outcome<Json<Vec<HarnessAccountView>>> {
    list(&db, user.id).await.map(Json).into()
}

async fn list(db: &Db, user: UserId) -> Result<Vec<HarnessAccountView>, ApiError> {
    let rows: Vec<HarnessAccountRow> = sql!(
        db,
        "SELECT id, harness, label, linked_at_unix, expires_at_unix \
         FROM harness_accounts WHERE user_id = {user} ORDER BY harness"
    )
    .fetch_all()
    .await?;

    Ok(rows.into_iter().map(Into::into).collect())
}

/// Links a harness credential, replacing the credential for that harness.
#[skyzen::openapi]
async fn link_harness_account(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    Json(request): Json<LinkHarnessAccount>,
    db: Db,
) -> Outcome<Created<Json<HarnessAccountView>>> {
    link(&db, &config, user.id, request)
        .await
        .map(|view| Created(Json(view)))
        .into()
}

fn validated(
    request: LinkHarnessAccount,
) -> Result<(String, HarnessKind, ClaudeCredential), ApiError> {
    let label = request.label.trim();
    if label.is_empty() {
        return Err(ApiError::InvalidHarnessCredential(
            "the account label must not be empty",
        ));
    }
    if request.credential.secret().trim().is_empty() {
        return Err(ApiError::InvalidHarnessCredential(
            "the credential must not be empty",
        ));
    }

    let harness = request.credential.harness();
    let credential = match request.credential {
        HarnessCredentialInput::ClaudeSetupToken { token } => ClaudeCredential::OauthToken {
            token: token.trim().to_owned(),
        },
        HarnessCredentialInput::ClaudeApiKey { key }
        | HarnessCredentialInput::CodexApiKey { key } => ClaudeCredential::ApiKey {
            key: key.trim().to_owned(),
        },
    };
    Ok((label.to_owned(), harness, credential))
}

async fn link(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    request: LinkHarnessAccount,
) -> Result<HarnessAccountView, ApiError> {
    let (label, harness, credential) = validated(request)?;
    let encoded = serde_json::to_string(&credential)
        .map_err(|_| ApiError::CorruptRecord("a harness credential could not be encoded"))?;
    let sealed = config.token_cipher().seal(&encoded)?;
    let id = HarnessAccountId::generate();
    let now = now_unix();
    let stored_label = label.clone();

    let stored_id: HarnessAccountId = sql!(
        db,
        "INSERT INTO harness_accounts \
         (id, user_id, harness, label, credential_enc, linked_at_unix, expires_at_unix) \
         VALUES ({id}, {user}, {harness}, {stored_label}, {sealed}, {now}, NULL) \
         ON CONFLICT (user_id, harness) DO UPDATE SET \
         label = excluded.label, credential_enc = excluded.credential_enc, \
         linked_at_unix = excluded.linked_at_unix, expires_at_unix = NULL \
         RETURNING id"
    )
    .fetch_scalar()
    .await?;

    tracing::info!(?harness, account = %stored_id, "linked a harness account");
    Ok(HarnessAccountView {
        id: stored_id,
        harness,
        label,
        linked_at_unix: now,
        expires_at_unix: None,
    })
}

/// Credential provisioned for a user's selected harness.
///
/// A missing link yields [`ClaudeCredential::Inherit`]. A stored value must
/// be a tagged credential compatible with the harness row; legacy untyped
/// tokens and mismatched records fail rather than being guessed into a mode.
///
/// # Errors
///
/// Returns [`ApiError`] when the database read, authenticated decryption, or
/// tagged credential decoding fails.
pub async fn credential(
    db: &Db,
    cipher: &crate::crypto::TokenCipher,
    user: UserId,
    harness: HarnessKind,
) -> Result<ClaudeCredential, ApiError> {
    let sealed: Option<String> = sql!(
        db,
        "SELECT credential_enc FROM harness_accounts \
         WHERE user_id = {user} AND harness = {harness}"
    )
    .fetch_scalar_optional()
    .await?;

    let Some(sealed) = sealed else {
        return Ok(ClaudeCredential::Inherit);
    };
    let encoded = cipher.open(&sealed)?;
    let credential: ClaudeCredential = serde_json::from_str(&encoded)
        .map_err(|_| ApiError::CorruptRecord("a harness credential has an unknown encoding"))?;

    match (&credential, harness) {
        (
            ClaudeCredential::OauthToken { .. } | ClaudeCredential::ApiKey { .. },
            HarnessKind::ClaudeCode,
        )
        | (ClaudeCredential::ApiKey { .. }, HarnessKind::Codex) => Ok(credential),
        (ClaudeCredential::Inherit | ClaudeCredential::OauthToken { .. }, HarnessKind::Codex)
        | (ClaudeCredential::Inherit, HarnessKind::ClaudeCode) => Err(ApiError::CorruptRecord(
            "a harness credential is incompatible with its account",
        )),
    }
}

/// Unlinks one account owned by the caller.
#[skyzen::openapi]
async fn unlink_harness_account(
    State(user): State<CurrentUser>,
    params: Params,
    db: Db,
) -> Outcome<NoContent> {
    unlink(&db, user.id, &params).await.into()
}

async fn unlink(db: &Db, user: UserId, params: &Params) -> Result<NoContent, ApiError> {
    let id: HarnessAccountId = path_id(params, "id")?;
    let removed = sql!(
        db,
        "DELETE FROM harness_accounts WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    if removed.rows_written == 0 {
        return Err(ApiError::HarnessAccountNotFound);
    }
    tracing::info!(account = %id, "unlinked a harness account");
    Ok(NoContent)
}

/// Reports observed usage for each linked harness account.
#[skyzen::openapi]
async fn llm_usage(State(user): State<CurrentUser>, db: Db) -> Outcome<Json<Vec<LlmUsageView>>> {
    observations::usage(&db, user.id).await.map(Json).into()
}

/// Authenticated harness-account routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/harness-accounts"
            .at(list_harness_accounts)
            .post(link_harness_account),
        "/v1/harness-accounts/{id}".delete(unlink_harness_account),
        "/v1/usage/llm".at(llm_usage),
    ))
    .into_route_nodes()
}
