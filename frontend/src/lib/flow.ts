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
  ProviderBonusHint,
  ProviderOauthChoice,
} from "../api/client";
import type { AzureServicePrincipal } from "./azureCredentials";
import type { BreakGlassKey } from "./sshKey";

/** The three stages of the first run; a settings-launched flow covers one. */
export type Stage = "meet" | "agent" | "compute";

/** How an agent is being linked, until it is. */
export type AgentRoute =
  /** The vendor's own sign-in: Anthropic's code, or OpenAI's device flow. */
  | "sign-in"
  /** The one-field page behind *Use an API key instead*. */
  | "api-key";

/** How a cloud with a consent screen is being linked. */
export type CloudRoute = "sign-in" | "cloud-shell";

/** The clouds whose own sign-in flyco can hand a link to. */
export type ConsentCloud = "azure" | "gcp";

/** What a vendor's consent screen handed back. */
export interface CloudConsent {
  readonly attemptId: string;
  /** Who signed in, as the vendor names them. */
  readonly account: string;
  /** What that account may link: subscriptions, or projects. */
  readonly choices: readonly ProviderOauthChoice[];
}

/** The accounts linked, by agent: before the flow opened, or during it. */
export type LinkedAgents = Readonly<
  Partial<Record<HarnessKind, HarnessAccountView>>
>;

/** Every answer the flow collects, in the order the pages ask for them. */
export interface FlowAnswers {
  /**
   * What is linked. The agent is never chosen here — a task chooses its
   * agent in the composer — so stage B is one page listing every agent
   * with its status, and an agent's sign-in pages follow it only while
   * that agent is the one being linked.
   */
  readonly agents: LinkedAgents;
  /** The agent chosen on the list, whose sign-in pages follow it. */
  readonly linking: HarnessKind | null;
  /** How each agent is being linked, until it is. */
  readonly routes: Readonly<Record<HarnessKind, AgentRoute>>;
  /**
   * The Claude sign-in that was opened, which the paste page redeems
   * against; the paste page exists only while there is one.
   */
  readonly claudeAttempt: ClaudeOauthStart | null;
  readonly compute: CloudProviderKind | null;
  /**
   * How Azure or Google Cloud is being linked: the vendor's own consent
   * screen, which is the road, or their terminal in the browser, which is
   * the quiet link for a tenant that blocks consent.
   */
  readonly cloudRoute: CloudRoute;
  /**
   * The consent that came back: who signed in and what they may link. The
   * choice page exists only while there is one.
   */
  readonly cloudConsent: CloudConsent | null;
  /** The subscription or project chosen from that consent. */
  readonly cloudChoice: string | null;
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
}

/** A flow that has asked nothing yet. */
export const NO_ANSWERS: FlowAnswers = {
  agents: {},
  linking: null,
  routes: { claude_code: "sign-in", codex: "sign-in" },
  claudeAttempt: null,
  compute: null,
  cloudRoute: "sign-in",
  cloudConsent: null,
  cloudChoice: null,
  newToProvider: null,
  student: null,
  programmes: [],
  azurePaste: "",
  azurePrincipal: null,
  azureSubscription: null,
  azureKey: null,
};

/** The accounts a readiness read found, keyed by agent, for `FlowAnswers.agents`. */
export function linkedAgents(
  accounts: readonly HarnessAccountView[],
): LinkedAgents {
  return Object.fromEntries(
    accounts.map((account) => [account.harness, account]),
  );
}

/** The agents flyco runs, in the order the agent stage links them. */
export const EVERY_AGENT: readonly HarnessKind[] = ["claude_code", "codex"];

/** Which stages this flow walks, where it is, and what it has been told. */
export interface FlowState {
  readonly stages: readonly Stage[];
  /**
   * Which agents the agent stage links, in order: every one on `/welcome`,
   * the one a settings card named when opened from there.
   */
  readonly agents: readonly HarnessKind[];
  /** Index into `pagesFor(state)`. */
  readonly position: number;
  readonly answers: FlowAnswers;
}

/** The cloud providers, which are every compute kind but a machine the user owns. */
export type CloudKind = Exclude<CloudProviderKind, "host">;

