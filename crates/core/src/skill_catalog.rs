//! The skill catalog: plugin marketplaces, read for the picker.
//!
//! A skill is a directory with a `SKILL.md` in it, and the open way to
//! publish a set of them is a Claude Code plugin marketplace: a GitHub
//! repository with `.claude-plugin/marketplace.json` naming its plugins and
//! the skill directories each one carries. flyco reads those repositories
//! rather than hosting a registry of its own — `anthropics/skills` is built
//! in, and the user may add any other — and an install copies the chosen
//! directory into the same zipped bundle a hand-uploaded skill is stored
//! as, so nothing downstream knows where a skill came from.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::id::MarketplaceId;

/// The marketplace every user has, which is not a row and cannot be removed.
pub const BUILT_IN_MARKETPLACE: &str = "anthropics/skills";

/// One row of `GET /v1/marketplaces`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MarketplaceView {
    /// Identifier, absent for the built-in marketplace.
    pub id: Option<MarketplaceId>,
    /// The GitHub repository, `owner/name`.
    pub repo: String,
    /// The branch or tag read, when the user pinned one. Absent reads the
    /// repository's default branch.
    pub git_ref: Option<String>,
    /// Whether flyco provides it, in which case it cannot be removed.
    pub built_in: bool,
    /// When it was added, seconds since the Unix epoch. Absent for the
    /// built-in one, which nobody added.
    pub added_at_unix: Option<u64>,
}

/// Request body of `POST /v1/marketplaces`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AddMarketplace {
    /// The GitHub repository, `owner/name`.
    pub repo: String,
    /// A branch or tag to pin. Omitted reads the default branch.
    #[serde(default)]
    pub git_ref: Option<String>,
}

/// One skill a marketplace offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CatalogSkill {
    /// The marketplace repository it comes from.
    pub marketplace: String,
    /// The plugin inside that marketplace that carries it.
    pub plugin: String,
    /// The directory name, which is what it is installed as.
    pub name: String,
    /// What its `SKILL.md` says it is for.
    pub description: String,
}

/// A marketplace flyco could not read, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MarketplaceProblem {
    /// The repository that could not be read.
    pub marketplace: String,
    /// What went wrong, in a sentence the user can act on.
    pub detail: String,
}

/// Response of `GET /v1/catalog/skills`.
///
/// A marketplace is either read, still being read, or unreadable, and the
/// three are kept apart: a picker that showed "no skills" for a repository
/// it had not looked at yet would be lying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SkillCatalog {
    /// Every skill flyco can install, marketplace by marketplace.
    pub skills: Vec<CatalogSkill>,
    /// Marketplaces still being read.
    pub pending: Vec<String>,
    /// Marketplaces that could not be read.
    pub failed: Vec<MarketplaceProblem>,
}

/// Request body of `POST /v1/catalog/skills`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InstallCatalogSkill {
    /// The marketplace repository, as the catalog listed it.
    pub marketplace: String,
    /// The plugin that carries it, as the catalog listed it. Two plugins
    /// of one marketplace may publish a skill under the same name, so the
    /// name alone does not say which directory to copy.
    pub plugin: String,
    /// The skill's directory name, as the catalog listed it.
    pub name: String,
}
