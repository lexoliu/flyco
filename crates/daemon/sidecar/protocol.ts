/**
 * The flycod⇄sidecar protocol, mirrored from
 * `crates/daemon/src/harness/claude/protocol.rs`.
 *
 * One JSON object per line in each direction. Every message is internally
 * tagged on `type` and uses `snake_case` field names — including where the
 * Agent SDK's own types are `camelCase`, which `sidecar.ts` translates at
 * the boundary so this protocol has exactly one naming convention.
 *
 * These are zod schemas rather than bare interfaces because the sidecar
 * validates every inbound line: a command it cannot decode is a contract
 * break between two halves of the same program, and it fails loudly instead
 * of guessing. `fixtures/protocol/` pins these declarations against the Rust
 * ones — see `protocol.test.ts`.
 */
import { z } from "zod";

/**
 * Any JSON value, required to be present.
 *
 * `z.unknown()` would also accept a missing key, which would let a field
 * silently disappear from the protocol; this keeps presence part of the
 * contract while passing the value through untouched.
 */
const jsonValue = z.custom<unknown>((value) => value !== undefined, {
  message: "expected a JSON value to be present",
});

/** Mirrors the SDK's own `PermissionMode` union. */
export const permissionModeSchema = z.enum([
  "default",
  "acceptEdits",
  "bypassPermissions",
  "plan",
  "dontAsk",
  "auto",
]);

/** How the sidecar authenticates the `claude` CLI it supervises. */
export const sidecarAuthSchema = z.discriminatedUnion("mode", [
  z.object({ mode: z.literal("inherit") }),
  z.object({ mode: z.literal("oauth_token"), token: z.string() }),
  z.object({ mode: z.literal("api_key"), key: z.string() }),
]);

/**
 * One MCP server, in Claude Code's own vocabulary.
 *
 * The single place this protocol is not `snake_case`, and for the same
 * reason `sdk_message` is opaque: the value is the SDK's own
 * `McpServerConfig`, not a flycod restatement of it, so it travels from
 * `crates/daemon/src/mount.rs` into `Options.mcpServers` untouched. It is
 * also, byte for byte, what the machine's root-owned `managed-mcp.json`
 * declares — one set of servers, described once.
 *
 * Nothing here is optional even where the SDK's own type allows it: under
 * `exactOptionalPropertyTypes` a `string[] | undefined` is not a `string[]`,
 * and requiring the empty value keeps one spelling for one server on both
 * sides of the protocol.
 */
export const claudeMcpServerSchema = z.discriminatedUnion("type", [
  z.object({
    type: z.literal("stdio"),
    command: z.string(),
    args: z.array(z.string()),
    env: z.record(z.string(), z.string()),
    alwaysLoad: z.boolean(),
  }),
  z.object({
    type: z.literal("http"),
    url: z.string(),
    headers: z.record(z.string(), z.string()),
  }),
]);

/**
 * Where one mounted server has got to.
 *
 * `pending` is its own state rather than "not connected": a CLI still
 * dialling a server has not answered the question yet, and flycod waits it
 * out instead of refusing the session for being asked early.
 */
export const mountStateSchema = z.enum(["connected", "pending", "failed"]);

/** What the CLI reports about one server it was told to mount. */
export const mountedServerSchema = z.object({
  name: z.string(),
  status: z.string(),
  state: mountStateSchema,
  tools: z.array(z.string()),
});

/**
 * One model the harness offers, in flyco's own vocabulary.
 *
 * `snake_case`, like the rest of this protocol, and deliberately not the
 * SDK's `ModelInfo`: the same shape has to come back from Codex, so the
 * translation happens here at the boundary rather than in three readers
 * downstream. `efforts` is empty for a model that accepts none, which is a
 * real answer rather than a missing one.
 */
export const modelOptionSchema = z.object({
  id: z.string(),
  label: z.string(),
  description: z.string(),
  is_default: z.boolean(),
  efforts: z.array(z.string()),
  default_effort: z.string().nullable(),
});

