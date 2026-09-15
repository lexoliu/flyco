/**
 * The typed REST client for flyco's control plane.
 *
 * Every function here is a thin wrapper around [`send`]: it builds a URL
 * (base URL from `import.meta.env.VITE_API_BASE`, defaulting to
 * same-origin), attaches `Authorization: Bearer <token>` from
 * `src/lib/session.ts` when one is stored, and turns any non-2xx response
 * into a typed error via `src/api/problem.ts`. Every request/response shape
 * is read straight off the generated `operations`/`components` types in
 * `schema.d.ts` — nothing here is a hand-retyped DTO.
 *
 * The session view reads events off the one per-user SSE stream
 * (`src/api/events.ts` on `GET /v1/events`) and sends every client
 * command over REST — `sendMessage`, `runShellCommand`,
 * `sendTerminalInput`, `resizeSessionTerminal`, `interruptSession`,
 * `compactSession` and `contextSession` below — which is the whole
 * transport: there is no socket, and the stream is the only
 * server-to-client path (`ControlToDaemon::is_client_command` names what
 * a client may send).
 */
import type { components, operations } from "./schema";
import { clearSessionToken, getSessionToken } from "../lib/session";
import { NetworkError, problemFromResponse } from "./problem";

type Schemas = components["schemas"];
export type CurrentUser = Schemas["CurrentUser"];
export type SessionSummary = Schemas["SessionSummary"];
export type SessionDetail = Schemas["SessionDetail"];
export type BudgetView = Schemas["BudgetView"];
export type EventPage = Schemas["EventPage"];
export type StoredEvent = Schemas["StoredEvent"];
export type EnvDocument = Schemas["EnvDocument"];
export type EnvEntry = Schemas["EnvEntry"];
export type ApprovalView = Schemas["ApprovalView"];
export type ApprovalState = Schemas["ApprovalState"];
export type McpServerView = Schemas["McpServerView"];
export type McpServerConfig = Schemas["McpServerConfig"];
export type SkillView = Schemas["SkillView"];
export type SkillScope = Schemas["SkillScope"];
export type ProviderAccountView = Schemas["ProviderAccountView"];
export type ProviderCredentials = Schemas["ProviderCredentials"];
export type ProviderBonusHint = Schemas["ProviderBonusHint"];
export type AwsIamPolicy = Schemas["AwsIamPolicy"];
export type CloudProviderKind = Schemas["CloudProviderKind"];
export type HostView = Schemas["HostView"];
export type HostFacts = Schemas["HostFacts"];
export type HostState = Schemas["HostState"];
export type EnrollmentToken = Schemas["EnrollmentToken"];
export type Enrollment = Schemas["Enrollment"];
export type ApiKeySummary = Schemas["ApiKeySummary"];
export type CreatedApiKey = Schemas["CreatedApiKey"];
export type AuthorizeUrl = Schemas["AuthorizeUrl"];
export type RepoSummary = Schemas["RepoSummary"];
export type BranchSummary = Schemas["BranchSummary"];
export type BranchPage = Schemas["BranchPage"];
export type HarnessKind = Schemas["HarnessKind"];
export type ModelOption = Schemas["ModelOption"];
export type ModelChoice = Schemas["ModelChoice"];
export type PermissionMode = Schemas["PermissionMode"];
export type SessionState = Schemas["SessionState"];
export type SessionActivity = Schemas["SessionActivity"];
export type InterruptedReason = Schemas["InterruptedReason"];
export type PausedReason = Schemas["PausedReason"];
export type UsageLimitPause = Schemas["UsageLimitPause"];
export type MachineCatalogEntry = Schemas["MachineCatalogEntry"];
export type MachineDefault = Schemas["MachineDefault"];
export type MachineChoice = Schemas["MachineChoice"];
export type MachineOrigin = Schemas["MachineOrigin"];
export type MachineView = Schemas["MachineView"];
export type MachineSpec = Schemas["MachineSpec"];
export type MachineState = Schemas["MachineState"];
export type MachinePricing = Schemas["MachinePricing"];
export type MachineCapacity = Schemas["MachineCapacity"];
export type MachineLineage = Schemas["MachineLineage"];
export type CpuArchitecture = Schemas["CpuArchitecture"];
export type BillingMinimum = Schemas["BillingMinimum"];
export type OsFamily = Schemas["OsFamily"];
export type Runtime = Schemas["Runtime"];
export type FreeGrant = Schemas["FreeGrant"];
export type HarnessAccountView = Schemas["HarnessAccountView"];
export type HarnessCredentialInput = Schemas["HarnessCredentialInput"];
export type ClaudeOauthStart = Schemas["ClaudeOauthStart"];
export type CodexOauthStart = Schemas["CodexOauthStart"];
export type MemoryNode = Schemas["MemoryNode"];
export type AgentsDocument = Schemas["AgentsDocument"];
export type PushSubscriptionView = Schemas["PushSubscriptionView"];
export type VapidPublicKey = Schemas["VapidPublicKey"];
export type TurnSummary = Schemas["TurnSummary"];
export type TurnPage = Schemas["TurnPage"];
export type RepoStatus = Schemas["RepoStatus"];
export type DirectoryListing = Schemas["DirectoryListing"];
export type DirectoryEntry = Schemas["DirectoryEntry"];
export type FileContent = Schemas["FileContent"];
export type WorkdirDiff = Schemas["WorkdirDiff"];
export type FileDiff = Schemas["FileDiff"];
export type FileChange = Schemas["FileChange"];
export type HarnessFeature = Schemas["HarnessFeature"];
export type Feature = Schemas["Feature"];
export type Availability = Schemas["Availability"];

