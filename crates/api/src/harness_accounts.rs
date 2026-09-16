//! Claude Code and Codex accounts, plus observed LLM usage.
//!
//! Credentials enter through an authenticated JSON request — or through the
//! Claude OAuth flow in [`crate::claude_oauth`], which ends in the same
//! [`link`] — are encoded with their authentication mode, and are sealed
//! before D1 sees them. No response type contains the credential. A user has
//! one credential per harness; linking that harness again replaces it
//! through the database's `(user_id, harness)` uniqueness invariant.
//!
//! # A credential that expires
//!
//! Two stored modes have a lifetime, one per vendor: the Claude
//! subscription grant and the `ChatGPT` grant. Both are refreshed where
//! they are *used* rather than on a schedule — [`credential`] is the one
//! place a sealed credential is opened for a session, so a grant near its
//! end is rotated there, persisted, and handed on. A cron pass would have
//! to guess which accounts matter; this one refreshes exactly the accounts
//! that are about to run something.

use flyco_core::{
    CurrentUser, HarnessAccountId, HarnessAccountView, HarnessCredentialInput, HarnessKind,
    LinkHarnessAccount, LlmUsageView, ModelOption, UsageWindow, UserId, builtin_models,
    normalize_models,
};
use flyco_provider::{ClaudeCredential, CodexCredential, DevinCredential, HarnessCredential};
use serde::{Deserialize, Serialize};
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::sql;
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::anthropic::{ClaudeOauth as _, TokenRequest, TokenSet};
use crate::clock::now_unix;
use crate::config::ApiConfig;
use crate::devin::DevinApi as _;
use crate::error::ApiError;
use crate::extract::path_id;
use crate::observations;
use crate::openai::{self, CodexOauth as _, Grant};
use crate::problem::Outcome;
use crate::respond::{Created, NoContent};
use crate::sessions;
use crate::vendors::Vendors;

/// How close to its end an access token is refreshed before it is used.
///
/// Thirty minutes, because what happens after this call is a provision: a
/// machine takes minutes to boot and then runs a turn on the token it was
/// given. Handing over a credential that expires during that is the same
/// failure as handing over none.
pub const REFRESH_WINDOW_SECONDS: u64 = 30 * 60;

/// What `harness_accounts.credential_enc` holds, unsealed.
///
/// Tagged `mode`, and the two long-lived modes are spelled exactly as
/// [`ClaudeCredential`] spells them, because that is what the column has
/// always held. The OAuth grant is the shape that could not be stored as a
/// daemon credential: the daemon is handed a bearer token, while the
/// control plane has to keep the refresh token and the expiry that make the
/// next bearer token possible.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum StoredCredential {
    /// A long-lived Claude subscription token from `claude setup-token`.
    OauthToken {
        /// The access token the daemon provisions as `.credentials.json`.
        token: String,
    },
    /// An Anthropic or `OpenAI` API key.
    ApiKey {
        /// Value for `ANTHROPIC_API_KEY` or `OPENAI_API_KEY`.
        key: String,
    },
    /// A Claude subscription grant from the browser OAuth flow.
    ClaudeOauth {
        /// The bearer token a machine provisions as `.credentials.json`,
        /// until it expires.
        access_token: String,
        /// Redeemed for the next pair.
        refresh_token: String,
        /// When [`access_token`](Self::ClaudeOauth::access_token) stops
        /// working, seconds since the Unix epoch.
        expires_at_unix: u64,
    },
    /// A `ChatGPT` subscription grant from the Codex device-code flow.
    ///
    /// Four values rather than two, because Codex's own `auth.json` is four
    /// values: the daemon writes all of them, and the control plane keeps
    /// the refresh token that produces the next set.
    CodexOauth {
        /// The id token, which names the account and the workspace.
        id_token: String,
        /// The bearer token Codex runs under, until it expires.
        access_token: String,
        /// Redeemed for the next set.
        refresh_token: String,
        /// `chatgpt_account_id`, the workspace the grant belongs to.
        account_id: String,
        /// When [`access_token`](Self::CodexOauth::access_token) stops
        /// working, seconds since the Unix epoch.
        expires_at_unix: u64,
    },
}

