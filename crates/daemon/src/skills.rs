//! Installing the owner's skills into the harness's global skills
//! directory.
//!
//! A skill is a zip uploaded on `Settings → Tools` or installed from a
//! plugin marketplace, stored by the control plane and indexed in
//! `skills`. Nothing pushes them to a machine: the daemon pulls the
//! owner's whole registry at start — before the harness exists, because a
//! harness reads this directory once at launch — and installs the scope
//! its harness will read.
//!
//! The directory is the harness's own: `<CLAUDE_CONFIG_DIR>/skills` for a
//! Claude Code session with an isolated config tree, `<agent
//! home>/.claude/skills` for one inheriting the host's login,
//! `$CODEX_HOME/skills` for Codex. Every skill's directory is rewritten
//! from its bundle on each start, so a replaced skill never lingers.
//! Whether the rest of the directory is reconciled to the registry —
//! directories nothing mounted are removed — depends on whose directory
//! it is: an isolated config tree is flyco's and is made to match
//! exactly, while a skills directory under the agent's own home may hold
//! skills flyco never heard of, and those are left alone.
//!
//! Files land read-only — a skill is changed by uploading a new bundle or
//! installing it from a marketplace, never by the agent editing it in
//! place — the same convention `[acp.files]` secrets use.
//!
//! # What a bundle may contain
//!
//! Everything lands under `<dir>/<name>/`. An entry that would step out
//! of that prefix — `..`, an absolute path, a symlink — fails the session
//! rather than the file, because a bundle is user content being unpacked
//! beside credentials.
//!
//! The wrapper is detected from the archive, not the upload's file name:
//! `__MACOSX` metadata and dot-files at the archive root are dropped, and
//! if the rest of the archive is one directory carrying `SKILL.md` — what
//! `zip -r` on the folder and Finder's *Compress* both produce — it is
//! unwrapped, so `SKILL.md` lands at `<dir>/<name>/SKILL.md` whichever
//! way it was packed.

use std::collections::BTreeSet;
use std::io::{Cursor, Read as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Component, Path, PathBuf};

use flyco_core::{DriverKind, SkillMount, SkillScope};

use crate::config::DaemonConfig;
use crate::control::{ControlApi, ControlApiError};

/// The mode unpacked files are left at: the agent reads a skill, and
/// nobody — the daemon included, until the next start rewrites it — edits
/// one in place.
const FILE_MODE: u32 = 0o444;
/// The mode directories are left at, for the same reason.
const DIR_MODE: u32 = 0o555;
/// The mode a directory is given back while the daemon rewrites or removes
/// it: unlinking an entry takes the write bit on the directory that holds
/// it, and [`DIR_MODE`] took it away.
const WRITABLE_DIR_MODE: u32 = 0o755;

/// The most a single bundle may unpack to, summed over its files.
///
/// The upload route bounds the compressed bundle; this bounds what it
/// inflates to, which is the number the machine actually pays. Enforced
/// *while* reading, entry by entry — a deflated 10 MiB upload can hide
/// gigabytes, and checking after `read_to_end` would be checking the OOM
/// kill that already happened.
const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;

/// The skills directory of the harness this config will run, when it has
/// one.
///
/// `None` is a real answer, not a failure: an ACP agent other than Codex
/// has no global skills directory flyco knows, and its session starts
/// without one rather than with a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Which registry scope belongs in `dir`.
    pub scope: SkillScope,
    /// The harness's global skills directory.
    pub dir: PathBuf,
    /// Whether `dir` is flyco's to reconcile.
    ///
    /// `true` exactly when the directory is an isolated config tree — an
    /// injected `CLAUDE_CONFIG_DIR`, or the `CODEX_HOME` a provisioned
    /// environment sets — where every entry came from the registry and a
    /// directory nothing mounts is a stale skill to remove. `false` under
    /// the agent's own home, where skills that are not flyco's live beside
    /// the ones that are, and pruning would delete them.
    pub reconcile: bool,
}