/** Extracts an operation's JSON request body type, or `never` if it has none. */
type JsonBody<Op extends keyof operations> = operations[Op] extends {
  requestBody: { content: { "application/json": infer B } };
}
  ? B
  : never;

/** Extracts an operation's JSON response body type for one status code. */
type JsonResponse<Op extends keyof operations, Status extends number> =
  operations[Op]["responses"] extends Record<
    Status,
    { content: { "application/json": infer R } }
  >
    ? R
    : void;

const API_BASE = import.meta.env.VITE_API_BASE ?? "";

/** The origin every relative API path resolves against. */
function resolveBase(): string {
  return API_BASE || window.location.origin;
}

/** Resolves an API path to an absolute URL, same-origin by default. */
export function apiUrl(path: string): URL {
  return new URL(path, resolveBase());
}

interface SendOptions {
  query?: Query;
  json?: unknown;
  octetStream?: Blob;
  signal?: AbortSignal | undefined;
}

type QueryValue = string | number | boolean | null | undefined;
type Query = Record<string, QueryValue>;

function applyQuery(url: URL, query: Query | undefined): void {
  if (!query) {
    return;
  }
  for (const [key, value] of Object.entries(query)) {
    if (value !== null && value !== undefined) {
      url.searchParams.set(key, String(value));
    }
  }
}

/** Issues one HTTP request, attaching auth and translating any failure. */
async function send(
  method: string,
  path: string,
  options: SendOptions = {},
): Promise<Response> {
  const url = apiUrl(path);
  applyQuery(url, options.query);

  const headers = new Headers();
  const token = getSessionToken();
  if (token !== null) {
    headers.set("Authorization", `Bearer ${token}`);
  }

  let body: BodyInit | null = null;
  if (options.octetStream !== undefined) {
    headers.set("Content-Type", "application/octet-stream");
    body = options.octetStream;
  } else if (options.json !== undefined) {
    headers.set("Content-Type", "application/json");
    body = JSON.stringify(options.json);
  }

  let response: Response;
  try {
    response = await fetch(url, {
      method,
      headers,
      body,
      signal: options.signal ?? null,
    });
  } catch (cause) {
    throw new NetworkError(cause);
  }

  if (!response.ok) {
    if (response.status === 401 && token !== null) {
      clearSessionToken();
    }
    throw await problemFromResponse(response);
  }
  return response;
}

async function requestJson<T>(
  method: string,
  path: string,
  options: SendOptions = {},
): Promise<T> {
  const response = await send(method, path, options);
  return (await response.json()) as T;
}

async function requestVoid(
  method: string,
  path: string,
  options: SendOptions = {},
): Promise<void> {
  await send(method, path, options);
}

// --- /v1/me ---------------------------------------------------------------

export function getMe(): Promise<JsonResponse<"flyco_api::app::me", 200>> {
  return requestJson("GET", "/v1/me");
}

export function updateMe(
  sessionCap: number | null,
): Promise<JsonResponse<"flyco_api::app::update_me", 200>> {
  const body: JsonBody<"flyco_api::app::update_me"> = {
    session_cap: sessionCap,
  };
  return requestJson("PATCH", "/v1/me", { json: body });
}

