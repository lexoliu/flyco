import { fireEvent, render, screen, waitFor } from "@solidjs/testing-library";
import { describe, expect, it, vi } from "vitest";
import HarnessAccountsTab from "./HarnessAccountsTab";

const ACCOUNT_ID = "019d1d12-43f0-7bf2-b1f4-a3e7838c5c01";

function jsonResponse(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

describe("HarnessAccountsTab", () => {
  it("unlinks the account by its listed resource id", async () => {
    const fetchMock = vi.mocked(fetch);
    fetchMock.mockImplementation((_input, init) => {
      if ((init?.method ?? "GET") === "DELETE") {
        return Promise.resolve(new Response(null, { status: 204 }));
      }
      return Promise.resolve(
        jsonResponse([
          {
            id: ACCOUNT_ID,
            harness: "codex",
            label: "lexoliu",
            linked_at_unix: 1_788_200_000,
            expires_at_unix: null,
          },
        ]),
      );
    });

    render(() => <HarnessAccountsTab />);
    await fireEvent.click(await screen.findByRole("button", { name: "Unlink" }));

    await waitFor(() => {
      expect(
        fetchMock.mock.calls.some(
          ([input, init]) =>
            String(input).endsWith(`/v1/harness-accounts/${ACCOUNT_ID}`) &&
            init?.method === "DELETE",
        ),
      ).toBe(true);
    });
  });
});
