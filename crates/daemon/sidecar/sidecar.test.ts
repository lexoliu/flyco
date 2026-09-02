/**
 * The sidecar's own plumbing: framing, key translation, and the environment
 * each auth mode builds. Nothing here starts an SDK session — `sidecar.ts`
 * guards its entry point on `import.meta.main` so these can be imported.
 */
import { describe, expect, test } from "bun:test";

import type { McpServerStatus, Query } from "@anthropic-ai/claude-agent-sdk";

import type { StartCommand } from "./protocol.ts";
import {
  environment,
  lines,
  sessionIdFor,
  sessionOptions,
  settledMount,
  toMountedServer,
  toWireKey,
  UserMessages,
} from "./sidecar.ts";

/** A stream of the given chunks, split wherever the caller split them. */
function streamOf(chunks: string[]): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  return new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) {
        controller.enqueue(encoder.encode(chunk));
      }
      controller.close();
    },
  });
}

async function collect(stream: ReadableStream<Uint8Array>): Promise<string[]> {
  const out: string[] = [];
  for await (const line of lines(stream)) {
    out.push(line);
  }
  return out;
}

function start(overrides: Partial<StartCommand> = {}): StartCommand {
  return {
    type: "start",
    cwd: "/tmp/flycod-dev/work",
    auth: { mode: "inherit" },
    config_dir: null,
    project_dir_name: null,
    model: null,
    permission_mode: "default",
    resume_session_id: null,
    mcp_servers: {
      flyco: {
        type: "stdio",
        command: "/usr/local/bin/flycod",
        args: ["mcp", "--config", "/tmp/flycod-dev/flycod.toml"],
        env: {},
        alwaysLoad: true,
      },
    },
    ...overrides,
  };
}

describe("line framing", () => {
  test("splits on newlines regardless of how the chunks fall", async () => {
    expect(await collect(streamOf(['{"a":1}\n{"b":', "2}\n"]))).toEqual(['{"a":1}', '{"b":2}']);
  });

  test("a multi-byte character split across chunks survives", async () => {
    const encoded = new TextEncoder().encode('{"text":"µ$"}\n');
    const controller = new ReadableStream<Uint8Array>({
      start(stream) {
        stream.enqueue(encoded.slice(0, 11));
        stream.enqueue(encoded.slice(11));
        stream.close();
      },
    });
    expect(await collect(controller)).toEqual(['{"text":"µ$"}']);
  });

  test("a trailing line without a newline is still yielded", async () => {
    expect(await collect(streamOf(['{"a":1}']))).toEqual(['{"a":1}']);
  });
});

describe("session keys", () => {
  test("the SDK's camelCase becomes the protocol's snake_case", () => {
    expect(toWireKey({ projectKey: "flyco", sessionId: "s-1" })).toEqual({
      project_key: "flyco",
      session_id: "s-1",
    });
  });

  test("a main transcript omits subpath rather than sending null", () => {
    expect("subpath" in toWireKey({ projectKey: "flyco", sessionId: "s-1" })).toBe(false);
  });

  test("a subagent transcript keeps its subpath", () => {
    expect(
      toWireKey({ projectKey: "flyco", sessionId: "s-1", subpath: "subagents/agent-01" }),
    ).toEqual({
      project_key: "flyco",
      session_id: "s-1",
      subpath: "subagents/agent-01",
    });
  });
});

describe("the supervised CLI's environment", () => {
  test("inherit injects no credential and no config directory", () => {
    const env = environment(start());
    expect(env.CLAUDE_CONFIG_DIR).toBeUndefined();
    expect(env.CLAUDE_CODE_OAUTH_TOKEN).toBeUndefined();
    expect(env.ANTHROPIC_API_KEY).toBeUndefined();
    expect(env.CLAUDE_AGENT_SDK_CLIENT_APP).toBe("flycod-sidecar/0.1.0");
  });

  test("an OAuth token comes with its isolated config tree", () => {
    const env = environment(
      start({
        auth: { mode: "oauth_token", token: "sk-ant-oat01-example" },
        config_dir: "/var/lib/flyco/claude",
        project_dir_name: "flyco-session",
      }),
    );
    expect(env.CLAUDE_CODE_OAUTH_TOKEN).toBe("sk-ant-oat01-example");
    expect(env.CLAUDE_CONFIG_DIR).toBe("/var/lib/flyco/claude");
    expect(env.CLAUDE_CODE_PROJECT_DIR_NAME).toBe("flyco-session");
    expect(env.ANTHROPIC_API_KEY).toBeUndefined();
  });

  test("an API key is the only credential its mode sets", () => {
    const env = environment(
      start({
        auth: { mode: "api_key", key: "sk-ant-api03-example" },
        config_dir: "/var/lib/flyco/claude",
        project_dir_name: "flyco-session",
      }),
    );
    expect(env.ANTHROPIC_API_KEY).toBe("sk-ant-api03-example");
    expect(env.CLAUDE_CODE_OAUTH_TOKEN).toBeUndefined();
  });
});

