//! Scheduled compute and persistent-storage budget accounting, and the
//! sweep that keeps every linked account's catalog current.
//!
//! Meter windows are at most one minute and carry a deterministic unique key.
//! Cloudflare may overlap or retry cron invocations; two invocations that see
//! the same cursor therefore attempt the same first window, and the ledger's
//! unique key admits it once. Cost is calculated from a fixed meter start,
//! so sub-microdollar fractions carry across windows without mutable rounding
//! state.

use flyco_core::{BudgetId, BudgetSignal, MachineId, MachineState, SpendKind, Usd};
use skyzen::sql;
use skyzen_services::Db;

use crate::budgets;
use crate::error::ApiError;
use crate::rooms::Rooms;
use crate::sessions;

const SECONDS_PER_HOUR: u64 = 3_600;
const WINDOW_SECONDS: u64 = 60;

/// The most metering windows one sweep writes, across every meter it reads.
///
/// A cron event runs on a CPU budget: a machine whose meter went unmetered
/// for a day has over a thousand one-minute windows queued, and writing all
/// of them in one tick once burned the whole allowance before the sweep's
/// other legs — the ones that release stalled provisions and idle machines
/// — could run. The cursor only moves forward and each window's ledger key
/// is deterministic, so what a cap leaves behind is not lost work: the next
/// tick picks the meter up exactly where this one stopped.
const MAX_WINDOWS_PER_SWEEP: u64 = 600;

#[derive(Debug, skyzen::FromRow)]
struct MeterRow {
    machine_id: MachineId,
    budget_id: BudgetId,
    machine_state: MachineState,
    compute_hourly_micros: Option<Usd>,
    storage_hourly_micros: Option<Usd>,
    compute_meter_started_at_unix: Option<u64>,
    compute_metered_at_unix: Option<u64>,
    storage_meter_started_at_unix: Option<u64>,
    storage_metered_at_unix: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct Meter {
    kind: SpendKind,
    hourly: Usd,
    started_at: u64,
    cursor: u64,
}

impl MeterRow {
    fn compute(&self) -> Option<Meter> {
        (self.machine_state == MachineState::Running).then_some(Meter {
            kind: SpendKind::Compute,
            hourly: self.compute_hourly_micros?,
            started_at: self.compute_meter_started_at_unix?,
            cursor: self.compute_metered_at_unix?,
        })
    }

    fn storage(&self) -> Option<Meter> {
        (self.machine_state != MachineState::Destroyed).then_some(Meter {
            kind: SpendKind::Storage,
            hourly: self.storage_hourly_micros?,
            started_at: self.storage_meter_started_at_unix?,
            cursor: self.storage_metered_at_unix?,
        })
    }
}

/// Accrues every priced machine through `at_unix`.
///
/// # Errors
///
/// Returns [`ApiError`] if any ledger or cursor write fails. A failed cron is
/// retried by Cloudflare, and deterministic window keys make that retry safe.
pub async fn accrue(db: &Db, at_unix: u64) -> Result<(), ApiError> {
    let destroyed = MachineState::Destroyed;
    let rows: Vec<MeterRow> = sql!(
        db,
        "SELECT m.id AS machine_id, s.budget_id, m.state AS machine_state, \
         m.hourly_micros AS compute_hourly_micros, m.storage_hourly_micros, \
         m.compute_meter_started_at_unix, m.compute_metered_at_unix, \
         m.storage_meter_started_at_unix, m.storage_metered_at_unix \
         FROM machines m JOIN sessions s ON s.id = m.session_id \
         WHERE m.state != {destroyed}"
    )
    .fetch_all()
    .await?;

    let mut windows_left = MAX_WINDOWS_PER_SWEEP;
    let mut touched = std::collections::HashSet::new();
    'rows: for row in &rows {
        for meter in [row.compute(), row.storage()].into_iter().flatten() {
            if accrue_meter(db, row, meter, at_unix, &mut windows_left).await? {
                touched.insert(row.budget_id);
            }
            if windows_left == 0 {
                break 'rows;
            }
        }
    }

    // One replay per budget whose ledger the sweep appended to. Running it
    // per window instead would read back the whole ledger per event —
    // O(events) per window — which on a busy budget once spent the cron's
    // whole CPU allowance and starved every sweep leg queued behind accrual.
    for budget in touched {
        budgets::reconcile(db, budget).await?;
    }
    Ok(())
}

