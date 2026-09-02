import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, waitFor } from "@solidjs/testing-library";
import HostCard from "./HostCard";
import type { HostView } from "../api/client";

const HOST: HostView = {
  id: "8d1a6f30-4b7c-4e21-b0f5-9c2d6a7e4b11",
  label: "mercury",
  state: "online",
  facts: {
    architecture: "x86_64",
    vcpus: 16,
    memory_mib: 64 * 1024,
    disk_free_gib: 812,
    podman_version: "5.4.0",
    kernel: "6.8.0-45-generic",
    hostname: "mercury",
  },
  last_seen_unix: 1_790_000_600,
  created_at_unix: 1_789_991_000,
};

/**
 * A response carrying an RFC 9457 problem, as the control plane answers.
 *
 * `extensions` are the typed members a problem type defines beside the four
 * every document has (RFC 9457 §3.2) — `active_sessions` on
 * `host-has-active-sessions`.
 */
function problemResponse(
  status: number,
  slug: string,
  detail: string,
  extensions: Record<string, unknown> = {},
): Response {
  return new Response(
    JSON.stringify({
      type: `https://flyco.dev/problems/${slug}`,
      title: "Conflict",
      status,
      detail,
      ...extensions,
    }),
    { status, headers: { "content-type": "application/problem+json" } },
  );
}

function mount(host: HostView = HOST, onChanged = vi.fn()) {
  const result = render(() => <HostCard host={host} onChanged={onChanged} />);
  return { ...result, onChanged };
}

beforeEach(() => {
  // One body serves both routes the card calls: `PATCH` answers with the
  // renamed host and `DELETE` answers with nothing the client reads.
  vi.stubGlobal(
    "fetch",
    vi.fn(() =>
      Promise.resolve(
        new Response(JSON.stringify(HOST), {
          status: 200,
          headers: { "content-type": "application/json" },
        }),
      ),
    ),
  );
});

