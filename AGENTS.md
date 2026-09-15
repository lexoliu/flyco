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
