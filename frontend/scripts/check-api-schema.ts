/**
 * Regenerates the typed OpenAPI schema into a scratch file and diffs it
 * against the committed `src/api/schema.d.ts`, so the generated client and
 * the REST contract (`../openapi.json`) can never quietly drift apart.
 *
 * Fails loudly — mismatched output, a missing committed file, or a
 * generator crash all exit non-zero with an explanation, rather than
 * silently passing.
 */
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

const committedPath = join(import.meta.dirname, "..", "src", "api", "schema.d.ts");
const scratchDir = mkdtempSync(join(tmpdir(), "flyco-api-schema-"));
const scratchPath = join(scratchDir, "schema.d.ts");

try {
  const result = spawnSync(
    "bunx",
    ["openapi-typescript", "../openapi.json", "-o", scratchPath],
    { cwd: join(import.meta.dirname, ".."), stdio: "inherit" },
  );
  if (result.status !== 0) {
    throw new Error(`openapi-typescript exited with status ${result.status}`);
  }

  let committed: string;
  try {
    committed = readFileSync(committedPath, "utf8");
  } catch {
    throw new Error(
      `${committedPath} is missing. Run \`bun run api:generate\` and commit its output.`,
    );
  }
  const fresh = readFileSync(scratchPath, "utf8");

  if (committed !== fresh) {
    console.error(
      "src/api/schema.d.ts is stale: it does not match what openapi-typescript " +
        "generates from ../openapi.json right now. Run `bun run api:generate` " +
        "and commit the result.",
    );
    process.exit(1);
  }

  console.log("src/api/schema.d.ts matches ../openapi.json.");
} finally {
  rmSync(scratchDir, { recursive: true, force: true });
}
