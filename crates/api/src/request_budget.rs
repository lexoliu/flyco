//! Per-principal request budgets: the circuit breaker that keeps one
//! runaway client from spending the account's whole Worker quota (issue
//! #342).
//!
//! The control plane runs on the free plan, whose ceilings are
//! account-wide: 100k Worker requests a day, 100k Durable Object requests,
//! 5M Durable Object row reads, 100k D1 row writes, 100k KV reads. Four
//! times a client loop has spent one of them and taken every session down
//! for the rest of the day — a daemon attach ping-pong at 3 req/s (#336), a
//! CLI gap-fill re-reading one page every 400 ms (#288), a catalog computed
//! inside the request (#174) — and each fix removed one loop. This module
//! is the bound that does not depend on the next loop being one we have
//! seen: every request is charged to a *principal*, and a principal that
//! spends more than its share is refused for the rest of the day while
//! everybody else keeps working.
//!
//! # Principals
//!
//! A request is charged to exactly one [`Principal`]: the user behind an
//! `fs_` or `fk_` credential, the session behind an `fd_` daemon token,
//! the machine behind an `fh_` host token, or the caller's address when it
//! presented nothing that resolved. A user with two API keys and a browser
//! has one budget, because the account is the thing being protected.
//!
//! # Two bounds, and why there are two
//!
//! 1. **A per-minute rate limit, before any storage is read.** Keyed by
//!    the hash of the presented credential (or the address when there is
//!    none) and checked through the platform's Rate Limiting binding —
//!    in-memory, per location, free — so a flood costs the account
//!    nothing, not even the KV read that resolving the credential would.
//!    It exists to stop the fast loop: a client at line rate is refused
//!    within a second.
//! 2. **A per-day ceiling, after the principal is known.** A slow loop
//!    inside the per-minute limit still spends the day — a daemon at one
//!    request a second is 86k requests — so each principal also carries a
//!    daily budget, sized far past real use. The ledger lives in D1, one
//!    row per principal per UTC day, and is charged in batches from a
//!    per-isolate [`Ledger`] tally so the ceiling costs one D1 write per
//!    [`FLUSH_EVERY`] requests rather than one per request: a bound that
//!    itself spent a write per request would exhaust the D1 write quota at
//!    exactly the rate it was protecting the request quota.
//!
//! The daily charge lands after the handler ran, because the principal
//! is only known once the route's own credential check has passed — a
//! forged session id in a path must never spend that session's budget.
//! The request that crosses the ceiling is therefore answered normally;
//! it is the *next* one that is refused, and it is refused before any
//! storage is read, because a spent principal's credential goes on the
//! per-isolate block list the pre-auth check consults first.
//!
//! # What a refusal looks like
//!
//! `429 Too Many Requests` with an RFC 9457 document — type `rate-limited`
//! for the per-minute bound, `request-budget-exhausted` for the daily one
//! — and a `Retry-After` header naming the wait in seconds: the window for
//! the former, the time to UTC midnight for the latter. Every client of
//! this control plane reads that header and sleeps at least that long;
//! the rule is written into `AGENTS.md`.
//!
//! # Sizing
//!
//! [`Limits::PRODUCTION`] states the numbers. They are circuit breakers,
//! not traffic shaping: a browser reloading a session page issues a few
//! dozen requests, a daemon on a busy turn a couple a second, and both are
//! an order of magnitude under their ceilings. What the ceilings do bound
//! is the damage: the worst a single principal can now do to the day's
//! quota is its own ceiling, which for the largest is a fifth of the
//! account's.

use core::fmt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use flyco_core::host::HOST_TOKEN_PREFIX;
use flyco_core::{CurrentUser, DAEMON_TOKEN_PREFIX, HostId, SessionId, UserId};
use skyzen::middleware::Next;
use skyzen::routing::Params;
use skyzen::sql;
use skyzen::utils::State;
use skyzen::{Error, Middleware, Request, Response};
use skyzen_services::Db;

use crate::authenticator::bearer_token;
use crate::clock::now_unix;
use crate::crypto::token_hash;
use crate::error::ApiError;
use crate::middleware::DaemonSession;
use crate::{api_keys, session};

/// The unit the daily ledger keys on, in the seconds `now_unix` counts.
pub const SECONDS_PER_DAY: u64 = 86_400;

