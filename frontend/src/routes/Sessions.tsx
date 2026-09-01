import { For, Show, createMemo, createResource, createSignal } from "solid-js";
import { useNavigate } from "@solidjs/router";
import SessionCard from "../components/SessionCard";
import ProblemNotice from "../components/ProblemNotice";
import {
  getMachineCatalog,
  getMe,
  listSessions,
  listRepos,
  type HarnessKind,
  type MachineCatalogEntry,
  type RepoSummary,
} from "../api/client";
import { requestNewSession, type NewSessionInput } from "../api/sessions";
import { cx } from "../lib/cx";
import { formatUsd } from "../lib/money";
import { PROVIDER_LABEL } from "../lib/providers";
import styles from "./Sessions.module.css";

const HARNESSES: readonly { value: HarnessKind; label: string }[] = [
  { value: "claude_code", label: "Claude Code" },
  { value: "codex", label: "Codex" },
];

const DEFAULT_BUDGET_DOLLARS = 10;

/** One line describing what a catalog entry costs, given whether the form wants spot capacity. */
function pricingLabel(entry: MachineCatalogEntry, wantsSpot: boolean): string {
  if (entry.pricing.kind === "user_owned") {
    return "your hardware — no metered price";
  }
  const useSpot = wantsSpot && entry.pricing.spot_hourly !== null && entry.pricing.spot_hourly !== undefined;
  const hourly = useSpot ? (entry.pricing.spot_hourly as number) : entry.pricing.on_demand_hourly;
  return `${formatUsd(hourly)}/hr ${useSpot ? "(spot)" : "(on-demand)"}`;
}

function catalogEntryKey(entry: MachineCatalogEntry): string {
  return `${entry.provider}/${entry.region}/${entry.machine_type}`;
}

