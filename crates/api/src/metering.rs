//! Scheduled compute and persistent-storage budget accounting.
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

    for row in rows {
        if let Some(meter) = row.compute() {
            accrue_meter(db, &row, meter, at_unix).await?;
        }
        if let Some(meter) = row.storage() {
            accrue_meter(db, &row, meter, at_unix).await?;
        }
    }
    Ok(())
}

async fn accrue_meter(db: &Db, row: &MeterRow, meter: Meter, at_unix: u64) -> Result<(), ApiError> {
    let mut start = meter.cursor;
    let elapsed = at_unix.saturating_sub(meter.started_at);
    let target = meter
        .started_at
        .saturating_add(elapsed / WINDOW_SECONDS * WINDOW_SECONDS);
    while start < target {
        let end = start.saturating_add(WINDOW_SECONDS).min(target);
        let amount = interval_cost(meter.hourly, meter.started_at, start, end);
        let key = format!("{}:{:?}:{start}:{end}", row.machine_id, meter.kind);
        let detail = format!("machine {} from {start} through {end}", row.machine_id);
        if amount != Usd::ZERO {
            budgets::record_metered(db, row.budget_id, meter.kind, amount, &detail, end, &key)
                .await?;
        }
        advance_cursor(db, row.machine_id, meter.kind, end).await?;
        start = end;
    }
    Ok(())
}

fn interval_cost(hourly: Usd, origin: u64, start: u64, end: u64) -> Usd {
    let cumulative = |instant: u64| {
        u128::from(hourly.micros()) * u128::from(instant.saturating_sub(origin))
            / u128::from(SECONDS_PER_HOUR)
    };
    let micros = cumulative(end).saturating_sub(cumulative(start));
    Usd::from_micros(u64::try_from(micros).expect("a metering interval fits in microdollars"))
}

async fn advance_cursor(
    db: &Db,
    machine: MachineId,
    kind: SpendKind,
    end: u64,
) -> Result<(), ApiError> {
    match kind {
        SpendKind::Compute => {
            sql!(
                db,
                "UPDATE machines SET compute_metered_at_unix = {end} \
                 WHERE id = {machine} AND compute_metered_at_unix < {end}"
            )
            .execute()
            .await?;
        }
        SpendKind::Storage => {
            sql!(
                db,
                "UPDATE machines SET storage_metered_at_unix = {end} \
                 WHERE id = {machine} AND storage_metered_at_unix < {end}"
            )
            .execute()
            .await?;
        }
    }
    Ok(())
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

    use skyzen_services::Db;

    use skyzen::wasm_bindgen_futures;

    use super::{accrue, deliver};
    use crate::app;
    use crate::config::{ApiConfig, binding};
    use crate::rooms::Rooms;

    #[skyzen::scheduled]
    async fn budget_meter(
        event: skyzen_cloudflare::CfScheduledEvent,
        env: skyzen::runtime::wasm::Env,
        _context: skyzen_cloudflare::CfScheduleContext,
    ) -> Result<(), skyzen_cloudflare::CfEventError> {
        let millis = event.scheduled_time_ms()?;
        let at_unix = u64::try_from(millis / 1_000).map_err(|_| {
            skyzen_cloudflare::CfEventError::Runtime(
                "the scheduled budget meter received a time before the Unix epoch".to_owned(),
            )
        })?;
        let d1 = skyzen_cloudflare::CfD1::from_env(&env, binding::DATABASE)
            .map_err(|error| skyzen_cloudflare::CfEventError::Runtime(error.to_string()))?;
        let config = ApiConfig::from_worker_env(&env)
            .map_err(|error| skyzen_cloudflare::CfEventError::Runtime(error.to_string()))?;
        let db = Db::new(d1);
        let rooms = Rooms::from_worker_env(env);
        accrue(&db, at_unix)
            .await
            .map_err(|error| skyzen_cloudflare::CfEventError::Runtime(error.to_string()))?;
        deliver(&db, &rooms)
            .await
            .map_err(|error| skyzen_cloudflare::CfEventError::Runtime(error.to_string()))?;
        app::archive_idle(&db, &config, &rooms, at_unix)
            .await
            .map_err(|error| skyzen_cloudflare::CfEventError::Runtime(error.to_string()))
    }
}
