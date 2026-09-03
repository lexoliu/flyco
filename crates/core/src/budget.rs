//! The compute-budget engine.
//!
//! Pure state machine, no I/O: the session's Durable Object owns a
//! [`BudgetState`] and feeds it [`SpendEvent`]s; the returned
//! [`BudgetSignal`]s are pushed to the daemon (injected into the harness
//! conversation) and to web-push.
//!
//! Semantics settled in the proposal discussion:
//! - notice at 50% spent, warning at 80%, final warning at 90%, hard pause
//!   at 100%;
//! - budgets under $5 stay quiet until 90% (only the final warning and the
//!   pause fire);
//! - budgets cover compute + storage, never LLM tokens.

use serde::{Deserialize, Serialize};

use crate::money::Usd;

/// Budget limits below this amount use the quiet signalling scheme
/// (final warning and pause only).
pub const SMALL_BUDGET_LIMIT: Usd = Usd::from_dollars(5);

/// Immutable configuration of a session budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BudgetConfig {
    limit: Usd,
}

/// Error constructing a [`BudgetConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BudgetConfigError {
    /// A zero budget cannot run anything and is rejected up front.
    #[error("budget limit must be greater than zero")]
    ZeroLimit,
}

impl BudgetConfig {
    /// Creates a budget configuration, rejecting a zero limit.
    ///
    /// # Errors
    ///
    /// Returns [`BudgetConfigError::ZeroLimit`] when `limit` is zero.
    pub fn new(limit: Usd) -> Result<Self, BudgetConfigError> {
        if limit == Usd::ZERO {
            return Err(BudgetConfigError::ZeroLimit);
        }
        Ok(Self { limit })
    }

    /// The spending limit.
    #[must_use]
    pub const fn limit(&self) -> Usd {
        self.limit
    }

    /// Whether this budget uses the quiet small-budget signalling scheme.
    #[must_use]
    pub fn is_small(&self) -> bool {
        self.limit < SMALL_BUDGET_LIMIT
    }
}

/// What a spend event paid for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum SpendKind {
    /// Machine time (on-demand or spot).
    Compute,
    /// Persistent disk kept for the session.
    Storage,
}

/// One accrual of cost against the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SpendEvent {
    /// What was paid for.
    pub kind: SpendKind,
    /// How much it cost.
    pub amount: Usd,
}

/// How far through the budget the session is. Monotonically increasing.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum BudgetStage {
    /// Below every threshold.
    Ok,
    /// Crossed 50% spent.
    Notice50,
    /// Crossed 80% spent.
    Warn80,
    /// Crossed 90% spent.
    Final90,
    /// Crossed 100%: the session must pause immediately.
    Exhausted,
}

impl BudgetStage {
    /// The stage implied by `spent` basis points of the limit.
    const fn for_basis_points(spent_bp: u64) -> Self {
        match spent_bp {
            0..5_000 => Self::Ok,
            5_000..8_000 => Self::Notice50,
            8_000..9_000 => Self::Warn80,
            9_000..10_000 => Self::Final90,
            _ => Self::Exhausted,
        }
    }

    /// How many thresholds this stage has crossed.
    ///
    /// The same scale [`BudgetSignal::ordinal`] counts on, so a stage and
    /// the signals it implies are comparable: a signal belongs to a budget
    /// standing at this stage exactly while its ordinal is no greater.
    /// That is what lets a raised limit drop the thresholds it un-crossed
    /// out of the delivery outbox, so re-reaching one fires it again.
    #[must_use]
    pub const fn ordinal(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Notice50 => 1,
            Self::Warn80 => 2,
            Self::Final90 => 3,
            Self::Exhausted => 4,
        }
    }
}

/// A signal the control plane must deliver when a threshold is crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub enum BudgetSignal {
    /// Half the budget is spent.
    Notice50,
    /// 80% of the budget is spent.
    Warn80,
    /// Final warning: 90% of the budget is spent.
    FinalWarn90,
    /// The budget is exhausted; the session pauses immediately.
    Pause,
}

