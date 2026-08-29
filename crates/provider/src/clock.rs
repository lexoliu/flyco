//! Time: a clock that only ever moves forward, a clock that says what time
//! it is, and a way to wait.
//!
//! An OAuth token is cached against elapsed time, never against wall-clock
//! time: the Worker's `Date.now()` can jump backwards when the host's clock
//! is corrected, and a token cache that reads a jump as "not expired yet"
//! keeps presenting a dead credential. Elapsed seconds since the driver was
//! built is all the cache needs, and it cannot go backwards.
//!
//! [`WallClock`] is the separate, narrower thing: what time it is *now*, in
//! seconds since the Unix epoch. Two signatures genuinely need it and no
//! amount of monotonic time will do — an AWS `SigV4` signature is computed over
//! an `X-Amz-Date` the service checks against its own clock, and a Google
//! service-account assertion carries `iat`/`exp` claims. It is a trait rather
//! than a call to the host because the alternative is a driver whose
//! signature cannot be asserted: with the instant supplied, a recorded test
//! pins the exact canonical request and the exact signature.
//!
//! Tests drive [`ManualClock`], [`ManualWallClock`] and
//! `crate::testing::RecordingTimer`, which make token expiry, a signing time
//! and a `Retry-After` into assertions rather than into sleeps.

/// Something that can pause an async task.
///
/// Separate from [`MonotonicClock`] because the two are needed in different
/// places — a token cache reads time, a polling loop waits — and because a
/// test wants a clock it advances by hand *and* a timer that returns
/// immediately while recording what it was asked to wait for.
pub trait Timer {
    /// Waits approximately `seconds`.
    fn sleep(&self, seconds: u32) -> impl Future<Output = ()>;
}

/// The host's timer: `futures-timer`, which is a real timer natively and
/// `setTimeout` on the Worker.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemTimer;

impl SystemTimer {
    /// Creates the timer.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Timer for SystemTimer {
    fn sleep(&self, seconds: u32) -> impl Future<Output = ()> {
        futures_timer::Delay::new(core::time::Duration::from_secs(u64::from(seconds)))
    }
}

/// A source of monotonically non-decreasing elapsed seconds.
pub trait MonotonicClock {
    /// Seconds elapsed since some fixed, unspecified origin.
    ///
    /// Only differences between two reads are meaningful.
    fn elapsed_seconds(&self) -> u64;
}

/// The host's monotonic clock: `Instant` natively, `performance.now()` on
/// the Worker.
#[derive(Debug, Clone)]
pub struct SystemClock {
    #[cfg(not(target_arch = "wasm32"))]
    origin: std::time::Instant,
    #[cfg(target_arch = "wasm32")]
    origin_millis: f64,
}

impl SystemClock {
    /// Starts a clock at the current instant.
    #[must_use]
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new() -> Self {
        Self {
            origin: std::time::Instant::now(),
        }
    }

    /// Starts a clock at the current instant.
    ///
    /// `performance.now()` is a monotonic millisecond counter that a clock
    /// correction does not move; `Date.now()` is not, which is why it is not
    /// used here.
    #[must_use]
    #[cfg(target_arch = "wasm32")]
    pub fn new() -> Self {
        Self {
            origin_millis: performance_now_millis(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_arch = "wasm32")]
fn performance_now_millis() -> f64 {
    // Workers expose `performance` on the global scope. Where it is absent
    // the value below stays constant, which makes every cached token look
    // freshly issued — so the fallback is a hard failure instead.
    js_sys::Reflect::get(
        &js_sys::global(),
        &wasm_bindgen::JsValue::from_str("performance"),
    )
    .ok()
    .and_then(|performance| {
        js_sys::Reflect::get(&performance, &wasm_bindgen::JsValue::from_str("now"))
            .ok()
            .and_then(|now| js_sys::Function::from(now).call0(&performance).ok())
            .and_then(|value| value.as_f64())
    })
    .expect("the Workers runtime exposes a monotonic `performance.now()`")
}

impl MonotonicClock for SystemClock {
    #[cfg(not(target_arch = "wasm32"))]
    fn elapsed_seconds(&self) -> u64 {
        self.origin.elapsed().as_secs()
    }

    #[cfg(target_arch = "wasm32")]
    fn elapsed_seconds(&self) -> u64 {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "an elapsed millisecond count is non-negative and far inside u64"
        )]
        let seconds = ((performance_now_millis() - self.origin_millis) / 1000.0) as u64;
        seconds
    }
}