// --- /v1/sessions -----------------------------------------------------------

export function listHarnessFeatures(): Promise<
  JsonResponse<"flyco_api::app::list_harness_features", 200>
> {
  return requestJson("GET", "/v1/harness-features");
}

export function listSessions(): Promise<
  JsonResponse<"flyco_api::app::list_sessions", 200>
> {
  return requestJson("GET", "/v1/sessions");
}

export function createSession(
  input: JsonBody<"flyco_api::app::create_session">,
): Promise<JsonResponse<"flyco_api::app::create_session", 201>> {
  return requestJson("POST", "/v1/sessions", { json: input });
}

export function getSession(
  id: string,
): Promise<JsonResponse<"flyco_api::app::get_session", 200>> {
  return requestJson("GET", `/v1/sessions/${id}`);
}

/**
 * Changes a session's title, its compute budget, the model it runs on, or
 * any of the three.
 *
 * Every field is optional and a body naming none of them is refused, so
 * callers pass exactly what they are changing. Raising `budgetLimit` (in
 * microdollars) past what the session has spent is what releases one paused
 * on an exhausted budget: the answer carries the session already `active`.
 * A new `model` reaches the running agent through its room, and the answer
 * carries the session already on it.
 */
export function updateSession(
  id: string,
  changes: {
    title?: string;
    budgetLimit?: number;
    model?: ModelChoice;
    permissionMode?: PermissionMode;
  },
): Promise<JsonResponse<"flyco_api::app::update_session", 200>> {
  const body: JsonBody<"flyco_api::app::update_session"> = {
    ...(changes.title === undefined ? {} : { title: changes.title }),
    ...(changes.budgetLimit === undefined
      ? {}
      : { budget_limit: changes.budgetLimit }),
    ...(changes.model === undefined ? {} : { model: changes.model }),
    ...(changes.permissionMode === undefined
      ? {}
      : { permission_mode: changes.permissionMode }),
  };
  return requestJson("PATCH", `/v1/sessions/${id}`, { json: body });
}

/**
 * Puts an interrupted, failed or archived session back on a machine.
 *
 * The machine row keeps its identity and the harness conversation is
 * reopened, so this is the session coming back rather than a new one beside
 * it. The session is `provisioning` in the answer: what follows is the
 * ordinary provisioning timeline.
 */
export function resumeSession(
  id: string,
): Promise<JsonResponse<"flyco_api::app::resume_session", 200>> {
  return requestJson("POST", `/v1/sessions/${id}/resume`);
}

export function archiveSession(
  id: string,
  options: { discardUncommitted?: boolean } = {},
): Promise<JsonResponse<"flyco_api::app::archive_session", 200>> {
  return requestJson("POST", `/v1/sessions/${id}/archive`, {
    query: {
      discard_uncommitted: options.discardUncommitted === true ? true : null,
    },
  });
}

export function getSessionBudget(
  id: string,
): Promise<JsonResponse<"flyco_api::app::get_session_budget", 200>> {
  return requestJson("GET", `/v1/sessions/${id}/budget`);
}

export function getSessionEvents(
  id: string,
  after?: number,
): Promise<JsonResponse<"flyco_api::app::get_session_events", 200>> {
  return requestJson("GET", `/v1/sessions/${id}/events`, {
    query: { after: after ?? null },
  });
}

/**
 * Opens the per-user event stream: one SSE response carrying every
 * session the caller owns, multiplexed by the `session` field of each
 * `SessionEvent` envelope.
 *
 * `after` resumes strictly past a position in the server's reconnect
 * buffer — the `id:` of the last SSE frame this client saw. Omitted opens
 * the stream live: the buffer is a reconnect window, not history, and the
 * past a session view wants is its own `events?after=` pages.
 *
 * Returns the raw response — the caller reads `body` as an SSE frame
 * stream; `signal` is what `dispose` aborts it with.
 */
export function openUserStream(
  after?: number,
  signal?: AbortSignal,
): Promise<Response> {
  return send("GET", "/v1/events", {
    query: { after: after ?? null },
    signal,
  });
}

export function getSessionEnv(
  id: string,
): Promise<JsonResponse<"flyco_api::app::get_session_env", 200>> {
  return requestJson("GET", `/v1/sessions/${id}/env`);
}

