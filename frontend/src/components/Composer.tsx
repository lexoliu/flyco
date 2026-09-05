/**
 * The composer: the one primary control on the home page.
 *
 * A textarea, four chips, and a send button in one rounded container. Each
 * chip is both a readout of a decision and the way to change it (docs/ux.md
 * §5), so the page never has a settings screen standing between the user and
 * starting work — and the user types once: the prompt goes out with the
 * session and becomes its first message.
 *
 * Send is enabled only when there is something to say and all three
 * prerequisites are present. A disabled button that does not say why is a
 * dead end, so the tooltip names the missing one.
 */
import { For, Show, createEffect, createMemo, createSignal, onCleanup } from "solid-js";
import { createQuery } from "../lib/query";
import { A } from "@solidjs/router";
import {
  AlertTriangle,
  ArrowUp,
  FolderGit2,
  GitBranch,
  Plus,
  Server,
  Wallet,
} from "lucide-solid";
import BudgetPicker, { DEFAULT_BUDGET } from "./BudgetPicker";
import ComposerShell from "./ComposerShell";
import MachinePicker from "./MachinePicker";
import Popover from "./Popover";
import Logomark, { HARNESS_MARK, PROVIDER_MARK } from "./Logomark";
import ProblemNotice from "./ProblemNotice";
import { useReadiness } from "./Readiness";
import {
  getDefaultMachine,
  getMachineCatalog,
  listBranches,
  listRepos,
  type HarnessKind,
  type MachineCatalogEntry,
  type MachineDefault,
  type ProviderAccountView,
  type RepoSummary,
} from "../api/client";
import type { NewSessionInput } from "../api/sessions";
import { cx } from "../lib/cx";
import {
  MAX_RECENT_REPOS,
  recentRepos,
  rememberRepo,
  setSpotPreference,
  spotPreference,
} from "../lib/localPreferences";
import {
  CATALOG_POLL_SECONDS,
  billingMinimumSentence,
  catalogNotReady,
  entryKey,
  hourlyLabel,
  readingMachines,
} from "../lib/machines";
import { PROVIDER_LABEL } from "../lib/providers";
import styles from "./Composer.module.css";

const HARNESS_LABEL: Record<HarnessKind, string> = {
  claude_code: "Claude Code",
  codex: "Codex",
};

export interface ComposerProps {
  /** Starts the session. Rejections surface as a notice under the chips. */
  onSend: (input: NewSessionInput) => Promise<void>;
}

