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
 * `send_message` and `interrupt_session` are deliberately absent: their
 * OpenAPI contracts exist, but the handlers are `todo!()` in this build
 * (tracked by `crates/api/src/tests/contract.rs`'s `EXPECTED` list) and
 * panic rather than answer. Both are also valid client-sendable frames on
 * the live relay socket (`ControlToDaemon::is_client_command`), which *is*
 * wired up today, so `src/api/relay.ts` sends them directly over the
 * WebSocket instead of through REST.
 */
import type { components, operations } from "./schema";
import { getSessionToken } from "../lib/session";
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

export function archiveSession(id: string): Promise<JsonResponse<"flyco_api::app::archive_session", 200>> {
  return requestJson("POST", `/v1/sessions/${id}/archive`);
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

// --- Auth and repo picker ---------------------------------------------------------

export function startGithubLogin(): Promise<JsonResponse<"flyco_api::oauth::start", 200>> {
  return requestJson("POST", "/v1/auth/github/start");
}

export function listRepos(q?: string): Promise<JsonResponse<"flyco_api::repos::list_repos", 200>> {
  return requestJson("GET", "/v1/github/repos", { query: { q } });
}
