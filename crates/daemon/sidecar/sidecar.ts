/**
 * flycod's Claude Code sidecar.
 *
 * The Claude Agent SDK is TypeScript-only, and it is the only supported
 * route to the three things flyco's product needs: `canUseTool` approval
 * callbacks, `interrupt()`, and a `SessionStore` flyco controls. This
 * process owns one SDK session and does nothing else — every decision about
 * what a message *means* is made in Rust. Its whole job is:
 *
 * - announce the session (`started`) the moment the query is constructed —
 *   the CLI is spawned and handshaking by then, so flyco has a warm,
 *   identified session before the user types,
 * - report the CLI's capability list (`capabilities`) from every
 *   `system/init` frame, which is the only place it appears and therefore
 *   only from the first turn onward,
 * - report the models this CLI build offers (`models`) as soon as the
 *   handshake is answered, so the composer's picker is right for the very
 *   first message,
 * - report the slash commands this session offers (`commands`) on the same
 *   handshake and again on every `commands_changed` frame, so the
 *   composer's `/` palette is the CLI's own list rather than a guess,
 * - turn flycod's `user_message` commands into a streaming-input generator,
 * - forward every SDK message out verbatim as `sdk_message`,
 * - park `canUseTool` on flycod until an `approval_decision` arrives,
 * - park `SessionStore` calls on flycod until a `store_response` arrives.
 *
 * It exits on `shutdown`, and it fails loudly — one `fatal` line, non-zero
 * status — on anything it cannot honour. flycod never signals it: a turn
 * ends through the SDK's own `interrupt()`.
 */
import { dirname, join } from "node:path";

import {
  query,
  type CanUseTool,
  type EffortLevel,
  type McpServerStatus,
  type ModelInfo,
  type Options,
  type PermissionResult,
  type Query,
  type SDKControlGetUsageResponse,
  type SDKUserMessage,
  type SessionKey as SdkSessionKey,
  type SessionStore,
  type SessionStoreEntry,
  type SlashCommand,
} from "@anthropic-ai/claude-agent-sdk";

import {
  describeError,
  sidecarCommandSchema,
  type ContextUsage,
  type HarnessCommand,
  type ModelOption,
  type MountedServer,
  type MountState,
  type PermissionMode,
  type SessionKey,
  type SidecarCommand,
  type SidecarEvent,
  type StartCommand,
  type StoreOp,
  type UsageWindow,
} from "./protocol.ts";

/** How this sidecar identifies itself in the CLI's User-Agent. */
const CLIENT_APP = "flycod-sidecar/0.1.0";

/** How far up from the SDK's entry point to look for its manifest. */
const MANIFEST_SEARCH_DEPTH = 8;

/** Writes one protocol event to flycod. */
function emit(event: SidecarEvent): void {
  process.stdout.write(`${JSON.stringify(event)}\n`);
}

/** Reports a cause flycod cannot recover from and exits. */
function fatal(error: unknown): never {
  emit({ type: "fatal", error: describeError(error) });
  process.exit(1);
}

/** Splits a byte stream into newline-delimited strings. */
export async function* lines(stream: ReadableStream<Uint8Array>): AsyncGenerator<string> {
  const decoder = new TextDecoder();
  let buffered = "";
  for await (const chunk of stream) {
    buffered += decoder.decode(chunk, { stream: true });
    for (let cut = buffered.indexOf("\n"); cut >= 0; cut = buffered.indexOf("\n")) {
      yield buffered.slice(0, cut);
      buffered = buffered.slice(cut + 1);
    }
  }
  buffered += decoder.decode();
  if (buffered.length > 0) {
    yield buffered;
  }
}

/**
 * The installed SDK's version, read from its own manifest.
 *
 * The package does not export `./package.json`, so this walks up from the
 * resolved entry point until it finds the manifest that declares it.
 */
async function sdkVersion(): Promise<string> {
  let directory = dirname(Bun.resolveSync("@anthropic-ai/claude-agent-sdk", import.meta.dir));
  for (let depth = 0; depth < MANIFEST_SEARCH_DEPTH; depth += 1) {
    const manifest = Bun.file(join(directory, "package.json"));
    if (await manifest.exists()) {
      const declared = (await manifest.json()) as { name?: string; version?: string };
      if (declared.name === "@anthropic-ai/claude-agent-sdk" && declared.version !== undefined) {
        return declared.version;
      }
    }
    const parent = dirname(directory);
    if (parent === directory) {
      break;
    }
    directory = parent;
  }
  throw new Error("could not find the installed @anthropic-ai/claude-agent-sdk manifest");
}