impl core::fmt::Debug for StoredCredential {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mode = match self {
            Self::OauthToken { .. } => "oauth_token",
            Self::ApiKey { .. } => "api_key",
            Self::ClaudeOauth { .. } => "claude_oauth",
            Self::CodexOauth { .. } => "codex_oauth",
        };
        f.debug_struct("StoredCredential")
            .field("mode", &mode)
            .finish_non_exhaustive()
    }
}

impl StoredCredential {
    /// The grant a Claude token exchange just produced.
    #[must_use]
    pub fn from_tokens(tokens: &TokenSet, now: u64) -> Self {
        Self::ClaudeOauth {
            access_token: tokens.access_token.clone(),
            refresh_token: tokens.refresh_token.clone(),
            expires_at_unix: tokens.expires_at_unix(now),
        }
    }

    /// The grant a Codex device sign-in or refresh just produced.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError`] if the grant's tokens do not name the account
    /// or the moment the access token stops working — flyco stores neither
    /// a nameless account nor a grant it cannot schedule a renewal for.
    pub fn from_grant(grant: &Grant) -> Result<Self, ApiError> {
        Ok(Self::CodexOauth {
            id_token: grant.id_token.clone(),
            access_token: grant.access_token.clone(),
            refresh_token: grant.refresh_token.clone(),
            account_id: grant.account_id()?,
            expires_at_unix: grant.expires_at_unix()?,
        })
    }

    /// This credential's `ChatGPT` grant, when it is one.
    fn grant(&self) -> Option<Grant> {
        match self {
            Self::OauthToken { .. } | Self::ApiKey { .. } | Self::ClaudeOauth { .. } => None,
            Self::CodexOauth {
                id_token,
                access_token,
                refresh_token,
                ..
            } => Some(Grant {
                id_token: id_token.clone(),
                access_token: access_token.clone(),
                refresh_token: refresh_token.clone(),
            }),
        }
    }

    /// When this credential stops working, if the vendor states a lifetime.
    #[must_use]
    pub const fn expires_at_unix(&self) -> Option<u64> {
        match self {
            Self::OauthToken { .. } | Self::ApiKey { .. } => None,
            Self::ClaudeOauth {
                expires_at_unix, ..
            }
            | Self::CodexOauth {
                expires_at_unix, ..
            } => Some(*expires_at_unix),
        }
    }

    /// Whether this credential can authenticate `harness`.
    ///
    /// An API key suits either — the column has always held both — but a
    /// subscription grant belongs to exactly one vendor, and one on the
    /// other vendor's row is a corrupt record rather than a mode to try.
    const fn suits(&self, harness: HarnessKind) -> bool {
        match (self, harness) {
            (Self::ApiKey { .. }, _)
            | (Self::OauthToken { .. } | Self::ClaudeOauth { .. }, HarnessKind::ClaudeCode)
            | (Self::CodexOauth { .. }, HarnessKind::Codex) => true,
            (
                Self::OauthToken { .. } | Self::ClaudeOauth { .. },
                HarnessKind::Codex | HarnessKind::Devin,
            )
            | (Self::CodexOauth { .. }, HarnessKind::ClaudeCode | HarnessKind::Devin) => false,
        }
    }

    /// The credential a machine's `flycod` is provisioned with.
    ///
    /// A Claude OAuth grant becomes the bearer token the setup-token path
    /// already hands over, so the daemon has one Claude mode rather than
    /// two. A `ChatGPT` grant travels whole, because Codex's `auth.json`
    /// wants all of it.
    ///
    /// `harness` decides only the API-key case, which is the one mode both
    /// harnesses share; every other mode names its own vendor, and
    /// [`suits`](Self::suits) has already refused the pairs that disagree.
    fn into_daemon_credential(self, harness: HarnessKind) -> HarnessCredential {
        match (self, harness) {
            (Self::OauthToken { token }, _) => {
                HarnessCredential::ClaudeCode(ClaudeCredential::OauthToken { token })
            }
            (Self::ClaudeOauth { access_token, .. }, _) => {
                HarnessCredential::ClaudeCode(ClaudeCredential::OauthToken {
                    token: access_token,
                })
            }
            (Self::ApiKey { key }, HarnessKind::ClaudeCode) => {
                HarnessCredential::ClaudeCode(ClaudeCredential::ApiKey { key })
            }
            (Self::ApiKey { key }, HarnessKind::Codex) => {
                HarnessCredential::Codex(CodexCredential::ApiKey { key })
            }
            (Self::ApiKey { key }, HarnessKind::Devin) => {
                HarnessCredential::Devin(DevinCredential::ApiKey { key })
            }
            (
                Self::CodexOauth {
                    id_token,
                    access_token,
                    refresh_token,
                    account_id,
                    ..
                },
                _,
            ) => HarnessCredential::Codex(CodexCredential::ChatGpt {
                id_token,
                access_token,
                refresh_token,
                account_id,
            }),
        }
    }
}

