/**
 * The connect-an-agent cards, as a state machine.
 *
 * Each card has three states — collapsed, mid-flow, and linked — and the
 * Claude card's flow has two steps of its own. What is asserted here is the
 * order they happen in: step two does not exist until flyco has an attempt
 * to redeem against, the button that redeems it stays disabled until there
 * is something to redeem, and what leaves the browser is the code the user
 * pasted rather than whatever else came with it.
 */
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, waitFor } from "@solidjs/testing-library";
import HarnessChooser from "./HarnessChooser";
import { ReadinessProvider } from "../Readiness";

const AUTHORIZE_URL =
  "https://claude.ai/oauth/authorize?code=true&client_id=test&state=the-state";
const ATTEMPT = "11111111-2222-4333-8444-555555555555";

function renderChooser() {
  const onLinked = vi.fn();
  const rendered = render(() => (
    <ReadinessProvider enabled={() => true}>
      <HarnessChooser onLinked={onLinked} />
    </ReadinessProvider>
  ));
  return { ...rendered, onLinked };
}

/** The body of the one `POST` the component made to `path`, as JSON. */
function postedTo(path: string): unknown {
  const call = vi
    .mocked(fetch)
    .mock.calls.find(([input, init]) => String(input).endsWith(path) && init?.method === "POST");
  expect(call, `no POST to ${path}`).toBeDefined();
  return JSON.parse(String(call?.[1]?.body));
}

/** Types `value` into a field, the way an input event carries it. */
function type(field: HTMLElement, value: string): void {
  fireEvent.input(field, { target: { value } });
}

describe("HarnessChooser", () => {
  it("offers both agents, collapsed, with neither linked", async () => {
    const { findByRole, getByRole, getAllByText } = renderChooser();

    expect(await findByRole("button", { name: "Connect Claude Code" })).toBeInTheDocument();
    expect(getByRole("button", { name: "Connect Codex" })).toBeInTheDocument();
    expect(getAllByText("Not linked")).toHaveLength(2);
    // Nothing of either flow is on screen until a card is opened.
    expect(document.querySelector("#claude-oauth-code")).toBeNull();
  });

  it("walks the Claude card from sign-in to a pasted code", async () => {
    const opened = vi.spyOn(window, "open").mockReturnValue(null);
    const { findByRole, findByLabelText, getByText, onLinked } = renderChooser();

    fireEvent.click(await findByRole("button", { name: "Connect Claude Code" }));
    const signIn = await findByRole("button", { name: /Sign in with Claude/ });

    // Step two is not reachable before there is an attempt to redeem into.
    expect(document.querySelector("#claude-oauth-code")).toBeNull();

    fireEvent.click(signIn);
    const field = await findByLabelText("Code from Anthropic");
    expect(opened).toHaveBeenCalledWith(AUTHORIZE_URL, "_blank", "noopener,noreferrer");
    // The sentence that tells the user what they are looking for.
    expect(getByText("CODE#STATE")).toBeInTheDocument();

    const link = await findByRole("button", { name: "Link Claude Code" });
    expect(link).toBeDisabled();

    type(field, "ac_the-code#the-state");
    await waitFor(() => expect(link).toBeEnabled());
    fireEvent.click(link);

    await waitFor(() => expect(onLinked).toHaveBeenCalled());
    expect(postedTo("/v1/harness-accounts/claude/oauth/complete")).toEqual({
      attempt_id: ATTEMPT,
      code: "ac_the-code#the-state",
    });
  });

  it("redeems a bare code, and sends it tidied", async () => {
    vi.spyOn(window, "open").mockReturnValue(null);
    const { findByRole, findByLabelText, onLinked } = renderChooser();

    fireEvent.click(await findByRole("button", { name: "Connect Claude Code" }));
    fireEvent.click(await findByRole("button", { name: /Sign in with Claude/ }));
    const field = await findByLabelText("Code from Anthropic");

    type(field, "  ac_the-code  ");
    const link = await findByRole("button", { name: "Link Claude Code" });
    await waitFor(() => expect(link).toBeEnabled());
    fireEvent.click(link);

    await waitFor(() => expect(onLinked).toHaveBeenCalled());
    expect(postedTo("/v1/harness-accounts/claude/oauth/complete")).toEqual({
      attempt_id: ATTEMPT,
      code: "ac_the-code",
    });
  });

  it("keeps the setup token and the API key under Advanced", async () => {
    const { findByRole, findByLabelText } = renderChooser();

    fireEvent.click(await findByRole("button", { name: "Connect Claude Code" }));
    expect(await findByLabelText("Setup token")).toBeInTheDocument();

    fireEvent.click(await findByRole("button", { name: "Anthropic API key" }));
    expect(await findByLabelText("Anthropic API key")).toBeInTheDocument();
  });

  it("links Codex with an OpenAI key and points at where one is made", async () => {
    const { findByRole, findByLabelText, onLinked } = renderChooser();

    fireEvent.click(await findByRole("button", { name: "Connect Codex" }));
    expect(await findByRole("link", { name: /Create one on the API keys page/ })).toHaveAttribute(
      "href",
      "https://platform.openai.com/api-keys",
    );

    const submit = await findByRole("button", { name: "Link Codex" });
    expect(submit).toBeDisabled();

    type(await findByLabelText("OpenAI API key"), "sk-proj-a-real-looking-key");
    await waitFor(() => expect(submit).toBeEnabled());
    fireEvent.click(submit);

    await waitFor(() => expect(onLinked).toHaveBeenCalled());
    expect(postedTo("/v1/harness-accounts")).toEqual({
      label: "OpenAI API key",
      credential: { kind: "codex_api_key", key: "sk-proj-a-real-looking-key" },
    });
  });
});