/// Resolves the skills directory for the harness this config will run.
///
/// The scope comes from the driver, the directory from the harness's own
/// notion of home — Claude's `CLAUDE_CONFIG_DIR` relocates its whole
/// `~/.claude` tree, skills included, so the isolated config dir is the
/// target when the session injects credentials, and the agent's real home
/// when it inherits one. A non-Codex ACP agent gets nothing and a log
/// line saying why.
#[must_use]
pub fn target(config: &DaemonConfig) -> Option<Target> {
    match config.harness {
        DriverKind::ClaudeCode => match config.claude().auth.isolation() {
            Some(isolation) => Some(Target {
                scope: SkillScope::Claude,
                dir: isolation.config_dir.join("skills"),
                reconcile: true,
            }),
            None => Some(Target {
                scope: SkillScope::Claude,
                dir: agent_home()?.join(".claude").join("skills"),
                reconcile: false,
            }),
        },
        DriverKind::Acp => {
            let acp = config.acp();
            if acp.agent != "codex" {
                tracing::info!(
                    agent = %acp.agent,
                    "this ACP agent has no global skills directory; installing no skills"
                );
                return None;
            }
            let (home, reconcile) = match acp.env.get("CODEX_HOME") {
                Some(home) => (PathBuf::from(home), true),
                None => (agent_home()?.join(".codex"), false),
            };
            Some(Target {
                scope: SkillScope::Codex,
                dir: home.join("skills"),
                reconcile,
            })
        }
    }
}

/// The home directory the agent process reads `~` under.
fn agent_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map_or_else(
        || {
            tracing::warn!("HOME is not set; there is no skills directory to install into");
            None
        },
        |home| Some(PathBuf::from(home)),
    )
}

/// Downloads each mounted bundle of the target's scope and installs it
/// under `dir`, then reconciles the directory when it is flyco's.
///
/// The set is reconciled wholesale rather than patched: a skill the user
/// replaced arrives as a fresh bundle under the same name, and a removed
/// one leaves a directory nothing references — which [`Target::reconcile`]
/// deletes on the next boot, when the directory is one flyco owns.
///
/// # Errors
///
/// Returns [`SkillError`] if a bundle could not be fetched or unpacked, or
/// the directory could not be rewritten. The error names the skill whose
/// install failed.
pub async fn install(api: &impl ControlApi, target: &Target) -> Result<(), SkillError> {
    let mounts = api.list_skills().await?;
    tokio::fs::create_dir_all(&target.dir)
        .await
        .map_err(|source| dir_err(&target.dir, source))?;
    let mut keep = BTreeSet::new();
    for mount in mounts.iter().filter(|mount| mount.scope == target.scope) {
        let bundle = api.skill_bundle(mount.id).await?;
        materialize(&target.dir, mount, bundle).await?;
        keep.insert(mount.name.clone());
    }
    if target.reconcile {
        prune(&target.dir, &keep).await?;
    }
    tracing::info!(
        dir = %target.dir.display(),
        skills = keep.len(),
        "installed the owner's skills"
    );
    Ok(())
}

/// Unpacks one bundle into `<dir>/<mount.name>/`.
///
/// Split from [`install`] so the tests exercise the write path without a
/// control plane. Decompression runs on `spawn_blocking`: it is CPU work
/// and nothing else should wait on it.
async fn materialize(dir: &Path, mount: &SkillMount, bundle: Vec<u8>) -> Result<(), SkillError> {
    let skill = mount.clone();
    let files = tokio::task::spawn_blocking(move || unpack(&skill, bundle, MAX_UNPACKED_BYTES))
        .await
        .expect("the bundle unpack task panicked")?;
    write(dir, mount, &files).await
}

/// One file a bundle carries, with its path relative to the skill's own
/// directory.
#[derive(Debug)]
struct Entry {
    path: PathBuf,
    contents: Vec<u8>,
}