/// The window the per-minute bound is measured over.
///
/// Also what the platform's Rate Limiting binding is configured with in
/// `Skyzen.toml`: the binding accepts ten or sixty seconds, and sixty is
/// the one that makes "requests per minute" a number a person can size.
pub const WINDOW_SECONDS: u64 = 60;

/// Requests a principal may accrue in one isolate before the tally reaches
/// D1.
///
/// The ledger's cost is one write per this many requests; what an isolate
/// can under-count by is at most this many minus one, per isolate — noise
/// against ceilings in the thousands.
pub const FLUSH_EVERY: u64 = 25;

/// How many days of ledger rows the cron keeps.
///
/// Nothing reads history — a day's row exists so the day's ceiling can be
/// enforced across isolates — so a week is generous, and the sweep that
/// drops older rows is one statement an hour.
pub const LEDGER_RETENTION_DAYS: u64 = 7;

/// Header Cloudflare sets to the address the request actually came from.
const CONNECTING_IP: &str = "cf-connecting-ip";

/// What a request that presents no address at all is keyed by — the test
/// client, and `skyzen dev` on a socket with no peer address.
const LOCAL_ADDRESS: &str = "local";

/// The kind of caller a credential says it is, read from the token's
/// prefix before the token is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    /// A browser session (`fs_`) or an API key (`fk_`): a person, or an
    /// agent acting for one.
    User,
    /// A session's daemon (`fd_`): `flycod` on the session's machine.
    Daemon,
    /// An enrolled machine (`fh_`): `flycod host`.
    Host,
    /// Nothing that could resolve: no credential, or one with a prefix
    /// flyco never minted.
    Public,
}

impl Class {
    /// Classifies a presented bearer token by its prefix.
    #[must_use]
    pub fn of(bearer: Option<&str>) -> Self {
        match bearer {
            Some(token)
                if token.starts_with(session::TOKEN_PREFIX)
                    || token.starts_with(api_keys::TOKEN_PREFIX) =>
            {
                Self::User
            }
            Some(token) if token.starts_with(DAEMON_TOKEN_PREFIX) => Self::Daemon,
            Some(token) if token.starts_with(HOST_TOKEN_PREFIX) => Self::Host,
            _ => Self::Public,
        }
    }

    /// The Cloudflare Rate Limiting binding that meters this class.
    ///
    /// One binding per class because a binding carries exactly one limit;
    /// the numbers are declared in `Skyzen.toml` and must equal
    /// [`Limits::PRODUCTION`], which a test asserts.
    #[must_use]
    pub const fn binding(self) -> &'static str {
        match self {
            Self::User => "LIMIT_USERS",
            Self::Daemon => "LIMIT_DAEMONS",
            Self::Host => "LIMIT_HOSTS",
            Self::Public => "LIMIT_PUBLIC",
        }
    }

    /// The word a refusal and a ledger key use for this class.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Daemon => "daemon",
            Self::Host => "host",
            Self::Public => "public",
        }
    }

    /// Every class, for anything that iterates the set.
    pub const ALL: [Self; 4] = [Self::User, Self::Daemon, Self::Host, Self::Public];
}

impl fmt::Display for Class {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// What one class of caller may spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    /// Requests per [`WINDOW_SECONDS`], per credential.
    pub per_minute: u32,
    /// Requests per UTC day, per principal.
    pub per_day: u64,
}

/// The ceilings, per class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Browsers and API keys.
    pub user: Limit,
    /// Session daemons.
    pub daemon: Limit,
    /// Enrolled hosts.
    pub host: Limit,
    /// Unauthenticated callers, per address.
    pub public: Limit,
}

impl Limits {
    /// The deployed ceilings.
    ///
    /// Read against the free plan's 100k requests a day and what each
    /// class does in a day of real use:
    ///
    /// * A **user** is a browser and a CLI. A session page load is a few
    ///   dozen requests, an event stream is one request per reconnect,
    ///   and a `flyco run` streams one request for its whole life. A
    ///   working day is low thousands; twenty thousand is the ceiling, a
    ///   fifth of the account's day.
    /// * A **daemon** attaches once, holds one command stream (re-opened
    ///   every ninety idle seconds, under a thousand a day), and posts
    ///   frames in batches coalesced over half a second — at most two a
    ///   second, and that only while a browser watches the session's
    ///   desktop or a build streams terminal output. Twenty-five thousand
    ///   is three and a half hours of that, or a whole working day of the
    ///   ordinary turn-by-turn rate; the attach ping-pong of #336 would
    ///   have been cut at it after two and a third hours instead of
    ///   running for three and a half. The per-minute bound sits at twice
    ///   the coalesced rate so a legitimate stream never touches it.
    /// * A **host** speaks only when a job starts or ends.
    /// * A **public** address reaches the health check, the sign-in
    ///   redirect, the CLI's two-second sign-in poll and the webhook. Its
    ///   per-minute bound is what a device-code poll needs and its day is
    ///   what an unattended poll left running would spend.
    pub const PRODUCTION: Self = Self {
        user: Limit {
            per_minute: 300,
            per_day: 20_000,
        },
        daemon: Limit {
            per_minute: 240,
            per_day: 25_000,
        },
        host: Limit {
            per_minute: 60,
            per_day: 5_000,
        },
        public: Limit {
            per_minute: 60,
            per_day: 2_000,
        },
    };

