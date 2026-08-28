# `codex app-server` protocol reference (for the flyco Codex driver)

Researched 2026-08-28 against `openai/codex` @ `6be2a6ca952a` (main), latest release `rust-v0.150.1`. Ground truth: `codex-rs/app-server-protocol/src/protocol/v2/` (37-file module; the old `v2.rs` is gone), `protocol/common.rs`, `src/rpc.rs`, `v2/tests.rs` fixtures. The app-server README contains verified wire-format ERRORS — trust the Rust types or `codex app-server generate-json-schema` output pinned to the exact binary.

## Transport
- Spawn `codex app-server` (stdio default; websocket transport explicitly unsupported for production). `CODEX_HOME` (default `~/.codex`), `RUST_LOG`, `LOG_FORMAT=json` (stderr).
- Framing: newline-delimited JSON, NO Content-Length, and the `"jsonrpc":"2.0"` field is OMITTED both directions. Discriminate by field presence: `{id,method,params?}`=request, `{method,params?}`=notification, `{id,result}`=response, `{id,error}`=error. `RequestId` = string or int.
- Handshake: one `initialize` (clientInfo, capabilities incl. `experimentalApi`, `optOutNotificationMethods`) → result has `codexHome`, platform; then `initialized` notification. No protocol version negotiation — pin via `generate-json-schema --out DIR [--experimental]` at build time.
- Backpressure: JSON-RPC error `-32001` "Server overloaded; retry later." → jittered backoff. `-32600` invalid request / paginated-thread single-writer lock; `-32601` unsupported method.
- `--strict-config` = hard error on unknown config keys (use it: catches drift on Codex upgrades).

## Threads and turns (v2)
- `thread/start` params (all optional, camelCase): model, modelProvider, cwd, `approvalPolicy` (KEBAB: `untrusted` | `on-request` | `never`; experimental `{granular:{...}}` needs `experimentalApi`), `approvalsReviewer` (`user` | `auto_review`), `sandbox` (SandboxMode KEBAB string: `read-only` | `workspace-write` | `danger-full-access`), `config` (arbitrary config.toml overrides per-thread — the cleanest takeover lever), personality, ephemeral. NOTE: response `sandbox` is a DIFFERENT type — tagged camelCase `SandboxPolicy` object (`{"type":"workspaceWrite","writableRoots":[...],"networkAccess":false,...}`). `turn/start.sandboxPolicy` also uses SandboxPolicy. Model the two types separately.
- Side effect: `thread/start` with cwd + workspace-write writes a TRUST MARKER for the project into user config.toml.
- `Thread`: id/sessionId (uuidv7), `status` tagged `notLoaded|idle|systemError|active`; `active.activeFlags` ∈ `waitingOnApproval`|`waitingOnUserInput` → stall detector. `path` = rollout JSONL path [UNSTABLE].
- `thread/resume {threadId, excludeTurns:true, ...same overrides}` → thread + `turnsBackwardsCursor`/`itemsBackwardsCursor`; page with `thread/turns/list` + `thread/items/list` (sortDirection desc). Full hydration deprecated. Resume defaults to persisted model/effort unless overridden. Single-writer: second process gets `-32600`.
- `turn/start {threadId, clientUserMessageId, input:[{type:"text",text,textElements:[]}], ...}`. UserInput types: text, image (data URLs only), localImage, audio/localAudio, skill `{name,path}`, mention. TRAP: `model/approvalPolicy/sandboxPolicy/cwd/effort` given on turn/start PERSIST as thread defaults for later turns; only `outputSchema` and `serviceTierForTurn` are turn-scoped.
- `turn/interrupt` → `{}` means "requested"; wait for `turn/completed` with `status:"interrupted"`. Doesn't kill background terminals (`thread/backgroundTerminals/clean`, experimental).
- TurnStatus: `completed|interrupted|failed|inProgress`.