export default function Composer(props: ComposerProps) {
  const readiness = useReadiness();

  const [prompt, setPrompt] = createSignal("");
  const [repo, setRepo] = createSignal<string | null>(recentRepos()[0] ?? null);
  // `null` is "whatever this repository's default branch is", which the
  // control plane resolves and records. Storing the choice rather than the
  // resolved name is what keeps the two from disagreeing when the repository
  // changes under it.
  const [branch, setBranch] = createSignal<string | null>(null);
  const [budget, setBudget] = createSignal(DEFAULT_BUDGET);
  const [spot, setSpot] = createSignal(spotPreference());
  const [chosenKey, setChosenKey] = createSignal<string | null>(null);
  const [sending, setSending] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  // The machine flyco would pick, re-asked when the spot preference moves:
  // spot changes both the price and, potentially, which type is cheapest.
  //
  // Neither request is made until compute is linked, for the same reason
  // the shell does not load readiness while signed out (see
  // components/Readiness.tsx): with no account there is no catalog to merge
  // and no default to name, so asking produces an error whose only possible
  // rendering is "link an account" — which the chip already says.
  const [automatic, { refetch: reaskDefault }] = createQuery(
    () => (readiness.compute().length > 0 ? spot() : undefined),
    (wanted: boolean) => getDefaultMachine(wanted),
  );
  // The whole curated catalog, not just Linux: the slider's `Advanced`
  // disclosure offers architecture and OS, and curation groups by both — so
  // filtering here is exactly what filtering on the server would have done,
  // one request instead of one per combination.
  const [catalog, { refetch: reaskCatalog }] = createQuery(
    () => (readiness.compute().length > 0 ? true : undefined),
    () => getMachineCatalog(),
  );

  /** The entries every account that has been read can offer. */
  const entries = createMemo<MachineCatalogEntry[]>(() => catalog()?.entries ?? []);

  /**
   * The linked accounts flyco has not finished reading, as their vendors.
   *
   * Three things mean the same wait and are said the same way: the catalog
   * naming accounts it has not read, the first request still being in
   * flight, and `GET /v1/machines/default` refusing with `catalog-not-ready`
   * because every account is still being read.
   */
  const pendingKinds = createMemo(() => {
    const linked = readiness.compute();
    if (linked.length === 0) {
      return [];
    }
    const answered = catalog();
    if (answered === undefined) {
      return catalog.error === undefined ? linked.map((account) => account.kind) : [];
    }
    const waiting = new Set(answered.pending_accounts);
    const named = linked.filter((account) => waiting.has(account.id));
    return named.length > 0 || !catalogNotReady(automatic.error)
      ? named.map((account) => account.kind)
      : linked.map((account) => account.kind);
  });

  /** The one sentence a pending catalog shows, or `null` once it is read. */
  const pending = createMemo(() =>
    pendingKinds().length === 0 ? null : readingMachines(pendingKinds()),
  );

  // A pending catalog ends by itself, seconds later, so the screen asks
  // again rather than making the user reload. The interval exists only
  // while something is actually pending.
  createEffect(() => {
    if (pending() === null) {
      return;
    }
    const timer = setInterval(() => {
      void reaskCatalog();
      void reaskDefault();
    }, CATALOG_POLL_SECONDS * 1000);
    onCleanup(() => clearInterval(timer));
  });

  /**
   * Why there is no machine, when that is a failure.
   *
   * `catalog-not-ready` is deliberately not one: it is the wait, it is
   * already on screen as a sentence, and rendering it as a problem would
   * tell the user something is wrong when nothing is.
   */
  const machineError = createMemo(() => {
    const failure = catalog.error ?? automatic.error;
    return catalogNotReady(failure) ? undefined : failure;
  });

  /** The harness a session opens on: the one linked account, or Claude. */
  const harness = createMemo<HarnessKind>(() => readiness.harness()[0]?.harness ?? "claude_code");

  /** The catalog entry the compute chip is showing. */
  const chosen = createMemo<MachineCatalogEntry | undefined>(() => {
    const key = chosenKey();
    if (key === null) {
      return automatic()?.entry;
    }
    return entries().find((entry) => entryKey(entry) === key);
  });

  /**
   * The sentence a license-bound machine has to state before it can be
   * started, from the entry the chip is showing.
   */
  const licenseNotice = createMemo(() => {
    const entry = chosen();
    return entry === undefined ? null : billingMinimumSentence(entry);
  });

  /** What stops this session from being started, in the order to fix it. */
  const blocker = createMemo<string | null>(() => {
    if (readiness.harness().length === 0) {
      return "Connect an agent first";
    }
    if (readiness.compute().length === 0) {
      return "Add compute first";
    }
    if (repo() === null) {
      return "Pick a repository first";
    }
    if (prompt().trim() === "") {
      return "Describe a task first";
    }
    return null;
  });

  function chooseRepo(slug: string): void {
    setRepo(slug);
    // A branch belongs to a repository. Carrying `dev` across to a
    // repository that has no `dev` would fail the clone minutes later, on a
    // machine, with a git error — so the choice is dropped and the new
    // repository's default takes over.
    setBranch(null);
    rememberRepo(slug);
  }

  /** Remembered, because it is a default for the next session too. */
  function chooseSpot(next: boolean): void {
    setSpot(next);
    setSpotPreference(next);
  }

  async function send(): Promise<void> {
    const slug = repo();
    if (blocker() !== null || slug === null) {
      return;
    }
    setSending(true);
    setError(null);
    try {
      const entry = chosen();
      const account = entry?.account;
      const chosenBranch = branch();
      await props.onSend({
        prompt: prompt().trim(),
        repo: slug,
        // Sent only when the user picked one: omitted, the control plane
        // reads the repository's default from GitHub and records *that*, so
        // the branch a session is on is never this browser's guess.
        ...(chosenBranch === null ? {} : { branch: chosenBranch }),
        harness: harness(),
        budgetLimitDollars: budget(),
        // An explicit machine is only sent when the user picked one: that
        // is what makes the session's `machine_origin` say `user`.
        ...(chosenKey() !== null && entry !== undefined && account !== null && account !== undefined
          ? {
              machine: {
                providerAccount: account,
                machineType: entry.machine_type,
                region: entry.region,
                spot: spot(),
              },
            }
          : { spot: spot() }),
      });
      setPrompt("");
    } catch (failure) {
      setError(failure);
    } finally {
      setSending(false);
    }
  }

  return (
    <ComposerShell
      value={prompt()}
      onInput={setPrompt}
      onSubmit={() => void send()}
      placeholder="Describe a task"
      label="Describe a task"
      // ⌘/Ctrl+Enter sends; plain Enter is a newline, because a prompt is
      // prose and prose has paragraphs.
      submitOn="mod-enter"
      controls={
        <div class={styles.chips}>
          <HarnessChip harness={harness()} linked={readiness.harness().length > 0} />
          <ComputeChip
            linked={readiness.compute().length > 0}
            accounts={readiness.compute()}
            automatic={automatic()}
            entry={chosen()}
            catalog={entries()}
            error={machineError()}
            pending={pending()}
            chosenKey={chosenKey()}
            spot={spot()}
            onChoose={setChosenKey}
            onSpot={chooseSpot}
          />
          <RepoChip slug={repo()} onChoose={chooseRepo} />
          <BranchChip slug={repo()} branch={branch()} onChoose={setBranch} />
          <BudgetChip dollars={budget()} onChange={setBudget} />
        </div>
      }
      action={
        <button
          type="button"
          class={styles.send}
          disabled={blocker() !== null || sending()}
          title={blocker() ?? "Start session"}
          aria-label={blocker() ?? "Start session"}
          onClick={() => void send()}
        >
          <ArrowUp size={16} aria-hidden="true" />
        </button>
      }
    >
      <Show when={licenseNotice()}>
        {(sentence) => (
          <p class={styles.licence}>
            <AlertTriangle size={14} aria-hidden="true" />
            {sentence()}
          </p>
        )}
      </Show>
      <ProblemNotice error={error()} />
    </ComposerShell>
  );
}

