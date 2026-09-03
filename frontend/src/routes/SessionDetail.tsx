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
import { useParams } from "@solidjs/router";
import { Match, Show, Switch, createMemo, createSignal, onCleanup } from "solid-js";
import { createQuery } from "../lib/query";
import { AlertTriangle } from "lucide-solid";
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
  sendMessage,
  startSessionMachine,
  stopSessionMachine,
  updateSession,
} from "../api/client";
import { ApiProblem } from "../api/problem";
import { createSessionRelay } from "../api/relay";
import { PROVIDER_LABEL } from "../lib/providers";
import { usdMicrosToDollars } from "../lib/money";
import { shellCommandIn } from "../lib/shell";
import { deriveStatus, liveSignalsFrom, type StatusView } from "../lib/status";
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

  const transcript = createMemo(() => foldTranscript(relay.events()));
  const waiting = createMemo(() => pendingApprovals(transcript()));
  const signals = createMemo(() => liveSignalsFrom(relay.events()));

  const status = createMemo((): StatusView => {
    const current = session();
    if (current === undefined) {
      // Nothing is known yet; the pill says so rather than guessing at a
      // lifecycle the request has not answered with.
      return { status: "idle", label: "Loading", tone: "quiet", breathing: false };
    }
    return deriveStatus(current, now(), signals());
  });

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

  const [error, setError] = createSignal<unknown>(null);
  const [deciding, setDeciding] = createSignal(false);
  const [archiving, setArchiving] = createSignal(false);
  const [pendingDirtySummary, setPendingDirtySummary] = createSignal<string | null>(null);
  const [panelRequest, setPanelRequest] = createSignal<{ panel: "machine" | "env"; at: number }>();

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
        // where those are listed; sending the user there beats a second
        // picker that would have to duplicate it.
        setPanelRequest({ panel: "machine", at: Date.now() });
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
      const updated = await updateSession(params.id, title);
      mutateSession(updated);
    } catch (failure) {
      setError(failure);
      mutateSession(previous);
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
        onArchive={() => void onArchive(false)}
        archiving={archiving()}
        onStartMachine={() => void onMachine("start")}
        onStopMachine={() => void onMachine("stop")}
        onOpenPanel={(panel) => setPanelRequest({ panel, at: Date.now() })}
      />

      <ProblemNotice error={session.error ?? machine.error} />
      <ProblemNotice error={error()} />

      <Show when={session()?.failure}>
        {(failure) => <p class={styles.failure}>{failure()}</p>}
      </Show>

      <Show when={pendingDirtySummary()}>
        {(summary) => (
          <div class={styles.archiveConfirm} role="alertdialog" aria-labelledby="archive-dirty">
            <h2 id="archive-dirty" class={styles.archiveTitle}>
              Uncommitted changes
            </h2>
            <p class={styles.archiveBody}>
              Archiving releases the disk without keeping this work. Commit it first, or discard it
              to archive anyway.
            </p>
            <pre class={styles.archiveSummary}>{summary()}</pre>
            <div class={styles.archiveActions}>
              <button
                type="button"
                class={styles.keep}
                onClick={() => setPendingDirtySummary(null)}
              >
                Keep session
              </button>
              <button
                type="button"
                class={styles.discard}
                disabled={archiving()}
                onClick={() => void onArchive(true)}
              >
                Discard and archive
              </button>
            </div>
          </div>
        )}
      </Show>

      <div class={styles.body}>
        <div class={styles.column}>
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
                <Match when={session()?.state === "provisioning" && session()}>
                  {(current) => (
                    <>
                      <ProvisioningTimeline
                        steps={[{ stage: "reserving", atUnix: current().created_at_unix }]}
                        recovery={status().status === "migrating"}
                        repo={current().repo}
                        provider={providerLabel()}
                        now={now()}
                      />
                      <p class={styles.empty}>
                        Your task is queued and will start as soon as the machine is ready.
                      </p>
                    </>
                  )}
                </Match>
                <Match when={session()?.state === "active"}>
                  <p class={styles.empty}>
                    Nothing has happened yet. Send a message to get the agent started.
                  </p>
                </Match>
                <Match when={session()}>
                  <p class={styles.empty}>
                    {status().label}
                    <Show when={status().detail}>{(detail) => <> · {detail()}</>}</Show>. Nothing
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
            />
          </Show>

          <div class={styles.composer}>
            <SessionComposer
              turnInFlight={signals().turnInFlight === true}
              disabled={session()?.state === "archived"}
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