## Notifications
- Lifecycle: `turn/started` `{threadId, turn}` (README wrongly omits threadId), `turn/completed` `{threadId, turn}` (terminal), `turn/diff/updated`, `turn/plan/updated`, `thread/tokenUsage/updated` `{threadId, turnId, tokenUsage}`, `thread/status/changed`, `thread/closed` (after 30-min unload timer), `error` `{threadId, turnId, error:{message, codexErrorInfo,...}, willRetry}` — `willRetry` is UNDOCUMENTED; when true the server retries internally, do NOT fail the turn. Only `turn/completed status:failed` is terminal.
- Items: `item/started` → deltas → `item/completed` (`{item, threadId, turnId, startedAtMs/completedAtMs}`). Deltas: `item/agentMessage/delta {delta}`, `item/reasoning/summaryTextDelta {delta, summaryIndex}`, `item/reasoning/textDelta {delta, contentIndex}`, `item/commandExecution/outputDelta`, `item/commandExecution/terminalInteraction {processId, stdin}` (undocumented), `item/mcpToolCall/progress {message}` (undocumented), `item/fileChange/patchUpdated`.
- ThreadItem tags (camelCase, tagged on `type`): userMessage, hookPrompt (undocumented), agentMessage, functionCallOutput, plan, reasoning, commandExecution (has `processId`, `source`, `commandActions`, `aggregatedOutput`, `exitCode`, `durationMs`; status `inProgress|completed|failed|declined`), fileChange, mcpToolCall, dynamicToolCall, collabAgentToolCall (README wrongly says collabToolCall), subAgentActivity, webSearch, imageView, sleep, imageGeneration, enteredReviewMode, exitedReviewMode, contextCompaction.
- Paths in commandExecution use the EXECUTOR's native convention — never treat as local paths.
- Noise control: `initialize.capabilities.optOutNotificationMethods` (loudest: `item/reasoning/summaryTextDelta`, `item/agentMessage/delta`).

