//! Installing the owner's skills into the harness's global skills
//! directory.
//!
//! A skill is a zip the user uploaded on `Settings → Tools`, stored by the
//! control plane and indexed in `skills`. Nothing pushes them to a
//! machine: the daemon pulls the owner's whole registry at start — before
//! the harness exists, because a harness reads this directory once at
//! launch — and installs the scope its harness will read.
//!
//! The directory is the harness's own: `<CLAUDE_CONFIG_DIR>/skills` for a
//! Claude Code session with an isolated config tree, `<agent
//! home>/.claude/skills` for one inheriting the host's login,
//! `$CODEX_HOME/skills` for Codex. Whatever lands inside it is the
//! daemon's: every skill's directory is rewritten from its bundle on each
//! start, and a directory no mount still names is removed, so a replaced
//! or deleted skill never lingers. Files land read-only — an agent
//! publishes a skill through `skill_upload`, never by writing here — the
//! same convention `[acp.files]` secrets use.
//!
//! # What a bundle may contain
//!
//! Everything lands under `<dir>/<name>/`. An entry that would step out
//! of that prefix — `..`, an absolute path, a symlink — fails the session
//! rather than the file, because a bundle is user content being unpacked
//! beside credentials. A bundle wrapped in a single top-level directory
//! of the skill's own name — `zip -r` on the folder — is unwrapped, so
//! `SKILL.md` lands at `<dir>/<name>/SKILL.md` whichever way it was packed.

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
/// inflates to, which is the number the machine actually pays.
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
        DriverKind::ClaudeCode => {
            let dir = match config.claude().auth.isolation() {
                Some(isolation) => isolation.config_dir.join("skills"),
                None => agent_home()?.join(".claude").join("skills"),
            };
            Some(Target {
                scope: SkillScope::Claude,
                dir,
            })
        }
        DriverKind::Acp => {
            let acp = config.acp();
            if acp.agent != "codex" {
                tracing::info!(
                    agent = %acp.agent,
                    "this ACP agent has no global skills directory; installing no skills"
                );
                return None;
            }
            let home = match acp.env.get("CODEX_HOME") {
                Some(home) => PathBuf::from(home),
                None => agent_home()?.join(".codex"),
            };
            Some(Target {
                scope: SkillScope::Codex,
                dir: home.join("skills"),
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
/// under `dir`, then removes what no mount still names.
///
/// The set is reconciled wholesale rather than patched: a skill the user
/// replaced arrives as a fresh bundle under the same name, a removed one
/// leaves a directory nothing references, and a boot after either makes
/// the directory exactly the registry.
///
/// # Errors
///
/// Returns [`SkillError`] if a bundle could not be fetched or unpacked, or
/// the directory could not be rewritten. The error names the skill whose
/// install failed.
pub async fn install(
    api: &impl ControlApi,
    mounts: &[SkillMount],
    target: &Target,
) -> Result<(), SkillError> {
    tokio::fs::create_dir_all(&target.dir)
        .await
        .map_err(|source| SkillError::Dir {
            dir: target.dir.clone(),
            source,
        })?;
    let mut keep = BTreeSet::new();
    for mount in mounts.iter().filter(|mount| mount.scope == target.scope) {
        let bundle = api.skill_bundle(mount.id).await?;
        materialize(&target.dir, mount, bundle).await?;
        keep.insert(mount.name.clone());
    }
    prune(&target.dir, &keep).await?;
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
/// control plane.
async fn materialize(dir: &Path, mount: &SkillMount, bundle: Vec<u8>) -> Result<(), SkillError> {
    let files = unpack(mount, bundle)?;
    write(dir, mount, &files).await
}

/// One file a bundle carries, with its path relative to the skill's own
/// directory.
#[derive(Debug)]
struct Entry {
    path: PathBuf,
    contents: Vec<u8>,
}

/// Reads a bundle into the files it installs.
///
/// Every entry is confined to the skill's directory: `enclosed_name`
/// refuses the paths that would escape it (`..`, absolute paths), and a
/// symlink is refused outright — there is no target it could name that
/// lands inside. A bundle whose every path starts with the skill's own
/// name — `zip -r` on the folder — is unwrapped before the files are
/// returned.
fn unpack(mount: &SkillMount, bundle: Vec<u8>) -> Result<Vec<Entry>, SkillError> {
    let bad = |reason: String| SkillError::Bundle {
        skill: mount.name.clone(),
        reason,
    };
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bundle)).map_err(|error| bad(error.to_string()))?;

    let mut files = Vec::new();
    let mut wrapped = true;
    let mut unpacked: u64 = 0;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| bad(error.to_string()))?;
        let name = entry.name().to_owned();
        // `enclosed_name` refuses `..` but *rewrites* an absolute path —
        // `/etc/x` would land at `<dir>/etc/x` — so the raw name is checked
        // too: every component must be an ordinary one.
        let confined = entry.enclosed_name().filter(|_| {
            Path::new(&name)
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        });
        let Some(path) = confined else {
            return Err(bad(format!(
                "the entry `{name}` would land outside the skill's directory"
            )));
        };
        if entry.is_symlink() {
            return Err(bad(format!("the entry `{name}` is a symlink")));
        }
        if !path.starts_with(&mount.name) {
            wrapped = false;
        }
        if entry.is_dir() {
            continue;
        }
        let mut contents = Vec::new();
        entry
            .read_to_end(&mut contents)
            .map_err(|error| bad(format!("the entry `{name}` cannot be read: {error}")))?;
        unpacked += contents.len() as u64;
        if unpacked > MAX_UNPACKED_BYTES {
            return Err(bad(format!(
                "it unpacks to more than {MAX_UNPACKED_BYTES} bytes"
            )));
        }
        files.push(Entry { path, contents });
    }

    if wrapped {
        let prefix = Path::new(&mount.name);
        for file in &mut files {
            let path = file.path.strip_prefix(prefix).map_err(|_| {
                bad(format!(
                    "the entry `{}` cannot be installed under its own name",
                    file.path.display()
                ))
            })?;
            if path.as_os_str().is_empty() {
                return Err(bad(format!(
                    "the entry `{}` cannot be installed under its own name",
                    file.path.display()
                )));
            }
            file.path = path.to_owned();
        }
    }
    Ok(files)
}

