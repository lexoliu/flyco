//! `flyco handoff` — send a local harness session's working state to a
//! fresh cloud session.
//!
//! A handoff is not a resume: the cloud session opens a new harness
//! conversation whose first message is a handoff brief — the local
//! session's own summary of its work, an explicit fresh-environment
//! notice, and the path map between the sender's workdir and
//! [`flyco_core::SESSION_WORKDIR`]. The working tree goes as a `git diff
//! --binary` patch against the local merge-base; the full transcript
//! goes verbatim and lands on the machine at
//! [`flyco_core::HANDOFF_TRANSCRIPT_PATH`] for the agent to consult.
//!
//! The summary is a model-written one — asked of the local session
//! through the harness's own non-interactive surface (`claude -p
//! --resume`, `codex app-server`'s `turn/start`, `devin --print
//! --resume`)
//! rather than `flyco` writing one. A native `compact` does not answer
//! the purpose: Claude's works, but Codex records compaction as an
//! encrypted context item with no readable text, so the uniform answer
//! is a handoff-shaped summary prompt each harness answers in its own
//! session.

use std::ffi::OsStr;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use flyco_core::{HandoffManifest, HarnessKind, LocalHandoff, SessionDetail, SessionSource};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Command;

use crate::cli::SessionSpec;
use crate::client::Api;
use crate::{Exit, Failure, Outcome, out, pick};

/// What `flyco handoff` was asked, once clap has parsed it.
#[derive(Debug)]
pub struct Args {
    /// Restrict the source search to one harness; absent means all three.
    pub from: Option<HarnessKind>,
    /// The local session id to send; picked or newest when absent.
    pub session: Option<String>,
    /// The harness the cloud session runs; absent means the source's own.
    pub harness: Option<HarnessKind>,
    /// An extra instruction appended to the handoff brief.
    pub message: Option<String>,
    /// Skip the model-written summary; the transcript alone carries over.
    pub no_summary: bool,
    /// Include untracked-but-unignored files in the patch.
    pub include_untracked: bool,
    /// The shared session-creation flags (`--budget`, `--machine`, …).
    pub spec: SessionSpec,
}

/// The longest a harness may take to write its summary. A long session's
/// summary turn is a full model call — minutes, not seconds.
const SUMMARY_TIMEOUT: core::time::Duration = core::time::Duration::from_mins(15);

/// The summary ask itself, sent to the local session through its own
/// harness. Deliberately handoff-shaped rather than a generic compact:
/// the answer is for the agent that continues the work, so it names what
/// a continuation needs — state, decisions, paths, next steps — and
/// forbids tool calls so the session is summarized, not worked on.
const SUMMARY_PROMPT: &str = "You are being handed off to a fresh cloud machine to continue \
this work. Write a handoff brief for the agent that takes over. Cover: the goal; key decisions \
already made and why; the current state — what is done, what is in progress, what is broken; \
the important file paths; how to build and test; and the next steps. Answer from this \
conversation alone — do not run tools. Output the brief as plain markdown and nothing else.";

/// `flyco handoff`.
///
/// Order matters: the summary turn lands in the local transcript, so it
/// runs before the transcript is snapshotted; the patch is generated
/// against the merge-base the session's provenance names, so the cloud
/// checkout can rewind to it and apply.
///
/// # Errors
/// Returns [`Failure`] on usage errors, discovery or summarization
/// failures, git failures, or any failed request.
pub async fn handoff(api: &Api, args: Args, mode: out::Mode) -> Outcome<Exit> {
    if args.spec.prompt.is_some() || args.spec.prompt_file.is_some() {
        return Err(Failure::usage(
            "a handoff writes its own prompt — extra instructions go in `--message`",
        ));
    }

    let repo = local_repo(args.spec.repo.as_deref(), args.spec.branch.as_deref()).await?;
    let session = pick_session(args.from, args.session.as_deref(), &repo.root).await?;
    note(
        mode,
        &format!(
            "handing off {} session `{}` (worked in {})",
            label(session.harness),
            session.session_id,
            session.cwd.display()
        ),
    );

    let patch = repo.patch(args.include_untracked).await?;
    let summary = if args.no_summary {
        None
    } else {
        note(mode, "asking the session to summarize itself");
        Some(summarize(&session).await?)
    };
    let transcript = snapshot(&session.transcript).await?;

    let brief = Brief {
        source: &session,
        repo: &repo,
        summary: summary.as_deref(),
        message: args.message.as_deref(),
    }
    .render();
    let mut spec = args.spec;
    let env = std::mem::take(&mut spec.env);
    if spec.repo.is_none() {
        spec.repo = Some(repo.slug.clone());
    }
    if interactive() {
        fill_spec(api, &mut spec).await?;
    }
    let mut request = spec.to_request(args.harness.unwrap_or(session.harness), brief)?;
    request.repo = repo.slug.clone();
    request.branch = Some(repo.branch.clone());
    request.source = Some(SessionSource::LocalHandoff(LocalHandoff {
        harness: session.harness,
        session_id: session.session_id.clone(),
        base_commit: repo.base_commit.clone(),
        local_workdir: repo.root.display().to_string(),
    }));

    let headers: &[(&str, &str)] = match &spec.idempotency_key {
        Some(key) => &[("Idempotency-Key", key.as_str())],
        None => &[],
    };
    let created: SessionDetail = api
        .post_with_headers("/v1/sessions", &request, headers)
        .await?;
    let id = created.summary.id;
    crate::session::set_env(api, &id, &env).await?;
    note(mode, &format!("session {id} — uploading the handoff"));

    // A failure past this point leaves a pending handoff; the session is
    // archived so it cannot sit holding a reserved machine until the
    // hour-long sweep finds it.
    let uploaded = upload(api, id, &patch, &transcript).await;
    let _ = tokio::fs::remove_file(&transcript).await;
    if let Err(failure) = uploaded {
        let _ = api
            .post::<serde_json::Value, serde_json::Value>(
                &format!("/v1/sessions/{id}/archive"),
                &serde_json::json!({}),
            )
            .await;
        return Err(failure);
    }

    match mode {
        out::Mode::Json => out::emit(&created)?,
        out::Mode::Human => {
            out::print(&format!(
                "{id}  handed off — provisioning once the payloads land"
            ))?;
        }
    }
    Ok(Exit::Ok)
}

