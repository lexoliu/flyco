//! Opening the control plane's SQL database.
//!
//! D1 on the Worker, a local SQLite file natively. This is wired by hand
//! rather than declared as a `[[database]]` in `Skyzen.toml` because skyzen
//! 0.1.2's database codegen does not compile (its generated
//! `if <bool> { WithMiddleware<E, Db> } else { E }` has mismatched arms).

use skyzen_services::Db;

/// Cloudflare binding the D1 database is exposed under.
#[cfg(target_arch = "wasm32")]
pub const BINDING: &str = "DB";

/// Environment variable holding the native SQLite connection string.
#[cfg(not(target_arch = "wasm32"))]
pub const URL_VAR: &str = "FLYCO_DATABASE_URL";

/// Opens the `main` database.
///
/// # Panics
///
/// Panics if the binding is absent or the connection cannot be established:
/// a control plane without its database has nothing to serve.
#[cfg(target_arch = "wasm32")]
pub async fn open() -> Db {
    let env = skyzen::runtime::wasm::current_env()
        .expect("the Cloudflare Workers environment is available during router construction");
    let backend = skyzen_cloudflare::CfD1::from_env(&env, BINDING)
        .unwrap_or_else(|error| panic!("failed to resolve the `{BINDING}` D1 binding: {error}"));
    Db::new(backend)
}

/// Opens the `main` database.
///
/// # Panics
///
/// Panics if `FLYCO_DATABASE_URL` is absent or the connection cannot be
/// established: a control plane without its database has nothing to serve.
#[cfg(not(target_arch = "wasm32"))]
pub async fn open() -> Db {
    let url = std::env::var(URL_VAR)
        .unwrap_or_else(|_| panic!("required configuration `{URL_VAR}` is missing"));
    Db::connect_sqlite(&url)
        .await
        .unwrap_or_else(|error| panic!("failed to open the database at `{url}`: {error}"))
}