/// Rewrites one skill's directory from its unpacked files.
async fn write(dir: &Path, mount: &SkillMount, files: &[Entry]) -> Result<(), SkillError> {
    let target = dir.join(&mount.name);
    // Remove first, always: a bundle that dropped a file must not leave it
    // behind for the harness to read.
    clear(&target, &mount.name).await?;
    tokio::fs::create_dir_all(&target)
        .await
        .map_err(|source| SkillError::Write {
            skill: mount.name.clone(),
            path: target.clone(),
            source,
        })?;
    for file in files {
        let path = target.join(&file.path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| SkillError::Write {
                    skill: mount.name.clone(),
                    path: parent.to_owned(),
                    source,
                })?;
        }
        tokio::fs::write(&path, &file.contents)
            .await
            .map_err(|source| SkillError::Write {
                skill: mount.name.clone(),
                path: path.clone(),
                source,
            })?;
    }
    seal(&target, &mount.name).await
}

/// Sets every file under `dir` to [`FILE_MODE`] and every directory to
/// [`DIR_MODE`].
///
/// The agent's skills directory is the daemon's work: a skill is changed
/// by uploading a new bundle, never by the agent editing it in place.
async fn seal(dir: &Path, skill: &str) -> Result<(), SkillError> {
    let mut stack = vec![dir.to_owned()];
    while let Some(path) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&path)
            .await
            .map_err(|source| SkillError::Write {
                skill: skill.to_owned(),
                path: path.clone(),
                source,
            })?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| SkillError::Write {
                skill: skill.to_owned(),
                path: path.clone(),
                source,
            })?
        {
            let child = entry.path();
            let is_dir = entry
                .file_type()
                .await
                .map_err(|source| SkillError::Write {
                    skill: skill.to_owned(),
                    path: child.clone(),
                    source,
                })?
                .is_dir();
            if is_dir {
                stack.push(child.clone());
            }
            let mode = if is_dir { DIR_MODE } else { FILE_MODE };
            set_mode(&child, mode, skill).await?;
        }
        set_mode(&path, DIR_MODE, skill).await?;
    }
    Ok(())
}

/// Gives `dir` and every directory beneath it [`WRITABLE_DIR_MODE`], so a
/// sealed tree can be removed.
async fn writable(dir: &Path, skill: &str) -> Result<(), SkillError> {
    let mut stack = vec![dir.to_owned()];
    while let Some(path) = stack.pop() {
        set_mode(&path, WRITABLE_DIR_MODE, skill).await?;
        let mut entries = tokio::fs::read_dir(&path)
            .await
            .map_err(|source| SkillError::Write {
                skill: skill.to_owned(),
                path: path.clone(),
                source,
            })?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| SkillError::Write {
                skill: skill.to_owned(),
                path: path.clone(),
                source,
            })?
        {
            if entry
                .file_type()
                .await
                .map_err(|source| SkillError::Write {
                    skill: skill.to_owned(),
                    path: entry.path(),
                    source,
                })?
                .is_dir()
            {
                stack.push(entry.path());
            }
        }
    }
    Ok(())
}

/// Removes `path` — the stale skill directory, or a file parked where a
/// skill's directory lands — restoring the write bits [`seal`] took.
async fn clear(path: &Path, skill: &str) -> Result<(), SkillError> {
    match tokio::fs::symlink_metadata(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SkillError::Write {
            skill: skill.to_owned(),
            path: path.to_owned(),
            source,
        }),
        Ok(metadata) if metadata.is_dir() => {
            writable(path, skill).await?;
            tokio::fs::remove_dir_all(path)
                .await
                .map_err(|source| SkillError::Write {
                    skill: skill.to_owned(),
                    path: path.to_owned(),
                    source,
                })
        }
        Ok(_) => tokio::fs::remove_file(path)
            .await
            .map_err(|source| SkillError::Write {
                skill: skill.to_owned(),
                path: path.to_owned(),
                source,
            }),
    }
}

/// Removes every directory under `dir` that no mount still names.
async fn prune(dir: &Path, keep: &BTreeSet<String>) -> Result<(), SkillError> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(|source| SkillError::Dir {
            dir: dir.to_owned(),
            source,
        })?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|source| SkillError::Dir {
            dir: dir.to_owned(),
            source,
        })?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if keep.contains(&name)
            || !entry
                .file_type()
                .await
                .map_err(|source| SkillError::Dir {
                    dir: dir.to_owned(),
                    source,
                })?
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
        .map_err(|source| SkillError::Write {
            skill: skill.to_owned(),
            path: path.to_owned(),
            source,
        })
}

/// Skills could not be installed.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// The control plane could not be asked or refused the download.
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

    use super::{materialize, prune};

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
    async fn a_zip_r_style_bundle_is_unwrapped() {
        let dir = TempDir::new();
        let bytes = bundle(&[("skill/SKILL.md", b"# wrapped"), ("skill/lib.py", b"x = 1")]);
        materialize(&dir.0, &mount("skill"), bytes)
            .await
            .expect("install the wrapped bundle");
        assert_eq!(
            std::fs::read(dir.0.join("skill/SKILL.md")).expect("read SKILL.md"),
            b"# wrapped"
        );
        assert!(!dir.0.join("skill/skill").exists());
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