impl BudgetSignal {
    /// Monotonic threshold order for durable delivery.
    #[must_use]
    pub const fn ordinal(self) -> u8 {
        match self {
            Self::Notice50 => 1,
            Self::Warn80 => 2,
            Self::FinalWarn90 => 3,
            Self::Pause => 4,
        }
    }

    /// The signal announcing entry into `stage`, if the scheme emits one.
    ///
    /// Small budgets (< $5) suppress the notice and the 80% warning.
    fn for_stage(stage: BudgetStage, config: BudgetConfig) -> Option<Self> {
        match stage {
            BudgetStage::Ok => None,
            BudgetStage::Notice50 => (!config.is_small()).then_some(Self::Notice50),
            BudgetStage::Warn80 => (!config.is_small()).then_some(Self::Warn80),
            BudgetStage::Final90 => Some(Self::FinalWarn90),
            BudgetStage::Exhausted => Some(Self::Pause),
        }
    }
}

/// Live budget accounting for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BudgetState {
    config: BudgetConfig,
    spent: Usd,
    stage: BudgetStage,
}

impl BudgetState {
    /// A fresh budget with nothing spent.
    #[must_use]
    pub const fn new(config: BudgetConfig) -> Self {
        Self {
            config,
            spent: Usd::ZERO,
            stage: BudgetStage::Ok,
        }
    }

    /// The configuration this state accounts against.
    #[must_use]
    pub const fn config(&self) -> &BudgetConfig {
        &self.config
    }

    /// Total spent so far.
    #[must_use]
    pub const fn spent(&self) -> Usd {
        self.spent
    }

    /// Budget remaining (zero once exhausted).
    #[must_use]
    pub const fn remaining(&self) -> Usd {
        self.config.limit().saturating_sub(self.spent)
    }

    /// The current stage.
    #[must_use]
    pub const fn stage(&self) -> BudgetStage {
        self.stage
    }

    /// Accrues a spend event and reports the signal to deliver, if the
    /// event crossed a threshold.
    ///
    /// When one event crosses several thresholds at once, only the signal
    /// for the highest newly-reached stage is emitted — a jump from 40% to
    /// 95% produces a single final warning, not a backlog of stale notices.
    /// The [`BudgetSignal::Pause`] signal is always emitted on exhaustion,
    /// regardless of budget size.
    #[must_use = "the returned signal must be delivered to the daemon and the user"]
    pub fn apply(&mut self, event: SpendEvent) -> Option<BudgetSignal> {
        self.spent += event.amount;
        let new_stage =
            BudgetStage::for_basis_points(self.spent.basis_points_of(self.config.limit));
        if new_stage <= self.stage {
            return None;
        }
        self.stage = new_stage;
        BudgetSignal::for_stage(new_stage, self.config)
    }
}

/// Budget accounting as the API serves it.
///
/// Every field is derived from replaying the spend ledger through
/// [`BudgetState`], so the API can never disagree with the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BudgetView {
    /// The spending limit.
    pub limit: Usd,
    /// Spent so far.
    pub spent: Usd,
    /// Left to spend; zero once exhausted.
    pub remaining: Usd,
    /// How far through the budget the session is.
    pub stage: BudgetStage,
}