/// Browser-visible account columns. The sealed credential is never selected.
#[derive(Debug, skyzen::FromRow)]
struct HarnessAccountRow {
    id: HarnessAccountId,
    harness: HarnessKind,
    label: String,
    linked_at_unix: u64,
    expires_at_unix: Option<u64>,
    /// The model list this account's last session reported, as JSON.
    ///
    /// `NULL` until one has, which is what [`builtin_models`] answers for.
    models_json: Option<String>,
    /// The plan-usage windows this account's last session reported, as JSON.
    ///
    /// `NULL` until one has, which reads back as an empty list — no
    /// session has asked the vendor, so there is nothing true to draw.
    usage_json: Option<String>,
}

impl TryFrom<HarnessAccountRow> for HarnessAccountView {
    type Error = ApiError;

    /// Fails rather than falling back to the built-in list when the stored
    /// JSON will not parse.
    ///
    /// The two states are opposite facts: no stored list means nothing has
    /// reported yet and the built-in one is the honest answer, and a stored
    /// list that does not parse means flyco wrote something it cannot read
    /// back. Answering the second with the first would hide the bug behind
    /// a picker that quietly showed the wrong models.
    fn try_from(row: HarnessAccountRow) -> Result<Self, ApiError> {
        Ok(Self {
            id: row.id,
            harness: row.harness,
            label: row.label,
            linked_at_unix: row.linked_at_unix,
            expires_at_unix: row.expires_at_unix,
            models: parse_models(row.harness, row.models_json.as_deref())?,
            usage: parse_usage(row.usage_json.as_deref())?,
        })
    }
}

/// The models an account offers: the stored list, or the built-in one,
/// normalized into the picker's shape — a harness like Devin whose ids
/// carry the effort is folded back into one row per model with the
/// levels on it, the same fold the daemon undoes when it sends a choice.
fn parse_models(harness: HarnessKind, stored: Option<&str>) -> Result<Vec<ModelOption>, ApiError> {
    let models = match stored {
        Some(stored) => serde_json::from_str(stored).map_err(|_| {
            ApiError::CorruptRecord("a stored harness model list could not be decoded")
        })?,
        None => builtin_models(harness),
    };
    Ok(normalize_models(harness, models))
}

/// The plan-usage windows an account last reported.
///
/// Fails rather than answering "nothing" when the stored JSON will not
/// parse, for the same reason [`parse_models`] does: no stored snapshot
/// means nobody has asked the vendor, and a snapshot flyco wrote and cannot
/// read back is a bug that would otherwise hide behind an empty row.
fn parse_usage(stored: Option<&str>) -> Result<Vec<UsageWindow>, ApiError> {
    let Some(stored) = stored else {
        return Ok(Vec::new());
    };
    serde_json::from_str(stored).map_err(|_| {
        ApiError::CorruptRecord("a stored harness usage snapshot could not be decoded")
    })
}

/// The models a session on the caller's account for `harness` may run on.
///
/// What `POST /v1/sessions` validates a requested model against, and what
/// it resolves a request that named none to. The account's own reported
/// list where one exists, because the harness build a machine runs is the
/// only authority on what it accepts.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails or the stored list is
/// malformed.
pub async fn models(
    db: &Db,
    user: UserId,
    harness: HarnessKind,
) -> Result<Vec<ModelOption>, ApiError> {
    let stored: Option<Option<String>> = sql!(
        db,
        "SELECT models_json FROM harness_accounts \
         WHERE user_id = {user} AND harness = {harness}"
    )
    .fetch_scalar_optional()
    .await?;
    // No row at all and a row that has never reported are the same answer:
    // nothing has told flyco otherwise, so the harness's built-in list
    // stands. A session cannot be opened without an account anyway — that
    // refusal belongs to provisioning, not to a model list.
    parse_models(harness, stored.flatten().as_deref())
}