describe("HostCard", () => {
  it("states what the machine is, and that it costs nothing to meter", () => {
    const { getByText } = mount();

    expect(getByText("mercury")).toBeInTheDocument();
    expect(getByText(/^x86-64 · 16 vCPU \/ 64 GiB$/)).toBeInTheDocument();
    expect(getByText("812 GiB free")).toBeInTheDocument();
    expect(getByText("Podman 5.4.0 · Linux 6.8.0-45-generic")).toBeInTheDocument();
    // Not `$0.00`: flyco meters nothing here, and a zero would tell a
    // budget it can run forever.
    expect(getByText("your hardware")).toBeInTheDocument();
  });

  it("names the hostname once a rename has hidden it", () => {
    const { getByText } = mount({ ...HOST, label: "the loud one under the desk" });

    expect(getByText("the loud one under the desk")).toBeInTheDocument();
    expect(getByText("· mercury")).toBeInTheDocument();
  });

  it("says whether the machine is reachable right now", () => {
    expect(mount().getByText("Online")).toBeInTheDocument();
    expect(mount({ ...HOST, state: "offline" }).getByText("Offline")).toBeInTheDocument();
  });

  it("renames the machine through PATCH /v1/hosts/{id}", async () => {
    const { getByRole, getByLabelText, onChanged } = mount();

    getByRole("button", { name: "Rename" }).click();
    const field = getByLabelText("Name for mercury");
    fireEvent.input(field, { target: { value: "the loud one under the desk" } });
    fireEvent.submit(field.closest("form") as HTMLFormElement);

    await waitFor(() => expect(onChanged).toHaveBeenCalled());
    const [url, init] = vi.mocked(fetch).mock.calls[0] ?? [];
    expect(String((url as URL).pathname)).toBe(`/v1/hosts/${HOST.id}`);
    expect(init?.method).toBe("PATCH");
    expect(JSON.parse(String(init?.body))).toEqual({ label: "the loud one under the desk" });
  });

  it("removes the machine when nothing is running on it", async () => {
    const { getByRole, onChanged } = mount();

    getByRole("button", { name: "Remove" }).click();

    await waitFor(() => expect(onChanged).toHaveBeenCalled());
    const [url, init] = vi.mocked(fetch).mock.calls[0] ?? [];
    expect((url as URL).pathname).toBe(`/v1/hosts/${HOST.id}`);
    expect((url as URL).searchParams.get("force")).toBeNull();
    expect(init?.method).toBe("DELETE");
  });

  it("names the work a refused removal would stop, and offers to stop it", async () => {
    vi.mocked(fetch)
      .mockResolvedValueOnce(
        problemResponse(
          409,
          "host-has-active-sessions",
          "2 session(s) still run on this host; pass force to stop them",
          { active_sessions: 2 },
        ),
      )
      .mockResolvedValueOnce(new Response(null, { status: 204 }));

    const { getByRole, findByText, onChanged } = mount();
    getByRole("button", { name: "Remove" }).click();

    expect(await findByText(/2 sessions are still running on mercury/)).toBeInTheDocument();
    expect(onChanged).not.toHaveBeenCalled();

    getByRole("button", { name: "Remove anyway" }).click();
    await waitFor(() => expect(onChanged).toHaveBeenCalled());

    const [url, init] = vi.mocked(fetch).mock.calls[1] ?? [];
    expect((url as URL).searchParams.get("force")).toBe("true");
    expect(init?.method).toBe("DELETE");
  });

  it("keeps the machine when the refusal is declined", async () => {
    vi.mocked(fetch).mockResolvedValueOnce(
      problemResponse(
        409,
        "host-has-active-sessions",
        "1 session(s) still run on this host; pass force to stop them",
        { active_sessions: 1 },
      ),
    );

    const { getByRole, findByText, queryByRole, onChanged } = mount();
    getByRole("button", { name: "Remove" }).click();

    expect(await findByText(/1 session is still running on mercury/)).toBeInTheDocument();
    getByRole("button", { name: "Keep it" }).click();

    expect(queryByRole("button", { name: "Remove anyway" })).toBeNull();
    expect(onChanged).not.toHaveBeenCalled();
    expect(vi.mocked(fetch)).toHaveBeenCalledTimes(1);
  });

  it("counts what the refusal states, not what its sentence says", async () => {
    // The card reads the `active_sessions` member; `detail` is prose for a
    // person and nothing parses it.
    vi.mocked(fetch).mockResolvedValueOnce(
      problemResponse(409, "host-has-active-sessions", "work is still running here", {
        active_sessions: 4,
      }),
    );

    const { getByRole, findByText } = mount();
    getByRole("button", { name: "Remove" }).click();

    expect(await findByText(/4 sessions are still running on mercury/)).toBeInTheDocument();
  });

  it("states no number when the refusal carries none", async () => {
    vi.mocked(fetch).mockResolvedValueOnce(
      problemResponse(409, "host-has-active-sessions", "sessions are still running"),
    );

    const { getByRole, findByText } = mount();
    getByRole("button", { name: "Remove" }).click();

    expect(await findByText(/Sessions are still running on mercury/)).toBeInTheDocument();
  });

  it("shows any other refusal as what it was", async () => {
    vi.mocked(fetch).mockResolvedValueOnce(
      problemResponse(404, "host-not-found", "no such host"),
    );

    const { getByRole, findByRole } = mount();
    getByRole("button", { name: "Remove" }).click();

    expect(await findByRole("alert")).toHaveTextContent("no such host");
  });

  it("offers no controls where renaming and removing are not on offer", () => {
    const { queryByRole } = render(() => (
      <HostCard host={HOST} onChanged={vi.fn()} editable={false} />
    ));

    expect(queryByRole("button", { name: "Rename" })).toBeNull();
    expect(queryByRole("button", { name: "Remove" })).toBeNull();
  });
});
