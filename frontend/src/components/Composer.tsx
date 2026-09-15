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
  ArrowUpToLine,
  Check,
  FolderGit2,
  Plus,
  Server,
  Wallet,
  X,
} from "lucide-solid";
import BudgetPicker, { DEFAULT_BUDGET } from "./BudgetPicker";
import ComposerShell from "./ComposerShell";
import MachinePicker from "./MachinePicker";
import EffortChip from "./EffortChip";
import ModelChip from "./ModelChip";
import Popover from "./Popover";
import RepoBranchPicker from "./RepoBranchPicker";
import Logomark, { HARNESS_MARK, PROVIDER_MARK } from "./Logomark";
import ProblemNotice from "./ProblemNotice";
import { useReadiness } from "./Readiness";
import {
  getDefaultMachine,
  getMachineCatalog,
  listRepos,
  type HarnessAccountView,
  type HarnessKind,
  type MachineCatalogEntry,
  type MachineDefault,
  type ModelChoice,
  type ProviderAccountView,
  type RepoSummary,
} from "../api/client";
import { beginGithubLogin, githubTokenRevoked } from "../api/auth";
import type { NewSessionInput, NewSessionRepo } from "../api/sessions";
import { cx } from "../lib/cx";
import { HARNESS_LABEL } from "../lib/harnesses";
import { defaultChoice, optionOf } from "../lib/models";
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
  chipLabel,
  entryKey,
  readingMachines,
  readingMachinesShort,
  runtimeOf,
} from "../lib/machines";
import styles from "./Composer.module.css";

/**
 * The one thing to do about GitHub refusing flyco's access: authorize it
 * again, from here. The flyco session stays; only the GitHub grant is
 * renewed, and the browser comes back to this page signed in.
 */
function reconnectGithub(error: unknown): { label: string; onClick: () => void } | undefined {
  return githubTokenRevoked(error)
    ? { label: "Reconnect GitHub", onClick: () => void beginGithubLogin() }
    : undefined;
}

export interface ComposerProps {
  /** Starts the session. Rejections surface as a notice under the chips. */
  onSend: (input: NewSessionInput) => Promise<void>;
}

