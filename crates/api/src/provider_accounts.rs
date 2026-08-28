//! Linked cloud-provider accounts, the bonus questionnaire, and the cloud
//! usage panel.
//!
//! Flyco provisions into the *user's own* cloud accounts, so linking one is
//! the step between signing in and running anything. Credentials are sealed
//! by [`crate::crypto::TokenCipher`] on the way in and never come back out:
//! no route here returns a credential, and
//! [`ProviderAccountView`](flyco_core::ProviderAccountView) has no field
//! that could hold one.

use flyco_core::{
    CloudProviderKind, CloudUsageView, CurrentUser, LinkProvider, ProviderAccountView,
    ProviderBonusHint, QuickstartAnswers,
};
use serde::Deserialize;
use skyzen::Response;
use skyzen::extract::Query;
use skyzen::routing::{CreateRouteNode, Params, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::Db;

use crate::problem::Outcome;
use crate::respond::Created;

/// Narrows the cloud usage panel to one provider.
#[derive(Debug, Default, Deserialize, skyzen::ToSchema)]
pub struct CloudUsageFilter {
    /// Only accounts with this provider.
    pub provider: Option<CloudProviderKind>,
}

/// Lists the caller's linked cloud-provider accounts.
#[skyzen::openapi]
async fn list_providers(
    State(_user): State<CurrentUser>,
    _db: Db,
) -> Outcome<Json<Vec<ProviderAccountView>>> {
    todo!("M4: list provider_accounts for the caller, without unsealing any credential")
}

/// Links a cloud-provider account, sealing its credentials at rest.
#[skyzen::openapi]
async fn link_provider(
    State(_user): State<CurrentUser>,
    Json(_request): Json<LinkProvider>,
    _db: Db,
) -> Outcome<Created<Json<ProviderAccountView>>> {
    todo!("M4: verify the credentials against the provider, then seal and store them")
}

/// Unlinks a cloud-provider account.
#[skyzen::openapi]
async fn unlink_provider(
    State(_user): State<CurrentUser>,
    _params: Params,
    _db: Db,
) -> Outcome<Response> {
    todo!("M4: delete the account, refusing while a live session still runs on it")
}

/// Suggests free-credit programmes the caller qualifies for.
///
/// Two questions decide it — new customer, student — because those are the
/// two facts every provider's credit programme keys on.
#[skyzen::openapi]
async fn provider_quickstart(
    State(_user): State<CurrentUser>,
    Json(_answers): Json<QuickstartAnswers>,
    _db: Db,
) -> Outcome<Json<Vec<ProviderBonusHint>>> {
    todo!("M4: match the answers against each provider's credit programmes")
}

/// Reports metered cloud spend per linked account.
#[skyzen::openapi]
async fn cloud_usage(
    State(_user): State<CurrentUser>,
    Query(_filter): Query<CloudUsageFilter>,
    _db: Db,
) -> Outcome<Json<Vec<CloudUsageView>>> {
    todo!("M6: read each provider's cost-management API for the current billing period")
}

/// The user-scoped provider-account routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new((
        "/v1/providers".at(list_providers).post(link_provider),
        "/v1/providers/quickstart".post(provider_quickstart),
        "/v1/providers/{id}".delete(unlink_provider),
        "/v1/usage/cloud".at(cloud_usage),
    ))
    .into_route_nodes()
}
