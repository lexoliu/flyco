//! The two ways into a session's live relay.
//!
//! Both routes end in the same place — the session's [`SessionRoom`] — and
//! differ only in how the caller proves who it is, because the daemon and a
//! browser have very different handshakes available to them.
//!
//! # The daemon: an ordinary bearer credential
//!
//! `flycod` is a native HTTP client, so it sets `Authorization: Bearer fd_…`
//! like every other flyco caller. The token is bound to one session and is
//! checked against the session id in the path.
//!
//! # The browser: a single-use ticket
//!
//! A browser cannot set headers on a WebSocket handshake. The usual answer
//! is to smuggle the credential through `Sec-WebSocket-Protocol` and echo
//! the selected subprotocol back; flyco does not, and now does not by
//! choice. A ticket is a credential minted for one room, spent once, and
//! dead a minute later, which is a smaller thing to leak than a session
//! token in a subprotocol header — and the exchange is one authenticated
//! REST call the browser already has a client for.
//!
//! So the browser exchanges its session token for a relay ticket over
//! ordinary authenticated REST, then opens
//! `wss://…/relay/client?ticket=frt_…`. The ticket lives [one
//! minute](TICKET_TTL_SECONDS), is single-use, and is bound to the session
//! it was minted for.
//!
//! Skyzen can carry headers on a `101` again, so the subprotocol handshake
//! is buildable — it is simply not what flyco does. Changing it would move
//! the browser's authentication mechanism, which is a product decision and
//! not something a version bump should make on its own.

use flyco_core::{CurrentUser, SessionId};
use serde::{Deserialize, Serialize};
use skyzen::extract::Query;
use skyzen_services::{Db, Kv};

use crate::crypto::{prefixed_token, token_hash};
use crate::error::ApiError;
use crate::room::Role;
use crate::rooms::Rooms;
use crate::{clock, daemon_tokens, expiring, sessions};

/// Marks a relay ticket, so a leaked one is recognisable.
pub const TICKET_PREFIX: &str = "frt_";

/// How long a relay ticket may sit unused.
///
/// Long enough for a page to finish loading and open its socket, short
/// enough that a ticket copied out of a URL is worthless by the time anyone
/// reads it.
pub const TICKET_TTL_SECONDS: u64 = 60;

/// What a relay ticket stands for.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ticket {
    /// The room the ticket opens.
    session: SessionId,
}

/// A minted relay ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RelayTicket {
    /// The single-use ticket, to be passed as the `ticket` query parameter
    /// of the client relay route.
    pub ticket: String,
    /// When it stops being accepted, seconds since the Unix epoch.
    pub expires_at_unix: u64,
}

/// Query of the client relay route.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct TicketQuery {
    /// The single-use ticket minted by `POST
    /// /v1/sessions/{id}/relay-ticket`.
    pub ticket: Option<String>,
}

fn kv_key(ticket: &str) -> String {
    let mut key = String::from("relay:ticket:");
    key.push_str(&token_hash(ticket));
    key
}

/// Mints a single-use relay ticket for one of `user`'s sessions.
///
/// # Errors
///
/// Returns [`ApiError::SessionNotFound`] if the session is not the
/// caller's, or [`ApiError`] if entropy is unavailable or KV rejects the
/// write.
pub async fn issue_ticket(
    kv: &Kv,
    db: &Db,
    user: &CurrentUser,
    session: SessionId,
) -> Result<RelayTicket, ApiError> {
    // The ownership check is the authorization: a ticket is a bearer
    // credential for the room, so it may only be minted by someone who
    // could already read the session.
    if !sessions::is_owned_by(db, user.id, session).await? {
        return Err(ApiError::SessionNotFound);
    }

    let ticket = prefixed_token(TICKET_PREFIX)?;
    expiring::put(
        kv,
        &kv_key(&ticket),
        &Ticket { session },
        TICKET_TTL_SECONDS,
    )
    .await?;

    Ok(RelayTicket {
        expires_at_unix: clock::now_unix().saturating_add(TICKET_TTL_SECONDS),
        ticket,
    })
}

/// Redeems a relay ticket, consuming it.
///
/// A ticket is consumed by being *presented*, not by being accepted: the
/// read deletes before the session is even compared, so a ticket replayed
/// by anyone who saw it in a URL, a proxy log, or a `Referer` finds nothing
/// — and there is no set of "failures that do not burn it" to get wrong. A
/// browser that presented one to the wrong room simply mints another.
///
/// # Errors
///
/// Returns [`ApiError::InvalidCredential`] if the ticket is unknown,
/// expired, already used, or was minted for another session.
async fn redeem(kv: &Kv, session: SessionId, presented: &str) -> Result<(), ApiError> {
    if !presented.starts_with(TICKET_PREFIX) {
        return Err(ApiError::InvalidCredential);
    }

    let ticket = expiring::take::<Ticket>(kv, &kv_key(presented))
        .await?
        .ok_or(ApiError::InvalidCredential)?;

    if ticket.session == session {
        Ok(())
    } else {
        Err(ApiError::InvalidCredential)
    }
}

