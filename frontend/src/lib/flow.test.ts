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

const ids = (state: FlowState): PageId[] => pagesFor(state).map((page) => page.id);

describe("pagesFor", () => {
  it("walks the three stages, each opening with its one question", () => {
    expect(ids(startFlow(["meet", "agent", "compute"]))).toEqual([
      "meet",
      "agent-choice",
      "compute-choice",
    ]);
  });

  it("covers one stage alone for a flow opened from settings", () => {
    expect(ids(startFlow(["agent"]))).toEqual(["agent-choice"]);
    expect(ids(startFlow(["compute"]))).toEqual(["compute-choice"]);
  });

  it("appends Claude's sign-in and paste pages when Claude Code is chosen", () => {
    const state = advance(startFlow(["agent"]), { agent: "claude_code" });
    expect(ids(state)).toEqual(["agent-choice", "claude-sign-in", "claude-paste", "agent-linked"]);
    expect(currentPage(state).id).toBe("claude-sign-in");
  });

  it("appends Codex's one sign-in page when Codex is chosen", () => {
    const state = advance(startFlow(["agent"]), { agent: "codex" });
    expect(ids(state)).toEqual(["agent-choice", "codex-sign-in", "agent-linked"]);
  });

  it("swaps the sign-in's second page for the API-key page behind the link", () => {
    const claude = advance(advance(startFlow(["agent"]), { agent: "claude_code" }), {
      agentRoute: "api-key",
    });
    expect(ids(claude)).toEqual(["agent-choice", "claude-sign-in", "api-key", "agent-linked"]);
    expect(currentPage(claude)).toEqual({ id: "api-key", agent: "claude_code" });

    const codex = advance(advance(startFlow(["agent"]), { agent: "codex" }), {
      agentRoute: "api-key",
    });
    expect(ids(codex)).toEqual(["agent-choice", "codex-sign-in", "api-key", "agent-linked"]);
  });

  it("goes straight to the linked page for an agent that was linked already", () => {
    const state = advance(startFlow(["agent"]), { agent: "claude_code", agentAccount: CLAUDE });
    expect(ids(state)).toEqual(["agent-choice", "agent-linked"]);
    expect(currentPage(state).id).toBe("agent-linked");
  });

  it("collapses the stage to its choice and linked page once the agent is linked", () => {
    const paste = advance(advance(startFlow(["agent"]), { agent: "claude_code" }));
    expect(currentPage(paste).id).toBe("claude-paste");

    const linked = advance(paste, { agentAccount: CLAUDE });
    expect(ids(linked)).toEqual(["agent-choice", "agent-linked"]);
    expect(currentPage(linked).id).toBe("agent-linked");
    // Back from there is the choice, not a code that has been redeemed.
    expect(currentPage(back(linked)).id).toBe("agent-choice");
  });

  it("collapses stage C the same way once compute is linked", () => {
    const keys = advance(advance(advance(advance(startFlow(["compute"]), { compute: "aws" }), { newToProvider: false }), { student: false, programmes: [] }));
    expect(currentPage(keys).id).toBe("aws-keys");
    const linked = advance(keys, {
      computeAccount: { id: "acct", kind: "aws", label: "AWS", linked_at_unix: 0 },
    });
    expect(ids(linked)).toEqual(["compute-choice", "compute-linked"]);
    expect(currentPage(linked).id).toBe("compute-linked");
    expect(isFinished(advance(linked))).toBe(true);
  });

  it("asks a cloud provider the two bonus questions before its own pages", () => {
    expect(ids(advance(startFlow(["compute"]), { compute: "azure" }))).toEqual([
      "compute-choice",
      "new-to-provider",
      "student",
      "azure-command",
      "azure-paste",
      "azure-key",
      "compute-linked",
    ]);
    expect(ids(advance(startFlow(["compute"]), { compute: "aws" }))).toEqual([
      "compute-choice",
      "new-to-provider",
      "student",
      "aws-policy",
      "aws-keys",
      "compute-linked",
    ]);
    expect(ids(advance(startFlow(["compute"]), { compute: "gcp" }))).toEqual([
      "compute-choice",
      "new-to-provider",
      "student",
      "gcp-commands",
      "gcp-key-file",
      "compute-linked",
    ]);
  });

  it("names the provider on the bonus pages", () => {
    const state = advance(startFlow(["compute"]), { compute: "gcp" });
    expect(currentPage(state)).toEqual({ id: "new-to-provider", provider: "gcp" });
  });

  it("asks a machine the user owns no bonus questions at all", () => {
    expect(ids(advance(startFlow(["compute"]), { compute: "host" }))).toEqual([
      "compute-choice",
      "host-enroll",
      "compute-linked",
    ]);
  });

  it("adds the credit page only when a programme matched the chosen provider", () => {
    const azure = advance(startFlow(["compute"]), { compute: "azure" });
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
      "compute-linked",
    ]);
    // The page carries only this provider's programmes: the AWS one is
    // for a choice the user did not make.
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
    const pasted = advance(startFlow(["compute"]), {
      compute: "azure",
      azurePrincipal: {
        clientId: "app",
        clientSecret: "secret",
        tenantId: "tenant",
        subscriptionId: null,
      },
    });
    expect(ids(pasted)).toContain("azure-subscription");
    expect(ids(pasted).indexOf("azure-subscription")).toBe(ids(pasted).indexOf("azure-key") - 1);

    const complete = advance(startFlow(["compute"]), {
      compute: "azure",
      azurePrincipal: {
        clientId: "app",
        clientSecret: "secret",
        tenantId: "tenant",
        subscriptionId: "sub",
      },
    });
    expect(ids(complete)).not.toContain("azure-subscription");
  });
});

