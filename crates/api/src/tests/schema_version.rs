//! The schema check, counted rather than inferred.
//!
//! `schema_version::ensure` runs against the SQLite-backed
//! [`SqliteDurableDb`] the other object tests use, wrapped in a backend
//! that counts every statement naming `schema_meta` — "the meta
//! statements ran once" is an observation, not an assumption. The
//! object-level test rebuilds the object from `Default` between requests
//! the way the runtime does for one that opted out of the state blob,
//! because a fresh activation's one read is the cost being pinned.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use flyco_core::wire::MessageOrigin;
use flyco_core::{ClientEvent, SessionId, UserId};
use skyzen::durable::DurableObject as _;
use skyzen::{Body, Method, Request};
use skyzen_services::durable::SqliteDurableDb;
use skyzen_services::{DbExecResult, DbValue, DurableDb, DurableDbBackend, DurableDbError};

use crate::room::{EmittedEvent, HEADER_INTERNAL, INTERNAL};
use crate::schema_version::{self, Cache};
use crate::user_events::{HEADER_USER, PublishEvents, UserEvents};

/// A [`DurableDbBackend`] that counts how many of its statements touch
/// the `schema_meta` table.
#[derive(Debug, Clone)]
struct LoggedDb {
    inner: SqliteDurableDb,
    /// The meta statements seen so far — `CREATE`, `SELECT`, and the
    /// `INSERT`/`UPDATE` that records the version.
    meta: Arc<AtomicUsize>,
}

impl LoggedDb {
    async fn open() -> Self {
        Self {
            inner: SqliteDurableDb::in_memory()
                .await
                .expect("an in-memory database"),
            meta: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// How many statements naming `schema_meta` this backend has run.
    fn meta_statements(&self) -> usize {
        self.meta.load(Ordering::Relaxed)
    }

    fn note(&self, query: &str) {
        if query.contains("schema_meta") {
            self.meta.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl DurableDbBackend for LoggedDb {
    async fn query(&self, query: &str, params: &[DbValue]) -> Result<DbExecResult, DurableDbError> {
        self.note(query);
        self.inner.query(query, params).await
    }

    async fn execute(
        &self,
        query: &str,
        params: &[DbValue],
    ) -> Result<DbExecResult, DurableDbError> {
        self.note(query);
        self.inner.execute(query, params).await
    }

    async fn database_size(&self) -> Result<u64, DurableDbError> {
        self.inner.database_size().await
    }
}

/// The meta-table statements one cold `ensure` costs: the read that
/// finds no table, the table's own `CREATE`, the read again, and the
/// write that records the version.
const COLD_COST: usize = 4;

/// What a fresh activation costs on an object already at the expected
/// version: the one read.
const WARM_COST: usize = 1;

/// What a bumped `expected` costs an activation that already verified the
/// old version: the read, and the write that records the bump.
const BUMP_COST: usize = 2;

/// A second `ensure` at a version the cache already covers runs no
/// statement at all; a bumped `expected` checks and records again.
#[skyzen::test]
async fn a_bumped_version_rechecks_and_records() {
    let backend = LoggedDb::open().await;
    let db = DurableDb::new(backend.clone());
    let cache = Cache::default();
    let ddl = &["CREATE TABLE IF NOT EXISTS thing (id INTEGER PRIMARY KEY)"];

    schema_version::ensure(&db, &cache, 2, ddl)
        .await
        .expect("the first check ran");
    assert_eq!(backend.meta_statements(), COLD_COST);

    schema_version::ensure(&db, &cache, 2, ddl)
        .await
        .expect("the cached version answered");
    assert_eq!(
        backend.meta_statements(),
        COLD_COST,
        "a version the object already verified touches nothing"
    );

    schema_version::ensure(&db, &cache, 3, ddl)
        .await
        .expect("the bumped version ran");
    assert_eq!(
        backend.meta_statements(),
        COLD_COST + BUMP_COST,
        "a version past the cached one reads again and records"
    );
    let version: i64 = db
        .query("SELECT version FROM schema_meta WHERE id = 0")
        .fetch_scalar()
        .await
        .expect("the version row exists");
    assert_eq!(version, 3, "the bump was recorded");

    // The read-back above is itself a meta statement; what matters here
    // is that the rechecked version adds none.
    let logged = backend.meta_statements();
    schema_version::ensure(&db, &cache, 3, ddl)
        .await
        .expect("the new version answered");
    assert_eq!(backend.meta_statements(), logged);
}

/// A second activation of an object costs one read, not a check: the
/// object is rebuilt from `Default` around every event, and what it
/// remembers within one event is the memo the poll steps share.
#[skyzen::test]
async fn a_second_activation_costs_one_read() {
    let backend = LoggedDb::open().await;
    let mut object = UserEvents::default();
    let user = UserId::generate();
    let session = SessionId::generate();

    let publish = || {
        let body = PublishEvents {
            session,
            events: vec![EmittedEvent {
                seq: None,
                event: ClientEvent::UserMessage {
                    text: "hello".to_owned(),
                    origin: MessageOrigin::User,
                },
            }],
        };
        let mut request = Request::new(Body::from(serde_json::to_vec(&body).expect("serialize")));
        *request.method_mut() = Method::POST;
        *request.uri_mut() = "https://user-events.flyco.invalid/internal/publish"
            .parse()
            .expect("a valid URL");
        request
            .headers_mut()
            .insert(HEADER_INTERNAL, INTERNAL.parse().expect("a valid header"));
        request.headers_mut().insert(
            HEADER_USER,
            user.to_string().parse().expect("a valid header"),
        );
        request.headers_mut().insert(
            skyzen::header::CONTENT_TYPE,
            skyzen::header::HeaderValue::from_static("application/json"),
        );
        request
            .extensions_mut()
            .insert(DurableDb::new(backend.clone()));
        request
    };

    let response = object
        .fetch()
        .go(publish())
        .await
        .expect("the object answered");
    assert_eq!(response.status().as_u16(), 204, "the publish landed");
    assert_eq!(backend.meta_statements(), COLD_COST);

    // What the runtime does around every event of an object that keeps
    // its state in storage: the next event starts from `Default`.
    object = UserEvents::default();

    let response = object
        .fetch()
        .go(publish())
        .await
        .expect("the object answered");
    assert_eq!(response.status().as_u16(), 204, "the publish landed");
    assert_eq!(
        backend.meta_statements(),
        COLD_COST + WARM_COST,
        "a fresh activation reads the version once and creates nothing"
    );

    // A second call within the same activation answers from the memo.
    let response = object
        .fetch()
        .go(publish())
        .await
        .expect("the object answered");
    assert_eq!(response.status().as_u16(), 204, "the publish landed");
    assert_eq!(
        backend.meta_statements(),
        COLD_COST + WARM_COST,
        "the activation remembered its schema"
    );
}
