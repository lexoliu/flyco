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
const DEVICE_URL = "https://auth.openai.com/codex/device";
const CODEX_ATTEMPT = "99999999-8888-4777-8666-555555555555";
const SETTINGS_URL = "https://chatgpt.com/#settings/Security";

/** A problem document, in the shape the API answers with. */
function problem(status: number, slug: string, detail: string): Response {
  return new Response(
    JSON.stringify({
      type: `https://flyco.dev/problems/${slug}`,
      title: "Conflict",
      status,
      detail,
    }),
    { status, headers: { "content-type": "application/problem+json" } },
  );
}

/**
 * Answers one path with `respond`, leaving every other route to the
 * in-memory control plane the test setup installs.
 */
function route(matches: (path: string, method: string) => boolean, respond: () => Response): void {
  const fallback = vi.mocked(fetch).getMockImplementation();
  if (fallback === undefined) {
    throw new Error("the test setup installs a fetch mock before every test");
  }
  vi.mocked(fetch).mockImplementation((input, init) => {
    const url = new URL(
      typeof input === "string" ? input : input instanceof URL ? input.href : input.url,
    );
    if (matches(url.pathname, (init?.method ?? "GET").toUpperCase())) {
      return Promise.resolve(respond());
    }
    return fallback(input, init);
  });
}

/** Matches the poll of a Codex sign-in. */
function isCodexPoll(path: string, method: string): boolean {
  return method === "GET" && path === `/v1/harness-accounts/codex/oauth/${CODEX_ATTEMPT}`;
}

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

  it("shows the Codex code, the page it is typed on, and that it is waiting", async () => {
    const opened = vi.spyOn(window, "open").mockReturnValue(null);
    const { findByRole, findByText, findByLabelText } = renderChooser();

    fireEvent.click(await findByRole("button", { name: "Connect Codex" }));
    // The code does not exist until OpenAI has issued one.
    expect(document.querySelector("[aria-label='One-time code']")).toBeNull();

    fireEvent.click(await findByRole("button", { name: /Sign in with ChatGPT/ }));

    expect(await findByLabelText("One-time code")).toHaveTextContent("FLYC-8QK2");
    expect(opened).toHaveBeenCalledWith(DEVICE_URL, "_blank", "noopener,noreferrer");
    expect(await findByRole("link", { name: /Open auth.openai.com\/codex\/device/ })).toHaveAttribute(
      "href",
      DEVICE_URL,
    );
    expect(await findByText(/Waiting for you to approve in the browser/)).toBeInTheDocument();
    expect(await findByRole("button", { name: /Copy code/ })).toBeInTheDocument();
  });

  it("links the account when a poll finds the code approved", async () => {
    vi.spyOn(window, "open").mockReturnValue(null);
    route(isCodexPoll, () =>
      new Response(
        JSON.stringify({
          id: "harness-3",
          harness: "codex",
          label: "me@lexo.cool",
          linked_at_unix: 1_787_000_000,
          expires_at_unix: 1_787_003_600,
        }),
        { status: 201, headers: { "content-type": "application/json" } },
      ),
    );
    const { findByRole, onLinked } = renderChooser();

    fireEvent.click(await findByRole("button", { name: "Connect Codex" }));
    fireEvent.click(await findByRole("button", { name: /Sign in with ChatGPT/ }));

    await waitFor(() => expect(onLinked).toHaveBeenCalled(), { timeout: 4000 });
  });

  it("explains a switched-off device flow and offers to try again", async () => {
    route(
      (path, method) => method === "POST" && path === "/v1/harness-accounts/codex/oauth/start",
      () =>
        problem(
          409,
          "codex-device-auth-disabled",
          `Turn on device code authorization at ${SETTINGS_URL}`,
        ),
    );
    const { findByRole, findByText } = renderChooser();

    fireEvent.click(await findByRole("button", { name: "Connect Codex" }));
    fireEvent.click(await findByRole("button", { name: /Sign in with ChatGPT/ }));

    expect(await findByText(/device code authorization/)).toBeInTheDocument();
    expect(await findByRole("link", { name: /Open ChatGPT security settings/ })).toHaveAttribute(
      "href",
      SETTINGS_URL,
    );
    expect(await findByRole("button", { name: "Try again" })).toBeInTheDocument();
  });

  it("keeps the OpenAI key under Advanced, and points at where one is made", async () => {
    const { findByRole, findByLabelText, onLinked } = renderChooser();

    fireEvent.click(await findByRole("button", { name: "Connect Codex" }));
    expect(await findByRole("link", { name: /Create a key on the API keys page/ })).toHaveAttribute(
      "href",
      "https://platform.openai.com/api-keys",
    );

    const submit = await findByRole("button", { name: "Link with an API key" });
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
