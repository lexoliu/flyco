/**
 * The first-run sequence, as data (docs/ux.md §4).
 *
 * The pages are a list computed from the answers so far rather than a tree
 * of components nested inside one another: choosing Azure appends Azure's
 * pages, answering the student question appends a credit page only when a
 * programme matched, and a pasted Azure block with no subscription in it
 * appends the one page that asks for it. `Back` goes one page back and
 * keeps every answer, so a page that is returned to shows what it showed
 * when it was left.
 *
 * Nothing here renders or fetches. The frame (`components/flow/Flow.tsx`)
 * holds one `FlowState`, asks this module which page is current and how far
 * along each stage is, and hands the page component `advance` — which is
 * how every answer gets in.
 */
import type {
  ClaudeOauthStart,
  CloudProviderKind,
  HarnessAccountView,
  HarnessKind,
  HostView,
  ProviderAccountView,
  ProviderBonusHint,
} from "../api/client";
import type { AzureServicePrincipal } from "./azureCredentials";
import type { BreakGlassKey } from "./sshKey";

/** The three stages of the first run; a settings-launched flow covers one. */
export type Stage = "meet" | "agent" | "compute";

/** How the chosen agent is being linked, until it is. */
export type AgentRoute =
  /** The vendor's own sign-in: Anthropic's code, or OpenAI's device flow. */
  | "sign-in"
  /** The one-field page behind *Use an API key instead*. */
  | "api-key";

/** Every answer the flow collects, in the order the pages ask for them. */
export interface FlowAnswers {
  readonly agent: HarnessKind | null;
  readonly agentRoute: AgentRoute;
  /** The Claude sign-in that was opened, which the paste page redeems against. */
  readonly claudeAttempt: ClaudeOauthStart | null;
  /**
   * The agent account, once one is linked or was found already linked.
   *
   * Set, it collapses stage B to the choice and the linked page: there is
   * nothing to sign in to twice, and `Back` from the linked page lands on
   * the choice rather than on a code that has already been redeemed.
   */
  readonly agentAccount: HarnessAccountView | null;
  readonly compute: CloudProviderKind | null;
  readonly newToProvider: boolean | null;
  readonly student: boolean | null;
  /** What `POST /v1/providers/quickstart` matched, for every provider it named. */
  readonly programmes: readonly ProviderBonusHint[];
  /** The pasted Azure block, as pasted, so the page shows it again on `Back`. */
  readonly azurePaste: string;
  /** The service principal read out of that block. */
  readonly azurePrincipal: AzureServicePrincipal | null;
  /** The subscription typed in when the pasted block carried none. */
  readonly azureSubscription: string | null;
  /**
   * The break-glass key minted for Azure, kept so that `Back` and forward
   * again shows the same key the user may already have downloaded.
   */
  readonly azureKey: BreakGlassKey | null;
  /** The cloud account, once linked; collapses stage C like `agentAccount`. */
  readonly computeAccount: ProviderAccountView | null;
  /** The machine, once it enrolled; likewise. */
  readonly host: HostView | null;
}

/** A flow that has asked nothing yet. */
export const NO_ANSWERS: FlowAnswers = {
  agent: null,
  agentRoute: "sign-in",
  claudeAttempt: null,
  agentAccount: null,
  compute: null,
  newToProvider: null,
  student: null,
  programmes: [],
  azurePaste: "",
  azurePrincipal: null,
  azureSubscription: null,
  azureKey: null,
  computeAccount: null,
  host: null,
};

/** Which stages this flow walks, where it is, and what it has been told. */
export interface FlowState {
  readonly stages: readonly Stage[];
  /** Index into `pagesFor(state)`. */
  readonly position: number;
  readonly answers: FlowAnswers;
}

/** The cloud providers, which are every compute kind but a machine the user owns. */
export type CloudKind = Exclude<CloudProviderKind, "host">;