    /// The ceiling one class runs under.
    #[must_use]
    pub const fn for_class(&self, class: Class) -> Limit {
        match class {
            Class::User => self.user,
            Class::Daemon => self.daemon,
            Class::Host => self.host,
            Class::Public => self.public,
        }
    }
}

/// Whom a request is charged to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Principal {
    /// The account behind a browser session or an API key.
    User(UserId),
    /// The session behind a daemon token.
    Session(SessionId),
    /// The machine behind a host token.
    Host(HostId),
    /// An address that presented nothing that resolved.
    Address(String),
}

impl Principal {
    /// The class whose ceiling applies.
    #[must_use]
    pub const fn class(&self) -> Class {
        match self {
            Self::User(_) => Class::User,
            Self::Session(_) => Class::Daemon,
            Self::Host(_) => Class::Host,
            Self::Address(_) => Class::Public,
        }
    }

    /// The ledger row's key.
    #[must_use]
    pub fn key(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User(id) => write!(f, "user:{id}"),
            Self::Session(id) => write!(f, "session:{id}"),
            Self::Host(id) => write!(f, "host:{id}"),
            Self::Address(address) => write!(f, "ip:{address}"),
        }
    }
}

/// The per-minute bound.
///
/// Two implementations: the Cloudflare Rate Limiting binding on the
/// Worker, and a fixed-window counter in memory for the native binary and
/// the tests. The trait takes the request so the Worker implementation can
/// reach its `env`; it must read what it needs before its future is
/// returned, because the future outlives the borrow.
pub trait RateLimiter: Send + Sync + 'static {
    /// Whether one more request under `key` fits inside `per_minute`.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::RateLimiterUnavailable`] when the limiter
    /// itself cannot answer — a missing binding, say. A limiter that
    /// cannot answer refuses: failing open here would make the one bound
    /// that costs nothing the first to disappear under load.
    fn allow(
        &self,
        request: &Request,
        key: String,
        per_minute: u32,
    ) -> impl Future<Output = Result<bool, ApiError>> + Send + 'static;
}

/// Fixed sixty-second windows in memory.
///
/// Exact within one process, which is all the native binary has; the
/// Worker's per-location counters are the platform's business.
#[derive(Debug, Default)]
pub struct MemoryRateLimiter {
    windows: Mutex<HashMap<String, (u64, u32)>>,
}

impl MemoryRateLimiter {
    /// An empty limiter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts one request under `key` in the window `now` falls in.
    fn admit(&self, key: String, per_minute: u32, now: u64) -> bool {
        let window = now / WINDOW_SECONDS;
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = windows.entry(key).or_insert((window, 0));
        if entry.0 != window {
            *entry = (window, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        let admitted = entry.1 <= per_minute;
        drop(windows);
        admitted
    }
}

impl RateLimiter for MemoryRateLimiter {
    fn allow(
        &self,
        _request: &Request,
        key: String,
        per_minute: u32,
    ) -> impl Future<Output = Result<bool, ApiError>> + Send + 'static {
        let admitted = self.admit(key, per_minute, now_unix());
        async move { Ok(admitted) }
    }
}

/// The Cloudflare Rate Limiting binding, one per [`Class`].
///
/// `env.LIMIT_<CLASS>.limit({ key })` answers `{ success }`; the limit
/// itself is the binding's configuration, so `per_minute` is not passed —
/// the test that pins `Skyzen.toml` to [`Limits::PRODUCTION`] is what
/// keeps the two the same number.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone, Copy, Default)]
pub struct BindingRateLimiter;

#[cfg(target_arch = "wasm32")]
impl RateLimiter for BindingRateLimiter {
    fn allow(
        &self,
        request: &Request,
        key: String,
        _per_minute: u32,
    ) -> impl Future<Output = Result<bool, ApiError>> + Send + 'static {
        use skyzen_cloudflare::worker::send::IntoSendFuture as _;