describe("advance and back", () => {
  it("keeps every answer on the way back", () => {
    const chosen = advance(startFlow(["compute"]), { compute: "azure" });
    const answered = advance(chosen, { newToProvider: true });
    expect(currentPage(answered).id).toBe("student");

    const returned = back(answered);
    expect(currentPage(returned).id).toBe("new-to-provider");
    expect(returned.answers.newToProvider).toBe(true);
    expect(returned.answers.compute).toBe("azure");

    // And back again to the choice, with the choice still made.
    expect(back(returned).answers.compute).toBe("azure");
    expect(currentPage(back(returned)).id).toBe("compute-choice");
  });

  it("refuses to go back from the first page", () => {
    expect(() => back(startFlow(["meet"]))).toThrow();
  });

  it("finishes when the last page advances", () => {
    const state = advance(startFlow(["meet"]));
    expect(isFinished(state)).toBe(true);
    expect(() => currentPage(state)).toThrow();
  });

  it("lands on the first page a new answer appended", () => {
    const state = advance(startFlow(["meet", "agent"]));
    expect(currentPage(state).id).toBe("agent-choice");
    const chosen = advance(state, { agent: "codex" });
    expect(currentPage(chosen).id).toBe("codex-sign-in");
  });

  it("records an answer without moving, and refuses one that removes the page", () => {
    const paste = advance(advance(startFlow(["compute"]), { compute: "azure" }), {
      newToProvider: true,
    });
    const kept = record(paste, { programmes: [AZURE_STUDENTS] });
    expect(currentPage(kept).id).toBe("student");
    expect(kept.answers.programmes).toEqual([AZURE_STUDENTS]);
    // Linking from the student page would remove it: not a recording.
    expect(() =>
      record(paste, { computeAccount: { id: "acct", kind: "azure", label: "Azure", linked_at_unix: 0 } }),
    ).toThrow();
  });

  it("can start further in, for a flow that already knows the agent", () => {
    const state = startFlow(["agent"], { agent: "codex" }, 1);
    expect(currentPage(state).id).toBe("codex-sign-in");
    expect(() => startFlow(["agent"], {}, 1)).toThrow();
  });
});

describe("progress", () => {
  it("fills each bar with the page position inside its stage", () => {
    const start = startFlow(["meet", "agent", "compute"]);
    expect(progress(start)).toEqual([
      { stage: "meet", fill: 0, current: true },
      { stage: "agent", fill: 0, current: false },
      { stage: "compute", fill: 0, current: false },
    ]);

    const paste = advance(advance(advance(start), { agent: "claude_code" }));
    expect(currentPage(paste).id).toBe("claude-paste");
    expect(progress(paste)).toEqual([
      { stage: "meet", fill: 1, current: false },
      { stage: "agent", fill: 0.5, current: true },
      { stage: "compute", fill: 0, current: false },
    ]);
  });

  it("has one bar per stage the flow covers", () => {
    expect(progress(startFlow(["compute"]))).toHaveLength(1);
  });
});