/** One page of the sequence, with whatever it needs beyond the answers. */
export type Page =
  | { readonly id: "meet" }
  | { readonly id: "agent-choice" }
  | { readonly id: "claude-sign-in" }
  | { readonly id: "claude-paste" }
  | { readonly id: "codex-sign-in" }
  | { readonly id: "api-key"; readonly agent: HarnessKind }
  | { readonly id: "agent-linked" }
  | { readonly id: "compute-choice" }
  | { readonly id: "new-to-provider"; readonly provider: CloudKind }
  | { readonly id: "student"; readonly provider: CloudKind }
  | { readonly id: "credit"; readonly provider: CloudKind; readonly programmes: readonly ProviderBonusHint[] }
  | { readonly id: "azure-command" }
  | { readonly id: "azure-paste" }
  | { readonly id: "azure-subscription" }
  | { readonly id: "azure-key" }
  | { readonly id: "aws-policy" }
  | { readonly id: "aws-keys" }
  | { readonly id: "gcp-commands" }
  | { readonly id: "gcp-key-file" }
  | { readonly id: "host-enroll" }
  | { readonly id: "compute-linked" };

export type PageId = Page["id"];

/** The stage a page belongs to. */
export function stageOf(page: Page): Stage {
  switch (page.id) {
    case "meet":
      return "meet";
    case "agent-choice":
    case "claude-sign-in":
    case "claude-paste":
    case "codex-sign-in":
    case "api-key":
    case "agent-linked":
      return "agent";
    case "compute-choice":
    case "new-to-provider":
    case "student":
    case "credit":
    case "azure-command":
    case "azure-paste":
    case "azure-subscription":
    case "azure-key":
    case "aws-policy":
    case "aws-keys":
    case "gcp-commands":
    case "gcp-key-file":
    case "host-enroll":
    case "compute-linked":
      return "compute";
  }
}

/** Stage B's pages, given what has been answered. */
function agentPages(answers: FlowAnswers): Page[] {
  const pages: Page[] = [{ id: "agent-choice" }];
  if (answers.agent === null) {
    return pages;
  }
  if (answers.agentAccount !== null) {
    return [...pages, { id: "agent-linked" }];
  }
  const signIn: Page = answers.agent === "claude_code" ? { id: "claude-sign-in" } : { id: "codex-sign-in" };
  switch (answers.agentRoute) {
    case "sign-in":
      pages.push(signIn);
      if (answers.agent === "claude_code") {
        pages.push({ id: "claude-paste" });
      }
      break;
    case "api-key":
      pages.push(signIn, { id: "api-key", agent: answers.agent });
      break;
  }
  pages.push({ id: "agent-linked" });
  return pages;
}

/** The programmes the quickstart matched for the chosen provider. */
export function matchingProgrammes(answers: FlowAnswers): readonly ProviderBonusHint[] {
  return answers.programmes.filter((hint) => hint.provider === answers.compute);
}

/** One cloud provider's own pages, after the bonus questions. */
function providerPages(answers: FlowAnswers, provider: CloudKind): Page[] {
  switch (provider) {
    case "azure": {
      const pages: Page[] = [{ id: "azure-command" }, { id: "azure-paste" }];
      // The CLI's default output names no subscription; the page that asks
      // for one exists only when the paste turned out to lack it.
      if (answers.azurePrincipal !== null && answers.azurePrincipal.subscriptionId === null) {
        pages.push({ id: "azure-subscription" });
      }
      pages.push({ id: "azure-key" });
      return pages;
    }
    case "aws":
      return [{ id: "aws-policy" }, { id: "aws-keys" }];
    case "gcp":
      return [{ id: "gcp-commands" }, { id: "gcp-key-file" }];
  }
}

/** Stage C's pages, given what has been answered. */
function computePages(answers: FlowAnswers): Page[] {
  const pages: Page[] = [{ id: "compute-choice" }];
  const provider = answers.compute;
  if (provider === null) {
    return pages;
  }
  if (answers.computeAccount !== null || answers.host !== null) {
    return [...pages, { id: "compute-linked" }];
  }
  if (provider === "host") {
    // No bonus questions: there is no free credit for hardware somebody
    // already bought, and "new to your own machine" has no answer.
    pages.push({ id: "host-enroll" });
  } else {
    pages.push({ id: "new-to-provider", provider }, { id: "student", provider });
    const programmes = matchingProgrammes(answers);
    if (programmes.length > 0) {
      pages.push({ id: "credit", provider, programmes });
    }
    pages.push(...providerPages(answers, provider));
  }
  pages.push({ id: "compute-linked" });
  return pages;
}