export function putSessionEnv(
  id: string,
  entries: EnvEntry[],
): Promise<JsonResponse<"flyco_api::app::put_session_env", 200>> {
  const body: JsonBody<"flyco_api::app::put_session_env"> = { entries };
  return requestJson("PUT", `/v1/sessions/${id}/env`, { json: body });
}

export function getRepoStatus(
  id: string,
): Promise<JsonResponse<"flyco_api::app::get_repo_status", 200>> {
  return requestJson("GET", `/v1/sessions/${id}/repo-status`);
}

/**
 * Lists one directory of a session's checkout. An empty path is the root.
 *
 * Answered live by the machine, so a session with no daemon connected
 * refuses with `session-daemon-offline` rather than an empty tree.
 */
export function listSessionFiles(
  id: string,
  path: string,
): Promise<JsonResponse<"flyco_api::app::list_session_files", 200>> {
  return requestJson("GET", `/v1/sessions/${id}/files`, { query: { path } });
}

/** Reads one text file out of a session's checkout. */
export function readSessionFile(
  id: string,
  path: string,
): Promise<JsonResponse<"flyco_api::app::read_session_file", 200>> {
  return requestJson("GET", `/v1/sessions/${id}/files/content`, {
    query: { path },
  });
}

/** Diffs a session's working tree against the branch it started from. */
export function getSessionDiff(
  id: string,
): Promise<JsonResponse<"flyco_api::app::get_session_diff", 200>> {
  return requestJson("GET", `/v1/sessions/${id}/diff`);
}

/** Sends a message to a session's agent; see the module doc comment above. */
export function sendMessage(id: string, text: string): Promise<void> {
  const body: JsonBody<"flyco_api::app::send_message"> = { text };
  return requestVoid("POST", `/v1/sessions/${id}/messages`, { json: body });
}

/** Runs a `!` shell command on a session's machine. */
export function runShellCommand(id: string, command: string): Promise<void> {
  const body: JsonBody<"flyco_api::app::run_shell"> = { command };
  return requestVoid("POST", `/v1/sessions/${id}/shell`, { json: body });
}

/** Writes raw input to a session's web terminal. */
export function sendTerminalInput(id: string, data: string): Promise<void> {
  const body: JsonBody<"flyco_api::app::terminal_input"> = { data };
  return requestVoid("POST", `/v1/sessions/${id}/terminal/input`, {
    json: body,
  });
}

/** Reports the web terminal's fitted size. */
export function resizeSessionTerminal(
  id: string,
  cols: number,
  rows: number,
): Promise<void> {
  const body: JsonBody<"flyco_api::app::terminal_resize"> = { cols, rows };
  return requestVoid("POST", `/v1/sessions/${id}/terminal/resize`, {
    json: body,
  });
}

/** Interrupts a session's current turn; see the module doc comment above. */
export function interruptSession(id: string): Promise<void> {
  return requestVoid("POST", `/v1/sessions/${id}/interrupt`);
}

/** Compacts session context; see the module doc comment above. */
export function compactSession(id: string): Promise<void> {
  return requestVoid("POST", `/v1/sessions/${id}/compact`);
}

export function contextSession(id: string): Promise<void> {
  return requestVoid("POST", `/v1/sessions/${id}/context`);
}

export function listTurns(
  id: string,
  page?: { cursor?: string; limit?: number },
): Promise<JsonResponse<"flyco_api::app::list_turns", 200>> {
  return requestJson("GET", `/v1/sessions/${id}/turns`, {
    query: { cursor: page?.cursor, limit: page?.limit },
  });
}

// --- /v1/sessions/{id}/machine -----------------------------------------------

export function getSessionMachine(
  id: string,
): Promise<JsonResponse<"flyco_api::machines::get_session_machine", 200>> {
  return requestJson("GET", `/v1/sessions/${id}/machine`);
}

/** Starts a stopped machine. The outcome arrives on the session relay, not in this response. */
export function startSessionMachine(id: string): Promise<void> {
  return requestVoid("POST", `/v1/sessions/${id}/machine/start`);
}

/** Stops (deallocates) a running machine. The outcome arrives on the session relay. */
export function stopSessionMachine(id: string): Promise<void> {
  return requestVoid("POST", `/v1/sessions/${id}/machine/stop`);
}

/** Moves compute to a different catalog machine type; the disk survives. */
export function resizeSessionMachine(
  id: string,
  machineType: string,
): Promise<void> {
  const body: JsonBody<"flyco_api::machines::resize_session_machine"> = {
    machine_type: machineType,
  };
  return requestVoid("POST", `/v1/sessions/${id}/machine/resize`, {
    json: body,
  });
}