describe("streaming input", () => {
  test("messages queued before the generator runs are not lost", async () => {
    const messages = new UserMessages();
    messages.push("first");
    messages.push("second");
    messages.close();

    const texts: unknown[] = [];
    for await (const message of messages.stream()) {
      texts.push(message.message.content);
    }
    expect(texts).toEqual(["first", "second"]);
  });

  test("a message pushed while the generator waits wakes it", async () => {
    const messages = new UserMessages();
    const stream = messages.stream();
    const pending = stream.next();
    messages.push("late");
    const first = await pending;
    expect(first.done).toBe(false);
    expect(first.value?.message.content).toBe("late");

    messages.close();
    expect((await stream.next()).done).toBe(true);
  });

  test("closing ends the session's input", async () => {
    const messages = new UserMessages();
    const stream = messages.stream();
    const pending = stream.next();
    messages.close();
    expect((await pending).done).toBe(true);
  });
});

describe("session identity", () => {
  test("a fresh session is given an id before the CLI says anything", () => {
    const id = sessionIdFor(start());
    expect(id).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/);
    expect(sessionIdFor(start())).not.toBe(id);
  });

  test("a resumed session keeps the id it is resuming", () => {
    const resumed = "9d0f4b1a-3b7e-4a3e-9e1a-4c2f8b6d7a10";
    expect(sessionIdFor(start({ resume_session_id: resumed }))).toBe(resumed);
  });
});

describe("session options", () => {
  const callbacks = {};

  test("a fresh session hands the chosen id to the SDK and does not resume", () => {
    const options = sessionOptions(start(), "chosen-id", callbacks);
    expect(options.sessionId).toBe("chosen-id");
    expect(options.resume).toBeUndefined();
  });

  test("a resumed session sets resume and never sessionId", () => {
    // The SDK rejects a chosen id alongside a resume unless the session is
    // being forked, so exactly one of the two is ever set.
    const resumed = "9d0f4b1a-3b7e-4a3e-9e1a-4c2f8b6d7a10";
    const options = sessionOptions(start({ resume_session_id: resumed }), resumed, callbacks);
    expect(options.resume).toBe(resumed);
    expect(options.sessionId).toBeUndefined();
  });

  test("partial messages are always on, because deltas are flyco's only text source", () => {
    expect(sessionOptions(start(), "id", callbacks).includePartialMessages).toBe(true);
  });

  test("an omitted model leaves the CLI's own default in place", () => {
    expect(sessionOptions(start(), "id", callbacks).model).toBeUndefined();
    expect(sessionOptions(start({ model: "claude-sonnet-4-5-20250929" }), "id", callbacks).model).toBe(
      "claude-sonnet-4-5-20250929",
    );
  });

  test("the permission mode and cwd are passed through unchanged", () => {
    const options = sessionOptions(start({ permission_mode: "acceptEdits" }), "id", callbacks);
    expect(options.permissionMode).toBe("acceptEdits");
    expect(options.cwd).toBe("/tmp/flycod-dev/work");
  });

  test("the session's MCP servers are exactly the ones flycod named", () => {
    const options = sessionOptions(start(), "id", callbacks);
    expect(Object.keys(options.mcpServers ?? {})).toEqual(["flyco"]);
    // Without this the CLI would also load the project's own .mcp.json,
    // the user settings and the plugin scopes — which is where a server the
    // agent wrote for itself would come from.
    expect(options.strictMcpConfig).toBe(true);
  });
});

describe("the mount the CLI reports", () => {
  /** A CLI that answers with the given statuses, one call at a time. */
  function cli(answers: McpServerStatus[][]): Pick<Query, "mcpServerStatus"> {
    let call = 0;
    return {
      mcpServerStatus: () => {
        const answer = answers[Math.min(call, answers.length - 1)] ?? [];
        call += 1;
        return Promise.resolve(answer);
      },
    };
  }

  const flyco: McpServerStatus = {
    name: "flyco",
    status: "connected",
    tools: [{ name: "machine_status" }, { name: "budget_status" }, { name: "machine_resize" }],
  };

  test("a connected server carries the tool names flycod checks", () => {
    expect(toMountedServer(flyco)).toEqual({
      name: "flyco",
      status: "connected",
      state: "connected",
      tools: ["machine_status", "budget_status", "machine_resize"],
    });
  });

  test("only `pending` is a server still on its way somewhere", () => {
    expect(toMountedServer({ name: "a", status: "pending" }).state).toBe("pending");
    for (const status of ["failed", "needs-auth", "disabled"] as const) {
      expect(toMountedServer({ name: "a", status }).state).toBe("failed");
    }
  });

  test("it waits out a server that is still connecting", async () => {
    const slept: number[] = [];
    const mounted = await settledMount(
      cli([[{ name: "flyco", status: "pending" }], [flyco]]),
      () => 0,
      (ms) => {
        slept.push(ms);
        return Promise.resolve();
      },
    );
    expect(slept.length).toBe(1);
    expect(mounted).toEqual([toMountedServer(flyco)]);
  });

  test("a server that never settles is reported as it stands rather than waited on forever", async () => {
    let clock = 0;
    const mounted = await settledMount(
      cli([[{ name: "flyco", status: "pending" }]]),
      () => (clock += 60_000),
      () => Promise.resolve(),
    );
    // The deadline is past on the first look, so flycod gets the pending
    // report and refuses the session in the CLI's own words.
    expect(mounted).toEqual([
      { name: "flyco", status: "pending", state: "pending", tools: [] },
    ]);
  });
});
