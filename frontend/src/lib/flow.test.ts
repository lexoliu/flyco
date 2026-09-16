/**
 * The first-run sequence as data (docs/ux.md §4): every branch of the page
 * list, the shortcuts, the conditional pages, and `Back` keeping answers.
 */
import { describe, expect, it } from "vitest";
import type { HarnessAccountView, ProviderBonusHint } from "../api/client";
import {
  advance,
  back,
  currentPage,
  finishLink,
  isFinished,
  linkedAgents,
  pagesFor,
  progress,
  record,
  startFlow,
  type FlowState,
  type PageId,
} from "./flow";

const CLAUDE: HarnessAccountView = {
  id: "harness-2",
  harness: "claude_code",
  label: "me@lexo.cool",
  linked_at_unix: 1_787_000_000,
  expires_at_unix: 1_787_028_800,
  models: [],
  usage: [],
};

const CODEX: HarnessAccountView = {
  id: "harness-3",
  harness: "codex",
  label: "me@lexo.cool",
  linked_at_unix: 1_787_000_000,
  expires_at_unix: null,
  models: [],
  usage: [],
};

const AZURE_STUDENTS: ProviderBonusHint = {
  provider: "azure",
  title: "Azure for Students",
  detail: "Verify with a school email.",
  url: "https://azure.microsoft.com/free/students/",
  credit: 100_000_000,
};

const AWS_ACTIVATE: ProviderBonusHint = {
  provider: "aws",
  title: "AWS Activate",
  detail: "For startups.",
  url: "https://aws.amazon.com/activate/",
  credit: null,
};

const ATTEMPT = {
  attempt_id: "a",
  authorize_url: "https://claude.ai/oauth/authorize?x",
};

const DEVIN_ATTEMPT = {
  attempt_id: "d",
  authorize_url: "https://app.devin.ai/auth/cli/continue?x",
};

const ids = (state: FlowState): PageId[] =>
  pagesFor(state).map((page) => page.id);