/// A source of wall-clock seconds since the Unix epoch.
///
/// Deliberately separate from [`MonotonicClock`]: the two answer different
/// questions and only one of them is allowed to move backwards. A driver
/// takes this only when a *signature* depends on the current time, which is
/// the one thing elapsed seconds cannot stand in for.
pub trait WallClock {
    /// Seconds since 1970-01-01T00:00:00Z.
    fn unix_seconds(&self) -> u64;
}

/// The host's wall clock: `SystemTime` natively, `Date.now()` on the Worker.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemWallClock;

impl SystemWallClock {
    /// Creates the clock.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl WallClock for SystemWallClock {
    #[cfg(not(target_arch = "wasm32"))]
    fn unix_seconds(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }

    /// `Date.now()`, which is the Worker's only source of civil time.
    ///
    /// Its backwards jumps are exactly why a token cache reads
    /// [`MonotonicClock`] instead; a signature has no such option, because
    /// the service checks the stated time against its own.
    #[cfg(target_arch = "wasm32")]
    fn unix_seconds(&self) -> u64 {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a Unix timestamp in seconds is non-negative and far inside u64"
        )]
        let seconds = (js_sys::Date::now() / 1000.0) as u64;
        seconds
    }
}

/// A wall clock stopped at an instant a test chose.
#[derive(Debug)]
pub struct ManualWallClock {
    at: core::cell::Cell<u64>,
}

impl ManualWallClock {
    /// A clock reading `unix_seconds` until it is told otherwise.
    #[must_use]
    pub const fn at(unix_seconds: u64) -> Self {
        Self {
            at: core::cell::Cell::new(unix_seconds),
        }
    }

    /// Moves the clock forward.
    pub fn advance(&self, seconds: u64) {
        self.at.set(self.at.get() + seconds);
    }
}

impl WallClock for ManualWallClock {
    fn unix_seconds(&self) -> u64 {
        self.at.get()
    }
}

impl<T: WallClock> WallClock for &T {
    fn unix_seconds(&self) -> u64 {
        (*self).unix_seconds()
    }
}

/// A clock a test advances by hand.
#[derive(Debug, Default)]
pub struct ManualClock {
    elapsed: core::cell::Cell<u64>,
}

impl ManualClock {
    /// Starts at zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            elapsed: core::cell::Cell::new(0),
        }
    }

    /// Moves the clock forward.
    pub fn advance(&self, seconds: u64) {
        self.elapsed.set(self.elapsed.get() + seconds);
    }
}

impl MonotonicClock for ManualClock {
    fn elapsed_seconds(&self) -> u64 {
        self.elapsed.get()
    }
}

impl<T: MonotonicClock> MonotonicClock for &T {
    fn elapsed_seconds(&self) -> u64 {
        (*self).elapsed_seconds()
    }
}

#[cfg(test)]
mod tests {
    use super::{ManualClock, ManualWallClock, MonotonicClock, SystemClock, WallClock};

    #[test]
    fn a_manual_wall_clock_reads_the_instant_it_was_given() {
        let clock = ManualWallClock::at(1_788_004_800);
        assert_eq!(clock.unix_seconds(), 1_788_004_800);
        clock.advance(60);
        assert_eq!(clock.unix_seconds(), 1_788_004_860);
    }

    #[test]
    fn a_manual_clock_only_moves_when_told_to() {
        let clock = ManualClock::new();
        assert_eq!(clock.elapsed_seconds(), 0);
        clock.advance(3_600);
        assert_eq!(clock.elapsed_seconds(), 3_600);
    }

    #[test]
    fn the_system_clock_starts_at_its_own_origin() {
        assert_eq!(SystemClock::new().elapsed_seconds(), 0);
    }
}
