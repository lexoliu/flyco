# Flyco repository notes

## Product invariant: browser-only users

Assume the user has **only a browser** — no local terminal, no CLI, nothing
installed on their machine. Every user-facing flow must be completable from
the PWA alone:

- Account linking is paste-a-key or an OAuth/device-code page, never "run
  `x login` locally and paste the result back".
- No flow may ask the user to execute a command in their own terminal and
  copy output into flyco.
- Session inspection, approvals, diffs, files, env, terminal — all through
  the web UI; the `flyco` CLI and `flycod` are agent/daemon surfaces, not
  user prerequisites.

## Deployment: CI only

Production deploys run in GitHub Actions (`.github/workflows/deploy.yml`),
triggered by pushes to `dev`. The job checks out the `dev` tip itself, so
production always runs merged code.

- **Never run `skyzen deploy`, `wrangler deploy`, or `wrangler pages deploy`
  against production from a local checkout or worktree.** Those commands
  ship whatever is in the working directory — uncommitted experiments, a
  stale branch, another session's in-flight work — and the last deploy wins.
  That race has already overwritten a production deployment once.
- Local commands are unaffected: `skyzen dev`, `skyzen build`, `cargo
  build/test`, `bun run build`, and `skyzen deploy --dry-run` are all fine.
- To redeploy production without a code change, dispatch the workflow:
  `gh workflow run deploy.yml`. It still deploys only the `dev` tip.
- Cloudflare credentials for the workflow live in GitHub secrets
  (`CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`, `FLYCO_DEPLOY_ENV`), not
  in this repository.
