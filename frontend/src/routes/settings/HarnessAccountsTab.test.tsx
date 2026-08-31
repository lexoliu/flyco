import { fireEvent, render, waitFor } from "@solidjs/testing-library";
import { beforeEach, describe, expect, it, vi } from "vitest";
import HarnessAccountsTab from "./HarnessAccountsTab";

function response(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

describe("HarnessAccountsTab", () => {
  const requests: unknown[] = [];

  beforeEach(() => {
    requests.length = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (_input: string | URL | Request, init?: RequestInit) => {
        if ((init?.method ?? "GET") === "POST") {
          requests.push(JSON.parse(String(init?.body)));
          return response(
            {
              id: crypto.randomUUID(),
              harness: "claude_code",
              label: "Personal",
              linked_at_unix: 1_788_000_000,
              expires_at_unix: null,
            },
            201,
          );
        }
        return response([]);
      }),
    );
  });

  it.each([
    ["Claude subscription setup token", "claude_setup_token", "token", "setup-secret"],
    ["Anthropic API key", "claude_api_key", "key", "anthropic-secret"],
  ] as const)("links with %s and clears the secret", async (_name, kind, field, secret) => {
    const view = render(() => <HarnessAccountsTab />);
    await view.findByText("No harness accounts linked yet.");

    fireEvent.input(view.getByLabelText("Label"), { target: { value: "Personal" } });
    fireEvent.change(view.getByLabelText("Credential"), { target: { value: kind } });
    const secretInput = view.getByLabelText(kind === "claude_setup_token" ? "Setup token" : "API key");
    fireEvent.input(secretInput, { target: { value: secret } });
    fireEvent.click(view.getByRole("button", { name: "Link account" }));

    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests[0]).toEqual({
      label: "Personal",
      credential: { kind, [field]: secret },
    });
    await waitFor(() => expect(secretInput).toHaveValue(""));
  });

  it("links Codex with an OpenAI API key", async () => {
    const view = render(() => <HarnessAccountsTab />);
    await view.findByText("No harness accounts linked yet.");

    fireEvent.change(view.getByLabelText("Harness"), { target: { value: "codex" } });
    fireEvent.input(view.getByLabelText("Label"), { target: { value: "Work" } });
    fireEvent.input(view.getByLabelText("API key"), { target: { value: "openai-secret" } });
    fireEvent.click(view.getByRole("button", { name: "Link account" }));

    await waitFor(() => expect(requests).toHaveLength(1));
    expect(requests[0]).toEqual({
      label: "Work",
      credential: { kind: "codex_api_key", key: "openai-secret" },
    });
  });
});