// --- /v1/machines/catalog -----------------------------------------------------

export function getMachineCatalog(filter?: {
  provider?: CloudProviderKind;
  account?: string;
  os?: OsFamily;
  region?: string;
}): Promise<JsonResponse<"flyco_api::machines::get_catalog", 200>> {
  return requestJson("GET", "/v1/machines/catalog", {
    query: {
      provider: filter?.provider,
      account: filter?.account,
      os: filter?.os,
      region: filter?.region,
    },
  });
}

/**
 * The machine flyco would provision right now, and the catalog entry behind
 * it — what the compute chip shows before anyone commits to a session.
 */
export function getDefaultMachine(
  spot: boolean,
  account?: string,
): Promise<JsonResponse<"flyco_api::machines::get_default_machine", 200>> {
  return requestJson("GET", "/v1/machines/default", {
    query: { spot, account },
  });
}

// --- /v1/approvals ----------------------------------------------------------

export function listApprovals(filter?: {
  session?: string;
  state?: ApprovalState;
}): Promise<JsonResponse<"flyco_api::app::list_approvals", 200>> {
  return requestJson("GET", "/v1/approvals", {
    query: { session: filter?.session, state: filter?.state },
  });
}

export function decideApproval(
  id: string,
  decision: Schemas["ApprovalDecision"],
): Promise<JsonResponse<"flyco_api::app::decide_approval", 200>> {
  const body: JsonBody<"flyco_api::app::decide_approval"> = { decision };
  return requestJson("POST", `/v1/approvals/${id}/decision`, { json: body });
}

// --- /v1/usage ---------------------------------------------------------------

/** One row of `GET /v1/usage/llm`, covering one linked harness account. */
export type LlmUsageRow = JsonResponse<
  "flyco_api::harness_accounts::llm_usage",
  200
>[number];

/** One row of `GET /v1/usage/cloud`, covering one linked provider account. */
export type CloudUsageRow = JsonResponse<
  "flyco_api::provider_accounts::cloud_usage",
  200
>[number];

/**
 * Observed usage per linked harness account.
 *
 * Every field is an observation, never a quota: the harness reports what it
 * spent and when it was limited, and flyco repeats that rather than
 * inventing a "requests remaining" it has no way to know.
 */
export function listLlmUsage(): Promise<
  JsonResponse<"flyco_api::harness_accounts::llm_usage", 200>
> {
  return requestJson("GET", "/v1/usage/llm");
}

/** Metered cloud spend per linked provider account, read from the provider's own meter. */
export function listCloudUsage(
  provider?: CloudProviderKind,
): Promise<JsonResponse<"flyco_api::provider_accounts::cloud_usage", 200>> {
  return requestJson("GET", "/v1/usage/cloud", { query: { provider } });
}

// --- /v1/mcp-servers ---------------------------------------------------------

export function listMcpServers(): Promise<
  JsonResponse<"flyco_api::mcp::list_mcp_servers", 200>
> {
  return requestJson("GET", "/v1/mcp-servers");
}

export function registerMcpServer(
  input: JsonBody<"flyco_api::mcp::register_mcp_server">,
): Promise<JsonResponse<"flyco_api::mcp::register_mcp_server", 201>> {
  return requestJson("POST", "/v1/mcp-servers", { json: input });
}

export function updateMcpServer(
  id: string,
  input: JsonBody<"flyco_api::mcp::update_mcp_server">,
): Promise<JsonResponse<"flyco_api::mcp::update_mcp_server", 200>> {
  return requestJson("PATCH", `/v1/mcp-servers/${id}`, { json: input });
}

export function deleteMcpServer(id: string): Promise<void> {
  return requestVoid("DELETE", `/v1/mcp-servers/${id}`);
}

// --- /v1/skills ---------------------------------------------------------------

export function listSkills(): Promise<
  JsonResponse<"flyco_api::skills::list_skills", 200>
> {
  return requestJson("GET", "/v1/skills");
}

export function uploadSkill(
  name: string,
  scope: SkillScope,
  bundle: Blob,
): Promise<JsonResponse<"flyco_api::skills::upload_skill", 201>> {
  return requestJson("POST", "/v1/skills", {
    query: { name, scope },
    octetStream: bundle,
  });
}

