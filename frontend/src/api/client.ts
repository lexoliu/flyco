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
 * `sendMessage`, `interruptSession`, and `compactSession` below hit the REST handlers
 * directly. The live session view (`src/routes/SessionDetail.tsx`) prefers
 * the relay socket instead (`src/api/relay.ts`, `ControlToDaemon::is_client_command`)
 * whenever it is connected — lower latency, and the echo comes back as a
 * `ClientEvent` on the same connection — and falls back to these REST calls
 * only while the socket isn't live (session paused, machine stopped, still
 * reconnecting), so a message or interrupt can still be recorded rather
 * than silently dropped.
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
export type RelayTicket = Schemas["RelayTicket"];
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
export type CloudProviderKind = Schemas["CloudProviderKind"];
export type ApiKeySummary = Schemas["ApiKeySummary"];
export type CreatedApiKey = Schemas["CreatedApiKey"];
export type AuthorizeUrl = Schemas["AuthorizeUrl"];
export type RepoSummary = Schemas["RepoSummary"];
export type HarnessKind = Schemas["HarnessKind"];
export type SessionState = Schemas["SessionState"];
export type MachineCatalogEntry = Schemas["MachineCatalogEntry"];
export type MachineDefault = Schemas["MachineDefault"];
export type MachineChoice = Schemas["MachineChoice"];
export type MachineOrigin = Schemas["MachineOrigin"];
export type MachineView = Schemas["MachineView"];
export type MachineSpec = Schemas["MachineSpec"];
export type MachineState = Schemas["MachineState"];
export type MachinePricing = Schemas["MachinePricing"];
export type MachineCapacity = Schemas["MachineCapacity"];
export type OsFamily = Schemas["OsFamily"];
export type HarnessAccountView = Schemas["HarnessAccountView"];
export type HarnessCredentialInput = Schemas["HarnessCredentialInput"];
export type ClaudeOauthStart = Schemas["ClaudeOauthStart"];
export type MemoryNode = Schemas["MemoryNode"];
export type AgentsDocument = Schemas["AgentsDocument"];
export type PushSubscriptionView = Schemas["PushSubscriptionView"];
export type VapidPublicKey = Schemas["VapidPublicKey"];
export type TurnSummary = Schemas["TurnSummary"];
export type TurnPage = Schemas["TurnPage"];
export type RepoStatus = Schemas["RepoStatus"];
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
type JsonResponse<Op extends keyof operations, Status extends number> = operations[Op]["responses"] extends Record<
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

/**
 * Resolves an API path to its `ws:`/`wss:` equivalent, for the relay
 * socket. Mirrors the scheme of the resolved HTTP(S) origin: `wss:` unless
 * the control plane itself is served over plain `http:` (local dev only).
 */
