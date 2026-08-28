# Harness feature matrix

The proposal promises the features of the official apps; this matrix is where that promise is tracked per harness. States: **supported** (works in flyco), **planned** (programmatically reachable, not built yet), **harness-limitation** (no programmatic route exists — becomes supported if the vendor ships one).

| Feature | Claude Code | Codex | Notes |
|---|---|---|---|
| Usage display | planned | planned | No remaining-quota API on either side; reactive (rate-limit events + cost telemetry). |
| Context window display | planned | planned | Claude: statusLine JSON feed. Codex: token counts in turn events. |
| Goal mode | planned | planned | Claude: `/goal` works in `-p`. |
| Auto mode (default) | planned | planned | Claude: `--permission-mode auto` (requires user-level or managed settings). |
| Side chat (btw) | harness-limitation | harness-limitation | Claude: terminal-only overlay; approximated via `forkSession`. |
| Ultra mode / dynamic workflows | planned | planned | Claude: `--effort ultracode` + `Workflow` permission rule. |
| Any setting | planned | planned | Claude: `--settings` / `applyFlagSettings`. Codex: `config.toml`. |
| Compact | planned | planned | Claude: explicit `/compact` unreachable headless — auto-compact thresholds instead. |
| Advisor | planned | — | Claude: `--advisor` flag / `advisorModel` setting. |
| Monitor | planned | — | Claude: Monitor tool; needs a persistent streaming session. |
| Background tasks | planned | planned | In-session background shells; background *sessions* are flyco sessions themselves. |
| Auto continue at usage reset | planned | planned | Claude: `autoContinueAtUsageLimit` managed setting. |
| Remote control | disabled | disabled | By design (proposal); also requires full-scope claude.ai login flyco doesn't hold. |
| Resume (harness-native) | disabled | disabled | Flyco owns session persistence (SessionStore / `thread/read`). |
| Skills | takeover | takeover | Read-only configs; uploads via flycod's MCP `skill_upload`. |
| MCP | takeover | takeover | Central registry; Claude: managed-mcp + `allowManagedMcpServersOnly`; Codex: allowlist. |
| Memory | takeover | takeover | Tree memory over MCP; harness-native file memory disabled. |
| Browser control | phase 2 | phase 2 | Playwright auto-install. |
| Computer control | phase 2 | phase 2 | Video streaming, GNOME images. |