/** Which agent will drive the session, or the way to link one. */
function HarnessChip(props: { harness: HarnessKind; linked: boolean }) {
  return (
    <Show
      when={props.linked}
      fallback={
        <A href="/connect/harness" class={cx(styles.chip, styles.chipMissing)}>
          <Plus size={13} aria-hidden="true" />
          Connect an agent
        </A>
      }
    >
      <A href="/connect/harness" class={styles.chip}>
        <Logomark mark={HARNESS_MARK[props.harness]} size={13} />
        <span class={styles.chipLabel}>{HARNESS_LABEL[props.harness]}</span>
      </A>
    </Show>
  );
}

/**
 * The machine flyco will actually use — not a dropdown of the catalog.
 *
 * The chip shows the answer `GET /v1/machines/default` gives, and the
 * popover is where that answer can be overridden. Picking a type here is
 * what makes the session's machine "chosen by you", which a later issue
 * uses to keep the agent from resizing out from under the user.
 */
function ComputeChip(props: {
  linked: boolean;
  accounts: ProviderAccountView[];
  automatic: MachineDefault | undefined;
  entry: MachineCatalogEntry | undefined;
  catalog: MachineCatalogEntry[];
  /** Why the catalog or the automatic pick is missing, when either failed. */
  error: unknown;
  /**
   * What flyco is still reading, when it is still reading something.
   *
   * The chip and the picker both say it. Until an account has been read it
   * offers no machine, no region and no architecture, and a picker that
   * showed empty selects and "this account offers no machine anywhere"
   * would be stating a fact nobody has established yet.
   */
  pending: string | null;
  chosenKey: string | null;
  spot: boolean;
  onChoose: (key: string | null) => void;
  onSpot: (spot: boolean) => void;
}) {
  const summary = createMemo(() => {
    const entry = props.entry;
    if (entry === undefined) {
      return null;
    }
    const parts = [
      PROVIDER_LABEL[entry.provider],
      entry.region,
      entry.machine_type,
      hourlyLabel(entry, props.spot),
    ];
    if (props.spot && entry.pricing.kind === "metered") {
      parts.push("spot");
    }
    return parts.join(" · ");
  });

  /** The sentence a license-bound machine has to show before send. */
  const warning = createMemo(() =>
    props.entry === undefined ? null : billingMinimumSentence(props.entry),
  );

  return (
    <Show
      when={props.linked}
      fallback={
        <A href="/connect/compute" class={cx(styles.chip, styles.chipMissing)}>
          <Plus size={13} aria-hidden="true" />
          Add compute
        </A>
      }
    >
      <Popover
        label="Compute"
        panelClass={styles.popoverWide}
        trigger={(attrs) => (
          <button
            id={attrs.id}
            onClick={attrs.onClick}
            aria-expanded={attrs.expanded()}
            aria-haspopup="dialog"
            type="button"
            class={cx(styles.chip, warning() !== null && styles.chipBound)}
          >
            <Show
              when={props.entry !== undefined && PROVIDER_MARK[props.entry.provider]}
              fallback={<Server size={13} aria-hidden="true" />}
            >
              {(mark) => <Logomark mark={mark()} size={13} />}
            </Show>
            <span class={styles.chipLabel}>
              {summary() ?? props.pending ?? "Choosing a machine\u2026"}
            </span>
            <span class={styles.chipDim}>
              {props.chosenKey === null ? "Auto" : "Chosen by you"}
            </span>
          </button>
        )}
      >
        {() => (
          <MachinePicker
            catalog={props.catalog}
            accounts={props.accounts}
            automatic={props.automatic}
            spot={props.spot}
            chosenKey={props.chosenKey}
            onChoose={props.onChoose}
            onSpot={props.onSpot}
            error={props.error}
            pending={props.pending ?? undefined}
            // What `Auto` picks is the slider's own line, under its track.
            // What is left for here is what choosing changes: who the
            // session records as having decided, and that the agent is told.
            note="Choosing a machine yourself is remembered with the session, and the agent is told you picked it."
          />
        )}
      </Popover>
    </Show>
  );
}

