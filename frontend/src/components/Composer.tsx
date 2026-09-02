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
import { For, Show, createMemo, createResource, createSignal } from "solid-js";
import { A } from "@solidjs/router";
import { ArrowUp, Cpu, FolderGit2, Plus, Server, Wallet } from "lucide-solid";
import Popover from "./Popover";
import Logomark, { HARNESS_MARK, PROVIDER_MARK } from "./Logomark";
import ProblemNotice from "./ProblemNotice";
import { useReadiness } from "./Readiness";
import {
  getDefaultMachine,
  getMachineCatalog,
  listRepos,
  type HarnessKind,
  type MachineCatalogEntry,
  type MachineDefault,
  type RepoSummary,
} from "../api/client";
import type { NewSessionInput } from "../api/sessions";
import { cx } from "../lib/cx";
import { MAX_RECENT_REPOS, recentRepos, rememberRepo } from "../lib/localPreferences";
import { formatUsd } from "../lib/money";
import { PROVIDER_LABEL } from "../lib/providers";
import styles from "./Composer.module.css";

/** What the budget slider spans, in whole dollars. */
const MIN_BUDGET = 1;
const MAX_BUDGET = 200;
const DEFAULT_BUDGET = 10;

const HARNESS_LABEL: Record<HarnessKind, string> = {
  claude_code: "Claude Code",
  codex: "Codex",
};

/** Identifies one catalog entry across the account, region and type it names. */
function entryKey(entry: MachineCatalogEntry): string {
  return `${entry.account ?? ""}/${entry.region}/${entry.machine_type}`;
}

/** `$0.04/hr`, or the honest absence of a price on hardware the user owns. */
function hourlyLabel(entry: MachineCatalogEntry, spot: boolean): string {
  if (entry.pricing.kind === "user_owned") {
    return "your hardware";
  }
  const spotHourly = entry.pricing.spot_hourly;
  const useSpot = spot && spotHourly !== null && spotHourly !== undefined;
  return `${formatUsd(useSpot ? spotHourly : entry.pricing.on_demand_hourly)}/hr`;
}

export interface ComposerProps {
  /** Starts the session. Rejections surface as a notice under the chips. */
  onSend: (input: NewSessionInput) => Promise<void>;
}

