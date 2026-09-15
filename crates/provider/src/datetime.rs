//! Turning an instant into the strings a provider's API asks for.
//!
//! Every one of these takes the instant as an argument rather than reading a
//! clock. The crate has exactly one wall clock ([`WallClock`]) and it lives
//! where a *signature* needs it; a billing window is a fact about the
//! caller's request, so the caller states it and the answer flyco reports
//! beside an amount is the window it actually asked for.
//!
//! [`WallClock`]: crate::clock::WallClock

use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime, Time};

use crate::ProviderError;

/// The instant, as a time the formatters can work from.
fn at(unix_seconds: u64) -> Result<OffsetDateTime, ProviderError> {
    i64::try_from(unix_seconds)
        .ok()
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
        .ok_or(ProviderError::Malformed(
            "a provider call named an instant outside the representable range",
        ))
}

/// The calendar month an instant falls in, as `(start, end)` Unix seconds.
///
/// A provider's month-to-date runs from midnight UTC on the first of the
/// current calendar month up to the moment the query was made, so the period
/// flyco reports beside an amount is the period the provider summed rather
/// than an approximation of it.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] if `now_unix` is not a time.
pub fn month_to_date(now_unix: u64) -> Result<(u64, u64), ProviderError> {
    let start = at(now_unix)?
        .replace_day(1)
        .map_err(|_| ProviderError::Malformed("every month has a first day"))?
        .replace_time(Time::MIDNIGHT)
        .unix_timestamp();

    let start = u64::try_from(start)
        .map_err(|_| ProviderError::Malformed("a billing month began before the Unix epoch"))?;
    Ok((start, now_unix))
}

/// The calendar year and month an instant falls in, as `(year, month)`.
///
/// GitHub's billing usage endpoint asks for the period as two query
/// parameters rather than as a range of dates, so this is what that
/// request is written with.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] if `unix_seconds` is not a time.
pub fn year_month(unix_seconds: u64) -> Result<(i32, u8), ProviderError> {
    let date = at(unix_seconds)?.date();
    Ok((date.year(), u8::from(date.month())))
}

/// The calendar date of an instant, as `YYYY-MM-DD`.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] if `unix_seconds` is not a time.
pub fn date(unix_seconds: u64) -> Result<String, ProviderError> {
    format_date(at(unix_seconds)?.date())
}

/// The calendar date of the day *after* an instant, as `YYYY-MM-DD`.
///
/// Cost Explorer's period end is exclusive, so today's spend is only inside
/// the window when the window ends tomorrow.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] if `unix_seconds` is not a time, or
/// if the day after it is not representable.
pub fn next_date(unix_seconds: u64) -> Result<String, ProviderError> {
    let tomorrow = at(unix_seconds)?
        .date()
        .next_day()
        .ok_or(ProviderError::Malformed(
            "a provider call named the last representable day",
        ))?;
    format_date(tomorrow)
}

/// A date as `YYYY-MM-DD`, which every AWS billing window is stated in.
fn format_date(date: Date) -> Result<String, ProviderError> {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    date.format(&format)
        .map_err(|_| ProviderError::Malformed("a date could not be formatted"))
}

/// An instant as RFC 3339, which is what EC2 timestamps are.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] if `unix_seconds` is not a time or
/// does not format.
pub fn rfc3339(unix_seconds: u64) -> Result<String, ProviderError> {
    at(unix_seconds)?
        .format(&Rfc3339)
        .map_err(|_| ProviderError::Malformed("an instant could not be formatted"))
}

#[cfg(test)]
mod tests {
    use super::{date, month_to_date, next_date, rfc3339};

    /// 2026-08-29T12:00:00Z.
    const QUERIED_AT: u64 = 1_788_004_800;

    /// 2026-08-01T00:00:00Z.
    const MONTH_START: u64 = 1_785_542_400;

    /// 2026-08-31T23:00:00Z, the last hour of a month.
    const MONTH_END: u64 = 1_788_217_200;

    #[test]
    fn a_billing_month_starts_at_midnight_on_the_first() {
        assert_eq!(
            month_to_date(QUERIED_AT).expect("a window"),
            (MONTH_START, QUERIED_AT)
        );
    }

    #[test]
    fn the_window_ends_where_the_query_did() {
        let (_, end) = month_to_date(MONTH_END).expect("a window");
        assert_eq!(end, MONTH_END);
    }

    #[test]
    fn a_date_and_the_day_after_it_are_both_stated_plainly() {
        assert_eq!(date(QUERIED_AT).expect("a date"), "2026-08-29");
        assert_eq!(next_date(QUERIED_AT).expect("a date"), "2026-08-30");
        // The day after the last of a month is the first of the next one,
        // which is what makes an exclusive end correct at a month boundary.
        assert_eq!(next_date(MONTH_END).expect("a date"), "2026-09-01");
    }

    #[test]
    fn an_instant_formats_as_rfc_3339() {
        assert_eq!(
            rfc3339(QUERIED_AT).expect("a timestamp"),
            "2026-08-29T12:00:00Z"
        );
    }
}