        let env = request
            .extensions()
            .get::<skyzen::runtime::wasm::WasmEnv>()
            .cloned();
        let class = Class::of(bearer_token(request.headers()));
        async move {
            let env = env.ok_or_else(|| {
                ApiError::RateLimiterUnavailable(
                    "the Worker environment was not in the request".to_owned(),
                )
            })?;
            let binding = skyzen_cloudflare::ffi::get_binding(env.as_js(), class.binding())
                .map_err(|error| ApiError::RateLimiterUnavailable(js_error(&error)))?;
            let limit = skyzen::js_sys::Reflect::get(&binding, &"limit".into())
                .ok()
                .and_then(|value| value.dyn_into::<skyzen::js_sys::Function>().ok())
                .ok_or_else(|| {
                    ApiError::RateLimiterUnavailable(format!(
                        "binding {} has no `limit` method",
                        class.binding()
                    ))
                })?;
            let options = skyzen::js_sys::Object::new();
            skyzen::js_sys::Reflect::set(&options, &"key".into(), &key.into())
                .map_err(|error| ApiError::RateLimiterUnavailable(js_error(&error)))?;
            let promise: skyzen::js_sys::Promise = limit
                .call1(&binding, &options)
                .map_err(|error| ApiError::RateLimiterUnavailable(js_error(&error)))?
                .dyn_into()
                .map_err(|error| ApiError::RateLimiterUnavailable(js_error(&error)))?;
            let answer = skyzen::wasm_bindgen_futures::JsFuture::from(promise)
                .into_send()
                .await
                .map_err(|error| ApiError::RateLimiterUnavailable(js_error(&error)))?;
            let success = skyzen::js_sys::Reflect::get(&answer, &"success".into())
                .map_err(|error| ApiError::RateLimiterUnavailable(js_error(&error)))?;
            success.as_bool().ok_or_else(|| {
                ApiError::RateLimiterUnavailable(
                    "the limiter answered without `success`".to_owned(),
                )
            })
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn js_error(error: &skyzen::wasm_bindgen::JsValue) -> String {
    error.as_string().unwrap_or_else(|| format!("{error:?}"))
}

#[cfg(target_arch = "wasm32")]
use skyzen::wasm_bindgen::JsCast as _;

/// One principal's presence in the ledger, as this isolate knows it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Tally {
    /// What D1 last answered: the day's total across every isolate.
    confirmed: u64,
    /// Requests counted here since, not yet written.
    unflushed: u64,
}

impl Tally {
    const fn total(self) -> u64 {
        self.confirmed.saturating_add(self.unflushed)
    }
}

/// What charging one request decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    /// The principal is at its ceiling, counting this request: refuse
    /// the next one.
    pub exhausted: bool,
    /// This many unflushed requests must reach D1 now.
    pub flush: Option<u64>,
}

/// The per-isolate half of the ledger.
///
/// Pure: every method takes the instant, and the D1 round trip happens
/// outside it, so the arithmetic is tested without a database and the
/// lock around it is never held across an await.
#[derive(Debug, Default)]
pub struct Ledger {
    tallies: HashMap<(Principal, u64), Tally>,
    /// Credential keys refused until an instant: a spent principal's
    /// credentials, so the pre-auth check refuses them without a storage
    /// read.
    blocked: HashMap<String, u64>,
    swept_day: u64,
}

impl Ledger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The instant a credential key is refused until, if it is.
    #[must_use]
    pub fn blocked_until(&self, key: &str, now: u64) -> Option<u64> {
        self.blocked.get(key).copied().filter(|until| *until > now)
    }

    /// Refuses a credential key until `until`.
    pub fn block(&mut self, key: String, until: u64) {
        self.blocked.insert(key, until);
    }

