# Flyco repository notes

## Product invariant: browser-only users

Assume the user has **only a browser** — no local terminal, no CLI, nothing
installed on their machine. Every user-facing flow must be completable from
the PWA alone:

- Account linking is paste-a-key or an OAuth/device-code page, never "run
  `x login` locally and paste the result back". A vendor whose OAuth
  client admits only localhost redirects gets its CLI's manual-code flow
  (the page shows the code, the user pastes the code), never a dead
  `127.0.0.1` redirect the user copies out of the browser's error page.
- No flow may ask the user to execute a command in their own terminal and
  copy output into flyco.
- Session inspection, approvals, diffs, files, env, terminal — all through
  the web UI; the `flyco` CLI and `flycod` are agent/daemon surfaces, not
  user prerequisites.

## Operating flyco: public interfaces only

Mutating flyco state goes through the same public surfaces a user has —
the web GUI (via the host's dedicated browser tool, never computer-use) or
the `flyco` CLI. Never call API endpoints directly (`curl POST /v1/...`),
and never write the database directly (`wrangler d1 execute` with
INSERT/UPDATE/DELETE). Direct calls bypass the product's own invariants —
a mislabelled harness-account row came from a raw
`POST /v1/harness-accounts`.

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
