//! The discovery commands: `catalog`, `repos`, `branches`, `harnesses`.
//!
//! Each is one route, answered verbatim under `--json` and rendered as a
//! table for a person.

use flyco_core::{BranchPage, HarnessAccountView, HarnessFeature, MachineCatalog, RepoSummary};

use crate::client::Api;
use crate::{Outcome, out};

/// `flyco catalog` — `GET /v1/machines/catalog`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the request fails or the answer does not decode.
pub async fn catalog(api: &Api, mode: out::Mode) -> Outcome<()> {
    let catalog: MachineCatalog = api.get("/v1/machines/catalog").await?;
    match mode {
        out::Mode::Json => out::emit(&catalog),
        out::Mode::Human => {
            let rows: Vec<Vec<String>> = catalog
                .entries
                .iter()
                .map(|entry| {
                    let capacity = entry.capacity.as_ref().map_or_else(
                        || "-".to_owned(),
                        |capacity| format!("{}c/{}GiB", capacity.vcpus, capacity.memory_mib / 1024),
                    );
                    vec![
                        entry.machine_type.clone(),
                        entry.region.clone(),
                        capacity,
                        pricing(entry),
                    ]
                })
                .collect();
            let mut text = out::table(&["MACHINE", "REGION", "SIZE", "PRICE"], &rows);
            if !catalog.pending_accounts.is_empty() {
                use core::fmt::Write as _;
                write!(
                    text,
                    "\n({} account catalogs still loading)",
                    catalog.pending_accounts.len()
                )
                .expect("a String cannot refuse a write");
            }
            out::print(&text)
        }
    }
}

/// `flyco repos` — `GET /v1/github/repos`.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the request fails or the answer does not decode.
pub async fn repos(api: &Api, mode: out::Mode) -> Outcome<()> {
    let repos: Vec<RepoSummary> = api.get("/v1/github/repos").await?;
    match mode {
        out::Mode::Json => out::emit(&repos),
        out::Mode::Human => {
            let rows: Vec<Vec<String>> = repos
                .iter()
                .map(|repo| {
                    vec![
                        repo.slug.to_string(),
                        if repo.private { "private" } else { "public" }.to_owned(),
                        repo.default_branch.to_string(),
                        repo.description.clone().unwrap_or_default(),
                    ]
                })
                .collect();
            out::print(&out::table(
                &["REPO", "VISIBILITY", "BRANCH", "DESCRIPTION"],
                &rows,
            ))
        }
    }
}

/// `flyco branches <o/r>` — `GET /v1/github/repos/{o}/{n}/branches`.
///
/// Pages are followed until the list is complete: a picker's default is
/// the first row of the first page, but an agent asking for the list wants
/// all of it.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the request fails or the answer does not decode.
pub async fn branches(api: &Api, repo: &str, mode: out::Mode) -> Outcome<()> {
    let mut page: BranchPage = api
        .get(&format!("/v1/github/repos/{repo}/branches"))
        .await?;
    let mut branches = page.branches;
    let mut pages = 0_u32;
    while let Some(cursor) = page.next_cursor.take() {
        // A page answering with the cursor it was asked with — and a walk
        // past the page cap — is a server paging forever, not a longer list.
        pages += 1;
        if pages >= crate::follow::MAX_PAGES {
            return Err(crate::follow::paging_stalled());
        }
        page = api
            .get(&format!("/v1/github/repos/{repo}/branches?cursor={cursor}"))
            .await?;
        if page.next_cursor.as_deref() == Some(cursor.as_str()) {
            return Err(crate::follow::paging_stalled());
        }
        branches.extend(page.branches);
    }
    match mode {
        out::Mode::Json => out::emit(&branches),
        out::Mode::Human => {
            let rows: Vec<Vec<String>> = branches
                .iter()
                .map(|branch| {
                    vec![
                        branch.name.to_string(),
                        if branch.is_default { "default" } else { "" }.to_owned(),
                    ]
                })
                .collect();
            out::print(&out::table(&["BRANCH", ""], &rows))
        }
    }
}

/// `flyco harnesses` — the linked accounts plus the feature matrix.
///
/// # Errors
/// Returns [`Failure`](crate::Failure) when the request fails or the answer does not decode.
pub async fn harnesses(api: &Api, mode: out::Mode) -> Outcome<()> {
    let accounts: Vec<HarnessAccountView> = api.get("/v1/harness-accounts").await?;
    let features: Vec<HarnessFeature> = api.get("/v1/harness-features").await?;
    match mode {
        out::Mode::Json => out::emit(&serde_json::json!({
            "accounts": accounts,
            "features": features,
        })),
        out::Mode::Human => {
            let mut text = String::new();
            let rows: Vec<Vec<String>> = accounts
                .iter()
                .map(|account| {
                    vec![
                        serde_json::to_string(&account.harness)
                            .unwrap_or_default()
                            .trim_matches('"')
                            .to_owned(),
                        account.label.clone(),
                        account
                            .models
                            .iter()
                            .map(|model| model.id.clone())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ]
                })
                .collect();
            text.push_str(&out::table(&["HARNESS", "ACCOUNT", "MODELS"], &rows));
            text.push_str("\n\n");
            let rows: Vec<Vec<String>> = features
                .iter()
                .map(|feature| {
                    vec![
                        format!("{:?}", feature.feature),
                        format!("{:?}", feature.claude_code),
                        format!("{:?}", feature.codex),
                    ]
                })
                .collect();
            text.push_str(&out::table(&["FEATURE", "CLAUDE", "CODEX"], &rows));
            out::print(&text)
        }
    }
}

/// A catalog entry's price, as a picker reads it.
pub(crate) fn pricing(entry: &flyco_core::MachineCatalogEntry) -> String {
    use flyco_core::MachinePricing as P;
    match &entry.pricing {
        P::UserOwned => "yours".to_owned(),
        P::Metered {
            on_demand_hourly,
            spot_hourly,
            ..
        } => {
            let spot = spot_hourly.map_or("-".to_owned(), |price| format!("{price}/h spot"));
            format!("{on_demand_hourly}/h · {spot}")
        }
    }
}
