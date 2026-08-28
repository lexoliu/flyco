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

/** The SDK's `SessionKey`, in this protocol's `snake_case`. */
export const sessionKeySchema = z.object({
  project_key: z.string(),
  session_id: z.string(),
  subpath: z.string().optional(),
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
    permission_mode: permissionModeSchema,
    resume_session_id: z.string().nullable(),
  }),
  z.object({ type: z.literal("user_message"), text: z.string() }),
  z.object({ type: z.literal("interrupt") }),
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
/** The SDK's `SessionKey`, in this protocol's spelling. */
export type SessionKey = z.infer<typeof sessionKeySchema>;
/** One `SessionStore` operation. */
export type StoreOp = z.infer<typeof storeOpSchema>;
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