/** Which repository the agent works in. */
function RepoChip(props: { slug: string | null; onChoose: (slug: string) => void }) {
  const [query, setQuery] = createSignal("");
  const [results] = createQuery(query, listRepos);
  const recents = createMemo(() => recentRepos().slice(0, MAX_RECENT_REPOS));

  /**
   * The rest of the account's repositories.
   *
   * While the search box is empty the recents are already listed above, and
   * repeating them under a second heading would make the popover look like
   * it had failed to notice.
   */
  const rest = createMemo(() => {
    const listed = new Set(query().trim() === "" ? recents() : []);
    return (results() ?? []).filter((candidate) => !listed.has(candidate.slug));
  });

  return (
    <Popover
      label="Repository"
      panelClass={styles.popoverWide}
      trigger={(attrs) => (
        <button
          id={attrs.id}
          onClick={attrs.onClick}
          aria-expanded={attrs.expanded()}
          aria-haspopup="dialog"
          type="button"
          class={cx(styles.chip, props.slug === null && styles.chipMissing)}
        >
          <FolderGit2 size={13} aria-hidden="true" />
          <span class={styles.chipLabel}>{props.slug ?? "Select repository"}</span>
        </button>
      )}
    >
      {(close) => (
        <div class={styles.popover}>
          <input
            class={styles.search}
            type="search"
            placeholder="Search your repositories"
            aria-label="Search your repositories"
            value={query()}
            onInput={(event) => setQuery(event.currentTarget.value)}
          />
          <Show when={query().trim() === "" && recents().length > 0}>
            <p class={styles.popoverTitle}>Recent</p>
            <ul class={styles.options}>
              <For each={recents()}>
                {(slug) => (
                  <li>
                    <button
                      type="button"
                      class={cx(styles.option, props.slug === slug && styles.optionChosen)}
                      onClick={() => {
                        props.onChoose(slug);
                        close();
                      }}
                    >
                      {slug}
                    </button>
                  </li>
                )}
              </For>
            </ul>
          </Show>
          <Show when={rest().length > 0}>
            <p class={styles.popoverTitle}>Your repositories</p>
            <ul class={styles.options}>
              <For each={rest()}>
                {(candidate: RepoSummary) => (
                  <li>
                    <button
                      type="button"
                      class={cx(styles.option, props.slug === candidate.slug && styles.optionChosen)}
                      onClick={() => {
                        props.onChoose(candidate.slug);
                        close();
                      }}
                    >
                      {candidate.slug}
                      <Show when={candidate.private}>
                        <span class={styles.optionMeta}>private</span>
                      </Show>
                    </button>
                  </li>
                )}
              </For>
            </ul>
          </Show>
          <ProblemNotice error={results.error} />
        </div>
      )}
    </Popover>
  );
}