export default function Composer(props: ComposerProps) {
  const readiness = useReadiness();

  const [prompt, setPrompt] = createSignal("");
  const [repo, setRepo] = createSignal<string | null>(recentRepos()[0] ?? null);
  const [budget, setBudget] = createSignal(DEFAULT_BUDGET);
  const [spot, setSpot] = createSignal(true);
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
  const [automatic] = createResource(
    () => (readiness.compute().length > 0 ? spot() : undefined),
    getDefaultMachine,
  );
  const [catalog] = createResource(
    () => (readiness.compute().length > 0 ? true : undefined),
    () => getMachineCatalog({ os: "linux" }),
  );

  /** The harness a session opens on: the one linked account, or Claude. */
  const harness = createMemo<HarnessKind>(() => readiness.harness()[0]?.harness ?? "claude_code");

  /** The catalog entry the compute chip is showing. */
  const chosen = createMemo<MachineCatalogEntry | undefined>(() => {
    const key = chosenKey();
    if (key === null) {
      return automatic()?.entry;
    }
    return (catalog() ?? []).find((entry) => entryKey(entry) === key);
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
    rememberRepo(slug);
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
      await props.onSend({
        prompt: prompt().trim(),
        repo: slug,
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
    <div>
      <div class={styles.composer}>
        <textarea
          class={styles.prompt}
          placeholder="Describe a task"
          aria-label="Describe a task"
          value={prompt()}
          rows="3"
          onInput={(event) => setPrompt(event.currentTarget.value)}
          onKeyDown={(event) => {
            // ⌘/Ctrl+Enter sends; plain Enter is a newline, because a
            // prompt is prose and prose has paragraphs.
            if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
              event.preventDefault();
              void send();
            }
          }}
        />
        <div class={styles.controls}>
          <div class={styles.chips}>
            <HarnessChip harness={harness()} linked={readiness.harness().length > 0} />
            <ComputeChip
              linked={readiness.compute().length > 0}
              automatic={automatic()}
              entry={chosen()}
              catalog={catalog() ?? []}
              chosenKey={chosenKey()}
              spot={spot()}
              onChoose={setChosenKey}
              onSpot={setSpot}
            />
            <RepoChip slug={repo()} onChoose={chooseRepo} />
            <BudgetChip dollars={budget()} onChange={setBudget} />
          </div>
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
        </div>
      </div>
      <ProblemNotice error={error()} />
    </div>
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
  automatic: MachineDefault | undefined;
  entry: MachineCatalogEntry | undefined;
  catalog: MachineCatalogEntry[];
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
            class={styles.chip}
          >
            <Show
              when={props.entry !== undefined && PROVIDER_MARK[props.entry.provider]}
              fallback={<Server size={13} aria-hidden="true" />}
            >
              {(mark) => <Logomark mark={mark()} size={13} />}
            </Show>
            <span class={styles.chipLabel}>{summary() ?? "Choosing a machine\u2026"}</span>
            <span class={styles.chipDim}>
              {props.chosenKey === null ? "Auto" : "Chosen by you"}
            </span>
          </button>
        )}
      >
        {(close) => (
          <div class={styles.popover}>
            <label class={styles.toggleRow}>
              <input
                type="checkbox"
                checked={props.spot}
                onChange={(event) => props.onSpot(event.currentTarget.checked)}
              />
              Use spot capacity — cheaper, and flyco handles eviction
            </label>

            <p class={styles.popoverTitle}>Machine</p>
            <ul class={styles.options}>
              <li>
                <button
                  type="button"
                  class={cx(styles.option, props.chosenKey === null && styles.optionChosen)}
                  onClick={() => {
                    props.onChoose(null);
                    close();
                  }}
                >
                  <Cpu size={14} aria-hidden="true" />
                  Let flyco choose
                  <Show when={props.automatic}>
                    {(chosenDefault) => (
                      <span class={styles.optionMeta}>
                        {chosenDefault().entry.machine_type}
                      </span>
                    )}
                  </Show>
                </button>
              </li>
              <For each={props.catalog}>
                {(entry) => (
                  <li>
                    <button
                      type="button"
                      class={cx(
                        styles.option,
                        props.chosenKey === entryKey(entry) && styles.optionChosen,
                      )}
                      onClick={() => {
                        props.onChoose(entryKey(entry));
                        close();
                      }}
                    >
                      <span class={styles.chipLabel}>
                        {entry.machine_type} · {entry.region}
                      </span>
                      <span class={styles.optionMeta}>{hourlyLabel(entry, props.spot)}</span>
                    </button>
                  </li>
                )}
              </For>
            </ul>
            <p class={styles.note}>
              Flyco picks the cheapest Linux machine of at least 4 vCPUs and 16 GiB that your
              linked accounts can deploy. Choosing one yourself is remembered with the session.
            </p>
          </div>
        )}
      </Popover>
    </Show>
  );
}

/** Which repository the agent works in. */
function RepoChip(props: { slug: string | null; onChoose: (slug: string) => void }) {
  const [query, setQuery] = createSignal("");
  const [results] = createResource(query, listRepos);
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

/** What the session may spend on compute before flyco stops it. */
function BudgetChip(props: { dollars: number; onChange: (dollars: number) => void }) {
  return (
    <Popover
      label="Budget"
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
    >
      {() => (
        <div class={styles.popover}>
          <div class={styles.sliderRow}>
            <span class={styles.amount}>${props.dollars}</span>
            <span class={styles.note}>
              ${MIN_BUDGET}–${MAX_BUDGET}
            </span>
          </div>
          <input
            class={styles.slider}
            type="range"
            min={MIN_BUDGET}
            max={MAX_BUDGET}
            step="1"
            value={props.dollars}
            aria-label="Session budget in dollars"
            onInput={(event) => props.onChange(Number(event.currentTarget.value))}
          />
          <p class={styles.note}>
            Covers the machine and its disk. Model tokens are billed by your Claude or Codex plan.
          </p>
        </div>
      )}
    </Popover>
  );
}