export default function Composer(props: ComposerProps) {
  const readiness = useReadiness();

  const [prompt, setPrompt] = createSignal("");
  /**
   * The repositories the session will work across, in the order the user
   * picked them: `repos()[0]` is primary — the one the session's header
   * names.
   *
   * A `null` branch is "whatever this repository's default branch is",
   * which the control plane resolves and records. Storing the choice rather
   * than the resolved name is what keeps the two from disagreeing when the
   * repository changes under it.
   */
  const [repos, setRepos] = createSignal<NewSessionRepo[]>(
    recentRepos()[0] === undefined ? [] : [{ repo: recentRepos()[0]! }],
  );
  const [budget, setBudget] = createSignal(DEFAULT_BUDGET);
  const [spot, setSpot] = createSignal(spotPreference());
  const [chosenKey, setChosenKey] = createSignal<string | null>(null);
  const [harnessChoice, setHarnessChoice] = createSignal<HarnessKind | null>(null);
  // `null` is "whatever the agent runs by default", resolved against the
  // agent's own list at send time; a choice is kept only while the list it
  // was made against is still the one on the chip.
  const [modelChoice, setModelChoice] = createSignal<ModelChoice | null>(null);
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

  /** The same wait, in the few words the chip has room for. */
  const pendingShort = createMemo(() =>
    pendingKinds().length === 0 ? null : readingMachinesShort(pendingKinds()),
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

  /**
   * The harness a session opens on: the one picked on the chip while it is
   * still linked, else the first linked account, else Claude.
   */
  const harness = createMemo<HarnessKind>(() => {
    const linked = readiness.harness();
    const wanted = harnessChoice();
    if (wanted !== null && linked.some((account) => account.harness === wanted)) {
      return wanted;
    }
    return linked[0]?.harness ?? "claude_code";
  });

  /** The models the chosen agent offers, as its account lists them. */
  const models = createMemo(
    () => readiness.harness().find((account) => account.harness === harness())?.models ?? [],
  );

  /**
   * The model the session opens on: what was picked while it is still on
   * the list, else the agent's default. Switching agents therefore drops a
   * pick made for the other one, because a Claude model id means nothing
   * to Codex.
   */
  const model = createMemo<ModelChoice | null>(() => {
    const list = models();
    if (list.length === 0) {
      return null;
    }
    const picked = modelChoice();
    return picked !== null && optionOf(list, picked) !== undefined ? picked : defaultChoice(list);
  });

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
    if (repos().length === 0) {
      return "Pick a repository first";
    }
    if (prompt().trim() === "") {
      return "Describe a task first";
    }
    return null;
  });

  /** Toggles a repository in or out of the selection, keeping pick order. */
  function toggleRepo(slug: string): void {
    const picked = repos();
    if (picked.some((entry) => entry.repo === slug)) {
      setRepos(picked.filter((entry) => entry.repo !== slug));
      return;
    }
    // No branch: the new repository's default takes it. Carrying `dev`
    // across to a repository that has no `dev` would fail the clone minutes
    // later, on a machine, with a git error.
    setRepos([...picked, { repo: slug }]);
    rememberRepo(slug);
  }

  /** Moves a picked repository to the front, making it the primary one. */
  function makePrimary(slug: string): void {
    setRepos((picked) => {
      const entry = picked.find((candidate) => candidate.repo === slug);
      return entry === undefined
        ? picked
        : [entry, ...picked.filter((candidate) => candidate.repo !== slug)];
    });
  }

  /** Pins a branch on one picked repository; `undefined` restores its default. */
  function setRepoBranch(slug: string, branch: string | undefined): void {
    setRepos((picked) =>
      picked.map((entry) =>
        entry.repo === slug
          ? { repo: entry.repo, ...(branch === undefined ? {} : { branch }) }
          : entry,
      ),
    );
  }

  /** Remembered, because it is a default for the next session too. */
  function chooseSpot(next: boolean): void {
    setSpot(next);
    setSpotPreference(next);
  }

  async function send(): Promise<void> {
    const picked = repos();
    if (blocker() !== null || picked.length === 0) {
      return;
    }
    setSending(true);
    setError(null);
    try {
      const entry = chosen();
      const account = entry?.account;
      const chosenModel = model();
      await props.onSend({
        prompt: prompt().trim(),
        repos: picked,
        ...(chosenModel === null ? {} : { model: chosenModel }),
        harness: harness(),
        budgetLimitDollars: budget(),
        // An explicit machine is only sent when the user picked one: that
        // is what makes the session's `machine_origin` say `user`.
        ...(chosenKey() !== null && entry !== undefined && account !== null && account !== undefined
          ? {
              machine: {
                providerAccount: account,
                machineType: entry.machine_type,
                runtime: runtimeOf(entry),
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
          <HarnessChip
            harness={harness()}
            accounts={readiness.harness()}
            onChoose={setHarnessChoice}
          />
          <ComputeChip
            linked={readiness.compute().length > 0}
            accounts={readiness.compute()}
            automatic={automatic()}
            entry={chosen()}
            catalog={entries()}
            error={machineError()}
            pending={pending()}
            pendingShort={pendingShort()}
            chosenKey={chosenKey()}
            spot={spot()}
            onChoose={setChosenKey}
            onSpot={chooseSpot}
          />
          <RepoChip
            repos={repos()}
            onToggle={toggleRepo}
            onPrimary={makePrimary}
            onBranch={setRepoBranch}
          />
          <BudgetChip dollars={budget()} onChange={setBudget} />
        </div>
      }
      trailing={
        /*
         * At the right, beside send, where both official apps keep their
         * model chip: the last thing checked before the task goes out,
         * and off the row of chips that decide where it runs.
         */
        <Show when={model()}>
          {(choice) => (
            <>
              <ModelChip models={models()} choice={choice()} align="end" onChoose={setModelChoice} />
              <EffortChip models={models()} choice={choice()} align="end" onChoose={setModelChoice} />
            </>
          )}
        </Show>
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

/**
 * Which agent will drive the session, or the way to link one.
 *
 * Linked, the chip is a picker of the linked agents with the connect flow
 * as its last row — a readout that leaves the page when clicked is not a
 * chip, it is a link wearing one.
 */
function HarnessChip(props: {
  harness: HarnessKind;
  accounts: HarnessAccountView[];
  onChoose: (harness: HarnessKind) => void;
}) {
  return (
    <Show
      when={props.accounts.length > 0}
      fallback={
        <A href="/connect/harness" class={cx(styles.chip, styles.chipMissing)}>
          <Plus size={13} aria-hidden="true" />
          Connect an agent
        </A>
      }
    >
      <Popover
        label="Agent"
        trigger={(attrs) => (
          <button
            id={attrs.id}
            onClick={attrs.onClick}
            aria-expanded={attrs.expanded()}
            aria-haspopup="dialog"
            type="button"
            class={styles.chip}
          >
            <Logomark mark={HARNESS_MARK[props.harness]} size={13} />
            <span class={styles.chipLabel}>{HARNESS_LABEL[props.harness]}</span>
          </button>
        )}
      >
        {(close) => (
          <div class={styles.popover}>
            <p class={styles.popoverTitle}>Your agents</p>
            <ul class={styles.options}>
              <For each={props.accounts}>
                {(account) => (
                  <li>
                    <button
                      type="button"
                      class={cx(
                        styles.option,
                        account.harness === props.harness && styles.optionChosen,
                      )}
                      onClick={() => {
                        props.onChoose(account.harness);
                        close();
                      }}
                    >
                      <Logomark mark={HARNESS_MARK[account.harness]} size={13} />
                      {HARNESS_LABEL[account.harness]}
                      <span class={styles.optionMeta}>{account.label}</span>
                    </button>
                  </li>
                )}
              </For>
            </ul>
            <A href="/connect/harness" class={styles.option}>
              <Plus size={13} aria-hidden="true" />
              Connect another agent
            </A>
          </div>
        )}
      </Popover>
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
  /** The same wait as `pending`, sized for the chip. */
  pendingShort: string | null;
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
    // Name and price: what decides whether to send. The logomark already
    // names the provider; region, spot and account are one click away in
    // the popover, and on the chip they were what pushed the row onto a
    // second line. A container carries its size too, because `Container`
    // alone names no machine — see `chipLabel`.
    return chipLabel(entry, props.spot);
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
        // Whether flyco chose the machine or the user did is the slider's
        // heading, not a word on the chip: the chip is the name and the
        // price, and a suffix was what pushed the price into an ellipsis.
        anchorClass={styles.anchorShrink}
        trigger={(attrs) => (
          <button
            id={attrs.id}
            onClick={attrs.onClick}
            aria-expanded={attrs.expanded()}
            aria-haspopup="dialog"
            type="button"
            class={cx(styles.chip, styles.chipShrink, warning() !== null && styles.chipBound)}
          >
            <Show
              when={props.entry !== undefined && PROVIDER_MARK[props.entry.provider]}
              fallback={<Server size={13} aria-hidden="true" />}
            >
              {(mark) => <Logomark mark={mark()} size={13} />}
            </Show>
            <span class={styles.chipLabel}>
              {summary() ?? props.pendingShort ?? "Choosing\u2026"}
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

/**
 * Which repositories the agent works across.
 *
 * Multi-select rather than the single choice it was: a session can work
 * over several checkouts, so every repository row is a toggle and the
 * popover keeps the picked set on top, in the order they were picked — the
 * first is the session's primary repository, the one its header names.
 * Each picked row carries its own branch picker, because a branch is a
 * fact about one repository and a session of three needs three answers.
 *
 * Picking stays open rather than closing on each click: choosing three
 * repositories through a popover that shut after each would be three trips
 * through the same search.
 */
function RepoChip(props: {
  repos: NewSessionRepo[];
  onToggle: (slug: string) => void;
  onPrimary: (slug: string) => void;
  onBranch: (slug: string, branch: string | undefined) => void;
}) {
  const [query, setQuery] = createSignal("");
  const [results] = createQuery(query, listRepos);
  const recents = createMemo(() => recentRepos().slice(0, MAX_RECENT_REPOS));

  /** The picked slugs, for the check each listed row carries. */
  const picked = createMemo(() => new Set(props.repos.map((entry) => entry.repo)));

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

  /** `flyco`, `flyco +2`, or the ask. */
  const label = createMemo(() => {
    const [first] = props.repos;
    if (first === undefined) {
      return "Select repositories";
    }
    const extra = props.repos.length - 1;
    return extra === 0 ? first.repo : `${first.repo} +${extra}`;
  });

  return (
    <Popover
      label="Repositories"
      panelClass={styles.popoverWide}
      trigger={(attrs) => (
        <button
          id={attrs.id}
          onClick={attrs.onClick}
          aria-expanded={attrs.expanded()}
          aria-haspopup="dialog"
          type="button"
          class={cx(styles.chip, props.repos.length === 0 && styles.chipMissing)}
        >
          <FolderGit2 size={13} aria-hidden="true" />
          <span class={styles.chipLabel}>{label()}</span>
        </button>
      )}
    >
      {() => (
        <div class={styles.popover}>
          <Show when={props.repos.length > 0}>
            <p class={styles.popoverTitle}>Selected — first is primary</p>
            <ul class={styles.options}>
              <For each={props.repos}>
                {(entry, index) => (
                  <li class={styles.pickedRow}>
                    <span class={styles.pickedSlug} title={entry.repo}>
                      {entry.repo}
                    </span>
                    <RepoBranchPicker
                      slug={entry.repo}
                      branch={entry.branch ?? null}
                      onChoose={(branch) => props.onBranch(entry.repo, branch)}
                    />
                    <Show when={index() > 0}>
                      <button
                        type="button"
                        class={styles.pickedAction}
                        title="Make primary"
                        aria-label={`Make ${entry.repo} the primary repository`}
                        onClick={() => props.onPrimary(entry.repo)}
                      >
                        <ArrowUpToLine size={13} aria-hidden="true" />
                      </button>
                    </Show>
                    <button
                      type="button"
                      class={styles.pickedAction}
                      title="Remove"
                      aria-label={`Remove ${entry.repo}`}
                      onClick={() => props.onToggle(entry.repo)}
                    >
                      <X size={13} aria-hidden="true" />
                    </button>
                  </li>
                )}
              </For>
            </ul>
          </Show>
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
                  <RepoRow
                    slug={slug}
                    picked={picked().has(slug)}
                    onToggle={props.onToggle}
                  />
                )}
              </For>
            </ul>
          </Show>
          <Show when={rest().length > 0}>
            <p class={styles.popoverTitle}>Your repositories</p>
            <ul class={styles.options}>
              <For each={rest()}>
                {(candidate: RepoSummary) => (
                  <RepoRow
                    slug={candidate.slug}
                    picked={picked().has(candidate.slug)}
                    isPrivate={candidate.private}
                    onToggle={props.onToggle}
                  />
                )}
              </For>
            </ul>
          </Show>
          <ProblemNotice error={results.error} action={reconnectGithub(results.error)} />
        </div>
      )}
    </Popover>
  );
}

/** One repository row in the picker: a toggle, checked when picked. */
function RepoRow(props: {
  slug: string;
  picked: boolean;
  isPrivate?: boolean;
  onToggle: (slug: string) => void;
}) {
  return (
    <li>
      <button
        type="button"
        role="checkbox"
        aria-checked={props.picked}
        class={cx(styles.option, props.picked && styles.optionChosen)}
        onClick={() => props.onToggle(props.slug)}
      >
        {props.slug}
        <Show when={props.isPrivate === true}>
          <span class={styles.optionMeta}>private</span>
        </Show>
        <Show when={props.picked}>
          <Check size={13} aria-hidden="true" class={cx(styles.pickedCheck)} />
        </Show>
      </button>
    </li>
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
