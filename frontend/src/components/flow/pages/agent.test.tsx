/**
 * Stage B's pages against the in-memory control plane (docs/ux.md §4 B).
 *
 * What is asserted is the order things happen in: the stage is one list
 * whatever the number of agents, nothing on it chooses an agent for good,
 * an agent's sign-in pages exist only behind its row, a finished link
 * returns to the list rather than to a page saying it linked, the paste
 * page does not exist until flyco has an attempt to redeem against, what
 * leaves the browser is the code the user pasted rather than whatever came
 * with it, and Codex's code is on screen the moment its page is — with
 * every button the page renders answering to a name.
 */
import { describe, expect, it, vi } from "vitest";
import { fireEvent, waitFor } from "@solidjs/testing-library";
import type { HarnessAccountView } from "../../../api/client";
import {
  expectEveryButtonNamed,
  json,
  postedTo,
  primary,
  problem,
  renderFlow,
  route,
  type,
} from "../testSupport";

const AUTHORIZE_URL =
  "https://claude.ai/oauth/authorize?code=true&client_id=test&state=the-state";
const ATTEMPT = "11111111-2222-4333-8444-555555555555";
const DEVICE_URL = "https://auth.openai.com/codex/device";
const CODEX_ATTEMPT = "99999999-8888-4777-8666-555555555555";
const SETTINGS_URL = "https://chatgpt.com/#settings/Security";
const LIST = "Link the agents you use";

const CLAUDE: HarnessAccountView = {
  id: "harness-2",
  harness: "claude_code",
  label: "me@lexo.cool",
  linked_at_unix: 1_787_000_000,
  expires_at_unix: 1_787_028_800,
};

/** Matches the poll of a Codex sign-in. */
function isCodexPoll(path: string, method: string): boolean {
  return (
    method === "GET" &&
    path === `/v1/harness-accounts/codex/oauth/${CODEX_ATTEMPT}`
  );
}