/// The two uploads and the manifest that frees provisioning.
async fn upload(
    api: &Api,
    id: flyco_core::SessionId,
    patch: &[u8],
    transcript: &Path,
) -> Outcome<()> {
    api.put_bytes(&format!("/v1/sessions/{id}/handoff/patch"), patch.to_vec())
        .await?;
    api.put_file(&format!("/v1/sessions/{id}/handoff/transcript"), transcript)
        .await?;
    api.post::<HandoffManifest, serde_json::Value>(
        &format!("/v1/sessions/{id}/handoff/complete"),
        &HandoffManifest {
            patch_sha256: hex::encode(Sha256::digest(patch)),
            patch_bytes: patch.len() as u64,
            transcript_sha256: sha256_file(transcript).await?,
            transcript_bytes: tokio::fs::metadata(transcript)
                .await
                .map_err(|error| Failure::transport(format!("{}: {error}", transcript.display())))?
                .len(),
        },
    )
    .await?;
    Ok(())
}

/// A local git worktree's half of the handoff: where it is, what the
/// remote calls it, and the commit the patch diffs against.
struct LocalRepo {
    /// `git rev-parse --show-toplevel`.
    root: PathBuf,
    /// `owner/name`, parsed off `origin`.
    slug: String,
    /// The remote branch the cloud session clones.
    branch: String,
    /// `git merge-base HEAD origin/<branch>` — what the patch is taken
    /// against and what the daemon rewinds the clone to.
    base_commit: String,
}

impl LocalRepo {
    /// The working tree's difference from the base commit, as a binary
    /// patch.
    ///
    /// Tracked-only is plain `git diff <base>` — worktree against commit
    /// for every path the index knows. `--include-untracked` instead
    /// stages the tree into a scratch index (`GIT_INDEX_FILE`, so the
    /// real index is never touched) and diffs that — `git add -A`
    /// respects `.gitignore`, so ignored files stay out either way.
    async fn patch(&self, include_untracked: bool) -> Outcome<Vec<u8>> {
        if !include_untracked {
            return Ok(
                git(&self.root, &["diff", "--binary", &self.base_commit, "--"])
                    .await?
                    .stdout,
            );
        }
        let index =
            std::env::temp_dir().join(format!("flyco-handoff-{}.index", std::process::id()));
        let staged = async {
            git_env(
                &self.root,
                &[OsStr::new("read-tree"), OsStr::new(&self.base_commit)],
                &index,
            )
            .await?;
            git_env(&self.root, &[OsStr::new("add"), OsStr::new("-A")], &index).await?;
            git_env(
                &self.root,
                &[
                    OsStr::new("diff"),
                    OsStr::new("--cached"),
                    OsStr::new("--binary"),
                    OsStr::new(&self.base_commit),
                    OsStr::new("--"),
                ],
                &index,
            )
            .await
        }
        .await;
        let _ = std::fs::remove_file(&index);
        Ok(staged?.stdout)
    }
}

/// Resolves the repository facts a handoff is taken from.
///
/// The branch must exist on `origin`: the cloud session clones it, and
/// the merge-base — the commit the patch diffs against — only exists on
/// the remote side if it is an ancestor of a pushed branch. A local-only
/// branch is not refused outright: it can still hand off with `--branch`
/// naming a pushed branch, in which case its unpushed commits ride inside
/// the patch.
async fn local_repo(repo: Option<&str>, branch: Option<&str>) -> Outcome<LocalRepo> {
    let root = PathBuf::from(
        git_text(Path::new("."), &["rev-parse", "--show-toplevel"])
            .await
            .map_err(|_| Failure::usage("`flyco handoff` runs inside a git worktree"))?,
    );
    let slug = if let Some(slug) = repo {
        slug.to_owned()
    } else {
        let url = git_text(&root, &["remote", "get-url", "origin"])
            .await
            .map_err(|_| {
                Failure::usage("no `origin` remote to infer the repo from — pass `--repo`")
            })?;
        repo_slug(&url).ok_or_else(|| {
            Failure::usage(format!(
                "cannot read owner/name out of `origin` ({url}) — pass `--repo`"
            ))
        })?
    };
    let branch = match branch {
        Some(branch) => branch.to_owned(),
        None => git_text(&root, &["branch", "--show-current"])
            .await
            .map_err(|_| {
                Failure::usage("HEAD is detached — name the remote branch with `--branch`")
            })?,
    };
    let remote_ref = format!("refs/remotes/origin/{branch}");
    let exists = git(&root, &["rev-parse", "--verify", "--quiet", &remote_ref])
        .await
        .is_ok();
    if !exists {
        return Err(Failure::usage(format!(
            "`origin/{branch}` is not known locally — `git fetch`, or the branch is local-only \
             and `--branch` should name a pushed one"
        )));
    }
    let base_commit = git_text(&root, &["merge-base", "HEAD", &remote_ref])
        .await
        .map_err(|_| {
            Failure::usage(format!(
                "HEAD and `origin/{branch}` share no commit — nothing to base a patch on"
            ))
        })?;
    Ok(LocalRepo {
        root,
        slug,
        branch,
        base_commit,
    })
}

