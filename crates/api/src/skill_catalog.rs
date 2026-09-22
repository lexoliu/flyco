//! The skill catalog: what a marketplace offers, and installing one of it.
//!
//! Reading a marketplace is a walk of its tree plus a read of every
//! `SKILL.md` in it — two dozen GitHub calls for the built-in one — so it
//! is not something a request does. Each marketplace has one document in
//! KV, written by the provisioning queue and only read on the request path,
//! exactly as a cloud account's machine catalog is ([`crate::catalog`]).
//!
//! The document is keyed by repository and ref rather than by user, because
//! that is what it describes: two users who added the same marketplace are
//! asking the same question, and the second one gets the first one's
//! answer. Which user's token was used to read it is not part of the
//! answer either — a private marketplace is readable by whoever can see the
//! repository, and one that nobody can see is a failure recorded like any
//! other.
//!
//! # Installing
//!
//! An install copies the skill's directory into the same zipped bundle a
//! hand-uploaded skill is stored as ([`crate::skills`]). Nothing
//! downstream knows a skill came from a marketplace: it is a bundle in
//! object storage and a row in `skills`, and the machine installs it the
//! way it installs any other.
//!
//! The archive is **stored rather than deflated**. A skill is text and a
//! few scripts, the bundle is unpacked on a machine seconds later, and a
//! compression backend is one more thing linked into a Worker.

use core::str::FromStr as _;
use std::collections::BTreeMap;

use flyco_core::{
    CatalogSkill, CurrentUser, InstallCatalogSkill, MarketplaceProblem, RepoSlug, SkillCatalog,
    SkillView, UserId,
};
use serde::{Deserialize, Serialize};
use skyzen::routing::{CreateRouteNode, Route, RouteNode, Routes as _};
use skyzen::utils::{Json, State};
use skyzen_services::{Db, Kv, Queue, Storage};

use crate::clock::now_unix;
use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::expiring;
use crate::github::{GithubClient, GithubOauth, GithubToken, TreeListing};
use crate::marketplaces::{self, Marketplace};
use crate::problem::Outcome;
use crate::provisioning_queue::{self, ProvisioningJob};
use crate::respond::Created;
use crate::skills;
use crate::users;

/// How long a marketplace's document stands before it is read again.
///
/// A marketplace changes when somebody publishes a skill to it, which is
/// not something the user is waiting on; six hours is the same life a
/// machine catalog's document has, for the same reason.
const TTL_SECONDS: u64 = 6 * 60 * 60;

/// How long a marketplace that could not be read stands before flyco tries
/// again.
///
/// Shorter than a success, because the cause is usually a token that has
/// since been reconnected or a repository that has since been shared.
const FAILURE_TTL_SECONDS: u64 = 10 * 60;

/// How long one asked-for read stands for, so a burst of readers asks once.
const REFRESH_CLAIM_SECONDS: u64 = 10 * 60;

/// The most files one skill directory may hold.
///
/// Each one is a GitHub call, and a Worker's subrequest budget is what this
/// protects; a skill is instructions and a few scripts, never two hundred
/// files.
const MAX_SKILL_FILES: usize = 200;

/// The file that makes a directory a skill.
const SKILL_FILE: &str = "SKILL.md";

/// The marketplace manifest, at a fixed path in the repository.
const MANIFEST_PATH: &str = ".claude-plugin/marketplace.json";

// ── The cached document ──

/// What flyco knows about one marketplace at one ref.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketplaceDocument {
    /// Bumped when the shape below changes, so an old document is a miss
    /// rather than a deserialization failure.
    version: u32,
    /// When the read happened, seconds since the Unix epoch.
    read_at_unix: u64,
    /// The skills the marketplace offers.
    skills: Vec<StoredSkill>,
    /// Why the read failed, when it did.
    failure: Option<String>,
}

/// One skill as the document records it: what the picker shows, plus
/// where the files are.
///
/// A plugin may live in a repository other than the marketplace's own, so
/// the location is three fields rather than a path: the same marketplace
/// can offer one skill from its own tree and the next from somebody
/// else's. Recording the ref that was *read* rather than resolving it
/// again at install time is what makes the bundle the thing that was
/// listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredSkill {
    /// The plugin inside the marketplace that carries it.
    plugin: String,
    /// The directory name, which is what it is installed as.
    name: String,
    /// What its `SKILL.md` says it is for.
    description: String,
    /// The repository the files are in, `owner/name`.
    repo: String,
    /// The ref they were read at.
    git_ref: String,
    /// The directory inside that repository.
    path: String,
}