/** One page of the sequence, with whatever it needs beyond the answers. */
export type Page =
  | { readonly id: "meet" }
  | { readonly id: "agents" }
  | { readonly id: "claude-sign-in" }
  | { readonly id: "claude-paste" }
  | { readonly id: "codex-sign-in" }
  | { readonly id: "api-key"; readonly agent: HarnessKind }
  | { readonly id: "compute-choice" }
  | { readonly id: "new-to-provider"; readonly provider: CloudKind }
  | { readonly id: "student"; readonly provider: CloudKind }
  | {
      readonly id: "credit";
      readonly provider: CloudKind;
      readonly programmes: readonly ProviderBonusHint[];
    }
  | { readonly id: "cloud-sign-in"; readonly provider: ConsentCloud }
  | { readonly id: "cloud-choice"; readonly provider: ConsentCloud }
  | { readonly id: "azure-command" }
  | { readonly id: "azure-paste" }
  | { readonly id: "azure-subscription" }
  | { readonly id: "azure-key" }
  | { readonly id: "aws-policy" }
  | { readonly id: "aws-keys" }
  | { readonly id: "gcp-commands" }
  | { readonly id: "gcp-key-file" }
  | { readonly id: "host-enroll" };

export type PageId = Page["id"];

/** The stage a page belongs to. */
export function stageOf(page: Page): Stage {
  switch (page.id) {
    case "meet":
      return "meet";
    case "agents":
    case "claude-sign-in":
    case "claude-paste":
    case "codex-sign-in":
    case "api-key":
      return "agent";
    case "compute-choice":
    case "new-to-provider":
    case "student":
    case "credit":
    case "cloud-sign-in":
    case "cloud-choice":
    case "azure-command":
    case "azure-paste":
    case "azure-subscription":
    case "azure-key":
    case "aws-policy":
    case "aws-keys":
    case "gcp-commands":
    case "gcp-key-file":
    case "host-enroll":
      return "compute";
  }
}

/**
 * One agent's sign-in pages: the vendor's own sign-in, and behind it
 * whatever its route still needs — the paste page while a Claude sign-in
 * is open, the key page behind *Use an API key instead*.
 */
function agentPages(answers: FlowAnswers, agent: HarnessKind): Page[] {
  const signIn: Page =
    agent === "claude_code"
      ? { id: "claude-sign-in" }
      : { id: "codex-sign-in" };
  switch (answers.routes[agent]) {
    case "sign-in":
      return agent === "claude_code" && answers.claudeAttempt !== null
        ? [signIn, { id: "claude-paste" }]
        : [signIn];
    case "api-key":
      return [signIn, { id: "api-key", agent }];
  }
}

/**
 * Stage B's pages.
 *
 * One page lists every agent with its status, however many agents there
 * are; the sign-in pages of the one chosen there follow it until that
 * agent is linked, when `finishLink` returns to the list. A flow opened
 * for one agent by name — a settings card's `Connect` — has nothing to
 * list and walks that agent's sign-in pages alone.
 */
function agentStagePages(state: FlowState): Page[] {
  const only = state.agents.length === 1 ? state.agents[0] : undefined;
  if (only !== undefined) {
    return agentPages(state.answers, only);
  }
  const linking = state.answers.linking;
  return [
    { id: "agents" },
    ...(linking === null ? [] : agentPages(state.answers, linking)),
  ];
}

/** The programmes the quickstart matched for the chosen provider. */
export function matchingProgrammes(
  answers: FlowAnswers,
): readonly ProviderBonusHint[] {
  return answers.programmes.filter((hint) => hint.provider === answers.compute);
}

/**
 * The consent road for Azure or Google Cloud: the vendor's sign-in, then
 * the choice of what to link once the consent is back. Azure's key page
 * follows separately, because it belongs to both roads.
 */
function consentPages(answers: FlowAnswers, provider: ConsentCloud): Page[] {
  const pages: Page[] = [{ id: "cloud-sign-in", provider }];
  if (answers.cloudConsent !== null) {
    pages.push({ id: "cloud-choice", provider });
  }
  return pages;
}

/** One cloud provider's own pages, after the bonus questions. */
function providerPages(answers: FlowAnswers, provider: CloudKind): Page[] {
  switch (provider) {
    case "azure": {
      if (answers.cloudRoute === "sign-in") {
        return [...consentPages(answers, "azure"), { id: "azure-key" }];
      }
      const pages: Page[] = [{ id: "azure-command" }, { id: "azure-paste" }];
      // The CLI's default output names no subscription; the page that asks
      // for one exists only when the paste turned out to lack it.
      if (
        answers.azurePrincipal !== null &&
        answers.azurePrincipal.subscriptionId === null
      ) {
        pages.push({ id: "azure-subscription" });
      }
      pages.push({ id: "azure-key" });
      return pages;
    }
    case "aws":
      return [{ id: "aws-policy" }, { id: "aws-keys" }];
    case "gcp":
      return answers.cloudRoute === "sign-in"
        ? consentPages(answers, "gcp")
        : [{ id: "gcp-commands" }, { id: "gcp-key-file" }];
  }
}