/// Reads a bundle into the files it installs, bounded by `limit` summed
/// over every entry.
///
/// Every entry is confined to the skill's directory: `enclosed_name`
/// refuses `..`, the raw name is refused when it carries a root — an
/// absolute path — or any other non-ordinary component (`./` prefixes are
/// tolerated; some archivers emit them), and a symlink is refused
/// outright because there is no target it could name that lands inside.
/// `__MACOSX` metadata and root dot-files are dropped, not installed.
///
/// Whether the archive wraps the skill in a directory is decided by the
/// archive's own shape rather than the skill's name, because the
/// directory inside the zip is whatever the packer called it — often not
/// the registered name, and beside `__MACOSX/` junk on a Finder archive:
/// when no `SKILL.md` sits at the root and every remaining entry shares
/// one root directory that carries `SKILL.md`, that directory is the
/// skill, and it is unwrapped.
fn unpack(mount: &SkillMount, bundle: Vec<u8>, limit: u64) -> Result<Vec<Entry>, SkillError> {
    let bad = |reason: String| SkillError::Bundle {
        skill: mount.name.clone(),
        reason,
    };
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bundle)).map_err(|error| bad(error.to_string()))?;

    let mut files = Vec::new();
    let mut unpacked: u64 = 0;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| bad(error.to_string()))?;
        let name = entry.name().to_owned();
        // `enclosed_name` refuses `..` but *rewrites* an absolute path —
        // `/etc/x` would land at `<dir>/etc/x` — so the raw name is
        // checked too: every component must be ordinary or `.`.
        let confined = entry.enclosed_name().filter(|_| {
            Path::new(&name)
                .components()
                .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
        });
        let Some(path) = confined else {
            return Err(bad(format!(
                "the entry `{name}` would land outside the skill's directory"
            )));
        };
        if entry.is_symlink() {
            return Err(bad(format!("the entry `{name}` is a symlink")));
        }
        if wrapper_junk(&path) || entry.is_dir() {
            continue;
        }
        // Read through the remaining allowance: what comes back can exceed
        // it by at most one byte, and the entry is refused rather than
        // held whole in memory.
        let remaining = limit - unpacked;
        let mut contents = Vec::new();
        entry
            .by_ref()
            .take(remaining + 1)
            .read_to_end(&mut contents)
            .map_err(|error| bad(format!("the entry `{name}` cannot be read: {error}")))?;
        unpacked += contents.len() as u64;
        if unpacked > limit {
            return Err(bad(format!(
                "the entry `{name}` unpacks to more than {limit} bytes"
            )));
        }
        files.push(Entry { path, contents });
    }

    let flat = files.iter().any(|file| file.path == Path::new("SKILL.md"));
    let roots = files
        .iter()
        .filter_map(|file| file.path.components().next())
        .filter_map(|component| match component {
            Component::Normal(component) => Some(component),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let single = if roots.len() == 1 {
        roots.iter().next().map(PathBuf::from)
    } else {
        None
    };
    if !flat
        && let Some(root) = single
        && files.iter().any(|file| file.path == root.join("SKILL.md"))
    {
        for file in &mut files {
            file.path = file
                .path
                .strip_prefix(&root)
                .expect("every file shares the root")
                .to_owned();
        }
    }
    Ok(files)
}

/// Whether an archive entry is wrapper junk rather than skill content:
/// macOS's `__MACOSX` metadata tree, or a dot-file at the archive root
/// (`.DS_Store` and friends).
fn wrapper_junk(path: &Path) -> bool {
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(first)) if first == "__MACOSX" => true,
        Some(Component::Normal(first)) => {
            components.next().is_none() && first.as_encoded_bytes().starts_with(b".")
        }
        _ => false,
    }
}

/// Rewrites one skill's directory from its unpacked files.
async fn write(dir: &Path, mount: &SkillMount, files: &[Entry]) -> Result<(), SkillError> {
    let target = dir.join(&mount.name);
    // Remove first, always: a bundle that dropped a file must not leave it
    // behind for the harness to read.
    clear(&target, &mount.name).await?;
    tokio::fs::create_dir_all(&target)
        .await
        .map_err(|source| write_err(&mount.name, &target, source))?;
    for file in files {
        let path = target.join(&file.path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| write_err(&mount.name, parent, source))?;
        }
        tokio::fs::write(&path, &file.contents)
            .await
            .map_err(|source| write_err(&mount.name, &path, source))?;
    }
    chmod_tree(&target, DIR_MODE, Some(FILE_MODE), &mount.name).await
}

/// Sets `dir_mode` on `root` and every directory beneath it, and
/// `file_mode` — when given — on every other entry.
///
/// One walk serves both directions of the seal: files and dirs read-only
/// after a write (`DIR_MODE`/`FILE_MODE`), dirs writable again before a
/// rewrite or removal (`WRITABLE_DIR_MODE` and `None`, since unlinking an
/// entry takes the write bit on the directory that holds it).
async fn chmod_tree(
    root: &Path,
    dir_mode: u32,
    file_mode: Option<u32>,
    skill: &str,
) -> Result<(), SkillError> {
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        set_mode(&dir, dir_mode, skill).await?;
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .map_err(|source| write_err(skill, &dir, source))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| write_err(skill, &dir, source))?
        {
            let child = entry.path();
            if entry
                .file_type()
                .await
                .map_err(|source| write_err(skill, &child, source))?
                .is_dir()
            {
                stack.push(child);
            } else if let Some(mode) = file_mode {
                set_mode(&child, mode, skill).await?;
            }
        }
    }
    Ok(())
}

