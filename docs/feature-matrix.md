# Harness feature matrix

The proposal promises the features of the official apps; this matrix is where that promise is tracked per harness. The live table is `GET /v1/harness-features`, generated from `flyco_core::matrix()`. States: **supported** (works in flyco), **planned** (programmatically reachable, not built yet), **harness-limitation** (no programmatic route exists), **disabled** (flyco owns the equivalent), **takeover** (flyco took the feature over), **phase 2**, **—** (this harness does not have it).

| Feature | Claude Code | Codex | Notes |
|---|---|---|---|
| Usage display | supported | supported | Reactive: rate-limit events plus harness cost telemetry. No remaining-quota API on either side. |
| Context window display | supported | supported | Claude: per-model usage on `result`. Codex: `modelContextWindow` on token-usage notifications. |
| Goal mode | supported | supported | User messages (including `/goal`) and CI-autofix notices. |
| Auto mode (default) | supported | supported | Sessions provision with `PermissionMode::Auto`; managed deny rules still bind. |
| Side chat (btw) | harness-limitation | harness-limitation | Claude: terminal-only overlay; approximated via `forkSession`. |
| Ultra mode / dynamic workflows | planned | planned | Claude: `--effort ultracode` + `Workflow` permission rule. |
| Any setting | planned | planned | Claude: `--settings` / `applyFlagSettings`. Codex: `config.toml`. |
| Compact | planned | planned | Claude: explicit `/compact` unreachable headless — auto-compact thresholds instead. Codex: `thread/compact/start`. |
| Advisor | planned | — | Claude: `--advisor` flag / `advisorModel` setting. |
| Monitor | planned | — | Claude: Monitor tool; needs a persistent streaming session. |
| Background tasks | supported | supported | In-session background shells; background *sessions* are flyco sessions themselves. |
| Auto continue at usage reset | supported | supported | A spent plan window pauses the session, releases the machine when the reset is far off, and continues on the user's behalf when it turns over (docs/ux.md §9.8). |
| Remote control | disabled | disabled | By design (proposal); also requires full-scope claude.ai login flyco doesn't hold. |
| Resume (harness-native) | disabled | disabled | Flyco owns session persistence (SessionStore / `thread/read`). |
| Skills | takeover | takeover | Read-only configs; uploads via flycod's MCP `skill_upload`. |
| MCP | takeover | takeover | Central registry; Claude: managed-mcp + `allowManagedMcpServersOnly`; Codex: allowlist. |
| Memory | takeover | takeover | Tree memory over MCP; harness-native file memory disabled. |
| Browser control | phase 2 | phase 2 | Playwright auto-install. |
| Computer control | phase 2 | phase 2 | Video streaming, GNOME images. |
