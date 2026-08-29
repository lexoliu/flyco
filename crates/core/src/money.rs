//! Money, represented exactly.
//!
//! All amounts are US dollars stored as integer **microdollars**
//! (1 USD = `1_000_000` µ$). Integer arithmetic keeps budget accounting
//! exact; floats never enter the domain model.

use core::fmt;
use core::iter::Sum;
use core::ops::{Add, AddAssign, Sub};

use serde::{Deserialize, Serialize};

/// An exact, non-negative amount of US dollars.
///
/// Serialized as the integer number of microdollars.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[serde(transparent)]
#[schema(value_type = u64, description = "Amount in integer microdollars (1 USD = 1e6)")]
#[cfg_attr(feature = "sql", derive(skyzen::Column))]
pub struct Usd(u64);

impl Usd {
    /// Zero dollars.
    pub const ZERO: Self = Self(0);

    /// Constructs from integer microdollars.
    #[must_use]
    pub const fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    /// Constructs from whole dollars.
    #[must_use]
    pub const fn from_dollars(dollars: u64) -> Self {
        Self(dollars * 1_000_000)
    }

    /// Constructs from whole cents.
    #[must_use]
    pub const fn from_cents(cents: u64) -> Self {
        Self(cents * 10_000)
    }

    /// The amount in integer microdollars.
    #[must_use]
    pub const fn micros(self) -> u64 {
        self.0
    }

    /// Saturating subtraction: never goes below zero.
    #[must_use]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    /// The fraction `self / whole` in basis points (1/100 of a percent),
    /// rounded down.
    ///
    /// # Panics
    ///
    /// Panics if `whole` is zero — comparing spend against a zero budget is
    /// a caller bug, and [`crate::budget::BudgetConfig::new`] rejects zero
    /// limits up front.
    #[must_use]
    pub fn basis_points_of(self, whole: Self) -> u64 {
        assert!(whole.0 > 0, "fraction of a zero amount is undefined");
        (u128::from(self.0) * 10_000 / u128::from(whole.0))
            .try_into()
            .expect("basis points of amounts within u64 range fit u64")
    }
}

impl Add for Usd {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self(
            self.0
                .checked_add(rhs.0)
                .expect("USD amounts within budget scales never overflow u64 microdollars"),
        )
    }
}

impl AddAssign for Usd {
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

impl Sub for Usd {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self(
            self.0
                .checked_sub(rhs.0)
                .expect("USD subtraction must not underflow; use saturating_sub for clamping"),
        )
    }
}

impl Sum for Usd {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, Add::add)
    }
}

impl fmt::Display for Usd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dollars = self.0 / 1_000_000;
        let cents = (self.0 % 1_000_000) / 10_000;
        write!(f, "${dollars}.{cents:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_renders_dollars_and_cents() {
        assert_eq!(Usd::from_cents(1234).to_string(), "$12.34");
        assert_eq!(Usd::from_dollars(5).to_string(), "$5.00");
        assert_eq!(Usd::from_micros(15_000).to_string(), "$0.01");
    }

    #[test]
    fn basis_points_are_exact() {
        let half = Usd::from_dollars(5).basis_points_of(Usd::from_dollars(10));
        assert_eq!(half, 5_000);
        let all = Usd::from_dollars(10).basis_points_of(Usd::from_dollars(10));
        assert_eq!(all, 10_000);
        let over = Usd::from_dollars(15).basis_points_of(Usd::from_dollars(10));
        assert_eq!(over, 15_000);
    }

    #[test]
    #[should_panic(expected = "zero amount")]
    fn basis_points_of_zero_panics() {
        let _ = Usd::from_dollars(1).basis_points_of(Usd::ZERO);
    }
}
