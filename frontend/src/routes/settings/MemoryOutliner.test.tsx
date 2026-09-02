import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, waitFor } from "@solidjs/testing-library";
import MemoryOutliner from "./MemoryOutliner";

const ROOT = {
  id: "019d1d12-43f0-7bf2-b1f4-a3e7838c5c01",
  title: "Rust",
  content: "Prefer traits over enums.",
  parent: null,
  updated_at_unix: 1_788_000_000,
};
const CHILD = {
  id: "019d1d12-43f0-7bf2-b1f4-a3e7838c5c02",
  title: "Lints",
  content: "",
  parent: ROOT.id,
  updated_at_unix: 1_788_000_100,
};

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

describe("MemoryOutliner", () => {
  const calls: { method: string; url: string; body: string | null }[] = [];

  beforeEach(() => {
    calls.length = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn((input: string | URL | Request, init?: RequestInit) => {
        const url = new URL(String(input));
        const method = (init?.method ?? "GET").toUpperCase();
        calls.push({ method, url: url.pathname + url.search, body: (init?.body as string) ?? null });

        if (method === "GET" && url.pathname === "/v1/memory") {
          const parent = url.searchParams.get("parent");
          return Promise.resolve(json(parent === ROOT.id ? [CHILD] : parent === null ? [ROOT] : []));
        }
        if (method === "PATCH") {
          return Promise.resolve(
            json({ ...ROOT, ...(JSON.parse(String(init?.body)) as Record<string, unknown>) }),
          );
        }
        if (method === "POST") {
          return Promise.resolve(json(CHILD, 201));
        }
        if (method === "DELETE") {
          return Promise.resolve(new Response(null, { status: 204 }));
        }
        return Promise.resolve(json([], 200));
      }),
    );
  });

  it("lists the roots and nothing beneath them", async () => {
    const view = render(() => <MemoryOutliner />);

    expect(await view.findByText("Rust")).toBeInTheDocument();
    expect(view.queryByText("Lints")).toBeNull();
  });

  it("fetches a node's children the first time it is expanded, and keeps them", async () => {
    const view = render(() => <MemoryOutliner />);
    await view.findByText("Rust");

    fireEvent.click(view.getByRole("button", { name: "Expand Rust" }));
    expect(await view.findByText("Lints")).toBeInTheDocument();

    const fetches = () => calls.filter((call) => call.url === `/v1/memory?parent=${ROOT.id}`).length;
    expect(fetches()).toBe(1);

    // Collapsing hides the subtree; reopening it costs no second request.
    fireEvent.click(view.getByRole("button", { name: "Collapse Rust" }));
    await waitFor(() => expect(view.queryByText("Lints")).toBeNull());
    fireEvent.click(view.getByRole("button", { name: "Expand Rust" }));
    expect(await view.findByText("Lints")).toBeInTheDocument();
    expect(fetches()).toBe(1);
  });

  it("renames a node in place through PATCH /v1/memory/{id}", async () => {
    const view = render(() => <MemoryOutliner />);
    await view.findByText("Rust");

    fireEvent.click(view.getByRole("button", { name: "Edit" }));
    fireEvent.input(view.getByLabelText("Title"), { target: { value: "Rust rules" } });
    fireEvent.click(view.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(view.getByText("Rust rules")).toBeInTheDocument());
    const patch = calls.find((call) => call.method === "PATCH");
    expect(patch?.url).toBe(`/v1/memory/${ROOT.id}`);
    expect(JSON.parse(patch?.body ?? "{}")).toEqual({
      title: "Rust rules",
      content: ROOT.content,
    });
  });

  it("adds a child under the node it was asked for, and opens the parent", async () => {
    const view = render(() => <MemoryOutliner />);
    await view.findByText("Rust");

    fireEvent.click(view.getByRole("button", { name: "Add a note under Rust" }));
    fireEvent.input(view.getByLabelText("New note under Rust"), { target: { value: "Lints" } });
    fireEvent.submit(view.getByLabelText("New note under Rust").closest("form") as HTMLFormElement);

    await waitFor(() => expect(calls.some((call) => call.method === "POST")).toBe(true));
    const post = calls.find((call) => call.method === "POST");
    expect(JSON.parse(post?.body ?? "{}")).toEqual({
      title: "Lints",
      content: "",
      parent: ROOT.id,
    });
    expect(await view.findByText("Lints")).toBeInTheDocument();
  });

  it("deletes a node and stops drawing it", async () => {
    const view = render(() => <MemoryOutliner />);
    await view.findByText("Rust");

    fireEvent.click(view.getByRole("button", { name: "Delete Rust" }));

    await waitFor(() => expect(view.queryByText("Rust")).toBeNull());
    expect(calls.some((call) => call.method === "DELETE" && call.url === `/v1/memory/${ROOT.id}`)).toBe(
      true,
    );
  });
});