export function apiWebSocketUrl(path: string): URL {
  const url = apiUrl(path);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  return url;
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

interface SendOptions {
  query?: Query;
  json?: unknown;
  octetStream?: Blob;
}

/** Issues one HTTP request, attaching auth and translating any failure. */
async function send(method: string, path: string, options: SendOptions = {}): Promise<Response> {
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
    response = await fetch(url, { method, headers, body });
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

async function requestJson<T>(method: string, path: string, options: SendOptions = {}): Promise<T> {
  const response = await send(method, path, options);
  return (await response.json()) as T;
}

async function requestVoid(method: string, path: string, options: SendOptions = {}): Promise<void> {
  await send(method, path, options);
}

// --- /v1/me ---------------------------------------------------------------

export function getMe(): Promise<JsonResponse<"flyco_api::app::me", 200>> {
  return requestJson("GET", "/v1/me");
}

export function updateMe(
  sessionCap: number | null,
): Promise<JsonResponse<"flyco_api::app::update_me", 200>> {
  const body: JsonBody<"flyco_api::app::update_me"> = { session_cap: sessionCap };
  return requestJson("PATCH", "/v1/me", { json: body });
}

// --- /v1/sessions -----------------------------------------------------------

export function listHarnessFeatures(): Promise<
  JsonResponse<"flyco_api::app::list_harness_features", 200>
> {
  return requestJson("GET", "/v1/harness-features");
}

export function listSessions(): Promise<JsonResponse<"flyco_api::app::list_sessions", 200>> {
  return requestJson("GET", "/v1/sessions");
}

export function createSession(
  input: JsonBody<"flyco_api::app::create_session">,
): Promise<JsonResponse<"flyco_api::app::create_session", 201>> {
  return requestJson("POST", "/v1/sessions", { json: input });
}

export function getSession(id: string): Promise<JsonResponse<"flyco_api::app::get_session", 200>> {
  return requestJson("GET", `/v1/sessions/${id}`);
}

/** Renames a session. The title is what every list row is identified by. */
export function updateSession(
  id: string,
  title: string,
): Promise<JsonResponse<"flyco_api::app::update_session", 200>> {
  const body: JsonBody<"flyco_api::app::update_session"> = { title };
  return requestJson("PATCH", `/v1/sessions/${id}`, { json: body });
}

export function archiveSession(
  id: string,
  options: { discardUncommitted?: boolean } = {},
): Promise<JsonResponse<"flyco_api::app::archive_session", 200>> {
  return requestJson("POST", `/v1/sessions/${id}/archive`, {
    query: { discard_uncommitted: options.discardUncommitted === true ? true : null },
  });
}

export function getSessionBudget(id: string): Promise<JsonResponse<"flyco_api::app::get_session_budget", 200>> {
  return requestJson("GET", `/v1/sessions/${id}/budget`);
}

export function getSessionEvents(
  id: string,
  after?: number,
): Promise<JsonResponse<"flyco_api::app::get_session_events", 200>> {
  return requestJson("GET", `/v1/sessions/${id}/events`, { query: { after: after ?? null } });
}

export function createRelayTicket(
  id: string,
): Promise<JsonResponse<"flyco_api::app::create_relay_ticket", 200>> {
  return requestJson("POST", `/v1/sessions/${id}/relay-ticket`);
}

export function getSessionEnv(id: string): Promise<JsonResponse<"flyco_api::app::get_session_env", 200>> {
  return requestJson("GET", `/v1/sessions/${id}/env`);
}

export function putSessionEnv(
  id: string,
  entries: EnvEntry[],
): Promise<JsonResponse<"flyco_api::app::put_session_env", 200>> {
  const body: JsonBody<"flyco_api::app::put_session_env"> = { entries };
  return requestJson("PUT", `/v1/sessions/${id}/env`, { json: body });
}

export function getRepoStatus(id: string): Promise<JsonResponse<"flyco_api::app::get_repo_status", 200>> {
  return requestJson("GET", `/v1/sessions/${id}/repo-status`);
}

/** REST fallback for sending a message; see the module doc comment above. */
export function sendMessage(id: string, text: string): Promise<void> {
  const body: JsonBody<"flyco_api::app::send_message"> = { text };
  return requestVoid("POST", `/v1/sessions/${id}/messages`, { json: body });
}

/** REST fallback for interrupting a turn; see the module doc comment above. */
export function interruptSession(id: string): Promise<void> {
  return requestVoid("POST", `/v1/sessions/${id}/interrupt`);
}

/** REST fallback for compacting session context; see the module doc comment above. */
export function compactSession(id: string): Promise<void> {
  return requestVoid("POST", `/v1/sessions/${id}/compact`);
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
export function resizeSessionMachine(id: string, machineType: string): Promise<void> {
  const body: JsonBody<"flyco_api::machines::resize_session_machine"> = { machine_type: machineType };
  return requestVoid("POST", `/v1/sessions/${id}/machine/resize`, { json: body });
}

// --- /v1/machines/catalog -----------------------------------------------------

export function getMachineCatalog(filter?: {
  provider?: CloudProviderKind;
  os?: OsFamily;
  region?: string;
}): Promise<JsonResponse<"flyco_api::machines::get_catalog", 200>> {
  return requestJson("GET", "/v1/machines/catalog", {
    query: { provider: filter?.provider, os: filter?.os, region: filter?.region },
  });
}

/**
 * The machine flyco would provision right now, and the catalog entry behind
 * it — what the compute chip shows before anyone commits to a session.
 */
export function getDefaultMachine(
  spot: boolean,
): Promise<JsonResponse<"flyco_api::machines::get_default_machine", 200>> {
  return requestJson("GET", "/v1/machines/default", { query: { spot } });
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

export function listMcpServers(): Promise<JsonResponse<"flyco_api::mcp::list_mcp_servers", 200>> {
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

export function listSkills(): Promise<JsonResponse<"flyco_api::skills::list_skills", 200>> {
  return requestJson("GET", "/v1/skills");
}

export function uploadSkill(
  name: string,
  scope: SkillScope,
  bundle: Blob,
): Promise<JsonResponse<"flyco_api::skills::upload_skill", 201>> {
  return requestJson("POST", "/v1/skills", { query: { name, scope }, octetStream: bundle });
}

export function deleteSkill(id: string): Promise<void> {
  return requestVoid("DELETE", `/v1/skills/${id}`);
}

// --- /v1/providers -------------------------------------------------------------

export function listProviders(): Promise<JsonResponse<"flyco_api::provider_accounts::list_providers", 200>> {
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
): Promise<JsonResponse<"flyco_api::provider_accounts::provider_quickstart", 200>> {
  return requestJson("POST", "/v1/providers/quickstart", { json: input });
}

// --- /v1/api-keys ---------------------------------------------------------------

export function listApiKeys(): Promise<JsonResponse<"flyco_api::app::list_api_keys", 200>> {
  return requestJson("GET", "/v1/api-keys");
}

export function createApiKey(label: string): Promise<JsonResponse<"flyco_api::app::create_api_key", 200>> {
  const body: JsonBody<"flyco_api::app::create_api_key"> = { label };
  return requestJson("POST", "/v1/api-keys", { json: body });
}

export function revokeApiKey(id: string): Promise<void> {
  return requestVoid("DELETE", `/v1/api-keys/${id}`);
}

// --- /v1/harness-accounts -----------------------------------------------------

export function listHarnessAccounts(): Promise<
  JsonResponse<"flyco_api::harness_accounts::list_harness_accounts", 200>
> {
  return requestJson("GET", "/v1/harness-accounts");
}

export function linkHarnessAccount(
  input: JsonBody<"flyco_api::harness_accounts::link_harness_account">,
): Promise<JsonResponse<"flyco_api::harness_accounts::link_harness_account", 201>> {
  return requestJson("POST", "/v1/harness-accounts", { json: input });
}

export function unlinkHarnessAccount(id: HarnessAccountView["id"]): Promise<void> {
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
  return requestJson("POST", "/v1/harness-accounts/claude/oauth/complete", { json: input });
}

// --- /v1/memory ----------------------------------------------------------------

export function listMemory(filter?: {
  parent?: string;
  repo?: string;
}): Promise<JsonResponse<"flyco_api::memory::list_memory", 200>> {
  return requestJson("GET", "/v1/memory", { query: { parent: filter?.parent, repo: filter?.repo } });
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

export function getAgentsMd(): Promise<JsonResponse<"flyco_api::agents_md::get_agents_md", 200>> {
  return requestJson("GET", "/v1/agents-md");
}

export function putAgentsMd(content: string): Promise<JsonResponse<"flyco_api::agents_md::put_agents_md", 200>> {
  const body: JsonBody<"flyco_api::agents_md::put_agents_md"> = { content };
  return requestJson("PUT", "/v1/agents-md", { json: body });
}

// --- /v1/push --------------------------------------------------------------------

export function getVapidPublicKey(): Promise<JsonResponse<"flyco_api::push::vapid_public_key", 200>> {
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

export function startGithubLogin(): Promise<JsonResponse<"flyco_api::oauth::start", 200>> {
  return requestJson("POST", "/v1/auth/github/start");
}

export function listRepos(q?: string): Promise<JsonResponse<"flyco_api::repos::list_repos", 200>> {
  return requestJson("GET", "/v1/github/repos", { query: { q } });
}
