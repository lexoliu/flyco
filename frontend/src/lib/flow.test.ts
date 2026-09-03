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
};

const CODEX: HarnessAccountView = {
  id: "harness-3",
  harness: "codex",
  label: "me@lexo.cool",
  linked_at_unix: 1_787_000_000,
  expires_at_unix: null,
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

const ids = (state: FlowState): PageId[] =>
  pagesFor(state).map((page) => page.id);

describe("pagesFor", () => {
  it("walks the three stages: meet, one page per agent, the compute question", () => {
    const state = startFlow({ stages: ["meet", "agent", "compute"] });
    expect(ids(state)).toEqual([
      "meet",
      "claude-sign-in",
      "codex-sign-in",
      "compute-choice",
    ]);
    expect(currentPage(state)).toEqual({ id: "meet" });
  });

  it("covers one stage alone for a flow opened from settings, and one agent when named", () => {
    expect(ids(startFlow({ stages: ["agent"] }))).toEqual([
      "claude-sign-in",
      "codex-sign-in",
    ]);
    expect(ids(startFlow({ stages: ["agent"], agents: ["codex"] }))).toEqual([
      "codex-sign-in",
    ]);
    expect(ids(startFlow({ stages: ["compute"] }))).toEqual(["compute-choice"]);
  });

  it("refuses an agent stage with no agent to link", () => {
    expect(() => startFlow({ stages: ["agent"], agents: [] })).toThrow(
      /at least one agent/,
    );
  });

  it("never asks which agent: the agent stage links agents, one page each", () => {
    expect(ids(startFlow({ stages: ["agent"] }))).not.toContain("agent-choice");
  });

  it("adds Claude's paste page only while a sign-in is open", () => {
    const state = startFlow({ stages: ["agent"] });
    expect(ids(state)).toEqual(["claude-sign-in", "codex-sign-in"]);
    const opened = advance(state, { claudeAttempt: ATTEMPT });
    expect(ids(opened)).toEqual([
      "claude-sign-in",
      "claude-paste",
      "codex-sign-in",
    ]);
    expect(currentPage(opened).id).toBe("claude-paste");
  });

  it("goes straight to Codex when Claude Code is declined, dropping an open sign-in", () => {
    const opened = advance(startFlow({ stages: ["agent"] }), {
      claudeAttempt: ATTEMPT,
    });
    const declined = advance(back(opened), { claudeAttempt: null });
    expect(ids(declined)).toEqual(["claude-sign-in", "codex-sign-in"]);
    expect(currentPage(declined).id).toBe("codex-sign-in");
  });

  it("swaps the sign-in's second page for the API-key page behind the link", () => {
    const claude = advance(startFlow({ stages: ["agent"] }), {
      routes: { claude_code: "api-key", codex: "sign-in" },
    });
    expect(ids(claude)).toEqual(["claude-sign-in", "api-key", "codex-sign-in"]);
    expect(currentPage(claude)).toEqual({
      id: "api-key",
      agent: "claude_code",
    });

    const codex = advance(startFlow({ stages: ["agent"], agents: ["codex"] }), {
      routes: { claude_code: "sign-in", codex: "api-key" },
    });
    expect(ids(codex)).toEqual(["codex-sign-in", "api-key"]);
    expect(currentPage(codex)).toEqual({ id: "api-key", agent: "codex" });
  });

  it("collapses a linked agent to its one page, which is where a link lands after", () => {
    const opened = advance(startFlow({ stages: ["agent"] }), {
      claudeAttempt: ATTEMPT,
    });
    const linked = advance(opened, { agents: { claude_code: CLAUDE } });
    expect(ids(linked)).toEqual(["claude-sign-in", "codex-sign-in"]);
    expect(currentPage(linked).id).toBe("codex-sign-in");
    expect(currentPage(back(linked)).id).toBe("claude-sign-in");
  });

  it("finishes the agent stage when the last agent links, with no page after it", () => {
    const state = startFlow({
      stages: ["agent"],
      answers: { agents: { claude_code: CLAUDE } },
    });
    const codexPage = advance(state);
    expect(currentPage(codexPage).id).toBe("codex-sign-in");
    expect(
      isFinished(
        advance(codexPage, { agents: { claude_code: CLAUDE, codex: CODEX } }),
      ),
    ).toBe(true);
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
      "azure-command",
      "azure-paste",
      "azure-key",
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
      "azure-command",
      "azure-paste",
      "azure-key",
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
    expect(currentPage(nothing).id).toBe("azure-command");
  });

  it("asks for the subscription only when the pasted Azure block lacked one", () => {
    const paste = advance(
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
    expect(currentPage(complete).id).toBe("azure-key");
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
    const opened = advance(startFlow({ stages: ["agent"] }), {
      claudeAttempt: ATTEMPT,
    });
    const kept = record(opened, { azurePaste: "kept" });
    expect(currentPage(kept).id).toBe("claude-paste");
    expect(kept.answers.azurePaste).toBe("kept");
    expect(() => record(opened, { claudeAttempt: null })).toThrow(
      /removed the claude-paste page/,
    );
  });

  it("can start further in, for a flow that already names the page", () => {
    const state = startFlow({ stages: ["agent"], position: 1 });
    expect(currentPage(state).id).toBe("codex-sign-in");
    expect(() => startFlow({ stages: ["agent"], position: 5 })).toThrow(
      /past its 2 pages/,
    );
  });
});

describe("progress", () => {
  it("fills each bar with the page position inside its stage", () => {
    const state = advance(
      advance(startFlow({ stages: ["meet", "agent", "compute"] })),
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