function stagePages(stage: Stage, answers: FlowAnswers): Page[] {
  switch (stage) {
    case "meet":
      return [{ id: "meet" }];
    case "agent":
      return agentPages(answers);
    case "compute":
      return computePages(answers);
  }
}

/** The pages the flow will walk, as far as the answers so far determine them. */
export function pagesFor(state: FlowState): Page[] {
  return state.stages.flatMap((stage) => stagePages(stage, state.answers));
}

/**
 * The page on screen.
 *
 * Fast fail: a position past the end is a bug in whoever advanced — the
 * frame finishes the flow instead of advancing off its last page.
 */
export function currentPage(state: FlowState): Page {
  const page = pagesFor(state)[state.position];
  if (page === undefined) {
    throw new Error(`flow position ${state.position} is past its ${pagesFor(state).length} pages`);
  }
  return page;
}

/** A flow at its first page, or at `position` when a caller starts it further in. */
export function startFlow(
  stages: readonly Stage[],
  answers: Partial<FlowAnswers> = {},
  position = 0,
): FlowState {
  if (stages.length === 0) {
    throw new Error("a flow needs at least one stage");
  }
  const state: FlowState = { stages, position, answers: { ...NO_ANSWERS, ...answers } };
  // Reading the page is the check: it throws when `position` is off the list.
  currentPage(state);
  return state;
}

/**
 * Where the flow stands once `answers` are merged in.
 *
 * The page on screen is found again in the recomputed sequence, so an
 * answer that appends pages (the provider choice, the student question, a
 * paste with no subscription) lands one past the page that gave it — and
 * an answer that removes pages (a link, which collapses the stage to its
 * choice and its linked page) lands one past the nearest page before it
 * that still exists. Positions are never carried across a change in the
 * sequence, only pages are.
 */
function settle(state: FlowState, answers: Partial<FlowAnswers>, forward: boolean): FlowState {
  const next: FlowState = { ...state, answers: { ...state.answers, ...answers } };
  const before = pagesFor(state);
  const after = pagesFor(next);
  for (let index = state.position; index >= 0; index -= 1) {
    const page = before[index] as Page;
    const found = after.findIndex((candidate) => candidate.id === page.id);
    if (found === -1) {
      if (!forward) {
        throw new Error(`recording an answer removed the ${page.id} page it was given on`);
      }
      continue;
    }
    return { ...next, position: forward ? found + 1 : found };
  }
  throw new Error("no page of the sequence survived the answer");
}

/** Records answers and moves one page on. */
export function advance(state: FlowState, answers: Partial<FlowAnswers> = {}): FlowState {
  return settle(state, answers, true);
}

/**
 * Records answers without moving: for what a page has to keep even if it
 * is left by `Back` — a key it minted, above all.
 */
export function record(state: FlowState, answers: Partial<FlowAnswers>): FlowState {
  return settle(state, answers, false);
}

/** Whether `advance` walked off the end, which is how the flow finishes. */
export function isFinished(state: FlowState): boolean {
  return state.position >= pagesFor(state).length;
}

/** One page back, answers intact. Fast fail from the first page. */
export function back(state: FlowState): FlowState {
  if (state.position === 0) {
    throw new Error("cannot go back from the first page");
  }
  return { ...state, position: state.position - 1 };
}

/** How far along one stage's bar is. */
export interface StageProgress {
  readonly stage: Stage;
  /** 0 before the stage, 1 after it, and the page position inside it while on it. */
  readonly fill: number;
  readonly current: boolean;
}

/** The three bars (or however many stages this flow covers). */
export function progress(state: FlowState): StageProgress[] {
  const pages = pagesFor(state);
  const current = stageOf(currentPage(state));
  const currentIndex = state.stages.indexOf(current);
  return state.stages.map((stage, index) => {
    if (index < currentIndex) {
      return { stage, fill: 1, current: false };
    }
    if (index > currentIndex) {
      return { stage, fill: 0, current: false };
    }
    const own = pages.filter((page) => stageOf(page) === stage);
    const at = own.indexOf(pages[state.position] as Page);
    return { stage, fill: at / own.length, current: true };
  });
}