    /// Counts one request against `principal`'s day.
    ///
    /// Three moments flush at once rather than waiting for a batch: the
    /// first request an isolate sees for a principal on a day, which is
    /// how the isolate learns the day's total — and how one that starts
    /// late in a spent day refuses on its second request rather than its
    /// twenty-sixth; the request that reaches the ceiling, so every other
    /// isolate learns it is reached; and every [`FLUSH_EVERY`]th between.
    /// A principal already at its ceiling is not counted: it is refused
    /// before the request runs, and the tally is what the refusal reads.
    pub fn charge(&mut self, principal: &Principal, limit: Limit, now: u64) -> Verdict {
        let day = now / SECONDS_PER_DAY;
        self.sweep(day, now);
        let tally = self.tallies.entry((principal.clone(), day)).or_default();
        if tally.total() >= limit.per_day {
            return Verdict {
                exhausted: true,
                flush: None,
            };
        }
        tally.unflushed = tally.unflushed.saturating_add(1);
        let exhausted = tally.total() >= limit.per_day;
        let first = tally.confirmed == 0 && tally.unflushed == 1;
        let flush =
            (first || exhausted || tally.unflushed >= FLUSH_EVERY).then_some(tally.unflushed);
        Verdict { exhausted, flush }
    }

    /// Records what D1 answered for a flush of `flushed` requests: the
    /// day's total. Returns whether the principal is now at its ceiling.
    ///
    /// Only the flushed count is retired: requests charged while the
    /// write was in flight stay unflushed for the next batch.
    pub fn confirm(
        &mut self,
        principal: &Principal,
        limit: Limit,
        now: u64,
        flushed: u64,
        total: u64,
    ) -> bool {
        let day = now / SECONDS_PER_DAY;
        let tally = self.tallies.entry((principal.clone(), day)).or_default();
        tally.confirmed = total;
        tally.unflushed = tally.unflushed.saturating_sub(flushed);
        tally.total() >= limit.per_day
    }

    /// Drops yesterday's tallies and expired blocks, once per day.
    fn sweep(&mut self, day: u64, now: u64) {
        if day == self.swept_day {
            return;
        }
        self.swept_day = day;
        self.tallies.retain(|(_, on), _| *on == day);
        self.blocked.retain(|_, until| *until > now);
    }
}

/// Seconds from `now` to the next UTC midnight — the `Retry-After` a
/// daily refusal carries.
#[must_use]
pub const fn until_midnight(now: u64) -> u64 {
    SECONDS_PER_DAY - now % SECONDS_PER_DAY
}

/// What one request presented, before anything was resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Caller {
    class: Class,
    /// The per-minute limiter's key and the block list's: the credential's
    /// hash for a credentialed class, the address otherwise.
    key: String,
    address: String,
}

impl Caller {
    fn of(request: &Request) -> Self {
        let bearer = bearer_token(request.headers());
        let class = Class::of(bearer);
        let address = request
            .headers()
            .get(CONNECTING_IP)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .or_else(|| {
                request
                    .extensions()
                    .get::<skyzen::extract::PeerAddr>()
                    .map(|peer| peer.0.ip().to_string())
            })
            .unwrap_or_else(|| LOCAL_ADDRESS.to_owned());
        let key = match (class, bearer) {
            (Class::Public, _) | (_, None) => format!("ip:{address}"),
            (class, Some(token)) => format!("{class}:{}", token_hash(token)),
        };
        Self {
            class,
            key,
            address,
        }
    }

    /// Whom the request turned out to be, once the route has run.
    ///
    /// Read from what the route's own credential check left behind: a
    /// [`CurrentUser`] or a [`DaemonSession`] in the extensions. A host
    /// route checks its token inside the handler and leaves nothing, so a
    /// host-class request that was not refused as unauthenticated is
    /// charged to the host in its path. Everything else — no credential,
    /// or one that did not resolve — is charged to the address.
    fn principal(&self, request: &Request, status: skyzen::StatusCode) -> Principal {
        let extensions = request.extensions();
        if let Some(State(user)) = extensions.get::<State<CurrentUser>>() {
            return Principal::User(user.id);
        }
        if let Some(State(DaemonSession(session))) = extensions.get::<State<DaemonSession>>() {
            return Principal::Session(*session);
        }
        if self.class == Class::Host
            && status != skyzen::StatusCode::UNAUTHORIZED
            && status != skyzen::StatusCode::FORBIDDEN
            && let Some(host) = extensions
                .get::<Params>()
                .and_then(|params| params.get("id").ok())
                .and_then(|raw| raw.parse::<HostId>().ok())
        {
            return Principal::Host(host);
        }
        Principal::Address(self.address.clone())
    }
}