/// `git@github.com:owner/name`, `https://github.com/owner/name`, or
/// `ssh://git@github.com/owner/name` → `owner/name`.
fn repo_slug(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches(".git").trim_end_matches('/');
    let path = if let Some(scp) = url.strip_prefix("git@") {
        scp.split(':').nth(1)?
    } else {
        let path = url.split_once("://").map_or(url, |(_, rest)| rest);
        path.split_once('/')?.1
    };
    let mut parts = path.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    if parts.next().is_some() || owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// One local harness session that could be handed off.
struct LocalSession {
    /// The harness it ran under — recorded as the handoff's provenance.
    harness: HarnessKind,
    /// The harness-native id: Claude's session UUID, Codex's thread id,
    /// Devin's session name.
    session_id: String,
    /// The transcript file to upload verbatim.
    transcript: PathBuf,
    /// The directory the session ran in — where its summarizer spawns.
    cwd: PathBuf,
    /// The transcript's mtime — the picker's recency sort.
    modified: std::time::SystemTime,
    /// A human label for the picker: a title where the harness has one.
    title: String,
}

/// Finds the local session to send.
///
/// `--session` names it outright. Otherwise every installed harness is
/// scanned for sessions whose recorded cwd sits inside the worktree; one
/// candidate is taken, several on a TTY go to a picker, and several on a
/// pipe are a usage error naming `--session` — an agent running `handoff`
/// on its own session always knows its id.
async fn pick_session(
    from: Option<HarnessKind>,
    named: Option<&str>,
    root: &Path,
) -> Outcome<LocalSession> {
    if let Some(id) = named {
        for harness in [
            HarnessKind::ClaudeCode,
            HarnessKind::Codex,
            HarnessKind::Devin,
        ] {
            if from.is_some_and(|wanted| wanted != harness) {
                continue;
            }
            if let Some(session) = find_named(harness, id, root).await? {
                return Ok(session);
            }
        }
        return Err(Failure::problem(
            Exit::NotFoundOrConflict,
            format!("no local session `{id}` — is the harness installed and the id right?"),
        ));
    }

    let mut candidates = Vec::new();
    for harness in [
        HarnessKind::ClaudeCode,
        HarnessKind::Codex,
        HarnessKind::Devin,
    ] {
        if from.is_some_and(|wanted| wanted != harness) {
            continue;
        }
        candidates.extend(discover(harness, root).await?);
    }
    candidates.sort_by_key(|session| std::cmp::Reverse(session.modified));
    match candidates.len() {
        0 => Err(Failure::problem(
            Exit::NotFoundOrConflict,
            "no local claude, codex or devin session has ever run in this worktree",
        )),
        1 => Ok(candidates.into_iter().next().expect("one")),
        _ if interactive() => {
            let chosen = pick::pick("Hand off which session", &candidates, |session| {
                format!(
                    "{}  {}  {}",
                    label(session.harness),
                    session.session_id,
                    session.title
                )
            })?;
            Ok(candidates.into_iter().nth(chosen).expect("picked in range"))
        }
        _ => Err(Failure::usage(format!(
            "{} local sessions could be handed off — name one with `--session` or one harness \
             with `--from`",
            candidates.len()
        ))),
    }
}

/// A picker may open when the command's stdin is a terminal.
fn interactive() -> bool {
    std::io::stdin().is_terminal()
}

/// The session-creation flags a handoff still needs answered on a TTY —
/// budget and machine — the same asks the launch flow makes. Off a TTY
/// they stay flags: `to_request` turns a missing one into usage.
async fn fill_spec(api: &Api, spec: &mut SessionSpec) -> Outcome<()> {
    if spec.machine.is_none() {
        // `pick_machine` answers account+type+region+spot+disk at once;
        // spread its answer over the flags `to_request` reads.
        let machine = crate::human::pick_machine(api, spec).await?;
        spec.account = Some(machine.provider_account.to_string());
        spec.machine = Some(machine.machine_type);
        spec.region = Some(machine.region);
        spec.disk = Some(machine.disk_gib);
        if !machine.spot {
            spec.on_demand = true;
        }
    }
    if spec.budget.is_none() {
        let text = pick::input("Budget for the session, in dollars", Some("10"))?;
        spec.budget = Some(crate::cli::parse_usd(&text).map_err(Failure::usage)?);
    }
    Ok(())
}

/// The `~`-rooted store a harness keeps its sessions in.
fn home() -> PathBuf {
    std::env::home_dir().expect("a home directory")
}

/// Every candidate session `harness` has inside `root`.
async fn discover(harness: HarnessKind, root: &Path) -> Outcome<Vec<LocalSession>> {
    match harness {
        HarnessKind::ClaudeCode => discover_claude(root).await,
        HarnessKind::Codex => discover_codex(root).await,
        HarnessKind::Devin => discover_devin(root).await,
    }
}

/// Locates the explicitly named session of one harness.
async fn find_named(harness: HarnessKind, id: &str, root: &Path) -> Outcome<Option<LocalSession>> {
    match harness {
        HarnessKind::ClaudeCode => {
            let dir = home().join(".claude/projects");
            let Some(mut projects) = read_dir(&dir).await else {
                return Ok(None);
            };
            while let Some(project) = entries_next(&mut projects).await {
                let path = project.path().join(format!("{id}.jsonl"));
                if path.is_file() {
                    return claude_session(&path, root, Some(id)).await;
                }
            }
            Ok(None)
        }
        HarnessKind::Codex => {
            // A rollout filename ends in the thread id: the directory walk
            // finds it without opening a single file.
            let Some(session) = codex_rollout(id).await? else {
                return Ok(None);
            };
            codex_session(&session, root, Some(id)).await
        }
        HarnessKind::Devin => {
            let transcript = devin_transcript(id);
            if !transcript.is_file() {
                return Ok(None);
            }
            let modified = mtime(&transcript).await;
            Ok(Some(LocalSession {
                harness: HarnessKind::Devin,
                session_id: id.to_owned(),
                transcript,
                cwd: root.to_path_buf(),
                modified,
                title: id.to_owned(),
            }))
        }
    }
}

/// Claude sessions live one file each in `~/.claude/projects/<enc-cwd>/`,
/// where `enc-cwd` is the launch directory with every `/`, `.` and `_`
/// folded to `-`. Only project dirs whose encoding could sit inside the
/// worktree are opened — a session's own `cwd` field is the authority, so
/// the prefix is only a read filter, and a foreign project's unreadable
/// transcript is never this command's problem.
async fn discover_claude(root: &Path) -> Outcome<Vec<LocalSession>> {
    let dir = home().join(".claude/projects");
    let Some(mut projects) = read_dir(&dir).await else {
        return Ok(Vec::new());
    };
    let encoded = claude_project_dir(root);
    let mut found = Vec::new();
    while let Some(project) = entries_next(&mut projects).await {
        if !project.file_name().to_string_lossy().starts_with(&encoded) {
            continue;
        }
        let Some(mut files) = read_dir(&project.path()).await else {
            continue;
        };
        while let Some(file) = entries_next(&mut files).await {
            let path = file.path();
            if path.extension() != Some(OsStr::new("jsonl")) {
                continue;
            }
            if let Some(session) = claude_session(&path, root, None).await? {
                found.push(session);
            }
        }
    }
    Ok(found)
}

/// `~/.claude/projects/` encodes a session's launch directory by folding
/// every path separator, dot, and underscore to `-`.
fn claude_project_dir(dir: &Path) -> String {
    dir.display()
        .to_string()
        .chars()
        .map(|c| match c {
            '/' | '.' | '_' => '-',
            other => other,
        })
        .collect()
}

/// A Claude transcript qualifies when its recorded cwd is inside the
/// worktree — or it is the explicitly named one, where the caller vouches.
async fn claude_session(
    path: &Path,
    root: &Path,
    named: Option<&str>,
) -> Outcome<Option<LocalSession>> {
    let mut cwd = None;
    let mut title = String::new();
    // The first lines suffice: `cwd` rides every event, and a
    // `custom-title` entry is early when one exists.
    for line in head_lines(path, 64 * 1024).await? {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if cwd.is_none()
            && let Some(dir) = entry.get("cwd").and_then(|value| value.as_str())
        {
            cwd = Some(PathBuf::from(dir));
        }
        if title.is_empty()
            && let Some(name) = entry.get("customTitle").and_then(|value| value.as_str())
        {
            name.clone_into(&mut title);
        }
        if cwd.is_some() && !title.is_empty() {
            break;
        }
    }
    let Some(cwd) = cwd else {
        return Ok(None);
    };
    if named.is_none() && !cwd.starts_with(root) {
        return Ok(None);
    }
    let session_id = path
        .file_stem()
        .and_then(OsStr::to_str)
        .expect("a jsonl filename")
        .to_owned();
    Ok(Some(LocalSession {
        harness: HarnessKind::ClaudeCode,
        title: if title.is_empty() {
            session_id.clone()
        } else {
            title
        },
        session_id,
        transcript: path.to_path_buf(),
        cwd,
        modified: mtime(path).await,
    }))
}

/// Codex sessions live in `~/.codex/sessions/<y>/<m>/<d>/rollout-*-<id>.jsonl`;
/// the first line is a `session_meta` event carrying `id` and `cwd`.
async fn discover_codex(root: &Path) -> Outcome<Vec<LocalSession>> {
    let dir = home().join(".codex/sessions");
    let Some(mut years) = read_dir(&dir).await else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    while let Some(year) = entries_next(&mut years).await {
        let Some(mut months) = read_dir(&year.path()).await else {
            continue;
        };
        while let Some(month) = entries_next(&mut months).await {
            let Some(mut days) = read_dir(&month.path()).await else {
                continue;
            };
            while let Some(day) = entries_next(&mut days).await {
                let Some(mut files) = read_dir(&day.path()).await else {
                    continue;
                };
                while let Some(file) = entries_next(&mut files).await {
                    let path = file.path();
                    if path.extension() != Some(OsStr::new("jsonl")) {
                        continue;
                    }
                    if let Some(session) = codex_session(&path, root, None).await? {
                        found.push(session);
                    }
                }
            }
        }
    }
    Ok(found)
}

/// Finds a rollout file by the thread id its filename ends in.
async fn codex_rollout(id: &str) -> Outcome<Option<PathBuf>> {
    let dir = home().join(".codex/sessions");
    let Some(mut years) = read_dir(&dir).await else {
        return Ok(None);
    };
    let suffix = format!("{id}.jsonl");
    while let Some(year) = entries_next(&mut years).await {
        let Some(mut months) = read_dir(&year.path()).await else {
            continue;
        };
        while let Some(month) = entries_next(&mut months).await {
            let Some(mut days) = read_dir(&month.path()).await else {
                continue;
            };
            while let Some(day) = entries_next(&mut days).await {
                let Some(mut files) = read_dir(&day.path()).await else {
                    continue;
                };
                while let Some(file) = entries_next(&mut files).await {
                    if file.file_name().to_string_lossy().ends_with(&suffix) {
                        return Ok(Some(file.path()));
                    }
                }
            }
        }
    }
    Ok(None)
}

/// A Codex rollout qualifies when its `session_meta.cwd` sits inside the
/// worktree — or it was named explicitly.
async fn codex_session(
    path: &Path,
    root: &Path,
    named: Option<&str>,
) -> Outcome<Option<LocalSession>> {
    let Some(first) = head_lines(path, 64 * 1024).await?.into_iter().next() else {
        return Ok(None);
    };
    let Ok(line) = serde_json::from_str::<serde_json::Value>(&first) else {
        return Ok(None);
    };
    let meta = &line["payload"];
    let (Some(id), Some(cwd)) = (
        meta["id"].as_str().map(str::to_owned),
        meta["cwd"].as_str().map(PathBuf::from),
    ) else {
        return Ok(None);
    };
    if let Some(named) = named
        && id != named
    {
        return Ok(None);
    }
    if named.is_none() && !cwd.starts_with(root) {
        return Ok(None);
    }
    Ok(Some(LocalSession {
        harness: HarnessKind::Codex,
        session_id: id.clone(),
        transcript: path.to_path_buf(),
        cwd,
        modified: mtime(path).await,
        title: id,
    }))
}

/// Devin sessions are listed by `devin list --format json` scoped to a
/// directory; the transcript each names lands in
/// `~/.local/share/devin/cli/transcripts/<id>.json`.
async fn discover_devin(root: &Path) -> Outcome<Vec<LocalSession>> {
    #[derive(Deserialize)]
    struct Listed {
        id: String,
        working_directory: PathBuf,
        last_activity_at: Option<u64>,
        title: Option<String>,
    }
    let output = Command::new("devin")
        .args(["list", "--format", "json"])
        .current_dir(root)
        .output()
        .await;
    let Ok(output) = output else {
        // devin not installed is no error — it just has no candidates.
        return Ok(Vec::new());
    };
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let Ok(listed) = serde_json::from_slice::<Vec<Listed>>(&output.stdout) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for session in listed {
        if !session.working_directory.starts_with(root) {
            continue;
        }
        let transcript = devin_transcript(&session.id);
        if !transcript.is_file() {
            continue;
        }
        found.push(LocalSession {
            harness: HarnessKind::Devin,
            title: session.title.clone().unwrap_or_else(|| session.id.clone()),
            session_id: session.id,
            transcript,
            cwd: session.working_directory,
            modified: session
                .last_activity_at
                .map_or(std::time::SystemTime::UNIX_EPOCH, |at| {
                    std::time::UNIX_EPOCH + core::time::Duration::from_secs(at)
                }),
        });
    }
    Ok(found)
}

fn devin_transcript(id: &str) -> PathBuf {
    home().join(format!(".local/share/devin/cli/transcripts/{id}.json"))
}

/// The session's own summary of its work, written by its own harness.
async fn summarize(session: &LocalSession) -> Outcome<String> {
    let summary = match session.harness {
        HarnessKind::ClaudeCode => claude_summary(session).await,
        HarnessKind::Codex => codex_summary(session).await,
        HarnessKind::Devin => devin_summary(session).await,
    }?;
    let summary = summary.trim().to_owned();
    if summary.is_empty() {
        return Err(Failure::problem(
            Exit::Problem,
            "the session wrote an empty summary — `--no-summary` hands off without one",
        ));
    }
    Ok(summary)
}

/// `claude -p --output-format json --resume <id> <prompt>`: the reply is
/// the JSON document's `result`.
async fn claude_summary(session: &LocalSession) -> Outcome<String> {
    let output = run_timed(
        Command::new("claude")
            .args([
                "-p",
                "--output-format",
                "json",
                "--resume",
                &session.session_id,
                SUMMARY_PROMPT,
            ])
            .current_dir(&session.cwd),
    )
    .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        return Err(command_failed("claude", &output));
    }
    // `result` is the reply text; anything else means a version drifted —
    // answer with the raw text rather than failing on a shape we can
    // still read.
    Ok(serde_json::from_str::<serde_json::Value>(&stdout)
        .ok()
        .and_then(|document| {
            document["result"]
                .as_str()
                .or_else(|| document["text"].as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| stdout.into_owned()))
}

/// `devin --resume=<id> --print=<prompt>`: the reply is stdout verbatim.
/// Both flags take optional values, so the `=` form keeps the prompt from
/// being read as a positional path.
async fn devin_summary(session: &LocalSession) -> Outcome<String> {
    let output = run_timed(
        Command::new("devin")
            .args([
                format!("--resume={}", session.session_id),
                format!("--print={SUMMARY_PROMPT}"),
            ])
            .current_dir(&session.cwd),
    )
    .await?;
    if !output.status.success() {
        return Err(command_failed("devin", &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Codex's summary rides `codex app-server`'s newline-JSON-RPC: resume
/// the thread, run one read-only turn asking for the brief, and take its
/// last agent message — from `turn/completed` when the server inlines the
/// items, else from the rollout's own `task_complete` record.
async fn codex_summary(session: &LocalSession) -> Outcome<String> {
    let mut child = Command::new("codex")
        .arg("app-server")
        .current_dir(&session.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| Failure::transport(format!("codex app-server: {error}")))?;
    let mut stdin = child.stdin.take().expect("piped");
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();

    let result = tokio::time::timeout(
        SUMMARY_TIMEOUT,
        codex_summary_turn(session, &mut stdin, &mut lines),
    )
    .await
    .map_err(|_| Failure::problem(Exit::Timeout, "codex did not answer the summary turn"))
    .and_then(|done| done);
    // A held-open child outlives nothing: kill it on the way out whether
    // the turn answered or not.
    let _ = child.kill().await;
    result
}

async fn codex_summary_turn(
    session: &LocalSession,
    stdin: &mut tokio::process::ChildStdin,
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) -> Outcome<String> {
    rpc_send(
        stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"clientInfo": {"name": "flyco", "version": env!("CARGO_PKG_VERSION")}},
        }),
    )
    .await?;
    rpc_reply(lines, 1).await?;
    rpc_send(
        stdin,
        serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
    )
    .await?;
    rpc_send(
        stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "thread/resume",
            "params": {"threadId": session.session_id, "cwd": session.cwd},
        }),
    )
    .await?;
    rpc_reply(lines, 2).await?;
    rpc_send(
        stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "turn/start",
            "params": {
                "threadId": session.session_id,
                "cwd": session.cwd,
                "input": [{"type": "text", "text": SUMMARY_PROMPT}],
                "approvalPolicy": "never",
                "sandboxPolicy": {"type": "readOnly"},
            },
        }),
    )
    .await?;
    rpc_reply(lines, 3).await?;

    // Notifications flow until the turn ends; an `agentMessage` item's
    // text is the summary when the server reports items inline.
    let mut inline = None;
    let inline = loop {
        let message = rpc_next(lines).await?;
        if message["method"] == "item/completed"
            && message["params"]["item"]["type"] == "agentMessage"
            && let Some(text) = message["params"]["item"]["text"].as_str()
        {
            inline = Some(text.to_owned());
        }
        if message["method"] == "turn/completed"
            && message["params"]["threadId"].as_str() == Some(session.session_id.as_str())
        {
            let turn = &message["params"]["turn"];
            if turn["status"].as_str() == Some("failed") {
                return Err(Failure::problem(
                    Exit::Problem,
                    format!(
                        "the codex summary turn failed: {}",
                        turn["error"]["message"].as_str().unwrap_or("unknown error")
                    ),
                ));
            }
            break inline;
        }
    };
    if let Some(text) = inline {
        return Ok(text);
    }
    codex_last_message(&session.transcript).await
}

