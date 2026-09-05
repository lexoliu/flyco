import { describe, expect, it } from "vitest";
import { ApiProblem, NetworkError, NotImplementedError, UnexpectedResponseError, problemFromResponse } from "./problem";

function problemResponse(status: number, type: string, extra?: Partial<{ title: string; detail: string }>): Response {
  const body = {
    type,
    title: extra?.title ?? "Something went wrong",
    status,
    detail: extra?.detail ?? "details here",
  };
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/problem+json" },
  });
}

describe("problemFromResponse", () => {
  it("parses a well-formed problem document into an ApiProblem", async () => {
    const response = problemResponse(409, "https://flyco.dev/problems/conflict", {
      title: "Conflict",
      detail: "already exists",
    });
    const error = await problemFromResponse(response);
    expect(error).toBeInstanceOf(ApiProblem);
    expect(error).not.toBeInstanceOf(NotImplementedError);
    const problem = error as ApiProblem;
    expect(problem.status).toBe(409);
    expect(problem.type).toBe("https://flyco.dev/problems/conflict");
    expect(problem.title).toBe("Conflict");
    expect(problem.detail).toBe("already exists");
    // The message is the detail alone: the status phrase adds nothing a
    // page has not already said.
    expect(problem.message).toBe("already exists");
  });

  it("recognizes a /not-implemented suffix as NotImplementedError", async () => {
    const response = problemResponse(501, "https://flyco.dev/problems/not-implemented");
    const error = await problemFromResponse(response);
    expect(error).toBeInstanceOf(NotImplementedError);
    expect(error).toBeInstanceOf(ApiProblem);
  });

  it("recognizes a /relay-unavailable suffix as NotImplementedError", async () => {
    const response = problemResponse(501, "https://flyco.dev/problems/relay-unavailable");
    const error = await problemFromResponse(response);
    expect(error).toBeInstanceOf(NotImplementedError);
  });

  it("does not treat an unrelated 501 body as NotImplementedError unless the suffix matches", async () => {
    const response = problemResponse(501, "https://flyco.dev/problems/rate-limited");
    const error = await problemFromResponse(response);
    expect(error).toBeInstanceOf(ApiProblem);
    expect(error).not.toBeInstanceOf(NotImplementedError);
  });

  it("wraps a non-problem body in UnexpectedResponseError", async () => {
    const response = new Response("<html>502 Bad Gateway</html>", {
      status: 502,
      headers: { "content-type": "text/html" },
    });
    const error = await problemFromResponse(response);
    expect(error).toBeInstanceOf(UnexpectedResponseError);
    const unexpected = error as UnexpectedResponseError;
    expect(unexpected.status).toBe(502);
    expect(unexpected.bodyText).toContain("Bad Gateway");
  });

  it("wraps a missing content-type as UnexpectedResponseError", async () => {
    const response = new Response("plain text failure", { status: 500 });
    const error = await problemFromResponse(response);
    expect(error).toBeInstanceOf(UnexpectedResponseError);
  });
});

describe("NetworkError", () => {
  it("carries the original cause and its message", () => {
    const cause = new TypeError("Failed to fetch");
    const error = new NetworkError(cause);
    expect(error.message).toBe("Failed to fetch");
    expect(error.cause).toBe(cause);
    expect(error.name).toBe("NetworkError");
  });

  it("stringifies a non-Error cause", () => {
    const error = new NetworkError("socket hang up");
    expect(error.message).toBe("socket hang up");
  });
});
