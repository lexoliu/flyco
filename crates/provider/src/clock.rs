//! A clock that only ever moves forward.
//!
//! An OAuth token is cached against elapsed time, never against wall-clock
//! time: the Worker's `Date.now()` can jump backwards when the host's clock
//! is corrected, and a token cache that reads a jump as "not expired yet"
//! keeps presenting a dead credential. Elapsed seconds since the driver was
//! built is all the cache needs, and it cannot go backwards.
//!
//! Tests drive [`ManualClock`], which makes token expiry an assertion rather
//! than a sleep.

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
    use super::{ManualClock, MonotonicClock, SystemClock};

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