/**
 * The streaming-input generator backing the session.
 *
 * `query` is handed this as its prompt, which keeps the session open for
 * many turns; `push` feeds it, and `close` ends the session.
 */
export class UserMessages {
  private readonly buffered: SDKUserMessage[] = [];
  private readonly waiting: Array<(next: SDKUserMessage | null) => void> = [];
  private closed = false;

  push(text: string): void {
    const message: SDKUserMessage = {
      type: "user",
      message: { role: "user", content: text },
      parent_tool_use_id: null,
    };
    const waiter = this.waiting.shift();
    if (waiter === undefined) {
      this.buffered.push(message);
    } else {
      waiter(message);
    }
  }

  close(): void {
    this.closed = true;
    for (const waiter of this.waiting.splice(0)) {
      waiter(null);
    }
  }

  async *stream(): AsyncGenerator<SDKUserMessage> {
    for (;;) {
      const buffered = this.buffered.shift();
      if (buffered !== undefined) {
        yield buffered;
        continue;
      }
      if (this.closed) {
        return;
      }
      const next = await new Promise<SDKUserMessage | null>((resolve) => {
        this.waiting.push(resolve);
      });
      if (next === null) {
        return;
      }
      yield next;
    }
  }
}

/** How long the SDK's named plan windows are, in minutes. */
const WINDOW_MINUTES = {
  five_hour: 5 * 60,
  seven_day: 7 * 24 * 60,
  seven_day_oauth_apps: 7 * 24 * 60,
  seven_day_opus: 7 * 24 * 60,
  seven_day_sonnet: 7 * 24 * 60,
} as const;

/** What each of those windows covers, when it covers less than the plan. */
const WINDOW_SCOPE: Record<keyof typeof WINDOW_MINUTES, string | null> = {
  five_hour: null,
  seven_day: null,
  seven_day_oauth_apps: "OAuth apps",
  seven_day_opus: "Opus",
  seven_day_sonnet: "Sonnet",
};

/** One utilization reading as the SDK states it: 0-100, or not known. */
interface SdkWindow {
  utilization: number | null;
  resets_at: string | null;
}

/**
 * A utilization reading as a whole percentage, where a hundred means a
 * hundred.
 *
 * Rounded *down* below full rather than to the nearest whole number, which is
 * the one place this matters: flycod reads `used_percent === 100` as the plan
 * window being spent and pauses the session until it turns over (issue #244).
 * Ordinary rounding would turn 99.6% into a hundred, and a session stopped
 * for five hours over a rounding decision is far worse than a ring that reads
 * 99 when it is nearly full.
 *
 * A vendor number at or above a hundred is clamped to a hundred, so the
 * predicate is exactly "the vendor said the window is spent".
 */
function fullOrRoundedDown(utilization: number): number {
  if (utilization >= 100) {
    return 100;
  }
  return Math.max(0, Math.floor(utilization));
}

/**
 * One window in this protocol's spelling, or `null` when there is nothing
 * true to say.
 *
 * A window the SDK reports with a null `utilization` is a bucket the plan
 * does not have, not a bucket at zero, and it is dropped rather than drawn
 * empty. `resets_at` is ISO 8601 in the usage response — unlike the stream's
 * `rate_limit_event`, which states the same instant as a Unix second — so it
 * is parsed here, at the one boundary that sees both.
 */
function toUsageWindow(
  window: SdkWindow | null | undefined,
  minutes: number | null,
  scope: string | null,
): UsageWindow | null {
  if (window === null || window === undefined || window.utilization === null) {
    return null;
  }
  const resets = window.resets_at === null ? null : Date.parse(window.resets_at);
  return {
    window_minutes: minutes,
    scope,
    used_percent: fullOrRoundedDown(window.utilization),
    // `Date.parse` answers NaN for a string it cannot read. A reset flyco
    // cannot place in time is no reset at all, and reporting NaN as a
    // timestamp would put "resets in 56 years" under the ring.
    resets_at_unix: resets === null || Number.isNaN(resets) ? null : Math.floor(resets / 1000),
  };
}

