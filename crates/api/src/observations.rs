//! The `harness_observations` table, and the panel built out of it.
//!
//! Every other usage figure in flyco is *read*: a budget is recomputed from
//! its ledger, cloud spend is queried from the provider that meters it. LLM
//! usage cannot be, because neither Anthropic nor `OpenAI` publishes a
//! remaining-quota API — so this table holds what flyco *saw*, one row per
//! observation a session's daemon made, and the panel is their sum.
//!
//! Which account an observation belongs to is derived here rather than
//! named by the caller. A daemon token resolves to a session, the session
//! names a user and a harness, and flyco holds at most one account per
//! harness per user — so the account is a join, and a daemon has no way to
//! post observations against somebody else's.

use flyco_core::{
    HarnessAccountId, HarnessKind, HarnessObservation, HarnessObservationId, LlmUsageView,
    OBSERVATION_WINDOW_SECONDS, SessionId, Usd, UserId,
};
use skyzen::sql;
use skyzen_services::Db;

use crate::clock::now_unix;
use crate::error::ApiError;

/// The account a session's observations are attributed to, and its owner.
#[derive(Debug, skyzen::FromRow)]
struct AttributionRow {
    id: HarnessAccountId,
    user_id: UserId,
}

/// One linked account, as the panel lists it before its numbers are added.
#[derive(Debug, skyzen::FromRow)]
struct AccountRow {
    id: HarnessAccountId,
    harness: HarnessKind,
    label: String,
}

/// The newest rate limit one account ran into.
#[derive(Debug, skyzen::FromRow)]
struct RateLimitRow {
    rate_limited_at_unix: u64,
    resets_at_unix: Option<u64>,
}

/// Records one observation a session's daemon made.
///
/// # Errors
///
/// Returns [`ApiError::EmptyObservation`] if the observation reports
/// nothing, [`ApiError::HarnessAccountNotFound`] if the session's user has
/// no account linked for the harness it runs — there is nothing to
/// attribute the observation to, and inventing an account to hold it would
/// be worse than saying so — or a database error.
pub async fn record(
    db: &Db,
    session: SessionId,
    observation: HarnessObservation,
) -> Result<(), ApiError> {
    if observation.is_empty() {
        return Err(ApiError::EmptyObservation);
    }
    let account = attribution(db, session).await?;

    let at = now_unix();
    // A rate limit is stamped with the moment flyco was told about it: the
    // harness reports that it *is* limited, not when it became limited.
    let rate_limited_at = observation.rate_limit.map(|_| at);
    let resets_at = observation
        .rate_limit
        .and_then(|limit| limit.resets_at_unix);

    sql!(
        db,
        "INSERT INTO harness_observations \
         (id, user_id, harness_account_id, session_id, observed_cost_micros, \
          rate_limited_at_unix, resets_at_unix, at_unix) \
         VALUES ({HarnessObservationId::generate()}, {account.user_id}, {account.id}, {session}, \
                 {observation.observed_cost}, {rate_limited_at}, {resets_at}, {at})"
    )
    .execute()
    .await?;

    tracing::debug!(%session, account = %account.id, "recorded a harness observation");
    Ok(())
}

/// The account a session's usage is attributed to.
///
/// The join is the scope check: a session reaches exactly the account its
/// own user linked for its own harness, so there is no account id on the
/// wire for a caller to substitute.
async fn attribution(db: &Db, session: SessionId) -> Result<AttributionRow, ApiError> {
    sql!(
        db,
        "SELECT a.id, a.user_id FROM harness_accounts a \
         JOIN sessions s ON s.user_id = a.user_id AND s.harness = a.harness \
         WHERE s.id = {session}"
    )
    .fetch_optional()
    .await?
    .ok_or(ApiError::HarnessAccountNotFound)
}

/// What flyco has observed of each of the caller's harness accounts.
///
/// One row per linked account, whether or not anything has been observed
/// about it: an account with no observations is a real answer — nothing has
/// happened — and dropping it would make the panel look like the account is
/// not linked.
///
/// # Errors
///
/// Returns [`ApiError`] if the database fails or a stored row is malformed.
pub async fn usage(db: &Db, user: UserId) -> Result<Vec<LlmUsageView>, ApiError> {
    let window_start = now_unix().saturating_sub(OBSERVATION_WINDOW_SECONDS);

    let accounts: Vec<AccountRow> = sql!(
        db,
        "SELECT id, harness, label FROM harness_accounts \
         WHERE user_id = {user} ORDER BY harness"
    )
    .fetch_all()
    .await?;

    // Read per account rather than in one grouped statement: the two numbers
    // are different shapes — a sum over a window, and the newest row of a
    // filtered subset — and a user holds at most one account per harness, so
    // this is a handful of statements rather than a page-walk.
    let mut panel = Vec::with_capacity(accounts.len());
    for account in accounts {
        let observed_cost = observed_cost(db, account.id, window_start).await?;
        let limit = last_rate_limit(db, account.id).await?;
        panel.push(LlmUsageView {
            account: account.id,
            harness: account.harness,
            label: account.label,
            period_start_unix: window_start,
            observed_cost,
            rate_limited_at_unix: limit.as_ref().map(|row| row.rate_limited_at_unix),
            resets_at_unix: limit.and_then(|row| row.resets_at_unix),
        });
    }
    Ok(panel)
}

/// What one account's turns cost over the window, as the harness reported
/// it.
///
/// `SUM` over no rows is `NULL`, which is exactly the answer: the harness
/// reported no cost, which is a different claim from "it cost nothing".
async fn observed_cost(
    db: &Db,
    account: HarnessAccountId,
    window_start: u64,
) -> Result<Option<Usd>, ApiError> {
    Ok(sql!(
        db,
        "SELECT SUM(observed_cost_micros) AS observed FROM harness_observations \
         WHERE harness_account_id = {account} AND at_unix >= {window_start}"
    )
    .fetch_scalar()
    .await?)
}

/// The last time one account ran into its limit, and when that reset.
///
/// Not narrowed to the window, unlike the cost: [`LlmUsageView`] says "when
/// this account last hit its usage limit, if it has", and hiding a limit
/// because it was yesterday would answer a different question.
///
/// Observations are stamped in whole seconds, so two can genuinely tie —
/// a session hits its limit on two turns at once. The tie goes to the
/// later reset time, because both limits are current and the one that runs
/// longest is the one still in force; an observation that named no reset
/// sorts last, since it makes the weakest claim.
async fn last_rate_limit(
    db: &Db,
    account: HarnessAccountId,
) -> Result<Option<RateLimitRow>, ApiError> {
    Ok(sql!(
        db,
        "SELECT rate_limited_at_unix, resets_at_unix FROM harness_observations \
         WHERE harness_account_id = {account} AND rate_limited_at_unix IS NOT NULL \
         ORDER BY rate_limited_at_unix DESC, resets_at_unix DESC LIMIT 1"
    )
    .fetch_optional()
    .await?)
}