/// The document shape this build writes and accepts.
const DOCUMENT_VERSION: u32 = 1;

/// The KV key one marketplace's document lives under.
fn document_key(marketplace: &Marketplace) -> String {
    format!(
        "skill-catalog:{}@{}",
        marketplace.repo,
        marketplace.git_ref.as_deref().unwrap_or("default")
    )
}

/// The key that says a read is already on its way.
fn claim_key(marketplace: &Marketplace) -> String {
    format!(
        "skill-catalog-refresh:{}@{}",
        marketplace.repo,
        marketplace.git_ref.as_deref().unwrap_or("default")
    )
}

/// Reads one marketplace's document, if it is there and still current.
async fn read_document(
    kv: &Kv,
    marketplace: &Marketplace,
) -> Result<Option<MarketplaceDocument>, ApiError> {
    let document: Option<MarketplaceDocument> =
        expiring::get(kv, &document_key(marketplace)).await?;
    Ok(document.filter(|document| document.version == DOCUMENT_VERSION))
}

/// Asks for a marketplace to be read, unless a read is already on its way.
///
/// Answers whether a job was enqueued. The claim is taken before the
/// message is sent and given back if the send fails, so a burst of readers
/// produces one read and a failed send does not silence the next ask.
///
/// # Errors
///
/// Returns [`ApiError`] if the store or the queue refuses.
pub async fn ask_for_refresh(
    kv: &Kv,
    queue: &Queue,
    user: UserId,
    marketplace: &Marketplace,
) -> Result<bool, ApiError> {
    let key = claim_key(marketplace);
    if expiring::get::<()>(kv, &key).await?.is_some() {
        return Ok(false);
    }
    expiring::put(kv, &key, &(), REFRESH_CLAIM_SECONDS).await?;
    let job = ProvisioningJob::RefreshMarketplace {
        user,
        repo: marketplace.repo.to_string(),
        git_ref: marketplace.git_ref.clone(),
    };
    if let Err(refused) = provisioning_queue::enqueue(queue, job).await {
        expiring::take::<()>(kv, &key).await?;
        return Err(refused);
    }
    Ok(true)
}

// ── Reading a marketplace ──

/// The manifest, in the fields flyco reads.
#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    metadata: ManifestMetadata,
    #[serde(default)]
    plugins: Vec<ManifestPlugin>,
}

/// The manifest's `metadata` block.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestMetadata {
    /// Directory a bare plugin source resolves under.
    #[serde(default)]
    plugin_root: Option<String>,
}

/// One plugin of the manifest.
#[derive(Debug, Deserialize)]
struct ManifestPlugin {
    name: String,
    /// Where the plugin's files are.
    #[serde(default)]
    source: Option<Source>,
    /// Paths to the plugin's skill directories, when it names them.
    #[serde(default)]
    skills: Option<serde_json::Value>,
}

/// A plugin's `source`: a path in this repository, or an object naming
/// where else the files are.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Source {
    /// `"./plugins/x"`, or a bare name under `metadata.pluginRoot`.
    Path(String),
    /// One of the object forms the marketplace format defines.
    Elsewhere(RemoteSource),
}

/// The object forms of a plugin `source`.
///
/// Three of them name a Git repository, and flyco follows those to GitHub.
/// The rest — an npm package, a zip somewhere, a command run on the
/// machine — describe files that are not in a repository flyco can read,
/// so a plugin published that way offers no skills here.
#[derive(Debug, Deserialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
enum RemoteSource {
    /// `{ "source": "github", "repo": "owner/name" }`.
    Github {
        repo: String,
        #[serde(default, rename = "ref")]
        git_ref: Option<String>,
        #[serde(default)]
        sha: Option<String>,
    },
    /// `{ "source": "url", "url": "https://github.com/owner/name.git" }`.
    Url {
        url: String,
        #[serde(default, rename = "ref")]
        git_ref: Option<String>,
        #[serde(default)]
        sha: Option<String>,
    },
    /// The same, with the plugin in a directory of that repository.
    GitSubdir {
        url: String,
        path: String,
        #[serde(default, rename = "ref")]
        git_ref: Option<String>,
        #[serde(default)]
        sha: Option<String>,
    },
    /// `npm`, `archive`, `command`, and whatever the format gains next.
    #[serde(other)]
    NotARepository,
}