/// The plan-usage windows the caller's account for `harness` last reported.
///
/// Empty until a session on it has asked its vendor, which is the honest
/// answer: nothing has been read, so nothing is drawn.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails or the stored snapshot is
/// malformed.
pub async fn usage(
    db: &Db,
    user: UserId,
    harness: HarnessKind,
) -> Result<Vec<UsageWindow>, ApiError> {
    let stored: Option<Option<String>> = sql!(
        db,
        "SELECT usage_json FROM harness_accounts \
         WHERE user_id = {user} AND harness = {harness}"
    )
    .fetch_scalar_optional()
    .await?;
    parse_usage(stored.flatten().as_deref())
}

/// Records what a session's harness said it offers.
///
/// Replaces the stored list wholesale: the harness answered the whole
/// question, and merging today's answer into yesterday's would keep a model
/// the vendor withdrew alive in the picker forever.
///
/// # Errors
///
/// Returns [`ApiError::CorruptRecord`] if `models` is empty — a harness
/// that lists nothing is a bug in the daemon, and storing it would leave
/// the account with a picker it can never open —
/// [`ApiError::HarnessAccountNotFound`] if the user has no account for that
/// harness, or [`ApiError`] if the database fails.
pub async fn record_models(
    db: &Db,
    user: UserId,
    harness: HarnessKind,
    models: &[ModelOption],
) -> Result<(), ApiError> {
    if models.is_empty() {
        return Err(ApiError::CorruptRecord(
            "a harness reported an empty model list",
        ));
    }
    let encoded = serde_json::to_string(models)
        .map_err(|_| ApiError::CorruptRecord("a harness model list could not be encoded"))?;
    // `RETURNING` rather than a bare `UPDATE`, so an account that is not
    // there is a refusal instead of a write that silently touched nothing
    // and a picker that quietly kept the built-in list. Unlinking is
    // refused while any session still runs on the harness, so a session
    // reporting its models always has an account to record them against.
    let stored: Option<HarnessAccountId> = sql!(
        db,
        "UPDATE harness_accounts SET models_json = {encoded} \
         WHERE user_id = {user} AND harness = {harness} RETURNING id"
    )
    .fetch_scalar_optional()
    .await?;
    let stored = stored.ok_or(ApiError::HarnessAccountNotFound)?;
    tracing::info!(
        ?harness,
        account = %stored,
        models = models.len(),
        "recorded a harness model list"
    );
    Ok(())
}

/// Records how much of this account's plan its harness says is spent.
///
/// Replaces the stored snapshot wholesale, like [`record_models`]: the
/// vendor answers the whole question every time, and a window merged out of
/// two answers would be a reading that was never true at any instant.
///
/// An empty list is a legitimate answer here — a session running on an API
/// key has no plan and no windows, so there is nothing to draw — and it is
/// stored as such rather than refused, so that an account that moves from a
/// subscription to a key stops showing yesterday's rings.
///
/// # Errors
///
/// Returns [`ApiError::HarnessAccountNotFound`] if the user has no account
/// for that harness, or [`ApiError`] if the database fails.
pub async fn record_usage(
    db: &Db,
    user: UserId,
    harness: HarnessKind,
    windows: &[UsageWindow],
) -> Result<(), ApiError> {
    let encoded = serde_json::to_string(windows)
        .map_err(|_| ApiError::CorruptRecord("a harness usage snapshot could not be encoded"))?;
    // `RETURNING`, like the model list beside it: an account that is not
    // there is a refusal rather than a write that silently touched nothing.
    let stored: Option<HarnessAccountId> = sql!(
        db,
        "UPDATE harness_accounts SET usage_json = {encoded} \
         WHERE user_id = {user} AND harness = {harness} RETURNING id"
    )
    .fetch_scalar_optional()
    .await?;
    let stored = stored.ok_or(ApiError::HarnessAccountNotFound)?;
    tracing::info!(
        ?harness,
        account = %stored,
        windows = windows.len(),
        "recorded a harness plan-usage snapshot"
    );
    Ok(())
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
        "SELECT id, harness, label, linked_at_unix, expires_at_unix, models_json, usage_json \
         FROM harness_accounts WHERE user_id = {user} ORDER BY harness"
    )
    .fetch_all()
    .await?;

    rows.into_iter().map(HarnessAccountView::try_from).collect()
}

