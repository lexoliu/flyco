import { For, Show, createResource, createSignal } from "solid-js";
import { useNavigate } from "@solidjs/router";
import SessionCard from "../components/SessionCard";
import ProblemNotice from "../components/ProblemNotice";
import { getMe, listSessions, listRepos, type HarnessKind, type RepoSummary } from "../api/client";
import { requestNewSession } from "../api/sessions";
import { cx } from "../lib/cx";
import styles from "./Sessions.module.css";

const HARNESSES: readonly { value: HarnessKind; label: string }[] = [
  { value: "claude_code", label: "Claude Code" },
  { value: "codex", label: "Codex" },
];

const DEFAULT_BUDGET_DOLLARS = 10;

function NewSessionForm(props: { onCreated: (id: string) => void; onCancel: () => void }) {
  const [repoQuery, setRepoQuery] = createSignal("");
  const [repos] = createResource(repoQuery, listRepos);
  const [repo, setRepo] = createSignal("");
  const [harness, setHarness] = createSignal<HarnessKind>("claude_code");
  const [budgetDollars, setBudgetDollars] = createSignal(DEFAULT_BUDGET_DOLLARS);
  const [spot, setSpot] = createSignal(true);
  const [submitting, setSubmitting] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  async function onSubmit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    if (repo().trim() === "") {
      setError(new Error("Pick a repository before starting a session."));
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      const created = await requestNewSession({
        repo: repo(),
        harness: harness(),
        budgetLimitDollars: budgetDollars(),
        spot: spot(),
      });
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
        <input
          id="new-session-repo"
          type="text"
          placeholder="owner/name"
          value={repo()}
          onInput={(event) => {
            setRepo(event.currentTarget.value);
            setRepoQuery(event.currentTarget.value);
          }}
        />
        <Show when={(repos() ?? []).length > 0}>
          <ul class={styles.repoSuggestions}>
            <For each={repos()}>
              {(candidate: RepoSummary) => (
                <li>
                  <button type="button" onClick={() => setRepo(candidate.slug)}>
                    {candidate.slug}
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
