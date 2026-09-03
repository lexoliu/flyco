import { describe, expect, it } from "vitest";

/** Every source file in the app, as text, keyed by its path from `src/lib`. */
const sources = import.meta.glob<string>("../**/*.{ts,tsx}", {
  query: "?raw",
  import: "default",
  eager: true,
});

/** The one file allowed to reach for the raw, throwing resource. */
const WRAPPER = "./query.ts";

/** Named imports of a single `from "solid-js"` statement. */
const SOLID_IMPORT = /import\s+(?:type\s+)?\{([^}]*)\}\s*from\s*["']solid-js["']/g;

function importsCreateResource(source: string): boolean {
  for (const match of source.matchAll(SOLID_IMPORT)) {
    const specifiers = (match[1] ?? "").split(",").map((name) => name.trim().split(/\s+/)[0]);
    if (specifiers.includes("createResource")) {
      return true;
    }
  }
  return false;
}

describe("resource boundary", () => {
  it("keeps createResource inside lib/query.ts", () => {
    const offenders = Object.entries(sources)
      .filter(([path]) => path !== WRAPPER)
      .filter(([, source]) => importsCreateResource(source))
      .map(([path]) => path);

    // A resource read after its fetcher rejected throws, so a component that
    // uses `createResource` directly blanks itself instead of rendering the
    // ProblemNotice beside it. `createQuery` is the only way in.
    expect(offenders).toEqual([]);
  });

  it("sees the files it is meant to police", () => {
    expect(sources[WRAPPER]).toContain("createResource");
    expect(Object.keys(sources).length).toBeGreaterThan(100);
  });
});