/**
 * Every plan window in one `/usage` answer, in flycod's spelling.
 *
 * Only the windows the SDK's own type declares are read. The live response
 * carries more — codename buckets the account is not on, and a `limits[]`
 * array the type does not mention — and reading an undeclared key would be
 * flyco depending on a shape nobody promised it.
 */
export function toUsageWindows(usage: SDKControlGetUsageResponse): UsageWindow[] {
  // The SDK says outright when a session has no plan behind it at all: an
  // API key, Bedrock, Vertex. That is an empty list rather than a guess.
  if (!usage.rate_limits_available || usage.rate_limits === null) {
    return [];
  }
  const limits = usage.rate_limits;
  const named = (Object.keys(WINDOW_MINUTES) as (keyof typeof WINDOW_MINUTES)[]).map((key) =>
    toUsageWindow(limits[key], WINDOW_MINUTES[key], WINDOW_SCOPE[key]),
  );
  // Per-model weekly buckets, which the server names itself.
  const scoped = (limits.model_scoped ?? []).map((row) =>
    toUsageWindow(row, WINDOW_MINUTES.seven_day, row.display_name),
  );
  return [...named, ...scoped].filter((window): window is UsageWindow => window !== null);
}

/** Round trips parked on flycod, keyed by the id it must echo back. */
class Parked<Id, Answer> {
  private readonly waiting = new Map<Id, (answer: Answer) => void>();

  park(id: Id): Promise<Answer> {
    return new Promise<Answer>((resolve) => {
      this.waiting.set(id, resolve);
    });
  }

  /** Resolves one round trip. False when nothing was waiting on `id`. */
  answer(id: Id, value: Answer): boolean {
    const resolve = this.waiting.get(id);
    if (resolve === undefined) {
      return false;
    }
    this.waiting.delete(id);
    resolve(value);
    return true;
  }
}

/**
 * Reduces one SDK MCP status to what flycod checks the mount against.
 *
 * The three-way reading is made here rather than in Rust because the set of
 * status strings is the SDK's to extend. Only `pending` is a server still on
 * its way somewhere; `connected` is the only one whose tool list is a real
 * answer; and everything else — `failed`, `disabled`, `needs-auth`, whatever
 * is added next — is a server this session will not get and there is no
 * point waiting for. The raw word travels alongside so the daemon's refusal
 * quotes what the CLI actually said.
 */
export function toMountedServer(status: McpServerStatus): MountedServer {
  const state: MountState =
    status.status === "connected" ? "connected" : status.status === "pending" ? "pending" : "failed";
  return {
    name: status.name,
    status: status.status,
    state,
    tools: (status.tools ?? []).map((tool) => tool.name),
  };
}

/** How long the sidecar waits for every server to stop dialling. */
const MOUNT_SETTLE_MS = 10_000;

/** How often it asks again while one is still pending. */
const MOUNT_POLL_MS = 250;

/**
 * The mount, once nothing is still dialling.
 *
 * MCP startup is not blocking in the CLI, so the first answer after the
 * `initialize` handshake can legitimately be "still connecting". Waiting is
 * the difference between reporting the mount and reporting a race; the
 * deadline is what stops a server that will never come up from holding the
 * session open forever, and a report that is still pending when it expires
 * is one flycod refuses on its own terms.
 */
export async function settledMount(
  session: Pick<Query, "mcpServerStatus">,
  now: () => number = Date.now,
  sleep: (ms: number) => Promise<void> = (ms) =>
    new Promise((resolve) => setTimeout(resolve, ms)),
): Promise<MountedServer[]> {
  const deadline = now() + MOUNT_SETTLE_MS;
  for (;;) {
    const servers = (await session.mcpServerStatus()).map(toMountedServer);
    if (servers.every((server) => server.state !== "pending") || now() >= deadline) {
      return servers;
    }
    await sleep(MOUNT_POLL_MS);
  }
}

/** Translates the SDK's `camelCase` session key to this protocol's. */
export function toWireKey(key: SdkSessionKey): SessionKey {
  return key.subpath === undefined
    ? { project_key: key.projectKey, session_id: key.sessionId }
    : { project_key: key.projectKey, session_id: key.sessionId, subpath: key.subpath };
}

