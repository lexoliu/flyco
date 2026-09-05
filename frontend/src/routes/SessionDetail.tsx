/**
 * The session (docs/ux.md §9).
 *
 * Three parts and nothing else: a header saying what this session is and
 * how it is doing, a transcript of everything that has happened, and a
 * composer to say the next thing. Everything occasional — the terminal, the
 * working tree, the machine, the `.env` — is in a drawer that starts
 * closed, so the conversation gets the width.
 *
 * The page holds no state the relay already has. Status, transcript,
 * approvals, usage and the provisioning timeline are all folded from one
 * event list (src/lib/transcript.ts, src/lib/status.ts), so two things on
 * screen can never disagree about what happened.
 */
import { useNavigate, useParams } from "@solidjs/router";
import { Match, Show, Switch, createEffect, createMemo, createSignal, on, onCleanup } from "solid-js";
import { createQuery } from "../lib/query";
import { AlertTriangle } from "lucide-solid";
import { BudgetRaise } from "../components/BudgetPicker";
import ConfirmDialog from "../components/ConfirmDialog";
import ProblemNotice from "../components/ProblemNotice";
import SessionComposer, { type SessionCommand } from "../components/SessionComposer";
import SessionDrawer from "../components/SessionDrawer";
import SessionHeader from "../components/SessionHeader";
import Transcript, { ProvisioningTimeline } from "../components/Transcript";
import {
  archiveSession,
  compactSession,
  decideApproval,
  getSession,
  getSessionMachine,
  interruptSession,
  resumeSession,
  sendMessage,
  startSessionMachine,
  stopSessionMachine,
  updateSession,
} from "../api/client";
import { ApiProblem } from "../api/problem";
import { createSessionRelay } from "../api/relay";
import { PROVIDER_LABEL } from "../lib/providers";
import { dollarsToUsdMicros, usdMicrosToDollars } from "../lib/money";
import { shellCommandIn } from "../lib/shell";
import {
  composerRefusal,
  deriveStatus,
  liveSignalsFrom,
  sessionNotice,
  type StatusView,
} from "../lib/status";
import { foldTranscript, pendingApprovals } from "../lib/transcript";
import styles from "./SessionDetail.module.css";

/**
 * How often the clock the page renders against advances.
 *
 * The provisioning timeline counts the stage it is waiting on, and the
 * status pill counts how long a machine has been building. One second is
 * the smallest unit either of them prints.
 */
const TICK_MS = 1000;