/** Stage C's pages, given what has been answered. */
function computePages(answers: FlowAnswers): Page[] {
  const pages: Page[] = [{ id: "compute-choice" }];
  const provider = answers.compute;
  if (provider === null) {
    return pages;
  }
  if (provider === "host") {
    // No bonus questions: there is no free credit for hardware somebody
    // already bought, and "new to your own machine" has no answer.
    pages.push({ id: "host-enroll" });
  } else {
    pages.push(
      { id: "new-to-provider", provider },
      { id: "student", provider },
    );
    const programmes = matchingProgrammes(answers);
    if (programmes.length > 0) {
      pages.push({ id: "credit", provider, programmes });
    }
    pages.push(...providerPages(answers, provider));
  }
  return pages;
}

function stagePages(stage: Stage, state: FlowState): Page[] {
  switch (stage) {
    case "meet":
      return [{ id: "meet" }];
    case "agent":
      return agentStagePages(state);
    case "compute":
      return computePages(state.answers);
  }
}

/** The pages the flow will walk, as far as the answers so far determine them. */
export function pagesFor(state: FlowState): Page[] {
  return state.stages.flatMap((stage) => stagePages(stage, state));
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
    throw new Error(
      `flow position ${state.position} is past its ${pagesFor(state).length} pages`,
    );
  }
  return page;
}

/** What a flow is opened with. */
export interface FlowStart {
  readonly stages: readonly Stage[];
  /** Which agents the agent stage links; every one unless a caller names one. */
  readonly agents?: readonly HarnessKind[] | undefined;
  /** Answers known before the first page: above all, what is linked already. */
  readonly answers?: Partial<FlowAnswers> | undefined;
  /** Where to start, when the answers above make the first pages moot. */
  readonly position?: number | undefined;
}

/** A flow at its first page, or at `position` when a caller starts it further in. */
export function startFlow(start: FlowStart): FlowState {
  if (start.stages.length === 0) {
    throw new Error("a flow needs at least one stage");
  }
  const agents = start.agents ?? EVERY_AGENT;
  if (start.stages.includes("agent") && agents.length === 0) {
    throw new Error("an agent stage needs at least one agent to link");
  }
  const state: FlowState = {
    stages: start.stages,
    agents,
    position: start.position ?? 0,
    answers: { ...NO_ANSWERS, ...start.answers },
  };
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
 * an answer that removes pages (a link, which collapses an agent's pages
 * to its one page; a declined agent, which drops its paste page) lands one
 * past the nearest page before it that still exists. Positions are never carried across a change in the
 * sequence, only pages are.
 */
function settle(
  state: FlowState,
  answers: Partial<FlowAnswers>,
  forward: boolean,
): FlowState {
  const next: FlowState = {
    ...state,
    answers: { ...state.answers, ...answers },
  };
  const before = pagesFor(state);
  const after = pagesFor(next);
  for (let index = state.position; index >= 0; index -= 1) {
    const page = before[index] as Page;
    const found = after.findIndex((candidate) => candidate.id === page.id);
    if (found === -1) {
      if (!forward) {
        throw new Error(
          `recording an answer removed the ${page.id} page it was given on`,
        );
      }
      continue;
    }
    return { ...next, position: forward ? found + 1 : found };
  }
  throw new Error("no page of the sequence survived the answer");
}

/** Records answers and moves one page on. */
export function advance(
  state: FlowState,
  answers: Partial<FlowAnswers> = {},
): FlowState {
  return settle(state, answers, true);
}

/**
 * Records answers without moving: for what a page has to keep even if it
 * is left by `Back` — a key it minted, above all.
 */
export function record(
  state: FlowState,
  answers: Partial<FlowAnswers>,
): FlowState {
  return settle(state, answers, false);
}

/**
 * An agent got linked: its sign-in is over and its pages go.
 *
 * The flow returns to the list the agent was chosen on, which now reads
 * `Linked` beside it. A flow with no list — one opened for this agent by
 * name — has nothing to return to and moves on, which finishes it.
 */
export function finishLink(
  state: FlowState,
  agent: HarnessKind,
  account: HarnessAccountView,
): FlowState {
  const answers: Partial<FlowAnswers> = {
    agents: { ...state.answers.agents, [agent]: account },
    linking: null,
    claudeAttempt: null,
    routes: { ...state.answers.routes, [agent]: "sign-in" },
  };
  const next: FlowState = {
    ...state,
    answers: { ...state.answers, ...answers },
  };
  const list = pagesFor(next).findIndex((page) => page.id === "agents");
  return list === -1
    ? settle(state, answers, true)
    : { ...next, position: list };
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