/// Where one plugin's files are: a repository, a ref, and a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PluginSource {
    /// The repository holding them.
    repo: RepoSlug,
    /// The ref the source pins, or `None` to follow the repository's own
    /// default — which, for the marketplace's own repository, is the ref
    /// the marketplace is read at.
    git_ref: Option<String>,
    /// The plugin's directory inside that repository, `""` for its root.
    root: String,
}

/// Normalizes a repository path: no `./`, no leading or trailing slash.
fn clean_path(path: &str) -> String {
    path.trim()
        .trim_start_matches("./")
        .trim_matches('/')
        .to_owned()
}

/// A JSON field that is either one string or an array of them.
fn strings(value: Option<&serde_json::Value>) -> Vec<String> {
    match value {
        Some(serde_json::Value::String(one)) => vec![one.clone()],
        Some(serde_json::Value::Array(many)) => many
            .iter()
            .filter_map(|item| item.as_str().map(ToOwned::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// The `owner/name` of a GitHub clone URL, or `None` for another host.
///
/// A marketplace may point a plugin at any Git server; flyco reads files
/// through the GitHub API and through nothing else, so one hosted
/// elsewhere is a plugin it cannot open.
fn github_slug(url: &str) -> Option<RepoSlug> {
    let rest = url
        .trim()
        .strip_prefix("https://github.com/")
        .or_else(|| url.trim().strip_prefix("http://github.com/"))
        .or_else(|| url.trim().strip_prefix("git@github.com:"))?
        .trim_end_matches('/');
    RepoSlug::from_str(rest.strip_suffix(".git").unwrap_or(rest)).ok()
}

/// Where a plugin's files are, or `None` when flyco cannot reach them.
///
/// A relative source is a directory in the marketplace itself; a bare name
/// is one under the manifest's `pluginRoot`; `github`, `url` and
/// `git-subdir` name another GitHub repository, which costs one more tree
/// read per plugin and is followed. An npm package, an archive, a command
/// or a repository on another Git host is not something flyco can open, so
/// the plugin contributes nothing.
fn plugin_source(
    plugin: &ManifestPlugin,
    root: Option<&str>,
    marketplace: &RepoSlug,
) -> Option<PluginSource> {
    /// A ref the source pins: an exact commit first, then a branch or tag.
    fn pinned(sha: Option<&String>, git_ref: Option<&String>) -> Option<String> {
        sha.or(git_ref).cloned()
    }

    match plugin.source.as_ref()? {
        Source::Path(source) if source.starts_with("./") => Some(PluginSource {
            repo: marketplace.clone(),
            git_ref: None,
            root: clean_path(source),
        }),
        Source::Path(bare) => {
            let root = clean_path(root?);
            let bare = clean_path(bare);
            Some(PluginSource {
                repo: marketplace.clone(),
                git_ref: None,
                root: if root.is_empty() {
                    bare
                } else {
                    format!("{root}/{bare}")
                },
            })
        }
        Source::Elsewhere(RemoteSource::Github { repo, git_ref, sha }) => Some(PluginSource {
            repo: RepoSlug::from_str(repo.trim()).ok()?,
            git_ref: pinned(sha.as_ref(), git_ref.as_ref()),
            root: String::new(),
        }),
        Source::Elsewhere(RemoteSource::Url { url, git_ref, sha }) => Some(PluginSource {
            repo: github_slug(url)?,
            git_ref: pinned(sha.as_ref(), git_ref.as_ref()),
            root: String::new(),
        }),
        Source::Elsewhere(RemoteSource::GitSubdir {
            url,
            path,
            git_ref,
            sha,
        }) => Some(PluginSource {
            repo: github_slug(url)?,
            git_ref: pinned(sha.as_ref(), git_ref.as_ref()),
            root: clean_path(path),
        }),
        Source::Elsewhere(RemoteSource::NotARepository) => None,
    }
}

/// Joins a plugin's root and one of its declared skill paths.
fn under(root: &str, path: &str) -> String {
    let path = clean_path(path);
    if root.is_empty() {
        path
    } else if path.is_empty() {
        root.to_owned()
    } else {
        format!("{root}/{path}")
    }
}

/// Every skill directory one plugin describes, inside its own repository.
///
/// A plugin that names its skill directories is taken at its word; one that
/// does not gets the default layout, `<root>/skills/<name>/`.
fn skill_paths(plugin: &ManifestPlugin, root: &str, tree: &TreeListing) -> Vec<String> {
    let declared = strings(plugin.skills.as_ref());
    let mut found: Vec<String> = Vec::new();
    if declared.is_empty() {
        let prefix = under(root, "skills");
        let marker = format!("/{SKILL_FILE}");
        for entry in &tree.entries {
            if entry.is_file
                && entry.path.starts_with(&format!("{prefix}/"))
                && entry.path.ends_with(&marker)
            {
                let directory = entry.path.trim_end_matches(&marker).to_owned();
                // Only a skill directly under `skills/`, never one nested
                // inside another skill's files.
                if directory.matches('/').count() == prefix.matches('/').count() + 1 {
                    found.push(directory);
                }
            }
        }
    } else {
        for path in declared {
            found.push(under(root, &path));
        }
    }
    found.sort();
    found.dedup();
    found
}

/// The `description` of a `SKILL.md`'s YAML frontmatter.
///
/// Deliberately not a YAML parser: the frontmatter of a skill is two or
/// three scalar fields, the one that is read here is a single line, and a
/// parser for the rest would be a dependency carried for nothing. A
/// description flyco cannot find is an empty one, never a refusal — the
/// skill is installable either way.
fn frontmatter_description(body: &str) -> String {
    let mut lines = body.lines();
    if lines.next().map(str::trim) != Some("---") {
        return String::new();
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("description:") {
            return value.trim().trim_matches(['"', '\'']).to_owned();
        }
    }
    String::new()
}

/// Reads one marketplace and writes its document.
///
/// The queue's side of the catalog. Every failure is recorded *in* the
/// document rather than returned: "read, and GitHub refused" is an answer
/// the picker shows, and a job that failed instead would be redelivered
/// against a repository that is still not readable.
///
/// # Errors
///
/// Returns [`ApiError`] only if the store refuses the write, which is the
/// one failure that leaves nothing recorded.
pub async fn refresh(
    db: &Db,
    config: &ApiConfig,
    kv: &Kv,
    github: &impl GithubOauth,
    user: UserId,
    marketplace: &Marketplace,
) -> Result<(), ApiError> {
    let read = read_marketplace(db, config, github, user, marketplace).await;
    let (skills, failure, ttl) = match read {
        Ok(skills) => (skills, None, TTL_SECONDS),
        Err(refusal) => {
            tracing::warn!(repo = %marketplace.repo, %refusal, "could not read a marketplace");
            (Vec::new(), Some(refusal), FAILURE_TTL_SECONDS)
        }
    };
    let document = MarketplaceDocument {
        version: DOCUMENT_VERSION,
        read_at_unix: now_unix(),
        skills,
        failure,
    };
    expiring::put(kv, &document_key(marketplace), &document, ttl).await?;
    tracing::info!(
        repo = %marketplace.repo,
        skills = document.skills.len(),
        failed = document.failure.is_some(),
        "read a marketplace"
    );
    Ok(())
}

/// What a marketplace offers, or a sentence saying why flyco cannot tell.
async fn read_marketplace(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    user: UserId,
    marketplace: &Marketplace,
) -> Result<Vec<StoredSkill>, String> {
    let token = users::github_token(db, config, github, user)
        .await
        .map_err(|error| error.to_string())?;
    let git_ref = resolve_ref(github, &token, marketplace)
        .await
        .map_err(|error| error.to_string())?;

    let tree = github
        .read_tree(&token, &marketplace.repo, &git_ref)
        .await
        .map_err(|error| error.to_string())?;
    if tree.truncated {
        return Err(format!(
            "{} is too large for flyco to read as a marketplace",
            marketplace.repo
        ));
    }

    let manifest = github
        .read_file(&token, &marketplace.repo, &git_ref, MANIFEST_PATH)
        .await
        .map_err(|_| {
            format!(
                "{} has no {MANIFEST_PATH}, so it is not a plugin marketplace",
                marketplace.repo
            )
        })?;
    let manifest: Manifest = serde_json::from_slice(&manifest)
        .map_err(|error| format!("{MANIFEST_PATH} is not a marketplace manifest: {error}"))?;

    // One tree per repository the manifest reaches into, read once and
    // shared by every plugin that points at it.
    let mut trees: Vec<(String, TreeListing)> =
        vec![(format!("{}@{git_ref}", marketplace.repo), tree)];
    let plugin_root = manifest.metadata.plugin_root.clone();

    let mut skills: Vec<StoredSkill> = Vec::new();
    for plugin in &manifest.plugins {
        let Some(source) = plugin_source(plugin, plugin_root.as_deref(), &marketplace.repo) else {
            continue;
        };
        let source_ref = match &source.git_ref {
            Some(pinned) => pinned.clone(),
            None if source.repo == marketplace.repo => git_ref.clone(),
            None => match github.get_repo(&token, &source.repo).await {
                Ok(repo) => repo.default_branch.to_string(),
                Err(error) => {
                    // One plugin flyco cannot open is not a marketplace it
                    // cannot read: the rest of them still have skills.
                    tracing::warn!(repo = %source.repo, %error, "skipped a plugin's repository");
                    continue;
                }
            },
        };

        let key = format!("{}@{source_ref}", source.repo);
        if !trees.iter().any(|(known, _)| *known == key) {
            match github.read_tree(&token, &source.repo, &source_ref).await {
                Ok(listing) if !listing.truncated => trees.push((key.clone(), listing)),
                Ok(_) => {
                    tracing::warn!(repo = %source.repo, "skipped a plugin repository too large to read");
                    continue;
                }
                Err(error) => {
                    tracing::warn!(repo = %source.repo, %error, "skipped a plugin's repository");
                    continue;
                }
            }
        }
        let listing = trees
            .iter()
            .find(|(known, _)| *known == key)
            .map(|(_, listing)| listing)
            .expect("the tree was just read or already held");

        for path in skill_paths(plugin, &source.root, listing) {
            let marker = format!("{path}/{SKILL_FILE}");
            if !listing
                .entries
                .iter()
                .any(|entry| entry.is_file && entry.path == marker)
            {
                continue;
            }
            let Some(name) = path.rsplit('/').next().map(ToOwned::to_owned) else {
                continue;
            };
            // The name becomes a directory on the machine, so a skill flyco
            // could not install is not offered.
            if skills::checked_name(&name).is_err() {
                continue;
            }
            let body = github
                .read_file(&token, &source.repo, &source_ref, &marker)
                .await
                .map_err(|error| error.to_string())?;
            skills.push(StoredSkill {
                plugin: plugin.name.clone(),
                name,
                description: frontmatter_description(&String::from_utf8_lossy(&body)),
                repo: source.repo.to_string(),
                git_ref: source_ref.clone(),
                path,
            });
        }
    }
    // By name, not by the plugin that happens to carry it: the picker
    // lists a marketplace's skills, and which plugin groups them is
    // bookkeeping the reader did not ask about.
    skills.sort_by(|left, right| (&left.name, &left.plugin).cmp(&(&right.name, &right.plugin)));
    skills.dedup_by(|left, right| left.name == right.name && left.plugin == right.plugin);
    Ok(skills)
}

/// The ref a marketplace is read at: the one pinned, else the repository's
/// default branch.
async fn resolve_ref(
    github: &impl GithubOauth,
    token: &GithubToken,
    marketplace: &Marketplace,
) -> Result<String, ApiError> {
    match &marketplace.git_ref {
        Some(pinned) => Ok(pinned.clone()),
        None => Ok(github
            .get_repo(token, &marketplace.repo)
            .await?
            .default_branch
            .to_string()),
    }
}

// ── Routes ──

/// Lists what every marketplace of the caller offers.
#[skyzen::openapi]
async fn list_catalog_skills(
    State(user): State<CurrentUser>,
    db: Db,
    kv: Kv,
    queue: Queue,
) -> Outcome<Json<SkillCatalog>> {
    catalog(&db, &kv, &queue, user.id).await.map(Json).into()
}

/// Reads each marketplace's document, asking for the missing ones.
///
/// A marketplace with no document has not been read yet, which is a
/// different answer from one that was read and offers nothing: the first
/// becomes skills in a few seconds and the second is a fact about the
/// repository. They are kept apart all the way out to the API.
async fn catalog(db: &Db, kv: &Kv, queue: &Queue, user: UserId) -> Result<SkillCatalog, ApiError> {
    let mut catalog = SkillCatalog {
        skills: Vec::new(),
        pending: Vec::new(),
        failed: Vec::new(),
    };
    for marketplace in marketplaces::all(db, user).await? {
        let repo = marketplace.repo.to_string();
        let Some(document) = read_document(kv, &marketplace).await? else {
            ask_for_refresh(kv, queue, user, &marketplace).await?;
            catalog.pending.push(repo);
            continue;
        };
        if let Some(detail) = document.failure {
            catalog.failed.push(MarketplaceProblem {
                marketplace: repo,
                detail,
            });
        } else {
            catalog
                .skills
                .extend(document.skills.into_iter().map(|skill| CatalogSkill {
                    marketplace: repo.clone(),
                    plugin: skill.plugin,
                    name: skill.name,
                    description: skill.description,
                }));
        }
    }
    Ok(catalog)
}

/// Installs one catalog skill.
#[skyzen::openapi]
async fn install_catalog_skill(
    State(user): State<CurrentUser>,
    State(config): State<ApiConfig>,
    State(github): State<GithubClient>,
    Json(request): Json<InstallCatalogSkill>,
    db: Db,
    kv: Kv,
    storage: Storage,
) -> Outcome<Created<Json<SkillView>>> {
    install(&db, &config, &github, &kv, &storage, user.id, request)
        .await
        .map(|view| Created(Json(view)))
        .into()
}

/// Copies the skill's directory out of its marketplace and stores it.
async fn install(
    db: &Db,
    config: &ApiConfig,
    github: &impl GithubOauth,
    kv: &Kv,
    storage: &Storage,
    user: UserId,
    request: InstallCatalogSkill,
) -> Result<SkillView, ApiError> {
    let marketplace = marketplaces::all(db, user)
        .await?
        .into_iter()
        .find(|candidate| candidate.repo.to_string() == request.marketplace.trim())
        .ok_or(ApiError::MarketplaceNotFound)?;
    let document =
        read_document(kv, &marketplace)
            .await?
            .ok_or_else(|| ApiError::SkillCatalogNotReady {
                repo: marketplace.repo.to_string(),
            })?;
    let skill = document
        .skills
        .iter()
        .find(|candidate| {
            candidate.name == request.name.trim() && candidate.plugin == request.plugin.trim()
        })
        .ok_or_else(|| ApiError::CatalogSkillNotFound {
            name: request.name.trim().to_owned(),
        })?;
    // The document holds the slug it read; a row whose slug no longer
    // parses is one flyco wrote, so it is a bug rather than input.
    let repo = RepoSlug::from_str(&skill.repo).map_err(|_| ApiError::CatalogSkillNotFound {
        name: request.name.trim().to_owned(),
    })?;

    let token = users::github_token(db, config, github, user).await?;
    let bundle = bundle(github, &token, &repo, &skill.git_ref, &skill.path).await?;

    let installed = skills::store(db, storage, user, &skill.name, &bundle).await?;
    tracing::info!(
        marketplace = %marketplace.repo,
        repo = %skill.repo,
        skill = %skill.name,
        bytes = bundle.len(),
        "installed a skill from a marketplace"
    );
    Ok(installed)
}

/// Builds the skill's bundle: every file under its directory, at the root
/// of the archive.
///
/// Root-level rather than nested under the skill's own name, because the
/// bundle *is* the skill's directory — a machine unpacks it into one named
/// by the row, and a second level would put `SKILL.md` one directory too
/// deep.
async fn bundle(
    github: &impl GithubOauth,
    token: &GithubToken,
    repo: &RepoSlug,
    git_ref: &str,
    path: &str,
) -> Result<Vec<u8>, ApiError> {
    let tree = github.read_tree(token, repo, git_ref).await?;
    let prefix = format!("{path}/");
    let files: Vec<&crate::github::TreeEntry> = tree
        .entries
        .iter()
        .filter(|entry| entry.is_file && entry.path.starts_with(&prefix))
        .collect();

    if files.is_empty() {
        return Err(ApiError::CatalogSkillNotFound {
            name: path.to_owned(),
        });
    }
    if files.len() > MAX_SKILL_FILES {
        return Err(ApiError::InvalidSkill("a skill may hold at most 200 files"));
    }
    let total: u64 = files.iter().map(|entry| entry.size).sum();
    if total > skills::MAX_BUNDLE_BYTES as u64 {
        return Err(ApiError::InvalidSkill("the skill is larger than 10 MiB"));
    }

    let mut bodies: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for entry in files {
        let body = github.read_file(token, repo, git_ref, &entry.path).await?;
        let relative = entry.path[prefix.len()..].to_owned();
        bodies.insert(relative, body);
    }
    write_archive(&bodies)
}

/// Packs the files into a stored (uncompressed) zip.
fn write_archive(files: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>, ApiError> {
    use std::io::Write as _;

    /// Every failure here is the same one: flyco could not produce an
    /// archive out of files it just read, which is a bug rather than
    /// anything the user typed.
    const CORRUPT: ApiError = ApiError::InvalidSkill("the skill could not be packed");

    let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (path, body) in files {
        archive
            .start_file(path.as_str(), options)
            .map_err(|_| CORRUPT)?;
        archive.write_all(body).map_err(|_| CORRUPT)?;
    }
    Ok(archive.finish().map_err(|_| CORRUPT)?.into_inner())
}

/// The user-scoped catalog routes.
pub fn routes() -> Vec<RouteNode> {
    Route::new(("/v1/catalog/skills"
        .at(list_catalog_skills)
        .post(install_catalog_skill),))
    .into_route_nodes()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::github::{TreeEntry, TreeListing};

    use super::{
        Manifest, PluginSource, RepoSlug, frontmatter_description, plugin_source, skill_paths,
        write_archive,
    };
    use core::str::FromStr as _;

    fn tree(paths: &[&str]) -> TreeListing {
        TreeListing {
            entries: paths
                .iter()
                .map(|path| TreeEntry {
                    path: (*path).to_owned(),
                    is_file: !path.ends_with('/'),
                    size: 10,
                })
                .collect(),
            truncated: false,
        }
    }

    /// Every skill directory a whole manifest describes, as the reader
    /// walks it: plugin by plugin, each inside its own root.
    fn every_skill(
        manifest: &Manifest,
        market: &RepoSlug,
        tree: &TreeListing,
    ) -> Vec<(String, String)> {
        let root = manifest.metadata.plugin_root.as_deref();
        let mut found = Vec::new();
        for plugin in &manifest.plugins {
            let Some(source) = plugin_source(plugin, root, market) else {
                continue;
            };
            if source.repo != *market {
                continue;
            }
            for path in skill_paths(plugin, &source.root, tree) {
                found.push((plugin.name.clone(), path));
            }
        }
        found.sort();
        found
    }

    fn market() -> RepoSlug {
        RepoSlug::from_str("example/marketplace").expect("a slug")
    }

    #[test]
    fn a_manifest_that_names_its_skills_is_taken_at_its_word() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"name":"anthropic-agent-skills","plugins":[
                {"name":"document-skills","source":"./","skills":["./skills/xlsx","./skills/pdf"]},
                {"name":"claude-api","source":"./","skills":"./skills/claude-api"}
            ]}"#,
        )
        .expect("a manifest");
        assert_eq!(
            every_skill(&manifest, &market(), &tree(&[])),
            vec![
                ("claude-api".to_owned(), "skills/claude-api".to_owned()),
                ("document-skills".to_owned(), "skills/pdf".to_owned()),
                ("document-skills".to_owned(), "skills/xlsx".to_owned()),
            ]
        );
    }

    #[test]
    fn a_plugin_without_a_skills_field_gets_the_default_layout() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"name":"m","metadata":{"pluginRoot":"./plugins"},"plugins":[{"name":"tools","source":"formatter"}]}"#,
        )
        .expect("a manifest");
        let listing = tree(&[
            "plugins/formatter/skills/lint/SKILL.md",
            "plugins/formatter/skills/lint/rules.md",
            "plugins/formatter/skills/format/SKILL.md",
            // Nested one level too deep: a file of a skill, not a skill.
            "plugins/formatter/skills/format/examples/SKILL.md",
            "plugins/other/skills/nope/SKILL.md",
        ]);
        assert_eq!(
            every_skill(&manifest, &market(), &listing),
            vec![
                (
                    "tools".to_owned(),
                    "plugins/formatter/skills/format".to_owned()
                ),
                (
                    "tools".to_owned(),
                    "plugins/formatter/skills/lint".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn a_plugin_in_another_github_repository_is_followed_there() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"name":"m","plugins":[
                {"name":"by-slug","source":{"source":"github","repo":"owner/other","ref":"v2"}},
                {"name":"by-url","source":{"source":"url","url":"https://github.com/owner/third.git"}},
                {"name":"in-a-subdir","source":{"source":"git-subdir","url":"https://github.com/acme/monorepo.git","path":"tools/plugin","sha":"9f1c0de"}}
            ]}"#,
        )
        .expect("a manifest");
        let sources: Vec<PluginSource> = manifest
            .plugins
            .iter()
            .filter_map(|plugin| plugin_source(plugin, None, &market()))
            .collect();
        assert_eq!(
            sources,
            vec![
                PluginSource {
                    repo: RepoSlug::from_str("owner/other").expect("a slug"),
                    git_ref: Some("v2".to_owned()),
                    root: String::new(),
                },
                PluginSource {
                    repo: RepoSlug::from_str("owner/third").expect("a slug"),
                    git_ref: None,
                    root: String::new(),
                },
                PluginSource {
                    repo: RepoSlug::from_str("acme/monorepo").expect("a slug"),
                    // An exact commit wins over a branch, and here it is
                    // the only thing the source pins.
                    git_ref: Some("9f1c0de".to_owned()),
                    root: "tools/plugin".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn a_plugin_that_is_not_in_a_github_repository_is_left_out() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"name":"m","plugins":[
                {"name":"npm","source":{"source":"npm","package":"@acme/p"}},
                {"name":"zip","source":{"source":"archive","url":"https://example.com/p.zip"}},
                {"name":"shell","source":{"source":"command","command":"p --path"}},
                {"name":"gitlab","source":{"source":"url","url":"https://gitlab.com/team/p.git"}},
                {"name":"nameless","source":"bare-without-a-plugin-root"}
            ]}"#,
        )
        .expect("a manifest");
        assert!(
            manifest
                .plugins
                .iter()
                .all(|plugin| plugin_source(plugin, None, &market()).is_none())
        );
    }

    #[test]
    fn a_description_comes_out_of_the_frontmatter() {
        let body =
            "---\nname: mcp-builder\ndescription: Guide for creating MCP servers.\n---\n\n# Body\n";
        assert_eq!(
            frontmatter_description(body),
            "Guide for creating MCP servers."
        );
        assert_eq!(frontmatter_description("# No frontmatter\n"), "");
        assert_eq!(frontmatter_description("---\nname: x\n---\n"), "");
        assert_eq!(
            frontmatter_description("---\ndescription: \"quoted\"\n---\n"),
            "quoted"
        );
    }

    #[test]
    fn an_archive_holds_every_file_at_its_own_path() {
        let mut files = BTreeMap::new();
        files.insert("SKILL.md".to_owned(), b"---\nname: x\n---\n".to_vec());
        files.insert("scripts/run.sh".to_owned(), b"echo hi\n".to_vec());
        let packed = write_archive(&files).expect("packed");
        assert_eq!(&packed[..4], b"PK\x03\x04");

        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(packed)).expect("a readable archive");
        assert_eq!(archive.len(), 2);
        let names: Vec<String> = archive.file_names().map(ToOwned::to_owned).collect();
        assert!(names.contains(&"SKILL.md".to_owned()));
        assert!(names.contains(&"scripts/run.sh".to_owned()));
        let mut entry = archive.by_name("scripts/run.sh").expect("the script");
        let mut body = String::new();
        std::io::Read::read_to_string(&mut entry, &mut body).expect("read it back");
        assert_eq!(body, "echo hi\n");
    }
}