/// Authenticates a daemon's relay upgrade and hands it to the room.
///
/// # Errors
///
/// Returns [`ApiError::InvalidDaemonCredential`] if the token is not this
/// session's, or [`ApiError::RelayUnavailable`] on a build with no rooms.
pub async fn open_daemon(
    rooms: &Rooms,
    db: &Db,
    session: SessionId,
    presented: Option<&str>,
) -> Result<skyzen::Response, ApiError> {
    let presented = presented.ok_or(ApiError::MissingCredential)?;
    if !daemon_tokens::authenticates(db, session, presented).await? {
        return Err(ApiError::InvalidDaemonCredential);
    }

    // A session goes live when its daemon greets the control plane, not when
    // a provider's API returned a machine: a machine that exists is not an
    // agent that is ready. The greeting is the `Hello` frame the room
    // validates on the socket this upgrade becomes — and a Durable Object
    // cannot reach D1, so the durable half of it happens here, on the
    // authenticated hop that carries the daemon into the room.
    sessions::daemon_arrived(db, session).await?;

    tracing::info!(%session, "a daemon joined its session room");
    join(rooms, session, Role::Daemon).await
}

/// Redeems a browser's ticket and hands its upgrade to the room.
///
/// # Errors
///
/// Returns [`ApiError::InvalidCredential`] if the ticket is not a live one
/// for this session, or [`ApiError::RelayUnavailable`] on a build with no
/// rooms.
pub async fn open_client(
    rooms: &Rooms,
    kv: &Kv,
    session: SessionId,
    ticket: Option<&str>,
) -> Result<skyzen::Response, ApiError> {
    let ticket = ticket.ok_or(ApiError::MissingCredential)?;
    redeem(kv, session, ticket).await?;
    tracing::info!(%session, "a browser joined a session room");
    join(rooms, session, Role::Client).await
}

/// Forwards an authenticated upgrade into the session's room.
#[cfg(target_arch = "wasm32")]
async fn join(rooms: &Rooms, session: SessionId, role: Role) -> Result<skyzen::Response, ApiError> {
    rooms.upgrade(session, role).await
}

/// Native builds authenticate the upgrade and then refuse it.
///
/// The credential check above still runs, so the tests can prove a bad
/// token is rejected before anything reaches a room; only the socket itself
/// is unavailable. See [`crate::room`] for why.
#[cfg(not(target_arch = "wasm32"))]
fn join(
    _rooms: &Rooms,
    _session: SessionId,
    _role: Role,
) -> impl core::future::Future<Output = Result<skyzen::Response, ApiError>> + Send {
    core::future::ready(Err(ApiError::RelayUnavailable(
        "a native control plane does not forward relay upgrades into a session room",
    )))
}

/// The ticket a client relay request presented, if any.
#[must_use]
pub fn presented_ticket(query: &Query<TicketQuery>) -> Option<&str> {
    query.0.ticket.as_deref()
}

#[cfg(test)]
mod tests {
    use flyco_core::SessionId;
    use skyzen_services::{Db, Kv};

    use super::{TICKET_PREFIX, issue_ticket, redeem};
    use crate::error::ApiError;
    use crate::testing::{migrate, seed_other_user, seed_session, seed_user};

    #[skyzen::test]
    async fn a_ticket_opens_its_own_session_exactly_once(kv: Kv, db: Db) {
        migrate(&db).await;
        let user = seed_user(&db).await;
        let session = seed_session(&db, &user).await;

        let minted = issue_ticket(&kv, &db, &user, session).await.expect("mint");
        assert!(minted.ticket.starts_with(TICKET_PREFIX));

        redeem(&kv, session, &minted.ticket).await.expect("redeem");
        assert!(
            matches!(
                redeem(&kv, session, &minted.ticket).await,
                Err(ApiError::InvalidCredential)
            ),
            "a redeemed ticket must not open a second socket"
        );
    }

    #[skyzen::test]
    async fn a_ticket_does_not_open_another_session(kv: Kv, db: Db) {
        migrate(&db).await;
        let user = seed_user(&db).await;
        let first = seed_session(&db, &user).await;
        let second = seed_session(&db, &user).await;

        let minted = issue_ticket(&kv, &db, &user, first).await.expect("mint");
        assert!(matches!(
            redeem(&kv, second, &minted.ticket).await,
            Err(ApiError::InvalidCredential)
        ));
    }

    #[skyzen::test]
    async fn an_unknown_or_foreign_shaped_ticket_is_refused(kv: Kv, db: Db) {
        migrate(&db).await;
        let user = seed_user(&db).await;
        let session = seed_session(&db, &user).await;

        for presented in ["frt_never-minted", "fs_a-session-token", ""] {
            assert!(matches!(
                redeem(&kv, session, presented).await,
                Err(ApiError::InvalidCredential)
            ));
        }
    }

    #[skyzen::test]
    async fn only_the_owner_may_mint_a_ticket(kv: Kv, db: Db) {
        migrate(&db).await;
        let owner = seed_user(&db).await;
        let stranger = seed_other_user(&db).await;
        let session = seed_session(&db, &owner).await;

        assert!(matches!(
            issue_ticket(&kv, &db, &stranger, session).await,
            Err(ApiError::SessionNotFound)
        ));
        assert!(matches!(
            issue_ticket(&kv, &db, &owner, SessionId::generate()).await,
            Err(ApiError::SessionNotFound)
        ));
    }
}