## Approvals — server→client JSON-RPC REQUESTS (must answer or the turn hangs forever)
Nine kinds: `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, `item/permissions/requestApproval`, `mcpServer/elicitation/request`, `item/tool/requestUserInput` (exp), `item/tool/call` (exp), `account/chatgptAuthTokens/refresh`, `attestation/generate` (opt-in), `currentTime/read` (exp). Deny-by-default the unimplemented ones; watch `serverRequest/resolved` + `thread/status/changed waitingOnApproval` as stall detectors.
- commandExecution approval response: `{"decision":"accept"|"acceptForSession"|"decline"|"cancel"}` or tagged struct variants `acceptWithExecpolicyAmendment` / `applyNetworkPolicyAmendment` (inner fields snake_case). `decline` = agent continues; `cancel` = turn aborts.
- fileChange approval: same four decisions; params include `grantRoot` [UNSTABLE].
- permissions approval: respond with granted SUBSET + `scope: "turn"(default)|"session"`.
- `approvalsReviewer:"auto_review"` (alias guardian_subagent) replaces blocking requests with `item/autoApprovalReview/*` notifications [UNSTABLE] — better than `approvalPolicy:"never"` for unattended runs.

## Auth (headless Linux)
- `~/.codex/auth.json` plaintext; backend via `cli_auth_credentials_store = "file"|"keyring"|"auto"` — set `"file"` explicitly on VMs.
- Modes: apiKey (recommended for automation), chatgpt (OAuth; browser or DEVICE CODE: `account/login/start {"type":"chatgptDeviceCode"}` → `{loginId, verificationUrl, userCode}` → `account/login/completed` notification; CLI `codex login --device-auth`, beta, must be enabled in ChatGPT security settings), bedrock (exp), personalAccessToken (`CODEX_ACCESS_TOKEN`).
- Copying auth.json between machines is OFFICIALLY documented (file store only). OAuth callback port 1455 (can ssh -L forward).
- `account/read`, `account/logout`, notifications `account/updated {authMode, planType}`, server→client `account/chatgptAuthTokens/refresh {reason, previousAccountId}` → expects `{accessToken, chatgptAccountId, chatgptPlanType}` — reply with an error if not implemented (fail fast, not hang).
- Managed: `forced_login_method`, `forced_chatgpt_workspace_id` (violation → logout + exit). `CODEX_CA_CERTIFICATE` for TLS proxies.

## Config takeover / lockdown
- Layers: `$CODEX_HOME/config.toml`, project `.codex/config.toml`, profiles, managed. Runtime: `thread/start.config` overrides (no disk), `config/value/write`, `config/batchWrite`, `config/read`, `config/mcpServer/reload`.
- MCP: `[mcp_servers.<id>]` — id IS the identity (no separate identity field; provenance via mcpToolCall.pluginId). Keys: command/args/env or url, `enabled`, `enabled_tools` (allow), `disabled_tools` (deny, applied after), `default_tools_approval_mode` (`auto|prompt|writes|approve`), per-tool `[mcp_servers.<id>.tools.<tool>].approval_mode`, `required`, timeouts, auth/oauth keys. Inspect: `mcpServerStatus/list`.
- Hooks: `[hooks.<Event>]` in config.toml or hooks.json. Events: PreToolUse, PermissionRequest, PostToolUse, Pre/PostCompact, SessionStart/End, SubagentStart/Stop, UserPromptSubmit, Stop. Handlers: `command`, `mcpTool`. `hooks/list` shows trustStatus — only trusted unmanaged hooks run.
- Lockdown = `requirements.toml` (NOT config.toml): `allow_managed_hooks_only`, `allowed_approval_policies`, `allowed_sandbox_modes`, `allowed_permission_profiles`, `default_permissions`, `network` (managed_allowed_domains_only etc.), `forced_login_method`, `[models.new_thread]`. Overlapping `config/value/write` rejected with `configRequirementReadonly`. Read via `configRequirements/read`.
- Sandbox self-protection: under workspace-write, `<root>/.git`, `<root>/.agents`, `<root>/.codex` are read-only — but `$CODEX_HOME` outside writable roots is NOT self-protected: mount it read-only at the VM layer. Linux sandbox = bwrap + seccomp; inside Docker/K8s it may not work — documented answer: container-level isolation + `danger-full-access`.
- AGENTS.md chain: `$CODEX_HOME/AGENTS.override.md` else `AGENTS.md` (global), then project root→cwd (override > AGENTS.md > fallbacks, one per dir), concatenated root-down, 32 KiB cap (`project_doc_max_bytes`). Verify via `ThreadStartResponse.instructionSources`.
- Skills: `~/.codex/skills/<name>/SKILL.md`; `[[skills.config]]` enable/disable; RPC `skills/config/write`, `skills/extraRoots/set`, `skills/list`, notification `skills/changed`. Invoke with `$<skill-name>` + `{"type":"skill"}` input item.

## Usage / limits
- `thread/tokenUsage/updated`: `{total:{totalTokens,inputTokens,cachedInputTokens,cacheWriteInputTokens,outputTokens,reasoningOutputTokens}, last:{...}, modelContextWindow}` — context gauge; drive `thread/compact/start` from total/modelContextWindow.
- `account/rateLimits/read`: `{rateLimits:{primary/secondary:{usedPercent,windowDurationMins,resetsAt}, spendControlReached, planType,...}, rateLimitsByLimitId:{...} (not in README), rateLimitResetCredits}`. `account/rateLimits/updated` is SPARSE — merge, don't replace. `account/rateLimitResetCredit/consume {idempotencyKey}` → refetch after. `account/usage/read` → daily buckets + per-thread `estimatedUsageUsdMicros` (micros!).
- Cap exhaustion = `error` notification (willRetry:false) + `turn/completed status:failed` with `codexErrorInfo`. Values (mixed casing!): `ContextWindowExceeded`, `SessionBudgetExceeded`, `UsageLimitExceeded`, `rateLimitExceeded`, `misalignmentPolicyViolation`, `HttpConnectionFailed{httpStatusCode?}`, `ResponseStream*`, `ActiveTurnNotSteerable`, `BadRequest`, `Unauthorized`, `SandboxError`, `InternalServerError`, `Other`.

## Persistence
- `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-<threadId>[_<rolloutId>].jsonl` (may be compressed); `archived_sessions/`; SQLite state DB (`sqlite_home`) indexes threads — `thread/list` prefers it (`useStateDbOnly:true` skips JSONL repair).
- Cross-machine `thread/resume` is NOT established: absolute cwd capture, separate SQLite index, rollout format migrations. If moving: copy whole `$CODEX_HOME`, same Codex version, same workspace path, prefer experimental `thread/resume.path`. Verify empirically. → flyco exports turn history via paginated `thread/turns/list`/`thread/items/list` instead; `itemsView` ∈ `notLoaded|summary|full` ("full" = app-server history projection, not raw rollout).

## Driver rules distilled
1. JSONL framing, no jsonrpc field, untagged envelope, string-or-int ids.
2. Generate types from `generate-json-schema` at the pinned binary; README examples are wrong (`unlessTrusted`→`untrusted`, `workspaceWrite`→`workspace-write`, missing threadId, collabToolCall→collabAgentToolCall).
3. Distinct Rust types for SandboxMode (kebab string) vs SandboxPolicy (tagged object); never combine with experimental `permissions`.
4. Answer all nine server→client requests; deny-by-default; stall detection via activeFlags + serverRequest/resolved.
5. Fail turns only on `turn/completed status:failed`; branch on `willRetry` and `codexErrorInfo`.
6. Unattended: `approvalsReviewer:"auto_review"` (flyco bridges approvals to its own UI anyway via the blocking requests when a human is reachable).
7. Lockdown via requirements.toml + read-only $CODEX_HOME mount + `--strict-config`.
8. Auth: user token via device-code flow or copied file-store auth.json; implement/fail-fast `account/chatgptAuthTokens/refresh`.
9. `excludeTurns:true` + cursor pagination everywhere; `useStateDbOnly:true` for lists.
10. Single writer per thread (`-32600` = supervisor state, not crash); 30-min unload → `thread/closed`; `-32001` → jittered backoff; opt out of loud deltas when not streaming to a client.