/// Removes `path` — the stale skill directory, or a file parked where a
/// skill's directory lands — restoring the write bits the seal took.
async fn clear(path: &Path, skill: &str) -> Result<(), SkillError> {
    match tokio::fs::symlink_metadata(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(write_err(skill, path, source)),
        Ok(metadata) if metadata.is_dir() => {
            chmod_tree(path, WRITABLE_DIR_MODE, None, skill).await?;
            tokio::fs::remove_dir_all(path)
                .await
                .map_err(|source| write_err(skill, path, source))
        }
        Ok(_) => tokio::fs::remove_file(path)
            .await
            .map_err(|source| write_err(skill, path, source)),
    }
}

/// Removes every directory under `dir` that no mount still names.
///
/// Called only for a directory flyco owns — [`Target::reconcile`]; under
/// the agent's own home the same walk would delete skills flyco never
/// heard of.
async fn prune(dir: &Path, keep: &BTreeSet<String>) -> Result<(), SkillError> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(|source| dir_err(dir, source))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|source| dir_err(dir, source))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if keep.contains(&name)
            || !entry
                .file_type()
                .await
                .map_err(|source| dir_err(dir, source))?
                .is_dir()
        {
            continue;
        }
        clear(&entry.path(), &name).await?;
    }
    Ok(())
}

async fn set_mode(path: &Path, mode: u32, skill: &str) -> Result<(), SkillError> {
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .await
        .map_err(|source| write_err(skill, path, source))
}

fn write_err(skill: &str, path: &Path, source: std::io::Error) -> SkillError {
    SkillError::Write {
        skill: skill.to_owned(),
        path: path.to_owned(),
        source,
    }
}

fn dir_err(dir: &Path, source: std::io::Error) -> SkillError {
    SkillError::Dir {
        dir: dir.to_owned(),
        source,
    }
}