describe("pagesFor", () => {
  it("walks the three stages: meet, the agents list, the compute question", () => {
    const state = startFlow({ stages: ["meet", "agent", "compute"] });
    expect(ids(state)).toEqual(["meet", "agents", "compute-choice"]);
    expect(currentPage(state)).toEqual({ id: "meet" });
  });

  it("covers one stage alone for a flow opened from settings, and one agent's pages when named", () => {
    expect(ids(startFlow({ stages: ["agent"] }))).toEqual(["agents"]);
    expect(ids(startFlow({ stages: ["agent"], agents: ["codex"] }))).toEqual([
      "codex-sign-in",
    ]);
    expect(
      ids(startFlow({ stages: ["agent"], agents: ["claude_code"] })),
    ).toEqual(["claude-sign-in"]);
    expect(ids(startFlow({ stages: ["compute"] }))).toEqual(["compute-choice"]);
  });

  it("refuses an agent stage with no agent to link", () => {
    expect(() => startFlow({ stages: ["agent"], agents: [] })).toThrow(
      /at least one agent/,
    );
  });

  it("stays one page however many agents there are: only the chosen one's pages follow", () => {
    const list = startFlow({ stages: ["agent"] });
    const claude = advance(list, { linking: "claude_code" });
    expect(ids(claude)).toEqual(["agents", "claude-sign-in"]);
    expect(currentPage(claude).id).toBe("claude-sign-in");
    const codex = advance(list, { linking: "codex" });
    expect(ids(codex)).toEqual(["agents", "codex-sign-in"]);
    expect(currentPage(back(codex)).id).toBe("agents");
  });

  it("adds Claude's paste page only while a sign-in is open", () => {
    const signIn = advance(startFlow({ stages: ["agent"] }), {
      linking: "claude_code",
    });
    const opened = advance(signIn, { claudeAttempt: ATTEMPT });
    expect(ids(opened)).toEqual(["agents", "claude-sign-in", "claude-paste"]);
    expect(currentPage(opened).id).toBe("claude-paste");
  });

  it("swaps the sign-in's second page for the API-key page behind the link", () => {
    const claude = advance(
      advance(startFlow({ stages: ["agent"] }), { linking: "claude_code" }),
      { routes: { claude_code: "api-key", codex: "sign-in", devin: "sign-in" } },
    );
    expect(ids(claude)).toEqual(["agents", "claude-sign-in", "api-key"]);
    expect(currentPage(claude)).toEqual({
      id: "api-key",
      agent: "claude_code",
    });

    const codex = advance(startFlow({ stages: ["agent"], agents: ["codex"] }), {
      routes: { claude_code: "sign-in", codex: "api-key", devin: "sign-in" },
    });
    expect(ids(codex)).toEqual(["codex-sign-in", "api-key"]);
    expect(currentPage(codex)).toEqual({ id: "api-key", agent: "codex" });
  });

  it("adds Devin's paste page only while a sign-in is open", () => {
    const signIn = advance(startFlow({ stages: ["agent"] }), {
      linking: "devin",
    });
    expect(ids(signIn)).toEqual(["agents", "devin-sign-in"]);
    expect(currentPage(signIn).id).toBe("devin-sign-in");

    const opened = advance(signIn, { devinAttempt: DEVIN_ATTEMPT });
    expect(ids(opened)).toEqual(["agents", "devin-sign-in", "devin-paste"]);
    expect(currentPage(opened).id).toBe("devin-paste");

    expect(
      ids(startFlow({ stages: ["agent"], agents: ["devin"] })),
    ).toEqual(["devin-sign-in"]);
  });

  it("puts Devin's key page behind its sign-in, like the other two", () => {
    const devin = advance(
      advance(startFlow({ stages: ["agent"] }), { linking: "devin" }),
      {
        routes: {
          claude_code: "sign-in",
          codex: "sign-in",
          devin: "api-key",
        },
      },
    );
    expect(ids(devin)).toEqual(["agents", "devin-sign-in", "api-key"]);
    expect(currentPage(devin)).toEqual({ id: "api-key", agent: "devin" });
  });

  it("returns to the list when an agent links, its pages gone and the list reading Linked", () => {
    const opened = advance(
      advance(startFlow({ stages: ["meet", "agent", "compute"] })),
      { linking: "claude_code" },
    );
    const paste = advance(opened, { claudeAttempt: ATTEMPT });
    expect(currentPage(paste).id).toBe("claude-paste");
    const linked = finishLink(paste, "claude_code", CLAUDE);
    expect(ids(linked)).toEqual(["meet", "agents", "compute-choice"]);
    expect(currentPage(linked).id).toBe("agents");
    expect(linked.answers.agents.claude_code).toEqual(CLAUDE);
    expect(linked.answers.linking).toBeNull();
    expect(linked.answers.claudeAttempt).toBeNull();
  });

  it("finishes a flow opened for one agent when that agent links", () => {
    const codex = startFlow({ stages: ["agent"], agents: ["codex"] });
    expect(isFinished(finishLink(codex, "codex", CODEX))).toBe(true);

    const key = advance(startFlow({ stages: ["agent"], agents: ["codex"] }), {
      routes: { claude_code: "sign-in", codex: "api-key", devin: "sign-in" },
    });
    expect(currentPage(key).id).toBe("api-key");
    expect(isFinished(finishLink(key, "codex", CODEX))).toBe(true);
  });

  it("reads what a readiness read found into the agents answer", () => {
    expect(linkedAgents([CLAUDE, CODEX])).toEqual({
      claude_code: CLAUDE,
      codex: CODEX,
    });
    expect(linkedAgents([])).toEqual({});
  });

  it("asks a cloud provider the two bonus questions before its own pages, and no linked page after", () => {
    const azure = advance(startFlow({ stages: ["compute"] }), {
      compute: "azure",
    });
    expect(ids(azure)).toEqual([
      "compute-choice",
      "new-to-provider",
      "student",
      "cloud-sign-in",
    ]);
    const aws = advance(startFlow({ stages: ["compute"] }), { compute: "aws" });
    expect(ids(aws)).toEqual([
      "compute-choice",
      "new-to-provider",
      "student",
      "aws-policy",
      "aws-keys",
    ]);
    const gcp = advance(startFlow({ stages: ["compute"] }), { compute: "gcp" });
    expect(ids(gcp)).toEqual([
      "compute-choice",
      "new-to-provider",
      "student",
      "cloud-sign-in",
    ]);
  });

  it("walks the vendor's consent: the choice page appears once the consent is back", () => {
    const signIn = advance(
      advance(
        advance(startFlow({ stages: ["compute"] }), { compute: "azure" }),
        { newToProvider: false },
      ),
      { student: false, programmes: [] },
    );
    expect(currentPage(signIn)).toEqual({
      id: "cloud-sign-in",
      provider: "azure",
    });
    const consent = {
      attemptId: "attempt-1",
      account: "me@lexo.cool",
      choices: [{ id: "sub-1", name: "Pay-As-You-Go" }],
    };
    const back = advance(signIn, { cloudConsent: consent, cloudChoice: null });
    expect(ids(back)).toEqual([
      "compute-choice",
      "new-to-provider",
      "student",
      "cloud-sign-in",
      "cloud-choice",
    ]);
    expect(currentPage(back)).toEqual({
      id: "cloud-choice",
      provider: "azure",
    });
    expect(isFinished(advance(back, { cloudChoice: "sub-1" }))).toBe(true);

    const gcp = advance(
      advance(advance(startFlow({ stages: ["compute"] }), { compute: "gcp" }), {
        newToProvider: false,
      }),
      { student: false, programmes: [] },
    );
    const chosen = advance(gcp, { cloudConsent: consent, cloudChoice: null });
    expect(currentPage(chosen)).toEqual({
      id: "cloud-choice",
      provider: "gcp",
    });
    expect(isFinished(advance(chosen, { cloudChoice: "proj-1" }))).toBe(true);
  });

  it("walks codespaces' sign-in alone: GitHub's consent finishes with nothing to choose", () => {
    const signIn = advance(
      advance(
        advance(startFlow({ stages: ["compute"] }), {
          compute: "codespaces",
        }),
        { newToProvider: false },
      ),
      { student: false, programmes: [] },
    );
    expect(currentPage(signIn)).toEqual({
      id: "cloud-sign-in",
      provider: "codespaces",
    });
    // No choice page exists for it: the page's own finish call is the last
    // step, and the plain advance it makes walks off the end of the flow.
    expect(ids(signIn)).toEqual([
      "compute-choice",
      "new-to-provider",
      "student",
      "cloud-sign-in",
    ]);
    expect(isFinished(advance(signIn, {}))).toBe(true);
  });

  it("swaps the consent for the vendor's terminal behind the quiet link", () => {
    const signIn = advance(
      advance(
        advance(startFlow({ stages: ["compute"] }), { compute: "azure" }),
        { newToProvider: false },
      ),
      { student: false, programmes: [] },
    );
    const shell = advance(signIn, { cloudRoute: "cloud-shell" });
    expect(ids(shell)).toEqual([
      "compute-choice",
      "new-to-provider",
      "student",
      "azure-command",
      "azure-paste",
    ]);
    expect(currentPage(shell).id).toBe("azure-command");
    const gcpShell = advance(
      advance(
        advance(
          advance(startFlow({ stages: ["compute"] }), { compute: "gcp" }),
          { newToProvider: false },
        ),
        { student: false, programmes: [] },
      ),
      { cloudRoute: "cloud-shell" },
    );
    expect(ids(gcpShell)).toEqual([
      "compute-choice",
      "new-to-provider",
      "student",
      "gcp-commands",
      "gcp-key-file",
    ]);
  });

  it("names the provider on the bonus pages", () => {
    const state = advance(startFlow({ stages: ["compute"] }), {
      compute: "gcp",
    });
    expect(currentPage(state)).toEqual({
      id: "new-to-provider",
      provider: "gcp",
    });
  });

  it("asks a machine the user owns no bonus questions at all", () => {
    const state = advance(startFlow({ stages: ["compute"] }), {
      compute: "host",
    });
    expect(ids(state)).toEqual(["compute-choice", "host-enroll"]);
  });

  it("adds the credit page only when a programme matched the chosen provider", () => {
    const azure = advance(startFlow({ stages: ["compute"] }), {
      compute: "azure",
    });
    const answered = advance(advance(azure, { newToProvider: true }), {
      student: true,
      programmes: [AZURE_STUDENTS, AWS_ACTIVATE],
    });
    expect(ids(answered)).toEqual([
      "compute-choice",
      "new-to-provider",
      "student",
      "credit",
      "cloud-sign-in",
    ]);
    expect(currentPage(answered)).toEqual({
      id: "credit",
      provider: "azure",
      programmes: [AZURE_STUDENTS],
    });

    const nothing = advance(advance(azure, { newToProvider: false }), {
      student: false,
      programmes: [AWS_ACTIVATE],
    });
    expect(ids(nothing)).not.toContain("credit");
    expect(currentPage(nothing).id).toBe("cloud-sign-in");
  });

  it("asks for the subscription only when the pasted Azure block lacked one", () => {
    const paste = advance(
      advance(
        advance(
          advance(
            advance(startFlow({ stages: ["compute"] }), { compute: "azure" }),
            { newToProvider: false },
          ),
          {
            student: false,
            programmes: [],
          },
        ),
        { cloudRoute: "cloud-shell" },
      ),
    );
    expect(currentPage(paste).id).toBe("azure-paste");

    const principal = {
      clientId: "c",
      clientSecret: "s",
      tenantId: "t",
      subscriptionId: null,
    };
    const asked = advance(paste, {
      azurePaste: "{}",
      azurePrincipal: principal,
    });
    expect(ids(asked)).toContain("azure-subscription");
    expect(currentPage(asked).id).toBe("azure-subscription");

    const complete = advance(paste, {
      azurePaste: "{}",
      azurePrincipal: { ...principal, subscriptionId: "sub" },
    });
    expect(ids(complete)).not.toContain("azure-subscription");
    expect(isFinished(complete)).toBe(true);
  });

  it("finishes the compute stage when the credential links, with no linked page after", () => {
    const keys = advance(
      advance(
        advance(
          advance(startFlow({ stages: ["compute"] }), { compute: "aws" }),
          { newToProvider: false },
        ),
        {
          student: false,
          programmes: [],
        },
      ),
    );
    expect(currentPage(keys).id).toBe("aws-keys");
    expect(isFinished(advance(keys))).toBe(true);
  });
});