/// Drains one meter's pending windows into the ledger, at most
/// `windows_left` of them.
///
/// The window inserts and the cursor advance land in one atomic batch, so
/// the ledger and the meter never disagree about where the meter stands:
/// either the whole batch commits — ledger rows plus the cursor moved past
/// them — or the tick retries the meter from the same place, where the
/// keyed inserts are no-ops anyway.
///
/// Returns whether any spend was recorded — the signal the caller uses to
/// decide whose budget needs a reconcile.
async fn accrue_meter(
    db: &Db,
    row: &MeterRow,
    meter: Meter,
    at_unix: u64,
    windows_left: &mut u64,
) -> Result<bool, ApiError> {
    let mut start = meter.cursor;
    let elapsed = at_unix.saturating_sub(meter.started_at);
    let target = meter
        .started_at
        .saturating_add(elapsed / WINDOW_SECONDS * WINDOW_SECONDS);

    let mut statements = Vec::new();
    let mut recorded = false;
    while start < target && *windows_left > 0 {
        let end = start.saturating_add(WINDOW_SECONDS).min(target);
        let amount = interval_cost(meter.hourly, meter.started_at, start, end);
        if amount != Usd::ZERO {
            let key = format!("{}:{:?}:{start}:{end}", row.machine_id, meter.kind);
            let detail = format!("machine {} from {start} through {end}", row.machine_id);
            statements.push(budgets::metered_statement(
                row.budget_id,
                meter.kind,
                amount,
                &detail,
                end,
                &key,
            ));
            recorded = true;
        }
        start = end;
        *windows_left -= 1;
    }
    if start == meter.cursor {
        return Ok(false);
    }
    statements.push(cursor_statement(row.machine_id, meter.kind, start));
    db.execute_batch(statements).await.map_err(ApiError::from)?;
    Ok(recorded)
}

/// The cursor advance one meter needs after its batched windows.
///
/// A cursor only ever moves forward — `AND metered_at < end` — so a batch
/// replayed after a crash cannot walk it backwards over windows a later
/// batch already covered.
fn cursor_statement(
    machine: MachineId,
    kind: SpendKind,
    end: u64,
) -> skyzen_services::BatchStatement {
    let column = match kind {
        SpendKind::Compute => "compute_metered_at_unix",
        SpendKind::Storage => "storage_metered_at_unix",
    };
    skyzen_services::BatchStatement::new(format!(
        "UPDATE machines SET {column} = ? WHERE id = ? AND {column} < ?"
    ))
    .bind(end)
    .bind(machine)
    .bind(end)
}

fn interval_cost(hourly: Usd, origin: u64, start: u64, end: u64) -> Usd {
    let cumulative = |instant: u64| {
        u128::from(hourly.micros()) * u128::from(instant.saturating_sub(origin))
            / u128::from(SECONDS_PER_HOUR)
    };
    let micros = cumulative(end).saturating_sub(cumulative(start));
    Usd::from_micros(u64::try_from(micros).expect("a metering interval fits in microdollars"))
}

