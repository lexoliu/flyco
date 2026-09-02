//! Console-backed `tracing` for the Worker build.
//!
//! Natively, skyzen's runtime installs a subscriber before the router is
//! built. On Cloudflare Workers nothing does, so every `tracing::error!`
//! the control plane emits — including the full reason behind each opaque
//! 5xx in [`crate::ApiError::problem`] — was dropped on the floor and never
//! reached Workers Logs (flyco #119). This layer writes each event through
//! `console.*`, which is exactly what Workers Logs retains.
//!
//! Installation is idempotent without a static of its own:
//! `set_global_default` refuses a second subscriber, and that refusal is the
//! signal that an earlier request on this isolate already installed one.

use tracing_subscriber::layer::SubscriberExt as _;
use tracing_web::MakeWebConsoleWriter;

/// Events at or above this level are written. `debug` because every refused
/// request (4xx) is logged at that level by [`crate::ApiError::problem`], and
/// a refused sign-in is exactly what the retained logs must be able to
/// explain; the dev deployment's volume is small enough to keep all of it.
const LEVEL: tracing::Level = tracing::Level::DEBUG;

/// Installs the console subscriber and the panic hook, once per isolate.
pub fn install() {
    console_error_panic_hook::set_once();

    let console = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .without_time()
        .with_writer(MakeWebConsoleWriter::new());
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::filter::LevelFilter::from_level(LEVEL))
        .with(console);

    // Already installed by a previous request on this isolate: nothing to do.
    drop(tracing::subscriber::set_global_default(subscriber));
}