function NewSessionForm(props: { onCreated: (id: string) => void; onCancel: () => void }) {
  const [repoQuery, setRepoQuery] = createSignal("");
  const [repos] = createResource(repoQuery, listRepos);
  const [selectedRepo, setSelectedRepo] = createSignal<RepoSummary | null>(null);
  const [harness, setHarness] = createSignal<HarnessKind>("claude_code");
  const [budgetDollars, setBudgetDollars] = createSignal(DEFAULT_BUDGET_DOLLARS);
  const [spot, setSpot] = createSignal(true);
  const [selectedMachineKey, setSelectedMachineKey] = createSignal<string | null>(null);
  const [submitting, setSubmitting] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  const [catalog] = createResource(() => getMachineCatalog());
  const catalogByProvider = createMemo(() => {
    const groups = new Map<string, MachineCatalogEntry[]>();
    for (const entry of catalog() ?? []) {
      const list = groups.get(entry.provider) ?? [];
      list.push(entry);
      groups.set(entry.provider, list);
    }
    return groups;
  });

  function onSelectRepo(candidate: RepoSummary): void {
    setSelectedRepo(candidate);
    setRepoQuery("");
  }

  async function onSubmit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const repo = selectedRepo();
    if (repo === null) {
      setError(new Error("Pick a repository before starting a session."));
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      const machineKey = selectedMachineKey();
      let machine: NewSessionInput["machine"];
      if (machineKey !== null) {
        const entry = (catalog() ?? []).find(
          (candidate) => catalogEntryKey(candidate) === machineKey,
        );
        if (entry === undefined || entry.account === null || entry.account === undefined) {
          setError(new Error("Pick a machine before starting a session."));
          setSubmitting(false);
          return;
        }
        machine = {
          providerAccount: entry.account,
          machineType: entry.machine_type,
          region: entry.region,
          spot: spot(),
        };
      }
      const request = {
        repo: repo.slug,
        harness: harness(),
        budgetLimitDollars: budgetDollars(),
      };
      const created = await requestNewSession(
        machine === undefined ? { ...request, spot: spot() } : { ...request, machine },
      );
      props.onCreated(created.id);
    } catch (err) {
      setError(err);
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <form class={styles.form} onSubmit={(event) => void onSubmit(event)}>
      <div class={styles.field}>
        <label for="new-session-repo">Repository</label>
        <Show
          when={selectedRepo()}
          fallback={
            <input
              id="new-session-repo"
              type="text"
              placeholder="Search your repositories…"
              value={repoQuery()}
              onInput={(event) => setRepoQuery(event.currentTarget.value)}
            />
          }
        >
          {(repo) => (
            <div class={styles.checkboxField}>
              <strong>{repo().slug}</strong>
              <button type="button" onClick={() => setSelectedRepo(null)}>
                Change
              </button>
            </div>
          )}
        </Show>
        <Show when={selectedRepo() === null && (repos() ?? []).length > 0}>
          <ul class={styles.repoSuggestions}>
            <For each={repos()}>
              {(candidate: RepoSummary) => (
                <li>
                  <button type="button" onClick={() => onSelectRepo(candidate)}>
                    {candidate.slug}
                    {candidate.private ? " (private)" : ""}
                  </button>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </div>

      <div class={styles.field}>
        <label for="new-session-harness">Harness</label>
        <select
          id="new-session-harness"
          value={harness()}
          onChange={(event) => setHarness(event.currentTarget.value as HarnessKind)}
        >
          <For each={HARNESSES}>{(option) => <option value={option.value}>{option.label}</option>}</For>
        </select>
      </div>

      <div class={styles.field}>
        <label for="new-session-budget">Budget limit (USD)</label>
        <input
          id="new-session-budget"
          type="number"
          min="1"
          step="1"
          value={budgetDollars()}
          onInput={(event) => setBudgetDollars(Number(event.currentTarget.value))}
        />
      </div>

      <label class={styles.checkboxField}>
        <input type="checkbox" checked={spot()} onChange={(event) => setSpot(event.currentTarget.checked)} />
        Use spot capacity (cheaper; flyco handles eviction)
      </label>

      <div class={styles.field}>
        <label for="new-session-machine">Machine</label>
        <select
          id="new-session-machine"
          value={selectedMachineKey() ?? ""}
          onChange={(event) => setSelectedMachineKey(event.currentTarget.value === "" ? null : event.currentTarget.value)}
        >
          <option value="">Let flyco choose</option>
          <For each={[...catalogByProvider().entries()]}>
            {([provider, entries]) => (
              <optgroup label={PROVIDER_LABEL[provider as MachineCatalogEntry["provider"]]}>
                <For each={entries}>
                  {(entry) => (
                    <option value={catalogEntryKey(entry)}>
                      {entry.region} · {entry.machine_type}
                      {entry.capacity ? ` · ${entry.capacity.vcpus} vCPU / ${Math.round(entry.capacity.memory_mib / 1024)} GiB` : ""}
                      {" · "}
                      {pricingLabel(entry, spot())}
                    </option>
                  )}
                </For>
              </optgroup>
            )}
          </For>
        </select>
        <ProblemNotice error={catalog.error} />
      </div>

      <ProblemNotice error={error()} />

      <div class={styles.formActions}>
        <button type="button" class={styles.cancelButton} onClick={props.onCancel} disabled={submitting()}>
          Cancel
        </button>
        <button type="submit" class={styles.newButton} disabled={submitting()}>
          {submitting() ? "Starting…" : "Start session"}
        </button>
      </div>
    </form>
  );
}

export default function Sessions() {
  const navigate = useNavigate();
  const [showArchived, setShowArchived] = createSignal(false);
  const [showForm, setShowForm] = createSignal(false);
  const [sessions, { refetch }] = createResource(listSessions);
  const [me] = createResource(getMe);

  const visible = () =>
    (sessions() ?? []).filter((session) => (showArchived() ? session.state === "archived" : session.state !== "archived"));

  function onCreated(id: string): void {
    setShowForm(false);
    void refetch();
    navigate(`/sessions/${id}`);
  }

  return (
    <section class={styles.page}>
      <header class={styles.header}>
        <h1>Sessions</h1>
        <Show when={!showForm()}>
          <button type="button" class={styles.newButton} onClick={() => setShowForm(true)}>
            New session
          </button>
        </Show>
      </header>

      <Show when={me()}>
        {(user) => (
          <p class={styles.cap}>
            {(sessions() ?? []).filter((s) => s.state !== "archived").length} / {user().session_cap} sessions in use
          </p>
        )}
      </Show>

      <Show when={showForm()}>
        <NewSessionForm onCreated={onCreated} onCancel={() => setShowForm(false)} />
      </Show>

      <div class={styles.filterRow} role="group" aria-label="Filter sessions">
        <button
          type="button"
          class={cx(!showArchived() && styles.filterActive)}
          onClick={() => setShowArchived(false)}
        >
          Active
        </button>
        <button
          type="button"
          class={cx(showArchived() && styles.filterActive)}
          onClick={() => setShowArchived(true)}
        >
          Archived
        </button>
      </div>

      <ProblemNotice error={sessions.error} />

      <Show when={!sessions.loading} fallback={<p class={styles.empty}>Loading sessions…</p>}>
        <Show
          when={sessions.error !== undefined || visible().length > 0}
          fallback={
            <p class={styles.empty}>
              {showArchived()
                ? "No archived sessions."
                : "No sessions yet. Start one to put an agent to work in a repo."}
            </p>
          }
        >
          <ul class={styles.grid}>
            <For each={visible()}>
              {(session) => (
                <li>
                  <SessionCard
                    id={session.id}
                    repo={session.repo}
                    harness={session.harness}
                    state={session.state}
                  />
                </li>
              )}
            </For>
          </ul>
        </Show>
      </Show>
    </section>
  );
}