/// Links a harness credential, replacing the credential for that harness.
#[skyzen::openapi]
async fn link_harness_account(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(vendors): State<Vendors>,
    Json(request): Json<LinkHarnessAccount>,
    db: Db,
) -> Outcome<Created<Json<HarnessAccountView>>> {
    link(&db, &config, &vendors, user.id, request)
        .await
        .map(|view| Created(Json(view)))
        .into()
}

fn validated(
    request: LinkHarnessAccount,
) -> Result<(Option<String>, HarnessKind, StoredCredential), ApiError> {
    let label = request
        .label
        .map(|label| label.trim().to_owned())
        .filter(|label| !label.is_empty());
    if request.credential.secret().trim().is_empty() {
        return Err(ApiError::InvalidHarnessCredential(
            "the credential must not be empty",
        ));
    }

    let harness = request.credential.harness();
    let credential = match request.credential {
        HarnessCredentialInput::ClaudeSetupToken { token } => StoredCredential::OauthToken {
            token: token.trim().to_owned(),
        },
        HarnessCredentialInput::ClaudeApiKey { key }
        | HarnessCredentialInput::CodexApiKey { key }
        | HarnessCredentialInput::DevinApiKey { key } => StoredCredential::ApiKey {
            key: key.trim().to_owned(),
        },
        HarnessCredentialInput::ClaudeOauth {
            access_token,
            refresh_token,
            expires_at_unix,
        } => {
            if refresh_token.trim().is_empty() {
                return Err(ApiError::InvalidHarnessCredential(
                    "an OAuth grant must carry the refresh token that renews it",
                ));
            }
            StoredCredential::ClaudeOauth {
                access_token: access_token.trim().to_owned(),
                refresh_token: refresh_token.trim().to_owned(),
                expires_at_unix,
            }
        }
        HarnessCredentialInput::CodexOauth {
            id_token,
            access_token,
            refresh_token,
            account_id,
            expires_at_unix,
        } => {
            if refresh_token.trim().is_empty() {
                return Err(ApiError::InvalidHarnessCredential(
                    "an OAuth grant must carry the refresh token that renews it",
                ));
            }
            if account_id.trim().is_empty() {
                return Err(ApiError::InvalidHarnessCredential(
                    "a ChatGPT grant must name the workspace it belongs to",
                ));
            }
            StoredCredential::CodexOauth {
                id_token: id_token.trim().to_owned(),
                access_token: access_token.trim().to_owned(),
                refresh_token: refresh_token.trim().to_owned(),
                account_id: account_id.trim().to_owned(),
                expires_at_unix,
            }
        }
    };
    Ok((label, harness, credential))
}

/// Seals a credential and writes it as the user's account for that harness.
///
/// The one write path: the JSON link route and the Claude OAuth completion
/// both end here, so "linking again replaces the credential" is one rule
/// rather than two implementations of one.
///
/// # Errors
///
/// Returns [`ApiError`] if the credential cannot be sealed or the database
/// refuses the write.
pub async fn store(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    label: &str,
    harness: HarnessKind,
    credential: &StoredCredential,
) -> Result<HarnessAccountView, ApiError> {
    let sealed = seal(config, credential)?;
    let id = HarnessAccountId::generate();
    let now = now_unix();
    let expires_at_unix = credential.expires_at_unix();

    let stored_id: HarnessAccountId = sql!(
        db,
        "INSERT INTO harness_accounts \
         (id, user_id, harness, label, credential_enc, linked_at_unix, expires_at_unix) \
         VALUES ({id}, {user}, {harness}, {label}, {sealed}, {now}, {expires_at_unix}) \
         ON CONFLICT (user_id, harness) DO UPDATE SET \
         label = excluded.label, credential_enc = excluded.credential_enc, \
         linked_at_unix = excluded.linked_at_unix, \
         expires_at_unix = excluded.expires_at_unix \
         RETURNING id"
    )
    .fetch_scalar()
    .await?;

    tracing::info!(?harness, account = %stored_id, "linked a harness account");
    Ok(HarnessAccountView {
        id: stored_id,
        harness,
        label: label.to_owned(),
        linked_at_unix: now,
        expires_at_unix,
        // Linking again replaces the credential and keeps the row, so a
        // relinked account keeps whatever its sessions have reported; a
        // fresh one has reported nothing and offers the built-in list.
        models: models(db, user, harness).await?,
        usage: usage(db, user, harness).await?,
    })
}