export default function SessionDetail() {
  const params = useParams<{ id: string }>();
  const navigate = useNavigate();
  const [session, { refetch: refetchSession, mutate: mutateSession }] = createQuery(
    () => params.id,
    getSession,
  );
  const [machine, { refetch: refetchMachine }] = createQuery(
    () => params.id,
    getSessionMachine,
  );

  const relay = createSessionRelay(params.id);
  onCleanup(() => relay.dispose());

  const [now, setNow] = createSignal(Date.now());
  const ticker = setInterval(() => setNow(Date.now()), TICK_MS);
  onCleanup(() => clearInterval(ticker));

  /**
   * How many times the room has said something that changes the machine
   * or the session it serves.
   *
   * Both are fetched once and would otherwise stay as they were read: the
   * header would go on quoting a rate for a machine the session was moved
   * off, or one that was released when the session failed (issue #135),
   * and a session that failed while the page was open would keep its
   * provisioning timeline ticking until a reload. A `machine_changed`
   * frame says the session moved; a `session_state_changed` frame says the
   * lifecycle moved — the session row carries the new state and its
   * failure reason, and the machine follows it into `deallocated` or
   * `destroyed`. Counting them rather than watching the last one means two
   * changes in a row are two refetches.
   */
  const machineNews = createMemo(
    () =>
      relay.events().filter(
        ({ event }) => event.type === "machine_changed" || event.type === "session_state_changed",
      ).length,
  );
  createEffect(
    on(
      machineNews,
      () => {
        void refetchMachine();
        void refetchSession();
      },
      { defer: true },
    ),
  );

  /**
   * When the session failed, for the provisioning timeline to stop at.
   * `fail` stamps `last_active_unix` as it records the reason, so that is
   * the instant the machine stopped being built.
   */
  const failedAtUnix = createMemo(() => {
    const current = session();
    return current?.state === "failed" ? current.last_active_unix : null;
  });

  /**
   * Whether a machine is being built and the room has not yet said so.
   *
   * The prompt reaches the room before the queue reaches the job, so for
   * the first seconds the transcript holds the user's message and nothing
   * else; a page that showed the timeline only over an empty transcript
   * went blank exactly then.
   */
  const awaitingFirstStage = createMemo(
    () =>
      session()?.state === "provisioning" &&
      !transcript().some((item) => item.kind === "provisioning"),
  );

  const transcript = createMemo(() => foldTranscript(relay.events()));
  const waiting = createMemo(() => pendingApprovals(transcript()));
  const signals = createMemo(() => liveSignalsFrom(relay.events()));

  const status = createMemo((): StatusView | undefined => {
    const current = session();
    // No session, no status: a page whose request 404'd has no lifecycle to
    // report, and a pill reading `Loading` over a session that will never
    // arrive is the header claiming something the notice below it denies.
    return current === undefined ? undefined : deriveStatus(current, now(), signals());
  });

  /**
   * The one failure that makes the whole page moot: the session could not
   * be read, or the relay stopped for good (a 404, a 403 — see
   * `isDefinitiveFailure`). Both mean there is nothing here to look at, so
   * the notice carries the way out rather than leaving the reader on a dead
   * page.
   */
  const fatal = createMemo(() => session.error ?? relay.failure());

  const latestUsage = createMemo(() => {
    const events = relay.events();
    for (let i = events.length - 1; i >= 0; i -= 1) {
      const entry = events[i];
      if (entry !== undefined && entry.event.type === "usage") {
        return entry.event.usage;
      }
    }
    return null;
  });

  /** Who the machine came from, for the timeline's `Reserving on …` line. */
  const providerLabel = createMemo(() => {
    const view = machine();
    return view === undefined ? null : PROVIDER_LABEL[view.spec.provider];
  });

  const liveRepoSummary = createMemo(() => {
    const events = relay.events();
    for (let i = events.length - 1; i >= 0; i -= 1) {
      const entry = events[i];
      if (entry !== undefined && entry.event.type === "repo_dirty") {
        return entry.event.summary;
      }
    }
    return null;
  });

  /**
   * What this session says for itself while it is not running, and the one
   * thing to do about it (docs/ux.md §6, issue #133).
   */
  const notice = createMemo(() => {
    const view = status();
    return view === undefined
      ? null
      : sessionNotice(view, {
          failure: session()?.failure,
          budgetLimit: session()?.budget.limit,
        });
  });

  /** Why the composer will not send, or `null` when it will. */
  const refusal = createMemo(() => {
    const view = status();
    return view === undefined ? null : composerRefusal(view.status);
  });

  const [error, setError] = createSignal<unknown>(null);
  const [deciding, setDeciding] = createSignal(false);
  const [resuming, setResuming] = createSignal(false);
  const [archiving, setArchiving] = createSignal(false);
  const [settingBudget, setSettingBudget] = createSignal(false);
  const [pendingDirtySummary, setPendingDirtySummary] = createSignal<string | null>(null);
  const [panelRequest, setPanelRequest] =
    createSignal<{ panel: "machine" | "env"; at: number; resize?: boolean }>();

  /**
   * Prefers the relay socket whenever it is live — lower latency, and the
   * echo comes back as an event on the same connection — and falls back to
   * the REST handler while it is not (a paused session, a stopped machine,
   * a reconnect in progress), so a message is recorded rather than
   * silently dropped. See `sendMessage`/`interruptSession` in
   * src/api/client.ts.
   */
  async function overRelay(
    live: () => void,
    rest: () => Promise<void>,
  ): Promise<void> {
    setError(null);
    try {
      if (relay.state() === "live") {
        live();
      } else {
        await rest();
      }
    } catch (failure) {
      setError(failure);
    }
  }

  /**
   * Sends what was typed to whichever of the two it was addressed to.
   *
   * A message beginning with `!` is for the machine's bash, not for the
   * agent (docs/ux.md §9.3), and it has no REST door: a user message that
   * misses the socket is conversation and waits in the room's mailbox, but
   * a shell command recorded now and run whenever the daemon comes back
   * would run against a working tree the user is no longer looking at. So
   * the relay has to be live, and the composer says so when it is not
   * rather than swallowing the command.
   */
  function onSend(text: string): void {
    const command = shellCommandIn(text);
    if (command === null) {
      void overRelay(
        () => relay.send({ type: "user_message", text }),
        () => sendMessage(params.id, text),
      );
      return;
    }
    setError(null);
    if (relay.state() !== "live") {
      setError(
        new Error("Reconnecting to the session — a shell command needs a live connection."),
      );
      return;
    }
    try {
      relay.send({ type: "shell_command", command });
    } catch (failure) {
      setError(failure);
    }
  }

  function onStop(): void {
    void overRelay(
      () => relay.send({ type: "interrupt" }),
      () => interruptSession(params.id),
    );
  }

  function onCommand(command: SessionCommand): void {
    switch (command) {
      case "compact":
        void overRelay(
          () => relay.send({ type: "compact" }),
          () => compactSession(params.id),
        );
        break;
      case "archive":
        void onArchive(false);
        break;
      case "resize":
        // Resizing is a choice among machine types, and the machine tab is
        // where that choice is made; the request carries the intent so the
        // tab opens on the control rather than beside it (issue #138).
        setPanelRequest({ panel: "machine", at: Date.now(), resize: true });
        break;
    }
  }

  async function onRename(title: string): Promise<void> {
    setError(null);
    const previous = session();
    // Shown immediately, because a rename the user typed should not wait a
    // round trip to appear; a refused rename puts the old title back.
    if (previous !== undefined) {
      mutateSession({ ...previous, title });
    }
    try {
      const updated = await updateSession(params.id, { title });
      mutateSession(updated);
    } catch (failure) {
      setError(failure);
      mutateSession(previous);
    }
  }

  /**
   * Sets the session's compute budget.
   *
   * Not shown before the answer lands, unlike the rename: raising a budget
   * can move the session out of `paused`, and a header that showed the new
   * limit beside the old status would be showing two halves of one change.
   * The answer carries both.
   */
  async function onSetBudget(dollars: number): Promise<void> {
    if (settingBudget()) {
      return;
    }
    setError(null);
    setSettingBudget(true);
    try {
      mutateSession(await updateSession(params.id, { budgetLimit: dollarsToUsdMicros(dollars) }));
    } catch (failure) {
      setError(failure);
    } finally {
      setSettingBudget(false);
    }
  }

  async function onArchive(discardUncommitted: boolean): Promise<void> {
    if (archiving()) {
      return;
    }
    setError(null);
    setArchiving(true);
    try {
      await archiveSession(params.id, { discardUncommitted });
      setPendingDirtySummary(null);
      await refetchSession();
    } catch (failure) {
      if (
        !discardUncommitted &&
        failure instanceof ApiProblem &&
        failure.type.endsWith("/dirty-archive")
      ) {
        setPendingDirtySummary(failure.detail);
      } else {
        setError(failure);
      }
    } finally {
      setArchiving(false);
    }
  }

  /**
   * Puts a stopped session back on a machine.
   *
   * The answer is the session in `provisioning`, so the page it comes back
   * to is the provisioning timeline rather than the dead end it was — no
   * refetch needed, and no window where the notice still offers a resume
   * that has already happened.
   */
  async function onResume(): Promise<void> {
    if (resuming()) {
      return;
    }
    setError(null);
    setResuming(true);
    try {
      mutateSession(await resumeSession(params.id));
      await refetchMachine();
    } catch (failure) {
      setError(failure);
    } finally {
      setResuming(false);
    }
  }

  async function onDecide(id: string, decision: "approved" | "denied"): Promise<void> {
    setError(null);
    setDeciding(true);
    try {
      await decideApproval(id, decision);
    } catch (failure) {
      setError(failure);
    } finally {
      setDeciding(false);
    }
  }

  async function onMachine(action: "start" | "stop"): Promise<void> {
    setError(null);
    try {
      await (action === "start"
        ? startSessionMachine(params.id)
        : stopSessionMachine(params.id));
      await refetchMachine();
    } catch (failure) {
      setError(failure);
    }
  }

  const budgetSpentUsd = () => {
    const usage = latestUsage();
    if (usage?.estimated_cost !== null && usage?.estimated_cost !== undefined) {
      return usdMicrosToDollars(usage.estimated_cost);
    }
    const budget = session()?.budget;
    return budget === undefined ? undefined : usdMicrosToDollars(budget.spent);
  };
  const budgetLimitUsd = () => {
    const budget = session()?.budget;
    return budget === undefined ? undefined : usdMicrosToDollars(budget.limit);
  };

  return (
    <section class={styles.page}>
      <SessionHeader
        session={session()}
        sessionId={params.id}
        status={status()}
        connection={relay.state()}
        machine={machine()}
        budgetSpentUsd={budgetSpentUsd()}
        budgetLimitUsd={budgetLimitUsd()}
        contextUsed={latestUsage()?.context?.used_tokens}
        contextSize={latestUsage()?.context?.size_tokens}
        onRename={(title) => void onRename(title)}
        onSetBudget={(dollars) => void onSetBudget(dollars)}
        settingBudget={settingBudget()}
        onArchive={() => void onArchive(false)}
        archiving={archiving()}
        onStartMachine={() => void onMachine("start")}
        onStopMachine={() => void onMachine("stop")}
        onOpenPanel={(request) => setPanelRequest({ ...request, at: Date.now() })}
      />

      {/*
        A session that could not be read takes everything derived from it
        down with it — the machine, the budget, the relay — and repeating
        the same 404 once per query would bury the way out of the page in
        copies of itself. So the fatal notice replaces them.
      */}
      <Show when={fatal()} fallback={<ProblemNotice error={machine.error} />}>
        <ProblemNotice
          error={fatal()}
          action={{ label: "Back to sessions", onClick: () => navigate("/") }}
        />
      </Show>
      <ProblemNotice error={error()} />

      <Show when={pendingDirtySummary()}>
        {(summary) => (
          <ConfirmDialog
            title="Uncommitted changes"
            body="Archiving releases the disk without keeping this work. Commit it first, or discard it to archive anyway."
            tone="danger"
            confirmLabel="Discard and archive"
            cancelLabel="Keep session"
            busy={archiving()}
            onConfirm={() => void onArchive(true)}
            onCancel={() => setPendingDirtySummary(null)}
          >
            <pre class={styles.archiveSummary}>{summary()}</pre>
          </ConfirmDialog>
        )}
      </Show>

      <div class={styles.body}>
        <div class={styles.column}>
          {/*
            Above the transcript rather than in the header, because it is
            not a label: it is the page telling the reader what happened and
            handing them the way on from it.
          */}
          <Show when={notice()}>
            {(state) => (
              <section class={styles.stateNotice} data-tone={state().tone} aria-label="Session state">
                <h2 class={styles.stateTitle}>{state().title}</h2>
                <p class={styles.stateBody}>{state().body}</p>
                {/*
                  Each way out is the control it actually is: a resume is a
                  button because it is one request, and a budget raise is
                  the picker of docs/ux.md §9.1 because the user has to say
                  how much before there is a request at all.
                */}
                <Show when={state().action}>
                  {(action) => (
                    <Switch>
                      <Match when={action().kind === "resume"}>
                        <button
                          type="button"
                          class={styles.stateAction}
                          disabled={resuming()}
                          onClick={() => void onResume()}
                        >
                          {resuming() ? "Resuming…" : action().label}
                        </button>
                      </Match>
                      <Match when={action().kind === "raise_budget" && session()}>
                        {(current) => (
                          <BudgetRaise
                            limitUsd={usdMicrosToDollars(current().budget.limit)}
                            spentUsd={usdMicrosToDollars(current().budget.spent)}
                            saving={settingBudget()}
                            onSet={(dollars) => void onSetBudget(dollars)}
                            label="Raise the session budget"
                            trigger={(attrs) => (
                              <button
                                id={attrs.id}
                                onClick={attrs.onClick}
                                aria-expanded={attrs.expanded()}
                                aria-haspopup="dialog"
                                type="button"
                                class={styles.stateAction}
                              >
                                {action().label}
                              </button>
                            )}
                          />
                        )}
                      </Match>
                    </Switch>
                  )}
                </Show>
              </section>
            )}
          </Show>

          {/*
            The banner is sticky so an approval raised a hundred rows ago is
            still one click away, and amber because it is the one thing on
            the page holding everything else up.
          */}
          <Show when={waiting().length > 0}>
            <p class={styles.approvalBanner}>
              <AlertTriangle size={14} aria-hidden="true" />
              {waiting().length === 1
                ? "The agent is waiting on your decision."
                : `The agent is waiting on ${waiting().length} decisions.`}
            </p>
          </Show>

          {/*
            An empty transcript says something different in every state,
            and the first one a new user meets is the one that matters
            most: the prompt has already gone out with `POST /v1/sessions`
            and the machine is being built, so asking for a message there
            would be asking for what was just given. The daemon has not
            spoken yet, so the timeline starts from the fact the page does
            hold — when the session was opened — and moves with the clock.
          */}
          <Show
            when={transcript().length > 0}
            fallback={
              <Switch>
                <Match when={session()?.state === "provisioning"}>
                  <p class={styles.empty}>
                    Your task is queued and will start as soon as the machine is ready.
                  </p>
                </Match>
                <Match when={session()?.state === "active"}>
                  <p class={styles.empty}>
                    Nothing has happened yet. Send a message to get the agent started.
                  </p>
                </Match>
                <Match when={session()}>
                  <p class={styles.empty}>
                    {status()?.label}
                    <Show when={status()?.detail}>{(detail) => <> · {detail()}</>}</Show>. Nothing
                    ran before it stopped.
                  </p>
                </Match>
              </Switch>
            }
          >
            <Transcript
              items={transcript()}
              repo={session()?.repo ?? "the repository"}
              provider={providerLabel()}
              onDecide={(id, decision) => {
                void onDecide(id, decision).then(() => refetchSession());
              }}
              deciding={deciding()}
              now={now()}
              failedAtUnix={failedAtUnix()}
            />
          </Show>

          {/*
            Until the queue announces its first stage, the page holds the
            timeline's place from the one fact it has — when the session was
            opened — whether the transcript is empty or already carries the
            prompt the session was opened with. The first `reserving` event
            takes over, in the same place, without the page having gone
            blank in between.
          */}
          <Show when={awaitingFirstStage() && session()}>
            {(current) => (
              <ProvisioningTimeline
                steps={[{ stage: "reserving", atUnix: current().created_at_unix }]}
                recovery={status()?.status === "migrating"}
                repo={current().repo}
                provider={providerLabel()}
                now={now()}
                failedAtUnix={failedAtUnix()}
              />
            )}
          </Show>

          <div class={styles.composer}>
            <SessionComposer
              turnInFlight={signals().turnInFlight === true}
              refusal={refusal()}
              onSend={onSend}
              onStop={onStop}
              onCommand={onCommand}
            />
          </div>
        </div>

        <SessionDrawer
          sessionId={params.id}
          relay={relay}
          liveRepoSummary={liveRepoSummary()}
          openPanel={panelRequest()}
        />
      </div>
    </section>
  );
}