export function deleteSkill(id: string): Promise<void> {
  return requestVoid("DELETE", `/v1/skills/${id}`);
}

// --- /v1/providers -------------------------------------------------------------

export function listProviders(): Promise<
  JsonResponse<"flyco_api::provider_accounts::list_providers", 200>
> {
  return requestJson("GET", "/v1/providers");
}

export function linkProvider(
  input: JsonBody<"flyco_api::provider_accounts::link_provider">,
): Promise<JsonResponse<"flyco_api::provider_accounts::link_provider", 201>> {
  return requestJson("POST", "/v1/providers", { json: input });
}

export function unlinkProvider(id: string): Promise<void> {
  return requestVoid("DELETE", `/v1/providers/${id}`);
}

export function providerQuickstart(
  input: JsonBody<"flyco_api::provider_accounts::provider_quickstart">,
): Promise<
  JsonResponse<"flyco_api::provider_accounts::provider_quickstart", 200>
> {
  return requestJson("POST", "/v1/providers/quickstart", { json: input });
}

/**
 * The least privilege an AWS access key needs, as a document to paste into
 * IAM.
 *
 * Served rather than checked in beside the wizard: it is rendered from the
 * driver's own call sites, so a copy here would be right on the day it was
 * written and quietly wrong afterwards.
 */
export function getAwsIamPolicy(): Promise<
  JsonResponse<"flyco_api::provider_accounts::aws_iam_policy", 200>
> {
  return requestJson("GET", "/v1/providers/aws/iam-policy");
}

// --- /v1/hosts -------------------------------------------------------------------

/**
 * Mints the one line the user runs on the machine they own.
 *
 * The command comes back rendered rather than assembled here: it names the
 * origin of *this* deployment, and a command built in the browser would
 * point at wherever the page happened to be served from.
 */
export function mintEnrollmentToken(): Promise<
  JsonResponse<"flyco_api::hosts::mint_enrollment_token", 201>
> {
  return requestJson("POST", "/v1/hosts/enrollment-tokens");
}

/**
 * Asks once whether the machine has arrived.
 *
 * `pending` until some machine spends the token, then `enrolled` carrying
 * the host it created — the whole of what the wizard waits on.
 */
export function getEnrollment(
  id: EnrollmentToken["id"],
): Promise<JsonResponse<"flyco_api::hosts::get_enrollment", 200>> {
  return requestJson("GET", `/v1/hosts/enrollment-tokens/${id}`);
}

/** The caller's enrolled machines, each refreshed from its own room. */
export function listHosts(): Promise<
  JsonResponse<"flyco_api::hosts::list_hosts", 200>
> {
  return requestJson("GET", "/v1/hosts");
}

/** One machine, with the state and facts its room last reported. */
export function getHost(
  id: HostView["id"],
): Promise<JsonResponse<"flyco_api::hosts::get_host", 200>> {
  return requestJson("GET", `/v1/hosts/${id}`);
}

/** Renames a machine. It opens as the hostname it enrolled with. */
export function renameHost(
  id: HostView["id"],
  label: string,
): Promise<JsonResponse<"flyco_api::hosts::update_host", 200>> {
  const body: JsonBody<"flyco_api::hosts::update_host"> = { label };
  return requestJson("PATCH", `/v1/hosts/${id}`, { json: body });
}

/**
 * Drains a machine and revokes its token.
 *
 * Refused as `host-has-active-sessions` while sessions are still running
 * there; `force` stops their containers and keeps the volumes, so the work
 * is still on the disk the user owns.
 */
export function removeHost(
  id: HostView["id"],
  options: { force?: boolean } = {},
): Promise<void> {
  return requestVoid("DELETE", `/v1/hosts/${id}`, {
    query: { force: options.force === true ? true : null },
  });
}

// --- /v1/api-keys ---------------------------------------------------------------

export function listApiKeys(): Promise<
  JsonResponse<"flyco_api::app::list_api_keys", 200>
> {
  return requestJson("GET", "/v1/api-keys");
}

export function createApiKey(
  label: string,
): Promise<JsonResponse<"flyco_api::app::create_api_key", 200>> {
  const body: JsonBody<"flyco_api::app::create_api_key"> = { label };
  return requestJson("POST", "/v1/api-keys", { json: body });
}

export function revokeApiKey(id: string): Promise<void> {
  return requestVoid("DELETE", `/v1/api-keys/${id}`);
}

// --- /v1/cli-sessions ---------------------------------------------------------------