/// Encodes and seals one credential for the `credential_enc` column.
fn seal(config: &ApiConfig, credential: &StoredCredential) -> Result<String, ApiError> {
    let encoded = serde_json::to_string(credential)
        .map_err(|_| ApiError::CorruptRecord("a harness credential could not be encoded"))?;
    Ok(config.token_cipher().seal(&encoded)?)
}

/// What a linked Devin account is called when the principal names no
/// person — the same role [`claude_oauth`](crate::claude_oauth)'s
/// `UNNAMED_ACCOUNT` plays for a nameless Claude grant.
const UNNAMED_DEVIN_ACCOUNT: &str = "Devin account";

async fn link(
    db: &Db,
    config: &ApiConfig,
    vendors: &Vendors,
    user: UserId,
    request: LinkHarnessAccount,
) -> Result<HarnessAccountView, ApiError> {
    let (label, harness, credential) = validated(request)?;
    // A Devin key opens its principal at `/v3/self`, so the account is
    // labelled with Devin's own name for it — like the OAuth flows beside
    // it, where the label is the vendor's answer rather than the caller's.
    // The read doubles as the key's validation: a key Devin refuses never
    // reaches the table.
    let label = match &credential {
        StoredCredential::ApiKey { key } if harness == HarnessKind::Devin => vendors
            .devin
            .self_identity(key)
            .await?
            .account_name()
            .unwrap_or(UNNAMED_DEVIN_ACCOUNT)
            .to_owned(),
        _ => label.ok_or(ApiError::InvalidHarnessCredential(
            "the account label must not be empty",
        ))?,
    };
    store(db, config, user, &label, harness, &credential).await
}

/// The stored credential for one user's harness, or `None` if unlinked.
///
/// # Errors
///
/// Returns [`ApiError`] when the database read, authenticated decryption, or
/// tagged credential decoding fails, or when the stored mode cannot
/// authenticate the harness its row names.
pub async fn stored(
    db: &Db,
    config: &ApiConfig,
    user: UserId,
    harness: HarnessKind,
) -> Result<Option<StoredCredential>, ApiError> {
    let sealed: Option<String> = sql!(
        db,
        "SELECT credential_enc FROM harness_accounts \
         WHERE user_id = {user} AND harness = {harness}"
    )
    .fetch_scalar_optional()
    .await?;

    let Some(sealed) = sealed else {
        return Ok(None);
    };
    let encoded = config.token_cipher().open(&sealed)?;
    let credential: StoredCredential = serde_json::from_str(&encoded)
        .map_err(|_| ApiError::CorruptRecord("a harness credential has an unknown encoding"))?;

    if credential.suits(harness) {
        Ok(Some(credential))
    } else {
        Err(ApiError::CorruptRecord(
            "a harness credential is incompatible with its account",
        ))
    }
}

/// Credential provisioned for a user's selected harness.
///
/// A missing link yields [`ClaudeCredential::Inherit`]. A stored OAuth grant
/// within [`REFRESH_WINDOW_SECONDS`] of its expiry is renewed at Anthropic
/// first and the rotated pair is persisted, so the daemon is handed a token
/// that will outlive the provision — and so a user who linked once never
/// pastes anything again.
///
/// This is the single unsealing point for every session path — first
/// provision and resume alike, because both build their machine through the
/// same provisioning job — which is what makes refreshing here enough.
///
/// # Errors
///
/// Returns [`ApiError`] when the database read, authenticated decryption, or
/// tagged credential decoding fails, or when Anthropic refuses to renew a
/// grant that has to be renewed before it can be used.
pub async fn credential(
    db: &Db,
    config: &ApiConfig,
    vendors: &Vendors,
    user: UserId,
    harness: HarnessKind,
) -> Result<HarnessCredential, ApiError> {
    let Some(credential) = stored(db, config, user, harness).await? else {
        return Ok(HarnessCredential::inherit(harness));
    };

    let credential = if expiring_soon(&credential) {
        renewed(db, config, vendors, user, harness, &credential).await?
    } else {
        credential
    };
    Ok(credential.into_daemon_credential(harness))
}