/// The last `task_complete` event's `last_agent_message` in a rollout —
/// the same reply the `turn/completed` notification declines to inline.
async fn codex_last_message(rollout: &Path) -> Outcome<String> {
    let mut file = tokio::fs::File::open(rollout)
        .await
        .map_err(|error| Failure::transport(format!("{}: {error}", rollout.display())))?;
    // The tail suffices — the completion is the last event of the file.
    let length = file
        .metadata()
        .await
        .map_err(|error| Failure::transport(format!("{}: {error}", rollout.display())))?
        .len();
    let start = length.saturating_sub(8 * 1024 * 1024);
    tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(start))
        .await
        .map_err(|error| Failure::transport(format!("{}: {error}", rollout.display())))?;
    let mut tail = String::new();
    file.read_to_string(&mut tail)
        .await
        .map_err(|error| Failure::transport(format!("{}: {error}", rollout.display())))?;
    for line in tail.lines().rev() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if entry["type"] == "event_msg"
            && entry["payload"]["type"] == "task_complete"
            && let Some(text) = entry["payload"]["last_agent_message"].as_str()
        {
            return Ok(text.to_owned());
        }
    }
    Err(Failure::problem(
        Exit::Problem,
        "the codex summary turn ended but its answer was not recorded",
    ))
}