describe("advance and back", () => {
  it("keeps every answer on the way back", () => {
    const state = advance(
      advance(startFlow({ stages: ["compute"] }), { compute: "azure" }),
      {
        newToProvider: true,
      },
    );
    const returned = back(back(state));
    expect(currentPage(returned).id).toBe("compute-choice");
    expect(returned.answers.compute).toBe("azure");
    expect(returned.answers.newToProvider).toBe(true);
  });

  it("refuses to go back from the first page", () => {
    expect(() => back(startFlow({ stages: ["meet"] }))).toThrow(/first page/);
  });

  it("finishes when the last page advances", () => {
    const state = startFlow({ stages: ["meet"] });
    expect(isFinished(state)).toBe(false);
    expect(isFinished(advance(state))).toBe(true);
  });

  it("lands on the first page a new answer appended", () => {
    const state = advance(startFlow({ stages: ["compute"] }), {
      compute: "host",
    });
    expect(currentPage(state).id).toBe("host-enroll");
  });

  it("records an answer without moving, and refuses one that removes the page", () => {
    const opened = advance(
      advance(startFlow({ stages: ["agent"] }), { linking: "claude_code" }),
      { claudeAttempt: ATTEMPT },
    );
    const kept = record(opened, { azurePaste: "kept" });
    expect(currentPage(kept).id).toBe("claude-paste");
    expect(kept.answers.azurePaste).toBe("kept");
    expect(() => record(opened, { claudeAttempt: null })).toThrow(
      /removed the claude-paste page/,
    );
  });

  it("can start further in, for a flow that already names the page", () => {
    const state = startFlow({ stages: ["meet", "agent"], position: 1 });
    expect(currentPage(state).id).toBe("agents");
    expect(() => startFlow({ stages: ["meet", "agent"], position: 5 })).toThrow(
      /past its 2 pages/,
    );
  });
});

describe("progress", () => {
  it("fills each bar with the page position inside its stage", () => {
    const state = advance(
      advance(startFlow({ stages: ["meet", "agent", "compute"] })),
      { linking: "codex" },
    );
    expect(currentPage(state).id).toBe("codex-sign-in");
    expect(progress(state)).toEqual([
      { stage: "meet", fill: 1, current: false },
      { stage: "agent", fill: 0.5, current: true },
      { stage: "compute", fill: 0, current: false },
    ]);
  });

  it("has one bar per stage the flow covers", () => {
    expect(progress(startFlow({ stages: ["compute"] }))).toEqual([
      { stage: "compute", fill: 0, current: true },
    ]);
  });
});