/**
 * The environment the supervised CLI runs under.
 *
 * `inherit` returns `process.env` untouched apart from the client tag, so
 * the CLI reads the host user's own `~/.claude`. Every other mode points it
 * at an isolated config tree and injects exactly one credential.
 */
export function environment(command: StartCommand): NonNullable<Options["env"]> {
  const env: Record<string, string | undefined> = {
    ...process.env,
    CLAUDE_AGENT_SDK_CLIENT_APP: CLIENT_APP,
  };
  if (command.config_dir !== null) {
    env.CLAUDE_CONFIG_DIR = command.config_dir;
  }
  if (command.project_dir_name !== null) {
    env.CLAUDE_CODE_PROJECT_DIR_NAME = command.project_dir_name;
  }
  switch (command.auth.mode) {
    case "inherit":
      break;
    case "oauth_token":
      env.CLAUDE_CODE_OAUTH_TOKEN = command.auth.token;
      break;
    case "api_key":
      env.ANTHROPIC_API_KEY = command.auth.key;
      break;
  }
  return env;
}

/**
 * The session's id, known before the CLI says anything.
 *
 * A fresh session gets an id the sidecar mints and hands to the SDK, so
 * flyco can record and address the session without waiting for a turn; a
 * resumed session already has one.
 */
export function sessionIdFor(command: StartCommand): string {
  return command.resume_session_id ?? crypto.randomUUID();
}

/**
 * The SDK options for one session.
 *
 * `sessionId` and `resume` are mutually exclusive — the SDK rejects a
 * chosen id alongside a resume unless the session is being forked — so the
 * two cases set exactly one of them.
 */
export function sessionOptions(
  command: StartCommand,
  sessionId: string,
  callbacks: Pick<Options, "canUseTool" | "sessionStore">,
): Options {
  return {
    cwd: command.cwd,
    env: environment(command),
    permissionMode: command.permission_mode,
    // flyco's own server and the user's registered ones, and nothing the
    // agent added: `strictMcpConfig` drops the project `.mcp.json`, the
    // user settings and the plugin scopes the CLI would otherwise
    // auto-discover. On a provisioned machine the root-owned
    // `managed-mcp.json` says the same thing at a scope the agent cannot
    // reach, and the CLI refuses to start when asked for both at once
    // (issue #195) — so flycod decides which of the two is in force and
    // says so here.
    mcpServers: command.mcp_servers,
    strictMcpConfig: command.strict_mcp_config,
    // What the CLI itself made of those declarations. The SDK swallows the
    // child's stderr unless a callback asks for it, so a server the CLI
    // ignored (an enterprise MCP config has exclusive control over the
    // scope) or refused (blocked by enterprise policy) reaches flycod as
    // nothing but "no MCP servers at all" — a mount failure with no
    // account of itself. `--debug mcp` narrows the CLI's account to the
    // mount, and forwarding it to this process's stderr puts it in the
    // tail flycod already quotes when the mount check fails.
    extraArgs: { debug: "mcp" },
    stderr: (data: string) => {
      process.stderr.write(data);
    },
    ...callbacks,
    // Assistant text reaches flyco only as partial-message deltas, so the
    // normalizer never has to choose between a delta and the complete
    // message that repeats it.
    includePartialMessages: true,
    ...(command.model === null ? {} : { model: command.model }),
    // Two independent choices, so two independent spreads: a model that
    // accepts no effort levels at all (Haiku) is started with none, and an
    // `effort: undefined` under `exactOptionalPropertyTypes` is not the
    // same as the key being absent.
    ...(command.effort === null ? {} : { effort: command.effort as EffortLevel }),
    ...(command.resume_session_id === null
      ? { sessionId }
      : { resume: command.resume_session_id }),
  };
}

/**
 * One SDK model row in flyco's own vocabulary.
 *
 * The SDK marks its default by naming the row `default` rather than with a
 * flag, and it states no per-model default effort at all — so `is_default`
 * is derived from the identifier and `default_effort` is honestly null,
 * which leaves the CLI's own choice in force.
 */
export function toModelOption(row: ModelInfo): ModelOption {
  return {
    id: row.value,
    label: row.displayName,
    description: row.description,
    is_default: row.value === "default",
    efforts: row.supportedEffortLevels ?? [],
    default_effort: null,
  };
}