/// Writes one JSON-RPC message to the app-server's stdin.
async fn rpc_send(
    stdin: &mut tokio::process::ChildStdin,
    message: serde_json::Value,
) -> Outcome<()> {
    let mut text = message.to_string();
    text.push('\n');
    stdin
        .write_all(text.as_bytes())
        .await
        .map_err(|error| Failure::transport(format!("codex app-server: {error}")))
}

/// Reads one JSON-RPC message off the app-server's stdout.
async fn rpc_next(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) -> Outcome<serde_json::Value> {
    let line = lines
        .next_line()
        .await
        .map_err(|error| Failure::transport(format!("codex app-server: {error}")))?
        .ok_or_else(|| Failure::transport("codex app-server closed its stream"))?;
    serde_json::from_str(&line)
        .map_err(|error| Failure::transport(format!("codex app-server said {line:?}: {error}")))
}

/// Reads messages until the response to `id` arrives, erroring on a
/// JSON-RPC error answer.
async fn rpc_reply(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    id: u64,
) -> Outcome<serde_json::Value> {
    loop {
        let message = rpc_next(lines).await?;
        if message["id"].as_u64() != Some(id) {
            continue;
        }
        if let Some(error) = message.get("error") {
            return Err(Failure::problem(
                Exit::Problem,
                format!("codex app-server refused: {}", error["message"]),
            ));
        }
        return Ok(message["result"].clone());
    }
}