/// Skills could not be installed.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// The control plane could not be asked or refused the download.
    ///
    /// A machine that cannot reach the control plane is still a machine
    /// that can run the session — the daemon warns and starts without
    /// skills rather than failing a checkout and a working harness over a
    /// fetch the user cannot fix.
    #[error(transparent)]
    Control(#[from] ControlApiError),
    /// The bundle is not one a machine may unpack.
    ///
    /// The sentence names the skill: a bad upload is the user's to fix,
    /// and which one is the whole of what they need.
    #[error("the `{skill}` skill bundle is not installable: {reason}")]
    Bundle {
        /// The mount the bundle belongs to.
        skill: String,
        /// What is wrong with it.
        reason: String,
    },
    /// A file beneath a skill's directory could not be written or removed.
    #[error("could not install the `{skill}` skill at {path}")]
    Write {
        /// The mount the path belongs to.
        skill: String,
        /// The path the operation failed on.
        path: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
    /// The skills directory itself could not be made or read.
    #[error("could not prepare the skills directory {dir}")]
    Dir {
        /// The directory the operation failed on.
        dir: PathBuf,
        /// The underlying cause.
        #[source]
        source: std::io::Error,
    },
}

impl SkillError {
    /// The wait the control plane named, when the failure was a refusal
    /// carrying `Retry-After`.
    #[must_use]
    pub fn retry_after(&self) -> Option<core::time::Duration> {
        match self {
            Self::Control(error) => error.retry_after(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io::{Cursor, Write as _};
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;

    use flyco_core::{SkillId, SkillMount, SkillScope};

    use super::{materialize, prune, unpack};

    /// A directory of the process's own, removed when the test ends.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "flycod-skills-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&dir).expect("make the test directory");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mount(name: &str) -> SkillMount {
        SkillMount {
            id: SkillId::generate(),
            name: name.to_owned(),
            scope: SkillScope::Claude,
            size_bytes: 0,
        }
    }

    /// Packs `(path, contents)` pairs into a stored zip.
    fn bundle(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (path, contents) in entries {
            writer.start_file(path, options).expect("start the entry");
            writer.write_all(contents).expect("write the entry");
        }
        writer.finish().expect("finish the zip").into_inner()
    }

    /// Packs a zip containing a symlink entry.
    fn symlink_bundle() -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_symlink(
                "skill/link",
                "/etc/passwd",
                zip::write::SimpleFileOptions::default(),
            )
            .expect("start the symlink");
        writer.finish().expect("finish the zip").into_inner()
    }

    #[tokio::test]
    async fn a_bundle_unpacked_into_its_named_directory() {
        let dir = TempDir::new();
        let bytes = bundle(&[("SKILL.md", b"# hello"), ("scripts/run.sh", b"echo hi")]);
        materialize(&dir.0, &mount("skill"), bytes)
            .await
            .expect("install the bundle");
        assert_eq!(
            std::fs::read(dir.0.join("skill/SKILL.md")).expect("read SKILL.md"),
            b"# hello"
        );
        assert_eq!(
            std::fs::read(dir.0.join("skill/scripts/run.sh")).expect("read run.sh"),
            b"echo hi"
        );
    }

    #[tokio::test]
    async fn a_wrapped_bundle_is_unpacked_whatever_the_directory_is_called() {
        let dir = TempDir::new();
        // The folder inside the zip is the packer's name for it, not the
        // registered name — `zip -r` on a renamed folder lands here.
        let bytes = bundle(&[
            ("renamed-folder/SKILL.md", b"# wrapped"),
            ("renamed-folder/lib.py", b"x = 1"),
        ]);
        materialize(&dir.0, &mount("skill"), bytes)
            .await
            .expect("install the wrapped bundle");
        assert_eq!(
            std::fs::read(dir.0.join("skill/SKILL.md")).expect("read SKILL.md"),
            b"# wrapped"
        );
        assert_eq!(
            std::fs::read(dir.0.join("skill/lib.py")).expect("read lib.py"),
            b"x = 1"
        );
        assert!(!dir.0.join("skill/renamed-folder").exists());
    }

    #[tokio::test]
    async fn a_finder_archive_is_unwrapped_and_its_metadata_dropped() {
        let dir = TempDir::new();
        // macOS Compress wraps the folder and adds `__MACOSX` metadata and
        // a root `.DS_Store` beside it.
        let bytes = bundle(&[
            ("folder/SKILL.md", b"# finder"),
            ("__MACOSX/folder/._SKILL.md", b"junk"),
            (".DS_Store", b"junk"),
        ]);
        materialize(&dir.0, &mount("skill"), bytes)
            .await
            .expect("install the Finder bundle");
        assert_eq!(
            std::fs::read(dir.0.join("skill/SKILL.md")).expect("read SKILL.md"),
            b"# finder"
        );
        assert!(!dir.0.join("skill/__MACOSX").exists());
        assert!(!dir.0.join("skill/.DS_Store").exists());
        assert!(!dir.0.join("skill/folder").exists());
    }

    #[tokio::test]
    async fn a_flat_bundle_is_not_unwrapped() {
        let dir = TempDir::new();
        let bytes = bundle(&[("SKILL.md", b"# flat"), ("assets/x.py", b"x")]);
        materialize(&dir.0, &mount("skill"), bytes)
            .await
            .expect("install the flat bundle");
        assert_eq!(
            std::fs::read(dir.0.join("skill/SKILL.md")).expect("read SKILL.md"),
            b"# flat"
        );
        assert!(dir.0.join("skill/assets/x.py").exists());
    }

    #[tokio::test]
    async fn a_bundle_with_two_roots_is_not_unwrapped() {
        let dir = TempDir::new();
        // Two top-level directories is not a wrapped skill — there is no
        // one directory to unwrap, and guessing would install the wrong
        // half. The archive lands as packed.
        let bytes = bundle(&[("a/SKILL.md", b"# a"), ("b/extra.txt", b"b")]);
        materialize(&dir.0, &mount("skill"), bytes)
            .await
            .expect("install the two-root bundle");
        assert!(dir.0.join("skill/a/SKILL.md").exists());
        assert!(dir.0.join("skill/b/extra.txt").exists());
        assert!(!dir.0.join("skill/SKILL.md").exists());
    }

    #[test]
    fn a_dot_prefixed_path_is_not_an_escape() {
        let bytes = bundle(&[("./SKILL.md", b"# dotted")]);
        let files = unpack(&mount("skill"), bytes, 1024).expect("install the bundle");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, PathBuf::from("SKILL.md"));
    }

    #[test]
    fn an_entry_past_the_limit_is_refused_before_it_is_read_whole() {
        // A deflate entry expands far past its stored size; the limit must
        // stop the read, not audit the Vec afterwards.
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        writer
            .start_file("SKILL.md", options)
            .expect("start the entry");
        writer
            .write_all(&vec![b'x'; 4096])
            .expect("write the entry");
        let bytes = writer.finish().expect("finish the zip").into_inner();
        assert!(bytes.len() < 200, "the compressed entry is tiny");
        let error = unpack(&mount("skill"), bytes, 512).expect_err("over the limit");
        let message = error.to_string();
        assert!(message.contains("skill"), "{message}");
        assert!(message.contains("SKILL.md"), "{message}");
    }

    #[tokio::test]
    async fn an_entry_escaping_the_directory_is_refused() {
        let dir = TempDir::new();
        for path in ["../escape", "skill/../../escape", "/etc/escape"] {
            let error = materialize(&dir.0, &mount("skill"), bundle(&[(path, b"x")]))
                .await
                .expect_err(&format!("`{path}` must be refused"));
            let message = error.to_string();
            assert!(message.contains("skill"), "{message}");
            assert!(message.contains(path.trim_start_matches('/')), "{message}");
        }
        assert!(!dir.0.join("escape").exists());
        assert!(!dir.0.join("skill").exists());
    }

    #[tokio::test]
    async fn a_symlink_entry_is_refused() {
        let dir = TempDir::new();
        let error = materialize(&dir.0, &mount("skill"), symlink_bundle())
            .await
            .expect_err("a symlink must be refused");
        let message = error.to_string();
        assert!(message.contains("skill"), "{message}");
        assert!(message.contains("link"), "{message}");
        assert!(!dir.0.join("skill").exists());
    }

    #[tokio::test]
    async fn a_replaced_skill_leaves_nothing_of_the_old_bundle() {
        let dir = TempDir::new();
        materialize(
            &dir.0,
            &mount("skill"),
            bundle(&[("SKILL.md", b"v1"), ("gone.txt", b"old")]),
        )
        .await
        .expect("install v1");
        materialize(&dir.0, &mount("skill"), bundle(&[("SKILL.md", b"v2")]))
            .await
            .expect("install v2");
        assert_eq!(
            std::fs::read(dir.0.join("skill/SKILL.md")).expect("read SKILL.md"),
            b"v2"
        );
        assert!(!dir.0.join("skill/gone.txt").exists());
    }

    #[tokio::test]
    async fn a_directory_no_mount_names_is_removed() {
        let dir = TempDir::new();
        std::fs::create_dir_all(dir.0.join("stale/nested")).expect("plant a stale skill");
        std::fs::write(dir.0.join("stale/nested/file.txt"), b"old").expect("write the file");
        std::fs::write(dir.0.join("stray.txt"), b"not a skill").expect("plant a stray file");
        let keep = BTreeSet::from(["kept".to_owned()]);
        std::fs::create_dir_all(dir.0.join("kept")).expect("plant a mounted skill");
        prune(&dir.0, &keep).await.expect("prune the directory");
        assert!(!dir.0.join("stale").exists());
        assert!(dir.0.join("kept").exists());
        assert!(dir.0.join("stray.txt").exists());
    }

    #[tokio::test]
    async fn installed_files_are_read_only() {
        let dir = TempDir::new();
        materialize(
            &dir.0,
            &mount("skill"),
            bundle(&[("SKILL.md", b"# locked"), ("sub/f.py", b"x")]),
        )
        .await
        .expect("install the bundle");
        let file = std::fs::metadata(dir.0.join("skill/SKILL.md")).expect("stat the file");
        assert_eq!(file.permissions().mode() & 0o777, 0o444);
        let sub = std::fs::metadata(dir.0.join("skill/sub")).expect("stat the dir");
        assert_eq!(sub.permissions().mode() & 0o777, 0o555);
    }
}