describe("the agents list", () => {
  it("lists every agent with its status, and cannot be passed with nothing linked", async () => {
    const { container, findByRole, getAllByText, getByRole, queryByText } =
      renderFlow(["agent"]);
    await findByRole("heading", { level: 1, name: LIST });
    expect(getByRole("radio", { name: /^Claude Code/ })).toBeInTheDocument();
    expect(getByRole("radio", { name: /^Codex/ })).toBeInTheDocument();
    expect(getAllByText(/^Not linked/)).toHaveLength(2);
    // Nothing here chooses the agent tasks run on.
    expect(queryByText(/Which agent should/)).not.toBeInTheDocument();
    expect(primary(container)).toHaveTextContent("Next");
    expect(primary(container)).toBeDisabled();
    expect(primary(container)).toHaveAttribute(
      "title",
      "Link at least one agent to continue",
    );
    expectEveryButtonNamed(container);
  });

  it("links the chosen agent on its own pages and comes back to the list, which now reads Linked", async () => {
    vi.spyOn(window, "open").mockReturnValue(null);
    const {
      container,
      findByRole,
      findByLabelText,
      findByText,
      getByRole,
      onDone,
    } = renderFlow(["agent"]);
    await findByRole("heading", { level: 1, name: LIST });

    fireEvent.click(getByRole("radio", { name: /^Claude Code/ }));
    await waitFor(() =>
      expect(primary(container)).toHaveTextContent("Link Claude Code"),
    );
    expect(primary(container)).toBeEnabled();
    fireEvent.click(primary(container));

    await findByRole("heading", { level: 1, name: "Link Claude Code" });
    fireEvent.click(primary(container));
    type(await findByLabelText("Code from Anthropic"), "ac_the-code#the-state");
    await waitFor(() => expect(primary(container)).toBeEnabled());
    fireEvent.click(primary(container));

    // Back on the list, not on a page saying it linked.
    await findByRole("heading", { level: 1, name: LIST });
    expect(await findByText(/^Linked · /)).toBeInTheDocument();
    expect(getByRole("radio", { name: /^Codex/ })).toHaveTextContent(
      /Not linked/,
    );
    expect(primary(container)).toHaveTextContent("Next");
    expect(primary(container)).toBeEnabled();

    fireEvent.click(primary(container));
    expect(onDone).toHaveBeenCalledOnce();
  });

  it("keeps the chosen agent when Back leaves its sign-in page", async () => {
    const { container, findByRole, getByRole } = renderFlow(["agent"]);
    await findByRole("heading", { level: 1, name: LIST });

    fireEvent.click(getByRole("radio", { name: /^Codex/ }));
    await waitFor(() =>
      expect(primary(container)).toHaveTextContent("Link Codex"),
    );
    fireEvent.click(primary(container));
    await findByRole("heading", { level: 1, name: "Link Codex" });

    fireEvent.click(getByRole("button", { name: "Back" }));
    await findByRole("heading", { level: 1, name: LIST });
    expect(getByRole("radio", { name: /^Codex/ })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    expect(primary(container)).toHaveTextContent("Link Codex");
  });

  it("shows what was linked before the flow opened, and choosing it changes nothing", async () => {
    const { container, findByRole, findByText, getByRole } = renderFlow(
      ["agent"],
      { answers: { agents: { claude_code: CLAUDE } } },
    );
    await findByRole("heading", { level: 1, name: LIST });
    expect(await findByText("Linked · me@lexo.cool")).toBeInTheDocument();
    expect(primary(container)).toHaveTextContent("Next");
    expect(primary(container)).toBeEnabled();

    fireEvent.click(getByRole("radio", { name: /^Claude Code/ }));
    await waitFor(() => expect(primary(container)).toHaveTextContent("Next"));
    expect(primary(container)).toBeEnabled();
  });

  it("asks OpenAI for nothing while Codex is only listed", async () => {
    const start = vi.fn();
    route(
      (path, method) =>
        method === "POST" && path === "/v1/harness-accounts/codex/oauth/start",
      () => {
        start();
        return problem(500, "unexpected", "should not be asked");
      },
    );
    const { findByRole, getByRole } = renderFlow(["agent"]);
    await findByRole("heading", { level: 1, name: LIST });
    fireEvent.click(getByRole("radio", { name: /^Codex/ }));
    expect(start).not.toHaveBeenCalled();
  });
});

describe("Claude Code, opened for it alone", () => {
  it("opens on its sign-in page and finishes when the pasted code links", async () => {
    const opened = vi.spyOn(window, "open").mockReturnValue(null);
    const { container, findByRole, findByLabelText, getByText, onDone } =
      renderFlow(["agent"], { agents: ["claude_code"] });
    await findByRole("heading", { level: 1, name: "Link Claude Code" });
    expect(
      getByText("Runs on your Claude subscription.", { exact: false }),
    ).toBeInTheDocument();
    expect(primary(container)).toHaveTextContent("Sign in with Claude");
    expectEveryButtonNamed(container);
    // The paste page is not reachable before there is an attempt to redeem into.
    expect(document.querySelector("#claude-oauth-code")).toBeNull();

    fireEvent.click(primary(container));
    const field = await findByLabelText("Code from Anthropic");
    expect(opened).toHaveBeenCalledWith(
      AUTHORIZE_URL,
      "_blank",
      "noopener,noreferrer",
    );
    await findByRole("heading", {
      level: 1,
      name: "Paste the code Anthropic shows you",
    });
    expect(getByText("CODE#STATE")).toBeInTheDocument();

    expect(primary(container)).toHaveTextContent("Link Claude Code");
    expect(primary(container)).toBeDisabled();
    expect(primary(container)).toHaveAttribute(
      "title",
      "Paste the code Anthropic showed you to continue",
    );

    type(field, "ac_the-code#the-state");
    await waitFor(() => expect(primary(container)).toBeEnabled());
    fireEvent.click(primary(container));

    await waitFor(() => expect(onDone).toHaveBeenCalledOnce());
    expect(postedTo("/v1/harness-accounts/claude/oauth/complete")).toEqual({
      attempt_id: ATTEMPT,
      code: "ac_the-code#the-state",
    });
  });

  it("redeems a bare code, and sends it tidied", async () => {
    const { container, findByRole, findByLabelText, onDone } = renderFlow(
      ["agent"],
      { agents: ["claude_code"] },
    );
    await findByRole("heading", { level: 1, name: "Link Claude Code" });
    fireEvent.click(primary(container));

    type(await findByLabelText("Code from Anthropic"), "  ac_the-code  ");
    await waitFor(() => expect(primary(container)).toBeEnabled());
    fireEvent.click(primary(container));

    await waitFor(() => expect(onDone).toHaveBeenCalledOnce());
    expect(postedTo("/v1/harness-accounts/claude/oauth/complete")).toEqual({
      attempt_id: ATTEMPT,
      code: "ac_the-code",
    });
  });

  it("puts a rejected code under the field and keeps the page", async () => {
    route(
      (path, method) =>
        method === "POST" &&
        path === "/v1/harness-accounts/claude/oauth/complete",
      () => problem(400, "invalid-code", "Anthropic did not accept that code."),
    );
    const { container, findByRole, findByLabelText, getByRole } = renderFlow(
      ["agent"],
      { agents: ["claude_code"] },
    );
    await findByRole("heading", { level: 1, name: "Link Claude Code" });
    fireEvent.click(primary(container));

    type(await findByLabelText("Code from Anthropic"), "ac_wrong");
    await waitFor(() => expect(primary(container)).toBeEnabled());
    fireEvent.click(primary(container));

    expect(await findByRole("alert")).toHaveTextContent(
      "Anthropic did not accept that code.",
    );
    expect(
      getByRole("heading", {
        level: 1,
        name: "Paste the code Anthropic shows you",
      }),
    ).toBeInTheDocument();
  });

  it("starts a fresh sign-in by the quiet link after a refusal, on the same page", async () => {
    const opened = vi.spyOn(window, "open").mockReturnValue(null);
    route(
      (path, method) =>
        method === "POST" &&
        path === "/v1/harness-accounts/claude/oauth/complete",
      () => problem(500, "internal", "The control plane failed."),
    );
    const { container, findByRole, findByLabelText, getByRole, queryByRole } =
      renderFlow(["agent"], { agents: ["claude_code"] });
    await findByRole("heading", { level: 1, name: "Link Claude Code" });
    fireEvent.click(primary(container));
    const field = await findByLabelText("Code from Anthropic");
    type(field, "ac_spent-by-a-failure");
    await waitFor(() => expect(primary(container)).toBeEnabled());
    fireEvent.click(primary(container));
    await findByRole("alert");

    fireEvent.click(getByRole("button", { name: "Start the sign-in again" }));
    await waitFor(() => expect(opened).toHaveBeenCalledTimes(2));
    // A new attempt, not the old page reopened: a second start went out.
    const starts = vi
      .mocked(fetch)
      .mock.calls.filter(
        ([input, init]) =>
          String(input).endsWith("/v1/harness-accounts/claude/oauth/start") &&
          init?.method === "POST",
      );
    expect(starts).toHaveLength(2);
    // The field and the refusal are cleared for the new code; the page stays.
    expect(field).toHaveValue("");
    expect(queryByRole("alert")).not.toBeInTheDocument();
    expect(
      getByRole("heading", {
        level: 1,
        name: "Paste the code Anthropic shows you",
      }),
    ).toBeInTheDocument();
  });

  it("leads by link to the API-key page, which links with an Anthropic key", async () => {
    const { container, findByRole, findByLabelText, getByRole, onDone } =
      renderFlow(["agent"], { agents: ["claude_code"] });
    await findByRole("heading", { level: 1, name: "Link Claude Code" });

    fireEvent.click(getByRole("button", { name: "Use an API key instead" }));
    await findByRole("heading", { level: 1, name: "Paste your API key" });
    expect(
      getByRole("link", { name: /Create a key on the Anthropic console/ }),
    ).toHaveAttribute("href", "https://console.anthropic.com/settings/keys");
    expect(primary(container)).toHaveTextContent("Link Claude Code");
    expect(primary(container)).toBeDisabled();
    expect(primary(container)).toHaveAttribute(
      "title",
      "Paste a key to continue",
    );

    type(
      await findByLabelText("Anthropic API key"),
      "sk-ant-api03-a-real-looking-key",
    );
    await waitFor(() => expect(primary(container)).toBeEnabled());
    fireEvent.click(primary(container));

    await waitFor(() => expect(onDone).toHaveBeenCalledOnce());
    expect(postedTo("/v1/harness-accounts")).toEqual({
      label: "Anthropic API key",
      credential: {
        kind: "claude_api_key",
        key: "sk-ant-api03-a-real-looking-key",
      },
    });
  });

  it("takes a setup token in the same field, and names both forms for anything else", async () => {
    const {
      container,
      findByRole,
      findByLabelText,
      findByText,
      getByRole,
      onDone,
    } = renderFlow(["agent"], { agents: ["claude_code"] });
    await findByRole("heading", { level: 1, name: "Link Claude Code" });
    fireEvent.click(getByRole("button", { name: "Use an API key instead" }));
    const field = await findByLabelText("Anthropic API key");
    expect(await findByText(/setup-token prints/)).toBeInTheDocument();

    // Neither prefix: the field says what it takes rather than guessing.
    type(field, "sk-proj-not-anthropic");
    expect(
      await findByText(/neither an Anthropic API key nor a setup token/),
    ).toBeInTheDocument();
    expect(primary(container)).toBeDisabled();
    expect(primary(container)).toHaveAttribute(
      "title",
      "Paste a key Anthropic issued to continue",
    );

    type(field, "sk-ant-oat01-a-setup-token");
    await waitFor(() => expect(primary(container)).toBeEnabled());
    fireEvent.click(primary(container));

    await waitFor(() => expect(onDone).toHaveBeenCalledOnce());
    expect(postedTo("/v1/harness-accounts")).toEqual({
      label: "Claude subscription",
      credential: {
        kind: "claude_setup_token",
        token: "sk-ant-oat01-a-setup-token",
      },
    });
  });
});

describe("Codex, opened for it alone", () => {
  it("shows the code as the page opens, the page it is typed on, and that it is waiting", async () => {
    const opened = vi.spyOn(window, "open").mockReturnValue(null);
    const { container, findByRole, findByText, findByLabelText, getByRole } =
      renderFlow(["agent"], { agents: ["codex"] });

    await findByRole("heading", { level: 1, name: "Link Codex" });
    // No button asked for the code: the page did, on entry.
    expect(await findByLabelText("One-time code")).toHaveTextContent(
      "FLYC-8QK2",
    );
    expect(
      getByRole("link", { name: /Open auth.openai.com\/codex\/device/ }),
    ).toHaveAttribute("href", DEVICE_URL);
    expect(
      await findByText(/Waiting for you to approve in the browser/),
    ).toBeInTheDocument();
    expect(getByRole("button", { name: /Copy code/ })).toBeInTheDocument();
    // Nothing opened a tab behind the user's back.
    expect(opened).not.toHaveBeenCalled();

    expect(primary(container)).toHaveTextContent("Next");
    expect(primary(container)).toBeDisabled();
    expect(primary(container)).toHaveAttribute(
      "title",
      "Approve the code in the browser to continue",
    );
    expectEveryButtonNamed(container);
  });

  it("finishes the stage by itself when a poll finds the code approved", async () => {
    route(isCodexPoll, () =>
      json(
        {
          id: "harness-3",
          harness: "codex",
          label: "me@lexo.cool",
          linked_at_unix: 1_787_000_000,
          expires_at_unix: 1_787_003_600,
        },
        201,
      ),
    );
    const { findByRole, onDone } = renderFlow(["agent"], { agents: ["codex"] });
    await findByRole("heading", { level: 1, name: "Link Codex" });

    await waitFor(() => expect(onDone).toHaveBeenCalledOnce(), {
      timeout: 4000,
    });
  });

  it("explains a switched-off device flow and makes the primary a retry", async () => {
    route(
      (path, method) =>
        method === "POST" && path === "/v1/harness-accounts/codex/oauth/start",
      () =>
        problem(
          409,
          "codex-device-auth-disabled",
          `Turn on device code authorization at ${SETTINGS_URL}`,
        ),
    );
    const { container, findByRole, findByText } = renderFlow(["agent"], {
      agents: ["codex"],
    });

    expect(await findByText(/device code authorization/)).toBeInTheDocument();
    expect(
      await findByRole("link", { name: /Open ChatGPT security settings/ }),
    ).toHaveAttribute("href", SETTINGS_URL);
    expect(primary(container)).toHaveTextContent("Try again");
    expect(primary(container)).toBeEnabled();
  });

  it("turns an expired code into Get a new code", async () => {
    route(isCodexPoll, () =>
      problem(
        410,
        "codex-oauth-attempt-expired",
        "That code is no longer valid.",
      ),
    );
    const { container, findByRole, findByText } = renderFlow(["agent"], {
      agents: ["codex"],
    });
    await findByRole("heading", { level: 1, name: "Link Codex" });

    expect(
      await findByText(/That code expired/, {}, { timeout: 4000 }),
    ).toBeInTheDocument();
    expect(primary(container)).toHaveTextContent("Get a new code");
    expect(primary(container)).toBeEnabled();
  });

  it("leads by link to the API-key page, and points at where a key is made", async () => {
    const { container, findByRole, findByLabelText, getByRole, onDone } =
      renderFlow(["agent"], {
        agents: ["codex"],
      });
    await findByRole("heading", { level: 1, name: "Link Codex" });

    fireEvent.click(getByRole("button", { name: "Use an API key instead" }));
    await findByRole("heading", { level: 1, name: "Paste your API key" });
    expect(
      getByRole("link", { name: /Create a key on the OpenAI API keys page/ }),
    ).toHaveAttribute("href", "https://platform.openai.com/api-keys");
    expect(primary(container)).toHaveTextContent("Link Codex");
    expect(primary(container)).toBeDisabled();

    type(await findByLabelText("OpenAI API key"), "sk-proj-a-real-looking-key");
    await waitFor(() => expect(primary(container)).toBeEnabled());
    fireEvent.click(primary(container));

    await waitFor(() => expect(onDone).toHaveBeenCalledOnce());
    expect(postedTo("/v1/harness-accounts")).toEqual({
      label: "OpenAI API key",
      credential: { kind: "codex_api_key", key: "sk-proj-a-real-looking-key" },
    });
  });
});