/// Whether this credential is close enough to its end to renew first.
fn expiring_soon(credential: &StoredCredential) -> bool {
    credential
        .expires_at_unix()
        .is_some_and(|at| at <= now_unix().saturating_add(REFRESH_WINDOW_SECONDS))
}

/// Redeems the refresh token of whichever grant this is.
async fn renewed(
    db: &Db,
    config: &ApiConfig,
    vendors: &Vendors,
    user: UserId,
    harness: HarnessKind,
    credential: &StoredCredential,
) -> Result<StoredCredential, ApiError> {
    let renewed = match credential {
        StoredCredential::ClaudeOauth { refresh_token, .. } => {
            let tokens = vendors
                .claude
                .exchange(TokenRequest::RefreshToken {
                    refresh_token,
                    client_id: config.claude_oauth_client_id(),
                })
                .await
                .map_err(ApiError::from)?;
            tracing::info!("refreshed a Claude OAuth grant before using it");
            StoredCredential::from_tokens(&tokens, now_unix())
        }
        StoredCredential::CodexOauth { refresh_token, .. } => {
            let previous = credential
                .grant()
                .ok_or(ApiError::CorruptRecord("a ChatGPT grant lost its tokens"))?;
            let rotated = vendors
                .codex
                .exchange(openai::TokenRequest::RefreshToken {
                    refresh_token,
                    client_id: config.codex_oauth_client_id(),
                })
                .await
                .map_err(ApiError::from)?
                .rotated(&previous);
            tracing::info!("refreshed a ChatGPT grant before using it");
            StoredCredential::from_grant(&rotated)?
        }
        // Only a grant has an end, and only a grant reaches here.
        StoredCredential::OauthToken { .. } | StoredCredential::ApiKey { .. } => {
            return Err(ApiError::CorruptRecord(
                "a credential with no lifetime was scheduled for renewal",
            ));
        }
    };

    let sealed = seal(config, &renewed)?;
    let expires_at_unix = renewed.expires_at_unix();
    sql!(
        db,
        "UPDATE harness_accounts \
         SET credential_enc = {sealed}, expires_at_unix = {expires_at_unix} \
         WHERE user_id = {user} AND harness = {harness}"
    )
    .execute()
    .await?;

    Ok(renewed)
}

/// Unlinks one account owned by the caller.
///
/// Refused with `harness-account-in-use` while any session of the caller's
/// still runs on that harness: the account is what those sessions renew
/// their grant against, so unlinking it would break them at whatever moment
/// the token happened to expire. The problem document carries the count as
/// its `active_sessions` member.
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
    // Read before deleting: an account that is not the caller's has to be a
    // 404 rather than a `DELETE` that quietly writes nothing, and the
    // refusal below has to count sessions on the harness *this* account
    // drives rather than on every harness the user has linked.
    let harness: HarnessKind = sql!(
        db,
        "SELECT harness FROM harness_accounts WHERE id = {id} AND user_id = {user}"
    )
    .fetch_scalar_optional()
    .await?
    .ok_or(ApiError::HarnessAccountNotFound)?;

    // The credential is what a running session's grant is renewed against,
    // and it is renewed where it is used. Unlinking under a live session
    // therefore breaks it at a moment nobody chose — whenever the token
    // happens to expire — so it is refused while any session could still
    // ask for one.
    let running = sessions::live_on_harness(db, user, harness).await?;
    if running > 0 {
        return Err(ApiError::HarnessAccountInUse { sessions: running });
    }

    sql!(
        db,
        "DELETE FROM harness_accounts WHERE id = {id} AND user_id = {user}"
    )
    .execute()
    .await?;

    tracing::info!(account = %id, ?harness, "unlinked a harness account");
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