/**
 * Which branch the agent starts from (docs/ux.md §9.1).
 *
 * Beside the repository chip because it is the same decision continued: a
 * repository without a branch is not somewhere an agent can be put to work.
 * The list is only fetched when the popover opens — the chip reads the
 * repository's default until then, and most sessions never change it.
 */
function BranchChip(props: {
  slug: string | null;
  branch: string | null;
  onChoose: (branch: string | null) => void;
}) {
  const [browsed, setBrowsed] = createSignal(false);

  // Keyed on the repository *and* on the chip having been opened at least
  // once, so browsing one repository's branches is not a request made for
  // every repository the user clicks past on the way to it.
  const [page] = createQuery(
    () => (browsed() && props.slug !== null ? props.slug : undefined),
    (slug: string) => listBranches(slug),
  );

  /** The branch a session would start on right now. */
  const effective = createMemo(
    () => props.branch ?? page()?.branches.find((candidate) => candidate.is_default)?.name ?? null,
  );

  // Nothing at all until a repository is chosen: a branch is a fact about
  // one repository, and a disabled `Branch` chip beside `Select repository`
  // is a second thing to wonder about on a page that should pose one
  // question at a time.
  return (
    <Show when={props.slug !== null}>
      <Popover
        label="Branch"
        trigger={(attrs) => (
          <button
            id={attrs.id}
            onClick={() => {
              setBrowsed(true);
              attrs.onClick();
            }}
            aria-expanded={attrs.expanded()}
            aria-haspopup="dialog"
            type="button"
            class={styles.chip}
          >
            <GitBranch size={13} aria-hidden="true" />
            <span class={styles.chipLabel}>{effective() ?? "Default branch"}</span>
          </button>
        )}
      >
        {(close) => (
          <div class={styles.popover}>
            <Show when={page.loading}>
              <p class={styles.note}>Reading branches…</p>
            </Show>
            <Show when={page()}>
              {(loaded) => (
                <ul class={styles.options}>
                  <For each={loaded().branches}>
                    {(candidate) => (
                      <li>
                        <button
                          type="button"
                          class={cx(
                            styles.option,
                            effective() === candidate.name && styles.optionChosen,
                          )}
                          onClick={() => {
                            // Choosing the default is choosing *the default*,
                            // not pinning today's name: a repository that
                            // renames it should carry the session with it.
                            props.onChoose(candidate.is_default ? null : candidate.name);
                            close();
                          }}
                        >
                          {candidate.name}
                          <Show when={candidate.is_default}>
                            <span class={styles.optionMeta}>default</span>
                          </Show>
                        </button>
                      </li>
                    )}
                  </For>
                </ul>
              )}
            </Show>
            <ProblemNotice error={page.error} />
          </div>
        )}
      </Popover>
    </Show>
  );
}

/** What the session may spend on compute before flyco stops it. */
function BudgetChip(props: { dollars: number; onChange: (dollars: number) => void }) {
  return (
    <BudgetPicker
      dollars={props.dollars}
      onChange={props.onChange}
      trigger={(attrs) => (
        <button
          id={attrs.id}
          onClick={attrs.onClick}
          aria-expanded={attrs.expanded()}
          aria-haspopup="dialog"
          type="button"
          class={styles.chip}
        >
          <Wallet size={13} aria-hidden="true" />
          <span class={styles.chipLabel}>${props.dollars}</span>
        </button>
      )}
    />
  );
}