/**
 * What the CLI calls a command flyco must not offer.
 *
 * The SDK lists its own machinery beside the user's skills —
 * `__remote-workflow` is one — and a palette that showed it would invite a
 * user to run something the product has no account of. The double
 * underscore is the CLI's own marker for them.
 */
const INTERNAL_COMMAND = "__";

/**
 * One `SlashCommand`, in flyco's vocabulary.
 *
 * The empty `argumentHint` becomes `null`, because "takes no argument" is
 * what the composer sends in one keystroke and it must not have to read an
 * empty string as a meaning. Aliases are dropped: they are more rows for
 * the same command.
 */
export function toHarnessCommand(command: SlashCommand): HarnessCommand {
  return {
    name: command.name,
    description: command.description,
    argument_hint: command.argumentHint === "" ? null : command.argumentHint,
  };
}

/** The commands a user may be offered, in the order the CLI listed them. */
export function offeredCommands(commands: SlashCommand[]): HarnessCommand[] {
  return commands
    .filter((command) => !command.name.startsWith(INTERNAL_COMMAND))
    .map(toHarnessCommand);
}

/** One live SDK session and everything parked on flycod for it. */
class Session {
  private readonly messages = new UserMessages();
  private readonly approvals = new Parked<string, PermissionResult>();
  private readonly stores = new Parked<number, unknown>();
  private readonly session: Query;
  private nextStoreId = 1;
  private closing = false;
  /** The id flyco addresses this session by. */
  readonly sessionId: string;
  /** Resolves when the CLI has answered its `initialize` handshake. */
  readonly warm: Promise<void>;
  readonly drained: Promise<void>;

  constructor(command: StartCommand) {
    this.sessionId = sessionIdFor(command);
    this.session = query({
      prompt: this.messages.stream(),
      options: sessionOptions(command, this.sessionId, {
        canUseTool: this.canUseTool,
        sessionStore: this.sessionStore,
      }),
    });
    // Order matters: `warmUp` announces the session synchronously up to its
    // first await, so `started` is always the first line of the session.
    this.warm = this.warmUp();
    this.drained = this.drain();
  }

  /**
   * Announces the session and waits for the CLI to finish booting.
   *
   * Constructing the query already spawned the CLI and sent its
   * `initialize` control request, so the session is identified and warming
   * from here — no user message required, which is the whole point. The
   * await that follows only turns a broken login or a missing CLI into a
   * `fatal` at start time instead of a silent wait for a turn that never
   * comes. It must never be awaited from the command loop: the handshake
   * can call `SessionStore.load`, which parks on a `store_response` only
   * that loop can deliver.
   */
  private async warmUp(): Promise<void> {
    emit({ type: "started", session_id: this.sessionId });
    try {
      await this.session.initializationResult();
      // What this build of the CLI can run on, before the first turn: the
      // list is a fact about the installed CLI rather than about a turn,
      // and flyco records it against the account so the *next* session's
      // picker opens on it too.
      emit({ type: "models", models: (await this.session.supportedModels()).map(toModelOption) });
      // What this session can be told to do, on the same terms — except
      // that this list is not a fact about the installed CLI alone: it
      // carries the checkout's own skills, so it is reported to the session
      // and never recorded against the account.
      emit({
        type: "commands",
        commands: offeredCommands(await this.session.supportedCommands()),
      });
      // The earliest moment the answer exists, and the whole answer: which
      // servers the CLI mounted, whether it reached them, and what they
      // advertise. flycod refuses the session if flyco's own is not among
      // them, so this is emitted before the first turn rather than
      // discovered from one.
      emit({ type: "mcp_servers", servers: await settledMount(this.session) });
      // How much of the plan is already spent, before the session costs
      // anything: the composer's rings are right for the first message
      // rather than only after the first turn has been paid for.
      await this.reportUsage();
    } catch (error) {
      // Shutting down rejects every in-flight control request; that is the
      // exit path, not a failure.
      if (this.closing) {
        return;
      }
      throw error;
    }
  }

  push(text: string): void {
    this.messages.push(text);
  }

  async interrupt(): Promise<void> {
    await this.session.interrupt();
  }

  /** Runs Claude Code's native manual-compaction command. */
  compact(): void {
    this.messages.push("/compact");
  }

