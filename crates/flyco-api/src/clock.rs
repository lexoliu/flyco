//! Wall-clock reads that work on both the Worker and native targets.
//!
//! Everything the control plane persists is timestamped in whole seconds
//! since the Unix epoch, so this is the entire surface.

/// Seconds since the Unix epoch.
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn now_unix() -> u64 {
    // `Date.now()` is a non-negative millisecond count; dividing by 1000 and
    // truncating stays inside `u64` for the next few hundred million years.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "Date.now() is a positive millisecond timestamp"
    )]
    let seconds = (js_sys::Date::now() / 1000.0) as u64;
    seconds
}

/// Seconds since the Unix epoch.
///
/// # Panics
///
/// Panics if the host clock is set before the Unix epoch — a broken machine
/// the control plane must not silently accommodate.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is set before the Unix epoch")
        .as_secs()
}