/**
 * Approves a `flyco login` attempt (`/cli/authorize`). The minted key never
 * reaches the browser: it is held server-side until the CLI's own poll
 * collects it, so approval here is the whole browser-side job.
 */
export function approveCliSession(id: string): Promise<void> {
  return requestVoid("POST", `/v1/cli-sessions/${id}/approve`);
}

/** Denies a `flyco login` attempt; the CLI's poll then stops waiting. */
export function denyCliSession(id: string): Promise<void> {
  return requestVoid("POST", `/v1/cli-sessions/${id}/deny`);
}

// --- /v1/harness-accounts -----------------------------------------------------

export function listHarnessAccounts(): Promise<
  JsonResponse<"flyco_api::harness_accounts::list_harness_accounts", 200>
> {
  return requestJson("GET", "/v1/harness-accounts");
}

export function linkHarnessAccount(
  input: JsonBody<"flyco_api::harness_accounts::link_harness_account">,
): Promise<
  JsonResponse<"flyco_api::harness_accounts::link_harness_account", 201>
> {
  return requestJson("POST", "/v1/harness-accounts", { json: input });
}

export function unlinkHarnessAccount(
  id: HarnessAccountView["id"],
): Promise<void> {
  return requestVoid("DELETE", `/v1/harness-accounts/${id}`);
}

/**
 * Begins the Claude sign-in (docs/ux.md §8.1).
 *
 * The PKCE verifier stays in the control plane; what comes back is the URL
 * to open and the opaque attempt id the pasted code is redeemed against.
 */
export function startClaudeOauth(): Promise<
  JsonResponse<"flyco_api::claude_oauth::start", 200>
> {
  return requestJson("POST", "/v1/harness-accounts/claude/oauth/start");
}

/** Redeems what Anthropic showed the user, linking the account. */
export function completeClaudeOauth(
  input: JsonBody<"flyco_api::claude_oauth::complete">,
): Promise<JsonResponse<"flyco_api::claude_oauth::complete", 201>> {
  return requestJson("POST", "/v1/harness-accounts/claude/oauth/complete", {
    json: input,
  });
}

/**
 * Begins the Codex sign-in (docs/ux.md §8.1).
 *
 * What comes back is what `codex login --device-auth` prints — a one-time
 * code, the page to type it on, and how often to ask whether it has been
 * approved. OpenAI's `device_auth_id`, the half that redeems the grant,
 * stays in the control plane.
 */
export function startCodexOauth(): Promise<
  JsonResponse<"flyco_api::codex_oauth::start", 200>
> {
  return requestJson("POST", "/v1/harness-accounts/codex/oauth/start");
}

/** How far one poll of a Codex sign-in got. */
export type CodexOauthProgress =
  { state: "pending" } | { state: "linked"; account: HarnessAccountView };

/**
 * Asks once whether the user has approved the code yet.
 *
 * The two outcomes are told apart by the status code — `200` is still
 * waiting, `201` created the account — so this reads the status rather than
 * guessing from the shape of the body.
 */
export async function pollCodexOauth(
  attemptId: CodexOauthStart["attempt_id"],
): Promise<CodexOauthProgress> {
  const response = await send(
    "GET",
    `/v1/harness-accounts/codex/oauth/${attemptId}`,
  );
  if (response.status === 201) {
    const account = (await response.json()) as JsonResponse<
      "flyco_api::codex_oauth::poll",
      201
    >;
    return { state: "linked", account };
  }
  return { state: "pending" };
}

// --- /v1/providers/{azure,gcp,codespaces}/oauth -----------------------------------

/** The clouds whose own consent screen links an account. */
export type ConsentCloudKind = "azure" | "gcp" | "codespaces";

/** `POST /v1/providers/{cloud}/oauth/start`. */
export type ProviderOauthStart = Schemas["ProviderOauthStart"];

/** One thing the signed-in account may link: a subscription, or a project. */
export type ProviderOauthChoice = Schemas["ProviderOauthChoice"];

/** `GET /v1/providers/{cloud}/oauth/{attempt}`: where the consent stands. */
export type ProviderOauthProgress = Schemas["ProviderOauthProgress"];

export async function startProviderOauth(
  cloud: ConsentCloudKind,
): Promise<ProviderOauthStart> {
  return requestJson("POST", `/v1/providers/${cloud}/oauth/start`);
}