impl From<BudgetState> for BudgetView {
    fn from(state: BudgetState) -> Self {
        Self {
            limit: state.config().limit(),
            spent: state.spent(),
            remaining: state.remaining(),
            stage: state.stage(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spend(dollars_micros: u64) -> SpendEvent {
        SpendEvent {
            kind: SpendKind::Compute,
            amount: Usd::from_micros(dollars_micros),
        }
    }

    fn state(limit: Usd) -> BudgetState {
        BudgetState::new(BudgetConfig::new(limit).expect("non-zero limit"))
    }

    #[test]
    fn zero_limit_is_rejected() {
        assert_eq!(
            BudgetConfig::new(Usd::ZERO),
            Err(BudgetConfigError::ZeroLimit)
        );
    }

    #[test]
    fn thresholds_fire_in_order_for_normal_budget() {
        let mut budget = state(Usd::from_dollars(10));
        assert_eq!(budget.apply(spend(4_900_000)), None); // 49%
        assert_eq!(budget.apply(spend(100_000)), Some(BudgetSignal::Notice50)); // 50%
        assert_eq!(budget.apply(spend(2_900_000)), None); // 79%
        assert_eq!(budget.apply(spend(100_000)), Some(BudgetSignal::Warn80)); // 80%
        assert_eq!(
            budget.apply(spend(1_000_000)),
            Some(BudgetSignal::FinalWarn90)
        ); // 90%
        assert_eq!(budget.apply(spend(999_999)), None); // 99.99…%
        assert_eq!(budget.apply(spend(1)), Some(BudgetSignal::Pause)); // 100%
        assert_eq!(budget.stage(), BudgetStage::Exhausted);
        assert_eq!(budget.remaining(), Usd::ZERO);
    }

    #[test]
    fn a_jump_emits_only_the_highest_signal() {
        let mut budget = state(Usd::from_dollars(10));
        assert_eq!(
            budget.apply(spend(9_500_000)),
            Some(BudgetSignal::FinalWarn90)
        );
        // Later smaller crossings below the reached stage stay silent.
        assert_eq!(budget.apply(spend(100_000)), None);
        assert_eq!(budget.apply(spend(400_000)), Some(BudgetSignal::Pause));
    }

    #[test]
    fn small_budget_stays_quiet_until_final_warning() {
        let mut budget = state(Usd::from_dollars(4));
        assert_eq!(budget.apply(spend(2_000_000)), None); // 50%
        assert_eq!(budget.apply(spend(1_200_000)), None); // 80%
        assert_eq!(budget.stage(), BudgetStage::Warn80); // stage advances silently
        assert_eq!(
            budget.apply(spend(400_000)),
            Some(BudgetSignal::FinalWarn90)
        ); // 90%
        assert_eq!(budget.apply(spend(400_000)), Some(BudgetSignal::Pause)); // 100%
    }

    #[test]
    fn a_stage_and_the_signal_announcing_it_count_the_same_thresholds() {
        // The outbox is pruned by comparing a signal's ordinal against the
        // stage a replay reached, which is only sound while the two scales
        // agree.
        let config = BudgetConfig::new(Usd::from_dollars(10)).expect("non-zero limit");
        for stage in [
            BudgetStage::Ok,
            BudgetStage::Notice50,
            BudgetStage::Warn80,
            BudgetStage::Final90,
            BudgetStage::Exhausted,
        ] {
            let announced = BudgetSignal::for_stage(stage, config);
            assert_eq!(
                announced.map(BudgetSignal::ordinal),
                (stage != BudgetStage::Ok).then_some(stage.ordinal()),
                "{stage:?}"
            );
        }
    }

    #[test]
    fn five_dollar_budget_is_not_small() {
        let config = BudgetConfig::new(Usd::from_dollars(5)).expect("non-zero limit");
        assert!(!config.is_small());
        let config = BudgetConfig::new(Usd::from_micros(4_999_999)).expect("non-zero limit");
        assert!(config.is_small());
    }

    #[test]
    fn overspend_beyond_limit_still_pauses_once() {
        let mut budget = state(Usd::from_dollars(1));
        assert_eq!(budget.apply(spend(3_000_000)), Some(BudgetSignal::Pause));
        assert_eq!(budget.apply(spend(1_000_000)), None); // already exhausted
        assert_eq!(budget.remaining(), Usd::ZERO);
    }

    #[test]
    fn exact_threshold_boundaries_belong_to_the_higher_stage() {
        let mut budget = state(Usd::from_dollars(10));
        assert_eq!(budget.apply(spend(5_000_000)), Some(BudgetSignal::Notice50));
        let mut budget = state(Usd::from_dollars(10));
        assert_eq!(budget.apply(spend(8_000_000)), Some(BudgetSignal::Warn80));
        let mut budget = state(Usd::from_dollars(10));
        assert_eq!(
            budget.apply(spend(9_000_000)),
            Some(BudgetSignal::FinalWarn90)
        );
        let mut budget = state(Usd::from_dollars(10));
        assert_eq!(budget.apply(spend(10_000_000)), Some(BudgetSignal::Pause));
    }
}