/// A child process under the summary timeout.
async fn run_timed(command: &mut Command) -> Outcome<std::process::Output> {
    match tokio::time::timeout(SUMMARY_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => Err(Failure::transport(format!("{command:?}: {error}"))),
        Err(_elapsed) => Err(Failure::problem(
            Exit::Timeout,
            "the harness did not answer the summary within 15 minutes",
        )),
    }
}

fn command_failed(name: &str, output: &std::process::Output) -> Failure {
    Failure::problem(
        Exit::Problem,
        format!(
            "`{name}` could not summarize the session ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    )
}

/// Copies the transcript to a private temp file — the upload must be a
/// fixed set of bytes so its checksum is stable and a live session's
/// appends mid-upload cannot shift it.
async fn snapshot(transcript: &Path) -> Outcome<PathBuf> {
    let snapshot =
        std::env::temp_dir().join(format!("flyco-handoff-{}-transcript", std::process::id()));
    tokio::fs::copy(transcript, &snapshot)
        .await
        .map_err(|error| Failure::transport(format!("{}: {error}", transcript.display())))?;
    Ok(snapshot)
}

/// The brief a handed-off session opens with.
struct Brief<'a> {
    source: &'a LocalSession,
    repo: &'a LocalRepo,
    summary: Option<&'a str>,
    message: Option<&'a str>,
}

impl Brief<'_> {
    fn render(&self) -> String {
        let summary = self.summary.map_or_else(
            || "No summary was written — the transcript below is the record.".to_owned(),
            |summary| rewrite_paths(summary, &self.repo.root.display().to_string()),
        );
        let message = self.message.map_or_else(String::new, |message| {
            format!("\n## From the sender\n{message}\n")
        });
        format!(
            "You are continuing work handed off from a local {harness} session `{session}` on \
             another machine.\n\n\
             ## Environment\n\
             - The repository `{repo}` is checked out at `{workdir}` on branch `{branch}`; the \
             sender's working tree was applied as a patch on commit `{base}`.\n\
             - This is a fresh cloud machine — nothing local survived: no processes, no \
             environment variables, no credentials, no installed tools. Rebuild and reinstall \
             what you need.\n\
             - Paths under `{local}` in this brief and in the transcript refer to the checkout \
             at `{workdir}`. Absolute paths outside that prefix (such as `/tmp` or `~`) did not \
             carry over — reconstruct them where the work needs them.\n\n\
             ## Summary of the work so far (written by the previous session)\n\
             {summary}\n\n\
             ## Transcript\n\
             The previous session's full transcript is at `{transcript}`. Consult it for detail \
             the summary omitted.\n{message}",
            harness = label(self.source.harness),
            session = self.source.session_id,
            repo = self.repo.slug,
            workdir = flyco_core::SESSION_WORKDIR,
            branch = self.repo.branch,
            base = self.repo.base_commit,
            local = self.repo.root.display(),
            transcript = flyco_core::HANDOFF_TRANSCRIPT_PATH,
        )
    }
}