export async function pollProviderOauth(
  cloud: ConsentCloudKind,
  attemptId: string,
): Promise<ProviderOauthProgress> {
  return requestJson("GET", `/v1/providers/${cloud}/oauth/${attemptId}`);
}

/** Creates flyco's own identity in the chosen subscription and links it. */
export async function finishAzureOauth(
  attemptId: string,
  body: Schemas["FinishAzureOauth"],
): Promise<ProviderAccountView> {
  return requestJson("POST", `/v1/providers/azure/oauth/${attemptId}/finish`, {
    json: body,
  });
}

/** Creates flyco's own service account in the chosen project and links it. */
export async function finishGcpOauth(
  attemptId: string,
  body: Schemas["FinishGcpOauth"],
): Promise<ProviderAccountView> {
  return requestJson("POST", `/v1/providers/gcp/oauth/${attemptId}/finish`, {
    json: body,
  });
}

/**
 * Links the GitHub account the consent came back with: the environment
 * repository is created control-plane side, so there is nothing left to
 * choose.
 */
export async function finishCodespacesOauth(
  attemptId: string,
): Promise<ProviderAccountView> {
  return requestJson(
    "POST",
    `/v1/providers/codespaces/oauth/${attemptId}/finish`,
    { json: {} },
  );
}

// --- /v1/memory ----------------------------------------------------------------

export function listMemory(filter?: {
  parent?: string;
  repo?: string;
}): Promise<JsonResponse<"flyco_api::memory::list_memory", 200>> {
  return requestJson("GET", "/v1/memory", {
    query: { parent: filter?.parent, repo: filter?.repo },
  });
}

export function createMemoryNode(
  input: JsonBody<"flyco_api::memory::create_memory_node">,
): Promise<JsonResponse<"flyco_api::memory::create_memory_node", 201>> {
  return requestJson("POST", "/v1/memory", { json: input });
}

export function updateMemoryNode(
  id: string,
  patch: JsonBody<"flyco_api::memory::update_memory_node">,
): Promise<JsonResponse<"flyco_api::memory::update_memory_node", 200>> {
  return requestJson("PATCH", `/v1/memory/${id}`, { json: patch });
}

export function deleteMemoryNode(id: string): Promise<void> {
  return requestVoid("DELETE", `/v1/memory/${id}`);
}

// --- /v1/agents-md ---------------------------------------------------------------

export function getAgentsMd(): Promise<
  JsonResponse<"flyco_api::agents_md::get_agents_md", 200>
> {
  return requestJson("GET", "/v1/agents-md");
}

export function putAgentsMd(
  content: string,
): Promise<JsonResponse<"flyco_api::agents_md::put_agents_md", 200>> {
  const body: JsonBody<"flyco_api::agents_md::put_agents_md"> = { content };
  return requestJson("PUT", "/v1/agents-md", { json: body });
}

// --- /v1/push --------------------------------------------------------------------

export function getVapidPublicKey(): Promise<
  JsonResponse<"flyco_api::push::vapid_public_key", 200>
> {
  return requestJson("GET", "/v1/push/vapid-public-key");
}

export function subscribePush(
  subscription: JsonBody<"flyco_api::push::subscribe_push">,
): Promise<JsonResponse<"flyco_api::push::subscribe_push", 201>> {
  return requestJson("POST", "/v1/push/subscriptions", { json: subscription });
}

export function unsubscribePush(id: string): Promise<void> {
  return requestVoid("DELETE", `/v1/push/subscriptions/${id}`);
}

// --- Auth and repo picker ---------------------------------------------------------

export function startGithubLogin(): Promise<
  JsonResponse<"flyco_api::oauth::start", 200>
> {
  return requestJson("POST", "/v1/auth/github/start");
}

export function listRepos(
  q?: string,
): Promise<JsonResponse<"flyco_api::repos::list_repos", 200>> {
  return requestJson("GET", "/v1/github/repos", { query: { q } });
}

/**
 * One page of a repository's branches, the default branch first.
 *
 * `slug` is `owner/name`; both halves are path segments, so they are
 * encoded rather than interpolated raw.
 */
export function listBranches(
  slug: string,
  cursor?: string,
): Promise<JsonResponse<"flyco_api::repos::list_branches", 200>> {
  const path = slug
    .split("/")
    .map((segment) => encodeURIComponent(segment))
    .join("/");
  return requestJson("GET", `/v1/github/repos/${path}/branches`, {
    query: { cursor },
  });
}