/// The middleware: outermost on the route tree, inside the service layers
/// that inject `Db`.
///
/// Holds the isolate's [`Ledger`] behind a mutex that is only ever taken
/// for a few map operations and never across an await; on the Worker the
/// isolate is single-threaded and the lock is never contended, natively
/// it is what lets the tests drive the router from several tasks.
#[derive(Debug)]
pub struct RequestBudget<L> {
    limiter: L,
    limits: Limits,
    ledger: Arc<Mutex<Ledger>>,
}

impl<L: RateLimiter> RequestBudget<L> {
    /// A budget with an empty ledger.
    pub fn new(limiter: L, limits: Limits) -> Self {
        Self {
            limiter,
            limits,
            ledger: Arc::new(Mutex::new(Ledger::new())),
        }
    }

    fn ledger(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Charges the request the route just answered.
    ///
    /// A ledger write that fails is logged and the request stays counted
    /// locally: the next flush carries it. Nothing here can fail the
    /// response — it was already produced.
    async fn charge(&self, request: &Request, caller: &Caller, principal: Principal, now: u64) {
        let limit = self.limits.for_class(principal.class());
        let verdict = self.ledger().charge(&principal, limit, now);
        let mut exhausted = verdict.exhausted;
        if let Some(flushed) = verdict.flush {
            match request.extensions().get::<Db>().cloned() {
                Some(db) => match flush(&db, &principal, now / SECONDS_PER_DAY, flushed).await {
                    Ok(total) => {
                        exhausted |= self
                            .ledger()
                            .confirm(&principal, limit, now, flushed, total);
                    }
                    Err(error) => {
                        tracing::warn!(%principal, %error, "a request budget flush did not reach D1");
                    }
                },
                None => {
                    tracing::error!(%principal, "a request budget flush found no database in the request");
                }
            }
        }
        if exhausted {
            self.refuse(caller, &principal, now);
        }
    }

    /// Puts a spent principal's credential on the block list and says so
    /// once, where Workers Logs keeps it.
    fn refuse(&self, caller: &Caller, principal: &Principal, now: u64) {
        let until = now + until_midnight(now);
        let mut ledger = self.ledger();
        if ledger.blocked_until(&caller.key, now).is_some() {
            return;
        }
        ledger.block(caller.key.clone(), until);
        drop(ledger);
        tracing::warn!(
            %principal,
            class = %principal.class(),
            per_day = self.limits.for_class(principal.class()).per_day,
            resets_at_unix = until,
            "a principal spent its daily request budget and is refused until UTC midnight"
        );
    }
}

impl<L: RateLimiter> Middleware for RequestBudget<L> {
    async fn handle(&self, request: &mut Request, next: Next<'_>) -> Result<Response, Error> {
        let now = now_unix();
        let caller = Caller::of(request);

        let blocked_until = self.ledger().blocked_until(&caller.key, now);
        if let Some(until) = blocked_until {
            return Ok(ApiError::RequestBudgetExhausted {
                class: caller.class,
                retry_after: until.saturating_sub(now),
            }
            .into_response());
        }

        let per_minute = self.limits.for_class(caller.class).per_minute;
        let admitted = self
            .limiter
            .allow(request, caller.key.clone(), per_minute)
            .await;
        match admitted {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    class = %caller.class,
                    key = %caller.key,
                    per_minute,
                    "a caller exceeded its per-minute request limit"
                );
                return Ok(ApiError::RateLimited {
                    class: caller.class,
                    retry_after: WINDOW_SECONDS,
                }
                .into_response());
            }
            Err(error) => return Ok(error.into_response()),
        }

        let response = next.run(request).await?;
        let principal = caller.principal(request, response.status());
        self.charge(request, &caller, principal, now).await;
        Ok(response)
    }
}

/// Adds `amount` to a principal's row for `day` and answers the day's
/// total across every isolate.
async fn flush(db: &Db, principal: &Principal, day: u64, amount: u64) -> Result<u64, ApiError> {
    let key = principal.key();
    Ok(sql!(
        db,
        "INSERT INTO request_budgets (principal, day, requests) VALUES ({key}, {day}, {amount}) \
         ON CONFLICT (principal, day) DO UPDATE SET requests = requests + excluded.requests \
         RETURNING requests"
    )
    .fetch_scalar()
    .await?)
}

