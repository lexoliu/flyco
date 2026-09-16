import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@solidjs/testing-library";
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import Login from "./Login";
import { json, postedTo, route } from "../components/flow/testSupport";
import type { RenderOptions, TurnstileApi } from "../lib/turnstile";
import { clearSessionToken } from "../lib/session";

/**
 * The widget API `api.js` installs on `window`, as a test double that
 * records the options it was rendered with instead of iframing Cloudflare.
 */
function stubTurnstile() {
  const rendered: { options?: RenderOptions } = {};
  const api: TurnstileApi = {
    render: vi.fn((_container: HTMLElement, options: RenderOptions) => {
      rendered.options = options;
      return "widget-1";
    }),
    execute: vi.fn(),
    reset: vi.fn(),
    remove: vi.fn(),
  };
  vi.stubGlobal("turnstile", api);
  return { api, rendered };
}

function mount(url = "/login") {
  const history = createMemoryHistory();
  history.set({ value: url, replace: true, scroll: false });
  return render(() => (
    <MemoryRouter history={history}>
      <Route path="/login" component={Login} />
    </MemoryRouter>
  ));
}

describe("Login", () => {
  beforeEach(() => {
    clearSessionToken();
  });
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("renders the human check, then signs in with its token", async () => {
    const { api, rendered } = stubTurnstile();
    route(
      (path, method) => method === "POST" && path === "/v1/auth/github/start",
      () =>
        json({
          authorize_url: "https://github.com/login/oauth/authorize?state=x",
        }),
    );
    const { findByRole } = mount();
    const button = await findByRole("button", { name: "Sign in with GitHub" });

    // Sign-in stays closed until the widget has judged the page.
    expect(button).toBeDisabled();
    await vi.waitFor(() => expect(api.render).toHaveBeenCalled());
    expect(rendered.options?.sitekey).toBe("1x00000000000000000000AA");
    expect(rendered.options?.action).toBe("login");
    expect(button).toBeDisabled();

    rendered.options?.callback?.("the-proof");
    expect(button).toBeEnabled();

    fireEvent.click(button);
    await vi.waitFor(() =>
      expect(postedTo("/v1/auth/github/start")).toEqual({
        turnstile_token: "the-proof",
      }),
    );
  });

  it("drops a spent proof and re-arms the widget when sign-in is refused", async () => {
    const { api, rendered } = stubTurnstile();
    route(
      (path, method) => method === "POST" && path === "/v1/auth/github/start",
      () =>
        new Response(
          JSON.stringify({
            type: "https://flyco.dev/problems/turnstile-refused",
            title: "the human check was refused",
            status: 403,
            detail: "the human check was refused",
          }),
          {
            status: 403,
            headers: { "content-type": "application/problem+json" },
          },
        ),
    );
    const { findByRole } = mount();
    const button = await findByRole("button", { name: "Sign in with GitHub" });
    await vi.waitFor(() => expect(api.render).toHaveBeenCalled());
    rendered.options?.callback?.("the-proof");
    expect(button).toBeEnabled();

    fireEvent.click(button);

    expect(await findByRole("alert")).toHaveTextContent(/human check/);
    expect(button).toBeDisabled();
    expect(api.reset).toHaveBeenCalledWith("widget-1");
  });

  it("disables sign-in again when the widget reports its proof expired", async () => {
    const { api, rendered } = stubTurnstile();
    const { findByRole } = mount();
    const button = await findByRole("button", { name: "Sign in with GitHub" });
    await vi.waitFor(() => expect(api.render).toHaveBeenCalled());
    rendered.options?.callback?.("the-proof");
    expect(button).toBeEnabled();

    rendered.options?.["expired-callback"]?.();

    expect(button).toBeDisabled();
  });

  it("says why sign-in is impossible when the check cannot be configured", async () => {
    route(
      (path, method) => method === "GET" && path === "/v1/config",
      () =>
        json(
          {
            type: "https://flyco.dev/problems/not-implemented",
            title: "Not implemented",
            status: 501,
            detail: "Not implemented",
          },
          501,
        ),
    );
    const { findByRole } = mount();
    const button = await findByRole("button", { name: "Sign in with GitHub" });

    expect(await findByRole("alert")).toHaveTextContent(/./);
    expect(button).toBeDisabled();
  });
});
