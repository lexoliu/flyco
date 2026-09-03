import { renderHook, waitFor } from "@solidjs/testing-library";
import { createSignal } from "solid-js";
import { describe, expect, it } from "vitest";
import { createQuery } from "./query";

describe("createQuery", () => {
  it("answers undefined and reports the error when the fetcher rejects", async () => {
    const failure = new Error("the control plane is unreachable");
    const { result } = renderHook(() =>
      createQuery(async () => {
        throw failure;
      }),
    );
    const [read] = result;

    await waitFor(() => expect(read.error).toBe(failure));
    // The point of the wrapper: this read is what Solid's own accessor would
    // have thrown from, blanking the component instead of showing the notice.
    expect(read()).toBeUndefined();
    expect(read.latest).toBeUndefined();
    expect(read.state).toBe("errored");
    expect(read.loading).toBe(false);
  });

  it("answers the value when the fetcher resolves", async () => {
    const { result } = renderHook(() => createQuery(async () => "octocat"));
    const [read] = result;

    await waitFor(() => expect(read()).toBe("octocat"));
    expect(read.error).toBeUndefined();
    expect(read.latest).toBe("octocat");
    expect(read.state).toBe("ready");
  });

  it("refetches from its source, and recovers once the source stops failing", async () => {
    const failure = new Error("no such account");
    const [account, setAccount] = createSignal("absent");
    const { result } = renderHook(() =>
      createQuery(account, async (id: string) => {
        if (id === "absent") {
          throw failure;
        }
        return `usage for ${id}`;
      }),
    );
    const [read] = result;

    await waitFor(() => expect(read.error).toBe(failure));
    expect(read()).toBeUndefined();

    setAccount("acct-1");
    await waitFor(() => expect(read()).toBe("usage for acct-1"));
    expect(read.error).toBeUndefined();
  });
});