/// Delivers the durable budget-signal outbox to session rooms.
///
/// A pause is written to the session table before the daemon sees it. If the
/// room call fails, the outbox row remains pending and the next cron repeats
/// the idempotent state transition and command.
///
/// # Errors
///
/// Returns [`ApiError`] if the outbox, session state, or room command fails.
pub async fn deliver(db: &Db, rooms: &Rooms) -> Result<(), ApiError> {
    for pending in budgets::pending_signals(db).await? {
        if pending.signal == BudgetSignal::Pause {
            sessions::pause_for_budget(db, pending.session_id).await?;
        }
        rooms
            .command(
                db,
                pending.session_id,
                &flyco_core::ControlToDaemon::Budget {
                    signal: pending.signal,
                },
            )
            .await?;
        budgets::mark_delivered(db, pending.id).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::interval_cost;
    use flyco_core::Usd;

    #[test]
    fn adjacent_windows_preserve_fractional_microdollars() {
        let hourly = Usd::from_micros(101);
        let first = interval_cost(hourly, 10, 10, 70);
        let second = interval_cost(hourly, 10, 70, 130);
        assert_eq!(first + second, interval_cost(hourly, 10, 10, 130));
    }
}

#[cfg(target_arch = "wasm32")]
mod worker {
    #![allow(
        missing_docs,
        reason = "`#[skyzen::scheduled]` generates the exported wrapper, docs and all"
    )]

    use skyzen_services::{Db, Kv, Queue};

    use skyzen::wasm_bindgen_futures;

    use super::{accrue, deliver};
    use crate::app;
    use crate::catalog;
    use crate::codespaces::{Codespaces, LiveCodespaces};
    use crate::config::{ApiConfig, binding};
    use crate::github::GithubClient;
    use crate::rooms::{HostRooms, Rooms};
    use crate::usage_limits;

    #[skyzen::scheduled]
    async fn budget_meter(
        event: skyzen_cloudflare::CfScheduledEvent,
        env: skyzen::runtime::wasm::Env,
        _context: skyzen_cloudflare::CfScheduleContext,
    ) -> Result<(), skyzen_cloudflare::CfEventError> {
        // A fresh isolate may see a queue or cron event before any request,
        // and without this it would log nothing of what it did.
        crate::telemetry::install();
        tracing::debug!("scheduled sweep: handler entered");
        let millis = event.scheduled_time_ms().inspect_err(|error| {
            tracing::error!(%error, "scheduled sweep: scheduled_time_ms failed");
        })?;
        let at_unix = u64::try_from(millis / 1_000).map_err(|_| {
            skyzen_cloudflare::CfEventError::Runtime(
                "the scheduled budget meter received a time before the Unix epoch".to_owned(),
            )
        })?;
        let d1 = skyzen_cloudflare::CfD1::from_env(&env, binding::DATABASE).map_err(|error| {
            tracing::error!(%error, "scheduled sweep: D1 binding failed");
            skyzen_cloudflare::CfEventError::Runtime(error.to_string())
        })?;
        let config = ApiConfig::from_worker_env(&env).map_err(|error| {
            tracing::error!(%error, "scheduled sweep: ApiConfig failed");
            skyzen_cloudflare::CfEventError::Runtime(error.to_string())
        })?;
        let db = Db::new(d1);
        // One environment, two namespaces: archiving a session releases its
        // machine, and a machine the user owns is released by asking the
        // machine.
        let env_for_services = env.clone();
        let wasm = skyzen::runtime::wasm::WasmEnv::new(env);
        let rooms = Rooms::from_wasm_env(wasm.clone());
        let hosts = HostRooms::from_wasm_env(wasm);
        // The catalog sweep rides this cron rather than adding a second
        // one: a Worker has one scheduled handler, and reading what every
        // linked account can deploy is exactly the periodic background work
        // this trigger exists for.
        let kv = skyzen_cloudflare::CfKv::from_env(&env_for_services, binding::AUTH_KV).map_err(
            |error| {
                tracing::error!(%error, "scheduled sweep: KV binding failed");
                skyzen_cloudflare::CfEventError::Runtime(error.to_string())
            },
        )?;
        let queue = skyzen_cloudflare::CfQueue::from_env(&env_for_services, binding::PROVISIONING)
            .map_err(|error| {
                tracing::error!(%error, "scheduled sweep: queue binding failed");
                skyzen_cloudflare::CfEventError::Runtime(error.to_string())
            })?;
        // Two handles onto one binding: the catalog sweep takes ownership of
        // its producer, and the usage-limit sweep enqueues the job that starts
        // a waiting session's machine again.
        let queue_for_waking = Queue::new(
            skyzen_cloudflare::CfQueue::from_env(&env_for_services, binding::PROVISIONING)
                .map_err(|error| skyzen_cloudflare::CfEventError::Runtime(error.to_string()))?,
        );

        // Each leg is named on the way in and on the way out: a leg that
        // returns `Err` says so with its name attached, and a leg that dies
        // on a runtime exception — which never reaches a `tracing` call —
        // is still identifiable as the last "running" marker before the
        // event ends.
        async fn leg<E: core::fmt::Display>(
            name: &'static str,
            run: impl Future<Output = Result<(), E>>,
        ) -> Result<(), skyzen_cloudflare::CfEventError> {
            tracing::debug!(leg = name, "scheduled sweep running");
            run.await.map_err(|error| {
                tracing::error!(leg = name, %error, "scheduled sweep leg failed");
                skyzen_cloudflare::CfEventError::Runtime(error.to_string())
            })
        }

        // One client for every leg that unseals a GitHub grant — the
        // codespaces reconcile, the machine releases — because a stored
        // grant near its end is renewed before it is used.
        let github = GithubClient::default();

        leg("accrue", accrue(&db, at_unix)).await?;
        leg("deliver", deliver(&db, &rooms)).await?;
        leg(
            "refresh_stale",
            catalog::refresh_stale(&db, &Kv::new(kv), &Queue::new(queue), at_unix),
        )
        .await?;
        leg(
            "fail_stalled_provisions",
            app::fail_stalled_provisions(&db, &config, &github, &rooms, &hosts, at_unix),
        )
        .await?;
        leg(
            "archive_idle",
            app::archive_idle(&db, &config, &github, &rooms, &hosts, at_unix),
        )
        .await?;
        // The clock behind issue #244: a session waiting out a spent harness
        // plan window is released, woken and continued from here. It rides
        // this cron rather than a delayed queue message because Cloudflare
        // Queues cap delivery delay at twelve hours and a weekly window
        // resets further out than that.
        leg(
            "usage_limits",
            usage_limits::sweep(
                &db,
                &config,
                &github,
                &rooms,
                &hosts,
                &queue_for_waking,
                at_unix,
            ),
        )
        .await?;
        // A codespace's state changes underneath flyco — GitHub stops it on
        // its own idle clock, a user deletes it from github.com — and the
        // only truth about either is asking, once a minute, of every
        // codespace the rows believe they hold.
        leg(
            "codespaces_reconcile",
            crate::codespaces::reconcile(
                &db,
                &config,
                &github,
                &rooms,
                &Codespaces::Live(LiveCodespaces::new()),
            ),
        )
        .await?;
        // What GitHub does for a codespace on its own clock, flyco does for
        // every other machine on this one: an active session that has said
        // nothing for thirty minutes has its compute released and its disk
        // kept, and the next message starts it again. After the reconcile,
        // so a codespace GitHub already suspended is classified by it
        // rather than asked about twice.
        leg(
            "suspend_idle",
            app::suspend_idle(&db, &config, &github, &rooms, &hosts, at_unix),
        )
        .await?;
        // Last, so it sees what the sweeps above have just ended.
        leg(
            "release_ended_machines",
            app::release_ended_machines(&db, &config, &github, &hosts),
        )
        .await
    }
}