  /**
   * Answers flyco's `/context`: what the window is spent on.
   *
   * A control request, not a turn — the CLI computes the breakdown locally
   * and nothing here is sent to the API. The same request backs the CLI's
   * own `/context` panel; flyco asks for the structure rather than the
   * rendered text because the panel is drawn by the client.
   */
  async contextUsage(): Promise<void> {
    const report = await this.session.getContextUsage();
    const usage: ContextUsage = {
      model: report.model,
      window: { used_tokens: report.totalTokens, size_tokens: report.maxTokens },
      categories: report.categories.map((category) => ({
        name: category.name,
        tokens: category.tokens,
        deferred: category.isDeferred ?? false,
      })),
      // A tool whose schema is not yet materialized in the window — the
      // SDK spells that `isLoaded: false` — still counts toward it.
      mcp_tools: report.mcpTools.map((tool) => ({
        name: tool.name,
        tokens: tool.tokens,
        deferred: tool.isLoaded === false,
      })),
      memory_files: report.memoryFiles.map((file) => ({
        name: file.path,
        tokens: file.tokens,
        deferred: false,
      })),
      agents: report.agents.map((agent) => ({
        name: agent.agentType,
        tokens: agent.tokens,
        deferred: false,
      })),
      skills:
        report.skills?.skillFrontmatter.map((skill) => ({
          name: skill.name,
          tokens: skill.tokens,
          deferred: false,
        })) ?? [],
    };
    if (report.isAutoCompactEnabled && report.autoCompactThreshold !== undefined) {
      usage.auto_compact = report.autoCompactThreshold;
    }
    emit({ type: "context_usage", usage });
  }

  /**
   * Moves the running query onto another model, at another effort.
   *
   * In this order, and both every time: the effort is a level *of a model*,
   * so applying one before the move would set a level of the model the
   * session is leaving. `null` clears the flag rather than leaving the
   * previous model's level in force, which is what a model with no effort
   * levels at all needs.
   */
  async setModel(model: string, effort: string | null): Promise<void> {
    await this.session.setModel(model);
    await this.session.applyFlagSettings({
      effortLevel: effort === null ? null : (effort as EffortLevel),
    });
  }

  /**
   * Puts the running query under another permission mode.
   *
   * One SDK call, applied to the conversation in progress: the SDK
   * resolves the mode against its own rules immediately, so a turn
   * already streaming answers under the mode this sets.
   */
  async setPermissionMode(mode: PermissionMode): Promise<void> {
    await this.session.setPermissionMode(mode);
  }

  /** Ends the streaming input, which ends the session. */
  close(): void {
    this.closing = true;
    this.messages.close();
  }

  decideApproval(id: string, result: PermissionResult): boolean {
    return this.approvals.answer(id, result);
  }

  answerStore(id: number, result: unknown): boolean {
    return this.stores.answer(id, result);
  }

  /**
   * Asks the CLI what is left of the plan and reports it.
   *
   * The control request needs a live transport, so this is only ever called
   * from inside the session's own lifetime — never after `close`.
   */
  private async reportUsage(): Promise<void> {
    const usage = await this.session.usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET();
    emit({ type: "plan_usage", windows: toUsageWindows(usage) });
  }

  /** Forwards every SDK message to flycod, verbatim. */
  private async drain(): Promise<void> {
    for await (const message of this.session) {
      if (message.type === "system" && message.subtype === "init") {
        // The only place the CLI names its capabilities, and it names them
        // per turn. Later frames revise the set; flycod keeps the newest.
        emit({ type: "capabilities", capabilities: message.capabilities ?? [] });
      }
      if (message.type === "system" && message.subtype === "commands_changed") {
        // The CLI discovers skills as the agent walks into subdirectories.
        // The frame carries the whole new list and the SDK documents it as
        // a replacement, so it is forwarded rather than answered with a
        // second `supportedCommands()` round trip — which would also mean
        // awaiting a control request from inside the message loop.
        emit({ type: "commands", commands: offeredCommands(message.commands) });
      }
      emit({ type: "sdk_message", message });
      // A `result` closes a turn, and a turn is the only thing that moves
      // the plan's meters — so the reading is taken here rather than on a
      // timer that would poll the vendor through every idle hour.
      if (message.type === "result" && !this.closing) {
        await this.reportUsage();
      }
    }
  }

