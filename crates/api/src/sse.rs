//! Server-Sent Events out of a Durable Object.
//!
//! An object serving a route shares no memory with the object's other
//! activations — every `fetch` may reconstruct it — so a stream cannot
//! subscribe to anything. What it can do is poll its own storage: each
//! tick asks the feed for what is new, waits when the answer is nothing,
//! and ends when the feed says the stream is over.
//!
//! The polling task writes into the bounded channel behind
//! [`Sse::channel_with_capacity`], because the response body must be
//! `Sync` and a `DurableDb` query future is not: the task needs only
//! `Send`, which the channel gives it for free on both targets — the
//! isolate's microtask queue on wasm32, the runtime's global executor
//! natively.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use skyzen::responder::Sse;
use skyzen::responder::sse::{Event, Sender};

/// How often an idle feed re-asks its storage.
///
/// Short enough that a command or an event feels immediate, long enough
/// that a quiet room costs a read a fraction of a second — which a
/// Durable Object's SQLite answers from its own disk cache.
const POLL: Duration = Duration::from_millis(150);

/// How long a stream may say nothing before it proves itself alive.
///
/// The interval is what keeps an idle stream's TCP flow known-good to
/// every middlebox on the path: a `ping` comment is data, and data is
/// what resets the idle timers that reclaim quiet connections.
pub const HEARTBEAT: Duration = Duration::from_secs(15);

/// How many events the channel buffers before a slow reader stalls the
/// feed. Backpressure rather than an unbounded queue: a daemon that has
/// gone away does not need its command log copied into memory.
const CAPACITY: usize = 64;

/// What one poll of a feed concluded.
#[derive(Debug)]
pub enum Poll {
    /// Events to emit, in the order they arrived. Never empty — an empty
    /// answer is [`Poll::Idle`].
    Emit(Vec<Event>),
    /// Nothing new this tick.
    Idle,
    /// The stream is over: the body ends cleanly.
    End,
}

/// A poll over a feed's state.
///
/// Boxed because the concrete future of each feed's poll is unnameable;
/// a plain `fn` returning this type satisfies the bound. `Send` is the
/// most the future can promise — a `DurableDb` query's boxed future is
/// never `Sync` — and all either executor needs.
pub type PollFn<'a> = Pin<Box<dyn Future<Output = Poll> + Send + 'a>>;

/// Serves `poll` over `state` as a `text/event-stream` responder.
///
/// Each tick calls `poll`: [`Poll::Emit`] puts the events on the channel,
/// [`Poll::Idle`] waits out [`POLL`], and [`Poll::End`] ends the body. A
/// stream quiet for `heartbeat` says `ping`, which is what the interval
/// is for. A gone reader turns the next `send` into an error, which ends
/// the task — the channel dropping is the only cancellation the feed
/// needs.
pub fn serve<S, F>(state: S, poll: F, heartbeat: Duration) -> Sse
where
    S: Send + 'static,
    F: for<'a> FnMut(&'a mut S) -> PollFn<'a> + Send + 'static,
{
    let (sender, sse) = Sse::channel_with_capacity(CAPACITY);
    spawn(drive(state, poll, sender, heartbeat));
    sse
}

/// The polling loop behind a served stream.
async fn drive<S, F>(mut state: S, mut poll: F, sender: Sender, heartbeat: Duration)
where
    F: for<'a> FnMut(&'a mut S) -> PollFn<'a>,
{
    // A `Delay` that expires when the next ping is due: kept rather than
    // a wall-clock read so the interval can be shorter than a second —
    // which `now_unix` cannot express — and so a test can ask for one.
    let mut ping_at = futures_timer::Delay::new(heartbeat);
    loop {
        match poll(&mut state).await {
            Poll::Emit(events) => {
                debug_assert!(
                    !events.is_empty(),
                    "a feed answered Emit with nothing to emit"
                );
                for event in events {
                    if sender.send(event).await.is_err() {
                        return;
                    }
                }
                ping_at.reset(heartbeat);
            }
            Poll::Idle => {
                if futures_util::FutureExt::now_or_never(&mut ping_at).is_some() {
                    ping_at.reset(heartbeat);
                    if sender.send(Event::comment("ping")).await.is_err() {
                        return;
                    }
                }
                futures_timer::Delay::new(POLL).await;
            }
            Poll::End => return,
        }
    }
}

/// Runs `task` to completion.
///
/// The Worker drives `spawn_local` futures on the isolate's microtask
/// queue — alive exactly as long as the response the task feeds. The
/// native runtime's global executor (a `SmolGlobal` the server installs
/// at start-up) takes the same task natively.
#[cfg(target_arch = "wasm32")]
fn spawn(task: impl Future<Output = ()> + 'static) {
    skyzen::wasm_bindgen_futures::spawn_local(task);
}

/// Runs `task` to completion on the runtime's global executor.
#[cfg(not(target_arch = "wasm32"))]
fn spawn(task: impl Future<Output = ()> + Send + 'static) {
    executor_core::spawn(task).detach();
}