/// Rewrites the local workdir prefix in summary text to the cloud
/// workdir — and only there.
///
/// A match counts only where the character after it cannot extend the
/// path segment — `/Coding/flyco2/…` must not be rewritten by the
/// `/Coding/flyco` rule. `/` is a boundary in the *permitted* direction:
/// a match followed by `/` names a path inside the worktree and is
/// rewritten. The transcript is never rewritten — it is history.
fn rewrite_paths(text: &str, local_root: &str) -> String {
    fn extends_path(c: char) -> bool {
        c.is_alphanumeric() || c == '-' || c == '_' || c == '.'
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(local_root) {
        let end = at + local_root.len();
        if rest[end..].chars().next().is_some_and(extends_path) {
            out.push_str(&rest[..end]);
        } else {
            out.push_str(&rest[..at]);
            out.push_str(flyco_core::SESSION_WORKDIR);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// A harness's name as a person reads it.
const fn label(harness: HarnessKind) -> &'static str {
    match harness {
        HarnessKind::ClaudeCode => "claude",
        HarnessKind::Codex => "codex",
        HarnessKind::Devin => "devin",
    }
}

/// One git call in `root`, stdout-or-failure.
async fn git(root: &Path, args: &[&str]) -> Outcome<std::process::Output> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .await
        .map_err(|error| Failure::transport(format!("git {}: {error}", args[0])))?;
    if output.status.success() {
        return Ok(output);
    }
    Err(Failure::usage(format!(
        "git {} failed: {}",
        args[0],
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// [`git`] with `GIT_INDEX_FILE` pointed at `index`.
async fn git_env(root: &Path, args: &[&OsStr], index: &Path) -> Outcome<std::process::Output> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_INDEX_FILE", index)
        .output()
        .await
        .map_err(|error| Failure::transport(format!("git: {error}")))?;
    if output.status.success() {
        return Ok(output);
    }
    Err(Failure::usage(format!(
        "git {} failed: {}",
        args[0].to_string_lossy(),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// [`git`], answered as trimmed stdout text.
async fn git_text(root: &Path, args: &[&str]) -> Outcome<String> {
    let output = git(root, args).await?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// A directory's entries, `None` when the directory does not exist — a
/// harness that was never installed reads as "no candidates", not an
/// error.
async fn read_dir(path: &Path) -> Option<tokio::fs::ReadDir> {
    tokio::fs::read_dir(path).await.ok()
}

/// The next directory entry, `None` on exhaustion or an entry error.
async fn entries_next(entries: &mut tokio::fs::ReadDir) -> Option<tokio::fs::DirEntry> {
    entries.next_entry().await.ok().flatten()
}

/// A file's mtime, epoch when it cannot be read — recency is a sort key,
/// never a refusal.
async fn mtime(path: &Path) -> std::time::SystemTime {
    tokio::fs::metadata(path)
        .await
        .and_then(|metadata| metadata.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

/// A file's first `limit` bytes as lines — enough to find the metadata
/// header records at the top of a transcript without reading a
/// potentially enormous file through. Lossy rather than fatal: a
/// transcript that is not UTF-8 has no usable header lines and is simply
/// not a candidate, not a reason to fail the scan.
async fn head_lines(path: &Path, limit: u64) -> Outcome<Vec<String>> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| Failure::transport(format!("{}: {error}", path.display())))?;
    let mut head = Vec::new();
    file.take(limit)
        .read_to_end(&mut head)
        .await
        .map_err(|error| Failure::transport(format!("{}: {error}", path.display())))?;
    Ok(String::from_utf8_lossy(&head)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// A file's SHA-256 as lowercase hex, read in chunks.
async fn sha256_file(path: &Path) -> Outcome<String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| Failure::transport(format!("{}: {error}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = file
            .read(&mut chunk)
            .await
            .map_err(|error| Failure::transport(format!("{}: {error}", path.display())))?;
        if read == 0 {
            return Ok(hex::encode(hasher.finalize()));
        }
        hasher.update(&chunk[..read]);
    }
}

/// A progress line on stderr — human mode only; JSON mode's stdout is
/// documents alone.
fn note(mode: out::Mode, text: &str) {
    if mode == out::Mode::Human && std::io::stderr().is_terminal() {
        let _ = out::raw_err(format!("{text}\n").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_url_parses_to_a_slug() {
        assert_eq!(
            repo_slug("git@github.com:lexoliu/flyco.git").as_deref(),
            Some("lexoliu/flyco")
        );
        assert_eq!(
            repo_slug("https://github.com/lexoliu/flyco").as_deref(),
            Some("lexoliu/flyco")
        );
        assert_eq!(
            repo_slug("ssh://git@github.com/lexoliu/flyco.git").as_deref(),
            Some("lexoliu/flyco")
        );
        assert_eq!(repo_slug("not a url"), None);
    }

    #[test]
    fn rewriting_a_summary_moves_the_workdir() {
        let text = "edit /Users/lexoliu/Coding/flyco/crates/api/src/app.rs and \
                    /Users/lexoliu/Coding/flyco2 stays, /tmp/x.out stays";
        let rewritten = rewrite_paths(text, "/Users/lexoliu/Coding/flyco");
        assert!(rewritten.contains("/srv/flyco/work/crates/api/src/app.rs"));
        assert!(rewritten.contains("/Users/lexoliu/Coding/flyco2 stays"));
        assert!(rewritten.contains("/tmp/x.out stays"));
    }

    #[test]
    fn a_trailing_boundary_still_rewrites() {
        // `.` can extend a filename segment — `flyco.` is not `flyco` —
        // so it must not be rewritten, while `:` and end-of-string are
        // boundaries.
        assert_eq!(rewrite_paths("see /r/a.", "/r/a"), "see /r/a.");
        assert_eq!(rewrite_paths("see /r/a:", "/r/a"), "see /srv/flyco/work:");
        assert_eq!(rewrite_paths("see /r/a", "/r/a"), "see /srv/flyco/work");
    }
}