  /**
   * Routes a permission prompt to flyco's own approval UI.
   *
   * Auto-approved tools never reach here, so what this sees is exactly the
   * set of calls a human has to decide.
   */
  private readonly canUseTool: CanUseTool = async (toolName, input, options) => {
    const id = crypto.randomUUID();
    const decision = this.approvals.park(id);
    // A turn can be interrupted while a prompt is still parked. Nothing
    // would ever answer it, and the SDK has no deadline of its own, so the
    // abort resolves it closed.
    options.signal.addEventListener("abort", () => {
      this.approvals.answer(id, {
        behavior: "deny",
        message: "The turn was interrupted before this tool call was decided.",
      });
    });
    emit({
      type: "approval_request",
      id,
      tool: toolName,
      input,
      suggestions: options.suggestions ?? null,
    });
    return await decision;
  };

  /**
   * Persists the transcript through flycod rather than to the VM's disk.
   *
   * This is what makes flyco's History work: the session's transcript
   * belongs to the control plane, so it can be resumed onto any machine.
   */
  private readonly sessionStore: SessionStore = {
    append: async (key: SdkSessionKey, entries: SessionStoreEntry[]): Promise<void> => {
      await this.roundTrip({ append: { key: toWireKey(key), entries } });
    },
    load: async (key: SdkSessionKey): Promise<SessionStoreEntry[] | null> => {
      const result = await this.roundTrip({ load: { key: toWireKey(key) } });
      // `null` is the SDK's "never written"; flycod sends it for a stream
      // it has nothing for.
      return result === null ? null : (result as SessionStoreEntry[]);
    },
  };

  private async roundTrip(op: StoreOp): Promise<unknown> {
    const id = this.nextStoreId;
    this.nextStoreId += 1;
    const answer = this.stores.park(id);
    emit({ type: "store_request", id, op });
    return await answer;
  }
}

/** Applies one decoded command. Returns false once the sidecar should exit. */
async function apply(command: SidecarCommand, session: Session | null): Promise<boolean> {
  if (session === null) {
    throw new Error(`received \`${command.type}\` before \`start\``);
  }
  switch (command.type) {
    case "start":
      throw new Error("received a second `start`; one sidecar drives one session");
    case "user_message":
      session.push(command.text);
      return true;
    case "interrupt":
      await session.interrupt();
      return true;
    case "compact":
      session.compact();
      return true;
    case "context_usage":
      await session.contextUsage();
      return true;
    case "set_model":
      await session.setModel(command.model, command.effort);
      return true;
    case "set_permission_mode":
      await session.setPermissionMode(command.mode);
      return true;
    case "approval_decision": {
      const result: PermissionResult = command.allow
        ? {
            behavior: "allow",
            updatedInput: (command.updated_input ?? {}) as Record<string, unknown>,
          }
        : {
            behavior: "deny",
            message: command.message ?? "Denied by flyco.",
          };
      if (!session.decideApproval(command.id, result)) {
        throw new Error(`no tool call is waiting on approval ${command.id}`);
      }
      return true;
    }
    case "store_response":
      if (!session.answerStore(command.id, command.result)) {
        throw new Error(`no store operation is waiting on request ${command.id}`);
      }
      return true;
    case "shutdown":
      session.close();
      await session.drained;
      return false;
  }
}

async function main(): Promise<void> {
  emit({ type: "ready", sdk_version: await sdkVersion() });

  let session: Session | null = null;
  for await (const line of lines(Bun.stdin.stream())) {
    if (line.trim().length === 0) {
      continue;
    }
    const command = sidecarCommandSchema.parse(JSON.parse(line));
    if (command.type === "start") {
      if (session !== null) {
        throw new Error("received a second `start`; one sidecar drives one session");
      }
      session = new Session(command);
      // Neither is awaited here: the command loop has to stay free to
      // answer the store and approval round trips they can provoke.
      session.warm.catch(fatal);
      session.drained.catch(fatal);
      continue;
    }
    if (!(await apply(command, session))) {
      return;
    }
  }
  // flycod closed stdin without a `shutdown`; end the session anyway.
  if (session !== null) {
    session.close();
    await session.drained;
  }
}

// Guarded so the tests can import the helpers above without starting a
// session.
if (import.meta.main) {
  await main().catch(fatal);
}