/**
 * One rolling plan window, as this sidecar read it from the SDK.
 *
 * The reading and not the finished window: the *label* ("5-hour",
 * "Weekly (Fable)") is derived in Rust, because the same rule has to serve
 * Codex and writing it twice would be two vocabularies. What the sidecar
 * contributes is what only it knows — that the SDK's `five_hour` key means
 * three hundred minutes, and that a `model_scoped` row is a weekly window
 * scoped to one model.
 *
 * `used_percent` is an integer 0-100: the SDK's `utilization` is an
 * unbounded number, and it is rounded and clamped here so the wire carries
 * one spelling of a percentage.
 */
export const usageWindowSchema = z.object({
  window_minutes: z.number().int().nullable(),
  scope: z.string().nullable(),
  used_percent: z.number().int().min(0).max(100),
  resets_at_unix: z.number().int().nullable(),
});

/**
 * One slash command the running CLI offers, in flyco's own vocabulary.
 *
 * `snake_case` and not the SDK's `SlashCommand`, for the reason the model
 * option above is not `ModelInfo`: the same shape has to come back from
 * Codex. `argument_hint` is `null` for a command that takes no argument —
 * the SDK spells that as an empty string, and a palette that has to tell
 * `""` from "unset" is a palette with a bug waiting in it. Aliases are
 * dropped: `/cost` and `/stats` are two more rows saying what `/usage`
 * already says.
 */
export const harnessCommandSchema = z.object({
  name: z.string(),
  description: z.string(),
  argument_hint: z.string().nullable(),
});

/** The SDK's `SessionKey`, in this protocol's `snake_case`. */
export const sessionKeySchema = z.object({
  project_key: z.string(),
  session_id: z.string(),
  subpath: z.string().optional(),
});

/**
 * One thing occupying the context window, as a context panel lists it.
 *
 * One shape for every section the CLI reports — a usage category, an MCP
 * tool's schema, a memory file, an agent, a skill's frontmatter — because
 * each answers the same question (what is this, and what does it cost) and
 * the panel's rows differ only in which list they came from. `deferred` is
 * counted toward the window but not materialized in it yet — a deferred
 * MCP tool's schema, for instance, summarized until first called.
 */
export const contextCostSchema = z.object({
  name: z.string(),
  tokens: z.number().int().nonnegative(),
  deferred: z.boolean(),
});

/** How much of a model's context window is spoken for. */
export const contextWindowSchema = z.object({
  used_tokens: z.number().int().nonnegative(),
  size_tokens: z.number().int().nonnegative(),
});

/**
 * What the context window is spent on — the answer to `/context`.
 *
 * The SDK's `getContextUsage` answers in its own shape (`totalTokens`,
 * `maxTokens`, `isDeferred`, …) and the mapping lives in the sidecar for
 * the same reason `usageWindowSchema` does: this shape has to come back
 * from Codex spelled the same way, so the vocabulary is flyco's from here.
 * `window.size_tokens` is the SDK's `maxTokens` — the effective ceiling
 * its `percentage` is measured against, not the model's `rawMaxTokens`.
 */
export const contextUsageSchema = z.object({
  model: z.string().optional(),
  window: contextWindowSchema.optional(),
  auto_compact: z.number().int().nonnegative().optional(),
  categories: z.array(contextCostSchema),
  mcp_tools: z.array(contextCostSchema),
  memory_files: z.array(contextCostSchema),
  agents: z.array(contextCostSchema),
  skills: z.array(contextCostSchema),
});

/** One `SessionStore` operation flycod must perform. */
export const storeOpSchema = z.union([
  z.object({
    append: z.object({ key: sessionKeySchema, entries: z.array(jsonValue) }),
  }),
  z.object({ load: z.object({ key: sessionKeySchema }) }),
]);