/// Reads a principal's day from the ledger, for tests and diagnostics.
///
/// # Errors
///
/// Returns [`ApiError`] if the ledger cannot be read.
pub async fn recorded(db: &Db, principal: &Principal, now: u64) -> Result<u64, ApiError> {
    let key = principal.key();
    let day = now / SECONDS_PER_DAY;
    Ok(sql!(
        db,
        "SELECT requests FROM request_budgets WHERE principal = {key} AND day = {day}"
    )
    .fetch_scalar_optional()
    .await?
    .unwrap_or(0))
}

/// Drops ledger rows older than [`LEDGER_RETENTION_DAYS`], on the hour.
///
/// Rides the minute cron; the hour gate keeps it to twenty-four
/// statements a day against a table whose old rows nobody reads.
///
/// # Errors
///
/// Returns [`ApiError`] if the delete fails.
pub async fn sweep(db: &Db, at_unix: u64) -> Result<(), ApiError> {
    if !at_unix.is_multiple_of(3_600) {
        return Ok(());
    }
    let keep_from = (at_unix / SECONDS_PER_DAY).saturating_sub(LEDGER_RETENTION_DAYS);
    sql!(db, "DELETE FROM request_budgets WHERE day < {keep_from}")
        .execute()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Class, FLUSH_EVERY, Ledger, Limit, Limits, MemoryRateLimiter, Principal, SECONDS_PER_DAY,
        Verdict, WINDOW_SECONDS, until_midnight,
    };
    use flyco_core::{SessionId, UserId};

    const NOON: u64 = 1_800_000_000 - 1_800_000_000 % SECONDS_PER_DAY + 12 * 3_600;

    fn small() -> Limit {
        Limit {
            per_minute: 3,
            per_day: 5,
        }
    }

    const EXHAUSTED: Verdict = Verdict {
        exhausted: true,
        flush: None,
    };

    const fn within(flush: Option<u64>) -> Verdict {
        Verdict {
            exhausted: false,
            flush,
        }
    }

    #[test]
    fn a_class_is_read_from_the_credential_prefix() {
        assert_eq!(Class::of(Some("fs_abc")), Class::User);
        assert_eq!(Class::of(Some("fk_abc")), Class::User);
        assert_eq!(Class::of(Some("fd_abc")), Class::Daemon);
        assert_eq!(Class::of(Some("fh_abc")), Class::Host);
        assert_eq!(Class::of(Some("xx_abc")), Class::Public);
        assert_eq!(Class::of(None), Class::Public);
    }

    #[test]
    fn the_first_sighting_flushes_at_once_and_then_every_batch() {
        let mut ledger = Ledger::new();
        let principal = Principal::User(UserId::generate());
        let limit = Limit {
            per_minute: 10,
            per_day: 1_000,
        };
        assert_eq!(ledger.charge(&principal, limit, NOON), within(Some(1)));
        assert!(!ledger.confirm(&principal, limit, NOON, 1, 1));
        for _ in 1..FLUSH_EVERY {
            assert_eq!(ledger.charge(&principal, limit, NOON), within(None));
        }
        assert_eq!(
            ledger.charge(&principal, limit, NOON),
            within(Some(FLUSH_EVERY))
        );
    }

    #[test]
    fn a_confirmed_total_at_the_ceiling_refuses_the_next_request() {
        let mut ledger = Ledger::new();
        let principal = Principal::Session(SessionId::generate());
        assert_eq!(ledger.charge(&principal, small(), NOON), within(Some(1)));
        // Another isolate spent the day already.
        assert!(ledger.confirm(&principal, small(), NOON, 1, 5));
        assert_eq!(ledger.charge(&principal, small(), NOON), EXHAUSTED);
    }

    #[test]
    fn requests_charged_during_a_flush_stay_unflushed() {
        let mut ledger = Ledger::new();
        let principal = Principal::User(UserId::generate());
        let limit = Limit {
            per_minute: 10,
            per_day: 1_000,
        };
        assert_eq!(ledger.charge(&principal, limit, NOON), within(Some(1)));
        // Two more land while the write is in flight.
        assert_eq!(ledger.charge(&principal, limit, NOON), within(None));
        assert_eq!(ledger.charge(&principal, limit, NOON), within(None));
        ledger.confirm(&principal, limit, NOON, 1, 1);
        let tally = ledger.tallies[&(principal, NOON / SECONDS_PER_DAY)];
        assert_eq!((tally.confirmed, tally.unflushed), (1, 2));
    }

    #[test]
    fn the_request_that_reaches_the_ceiling_is_counted_flushed_and_the_last() {
        let mut ledger = Ledger::new();
        let principal = Principal::Address("203.0.113.9".to_owned());
        assert_eq!(ledger.charge(&principal, small(), NOON), within(Some(1)));
        ledger.confirm(&principal, small(), NOON, 1, 1);
        for _ in 2..5 {
            assert_eq!(ledger.charge(&principal, small(), NOON), within(None));
        }
        // The fifth is the ceiling: counted, and written so every isolate
        // sees the day is spent.
        assert_eq!(
            ledger.charge(&principal, small(), NOON),
            Verdict {
                exhausted: true,
                flush: Some(4),
            }
        );
        assert_eq!(ledger.charge(&principal, small(), NOON), EXHAUSTED);
    }

    #[test]
    fn a_new_day_starts_the_count_over_and_drops_expired_blocks() {
        let mut ledger = Ledger::new();
        let principal = Principal::User(UserId::generate());
        ledger.charge(&principal, small(), NOON);
        ledger.confirm(&principal, small(), NOON, 1, 5);
        assert_eq!(ledger.charge(&principal, small(), NOON), EXHAUSTED);
        let midnight = NOON + until_midnight(NOON);
        ledger.block("user:abc".to_owned(), midnight);
        assert_eq!(ledger.blocked_until("user:abc", NOON), Some(midnight));

        let tomorrow = midnight + 1;
        assert_eq!(
            ledger.charge(&principal, small(), tomorrow),
            within(Some(1))
        );
        assert_eq!(ledger.blocked_until("user:abc", tomorrow), None);
        assert!(ledger.tallies.len() == 1, "yesterday's tally is gone");
    }

    #[test]
    fn until_midnight_counts_to_the_next_utc_day() {
        assert_eq!(until_midnight(NOON), 12 * 3_600);
        assert_eq!(until_midnight(NOON - 12 * 3_600), SECONDS_PER_DAY);
        assert_eq!(until_midnight(NOON + 12 * 3_600 - 1), 1);
    }

    #[test]
    fn the_memory_limiter_admits_a_window_and_refuses_the_rest() {
        let limiter = MemoryRateLimiter::new();
        for _ in 0..3 {
            assert!(limiter.admit("k".to_owned(), 3, NOON));
        }
        assert!(!limiter.admit("k".to_owned(), 3, NOON));
        assert!(
            limiter.admit("other".to_owned(), 3, NOON),
            "keys are independent"
        );
        assert!(
            limiter.admit("k".to_owned(), 3, NOON + WINDOW_SECONDS),
            "the next window starts over"
        );
    }

    #[test]
    fn production_limits_are_ordered_by_what_each_class_does() {
        let limits = Limits::PRODUCTION;
        for class in Class::ALL {
            let limit = limits.for_class(class);
            assert!(limit.per_minute > 0 && limit.per_day > u64::from(limit.per_minute));
            // A minute's worth times the day's minutes is what a client
            // pinned at the per-minute bound would spend; the day's
            // ceiling must be what actually bounds it.
            assert!(
                limit.per_day < u64::from(limit.per_minute) * (SECONDS_PER_DAY / WINDOW_SECONDS),
                "{class}: the daily ceiling must be the binding one"
            );
        }
        // No single principal may take more than a quarter of the free
        // plan's 100k requests a day.
        for class in Class::ALL {
            assert!(limits.for_class(class).per_day <= 25_000, "{class}");
        }
    }

    /// `Skyzen.toml` declares the Rate Limiting bindings the Worker runs
    /// under; the numbers there are what the platform enforces, so they
    /// must be the numbers this module documents and the native limiter
    /// applies.
    #[test]
    fn the_manifest_declares_every_class_binding_with_the_production_limit() {
        let manifest = include_str!("../Skyzen.toml");
        for class in Class::ALL {
            let declaration = format!("name = \"{}\"", class.binding());
            let at = manifest
                .find(&declaration)
                .unwrap_or_else(|| panic!("Skyzen.toml declares no binding {}", class.binding()));
            let block = &manifest[at..manifest.len().min(at + 200)];
            let expected = format!(
                "simple = {{ limit = {}, period = {WINDOW_SECONDS} }}",
                Limits::PRODUCTION.for_class(class).per_minute
            );
            assert!(
                block.contains(&expected),
                "{}: expected `{expected}` in\n{block}",
                class.binding()
            );
        }
    }
}