/** A command flycod writes to the sidecar. */
export const sidecarCommandSchema = z.discriminatedUnion("type", [
  z.object({
    type: z.literal("start"),
    cwd: z.string(),
    auth: sidecarAuthSchema,
    config_dir: z.string().nullable(),
    project_dir_name: z.string().nullable(),
    model: z.string().nullable(),
    effort: z.string().nullable(),
    permission_mode: permissionModeSchema,
    resume_session_id: z.string().nullable(),
    mcp_servers: z.record(z.string(), claudeMcpServerSchema),
    strict_mcp_config: z.boolean(),
  }),
  z.object({ type: z.literal("user_message"), text: z.string() }),
  z.object({ type: z.literal("interrupt") }),
  z.object({ type: z.literal("compact") }),
  z.object({ type: z.literal("context_usage") }),
  z.object({
    type: z.literal("set_model"),
    model: z.string(),
    effort: z.string().nullable(),
  }),
  z.object({
    type: z.literal("set_permission_mode"),
    mode: permissionModeSchema,
  }),
  z.object({
    type: z.literal("approval_decision"),
    id: z.string(),
    allow: z.boolean(),
    updated_input: jsonValue,
    message: z.string().nullable(),
  }),
  z.object({
    type: z.literal("store_response"),
    id: z.number().int(),
    result: jsonValue,
  }),
  z.object({ type: z.literal("shutdown") }),
]);

/** An event the sidecar writes to flycod. */
export const sidecarEventSchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("ready"), sdk_version: z.string() }),
  z.object({ type: z.literal("started"), session_id: z.string() }),
  z.object({ type: z.literal("capabilities"), capabilities: z.array(z.string()) }),
  z.object({ type: z.literal("models"), models: z.array(modelOptionSchema) }),
  z.object({ type: z.literal("plan_usage"), windows: z.array(usageWindowSchema) }),
  z.object({ type: z.literal("context_usage"), usage: contextUsageSchema }),
  z.object({ type: z.literal("commands"), commands: z.array(harnessCommandSchema) }),
  z.object({ type: z.literal("mcp_servers"), servers: z.array(mountedServerSchema) }),
  z.object({ type: z.literal("sdk_message"), message: jsonValue }),
  z.object({
    type: z.literal("approval_request"),
    id: z.string(),
    tool: z.string(),
    input: jsonValue,
    suggestions: jsonValue,
  }),
  z.object({
    type: z.literal("store_request"),
    id: z.number().int(),
    op: storeOpSchema,
  }),
  z.object({ type: z.literal("fatal"), error: z.string() }),
]);

/** The permission mode the session runs under. */
export type PermissionMode = z.infer<typeof permissionModeSchema>;
/** How the supervised CLI authenticates. */
export type SidecarAuth = z.infer<typeof sidecarAuthSchema>;
/** One MCP server, as the SDK's `mcpServers` option takes it. */
export type ClaudeMcpServer = z.infer<typeof claudeMcpServerSchema>;
/** One model the harness offers. */
export type ModelOption = z.infer<typeof modelOptionSchema>;
/** One rolling plan window, as read from the SDK. */
export type UsageWindow = z.infer<typeof usageWindowSchema>;
/** One slash command the harness offers. */
export type HarnessCommand = z.infer<typeof harnessCommandSchema>;
/** Where one mounted MCP server has got to. */
export type MountState = z.infer<typeof mountStateSchema>;
/** What the CLI reports about one mounted MCP server. */
export type MountedServer = z.infer<typeof mountedServerSchema>;
/** The SDK's `SessionKey`, in this protocol's spelling. */
export type SessionKey = z.infer<typeof sessionKeySchema>;
/** One `SessionStore` operation. */
export type StoreOp = z.infer<typeof storeOpSchema>;
/** One thing occupying the context window. */
export type ContextCost = z.infer<typeof contextCostSchema>;
/** What the context window is spent on. */
export type ContextUsage = z.infer<typeof contextUsageSchema>;
/** A command flycod writes to the sidecar. */
export type SidecarCommand = z.infer<typeof sidecarCommandSchema>;
/** An event the sidecar writes to flycod. */
export type SidecarEvent = z.infer<typeof sidecarEventSchema>;
/** The `start` command, which opens the session. */
export type StartCommand = Extract<SidecarCommand, { type: "start" }>;

/**
 * A single-line description of a failure, suitable for a `fatal` event.
 *
 * A zod failure is rendered from its issue list rather than its default
 * JSON dump, so a protocol mismatch reads as a sentence in flycod's log.
 */
export function describeError(error: unknown): string {
  if (error instanceof z.ZodError) {
    return `the line does not match the protocol: ${z
      .prettifyError(error)
      .split("\n")
      .map((part) => part.trim())
      .filter((part) => part.length > 0)
      .join("; ")}`;
  }
  return error instanceof Error ? error.message : String(error);
}
