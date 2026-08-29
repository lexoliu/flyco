import { useParams } from "@solidjs/router";
import { For, Match, Show, Switch, createMemo, createResource, createSignal, onCleanup } from "solid-js";
import BudgetBar from "../components/BudgetBar";
import UsageMeter from "../components/UsageMeter";
import ApprovalsPanel from "../components/ApprovalsPanel";
import ProblemNotice from "../components/ProblemNotice";
import TerminalPanel from "../components/terminal/TerminalPanel";
import MachinePanel from "../components/MachinePanel";
import RepoStatusPanel from "../components/RepoStatusPanel";
import EnvEditor from "../components/EnvEditor";
import { getSession, decideApproval, interruptSession, sendMessage } from "../api/client";
import { createSessionRelay, type ConnectionState } from "../api/relay";
import { foldTranscript, type TranscriptItem } from "../lib/transcript";
import { foldApprovals } from "../lib/approvals";
import { usdMicrosToDollars } from "../lib/money";
import styles from "./SessionDetail.module.css";

/** One transcript row. A plain switch, not nested `<Show>`s, so the union narrows without casts. */
function TranscriptRow(props: { item: TranscriptItem }) {
  return (
    <Switch>
      <Match when={props.item.kind === "user_message" && props.item}>
        {(item) => <p class={styles.userMessage}>{item().text}</p>}
      </Match>
      <Match when={props.item.kind === "notice" && props.item}>
        {(item) => <p class={styles.notice}>{item().text}</p>}
      </Match>
      <Match when={props.item.kind === "turn" && props.item}>
        {(item) => (
          <div class={styles.turn} data-status={item().status}>
            <Show when={item().text !== ""}>
              <p class={styles.assistantText}>{item().text}</p>
            </Show>
            <For each={item().tools}>
              {(tool) => (
                <details class={styles.toolRow}>
                  <summary>
                    {tool.tool}
                    <span class={styles.toolStatus} data-ok={tool.ok === null ? "pending" : tool.ok}>
                      {tool.ok === null ? "running" : tool.ok ? "done" : "failed"}
                    </span>
                  </summary>
                  <pre class={styles.toolInput}>{JSON.stringify(tool.input, null, 2)}</pre>
                </details>
              )}
            </For>
            <Show when={item().status === "failed"}>
              <p class={styles.turnError}>{item().error}</p>
            </Show>
          </div>
        )}
      </Match>
    </Switch>
  );
}

const CONNECTION_LABEL: Record<ConnectionState, string> = {
  connecting: "Connecting…",
  live: "Live",
  reconnecting: "Reconnecting…",
  closed: "Closed",
};

/**
 * The session view: budget/usage meters seeded from `GET /v1/sessions/{id}`
 * then kept current from the relay's own `usage`/`session_state_changed`
 * events, a folded transcript, approvals, and the terminal pane. See
 * docs/ARCHITECTURE.md's "Session relay" section for the protocol this
 * wires to (`src/api/relay.ts`).
 */
export default function SessionDetail() {
  const params = useParams<{ id: string }>();
  const [session, { refetch: refetchSession }] = createResource(() => params.id, getSession);

  const relay = createSessionRelay(params.id);
  onCleanup(() => relay.dispose());

  const transcript = createMemo(() => foldTranscript(relay.events()));
  const approvals = createMemo(() => foldApprovals(relay.events()));
  const latestUsage = createMemo(() => {
    const events = relay.events();
    for (let i = events.length - 1; i >= 0; i -= 1) {
      const event = events[i];
      if (event !== undefined && event.type === "usage") {
        return event.usage;
      }
    }
    return null;
  });

  const [messageText, setMessageText] = createSignal("");
  const [sendError, setSendError] = createSignal<unknown>(null);
  const [decideError, setDecideError] = createSignal<unknown>(null);
  const [sending, setSending] = createSignal(false);
  const [interrupting, setInterrupting] = createSignal(false);

  /**
   * The relay socket is preferred whenever it's live — lower latency, and
   * the echo comes back as a `ClientEvent` on the same connection. While
   * it isn't (paused session, stopped machine, still reconnecting), these
   * fall back to the REST handlers so a message or interrupt is still
   * recorded rather than silently dropped. See the module doc comment on
   * `sendMessage`/`interruptSession` in `src/api/client.ts`.
   */
  async function submitMessage(): Promise<void> {
    const text = messageText().trim();
    if (text === "" || sending()) {
      return;
    }
    setSendError(null);
    setSending(true);
    try {
      if (relay.state() === "live") {
        relay.send({ type: "user_message", text });
      } else {
        await sendMessage(params.id, text);
      }
      setMessageText("");
    } catch (err) {
      setSendError(err);
    } finally {
      setSending(false);
    }
  }

  async function onInterrupt(): Promise<void> {
    if (interrupting()) {
      return;
    }
    setSendError(null);
    setInterrupting(true);
    try {
      if (relay.state() === "live") {
        relay.send({ type: "interrupt" });
      } else {
        await interruptSession(params.id);
      }
    } catch (err) {
      setSendError(err);
    } finally {
      setInterrupting(false);
    }
  }

  async function onDecide(id: string, decision: "approved" | "denied"): Promise<void> {
    setDecideError(null);
    try {
      await decideApproval(id, decision);
    } catch (err) {
      setDecideError(err);
    }
  }

  const budgetSpentUsd = () => {
    const usage = latestUsage();
    const budget = session()?.budget;
    if (usage?.estimated_cost !== null && usage?.estimated_cost !== undefined) {
      return usdMicrosToDollars(usage.estimated_cost);
    }
    return budget !== undefined ? usdMicrosToDollars(budget.spent) : undefined;
  };
  const budgetLimitUsd = () => {
    const budget = session()?.budget;
    return budget !== undefined ? usdMicrosToDollars(budget.limit) : undefined;
  };

  return (
    <section class={styles.page}>
      <header class={styles.header}>
        <h1>{params.id}</h1>
        <span class={styles.connection} data-state={relay.state()}>
          <span class={styles.connectionDot} />
          {CONNECTION_LABEL[relay.state()]}
        </span>
      </header>

      <ProblemNotice error={session.error} />

      <div class={styles.meters}>
        <BudgetBar label="Budget" spentUsd={budgetSpentUsd()} limitUsd={budgetLimitUsd()} />
        <UsageMeter
          label="Context window"
          used={latestUsage()?.context?.used_tokens}
          total={latestUsage()?.context?.size_tokens}
          unit="tokens"
        />
        <div class={styles.tokenReadout}>
          <span>LLM usage</span>
          <Show when={latestUsage()} fallback={<span class={styles.muted}>Not loaded yet</span>}>
            {(usage) => (
              <span>
                {usage().input_tokens.toLocaleString()} in / {usage().output_tokens.toLocaleString()} out
              </span>
            )}
          </Show>
        </div>
      </div>

      <div class={styles.body}>
        <div class={styles.transcriptColumn}>
          <ul class={styles.transcript} aria-label="Transcript">
            <Show
              when={transcript().length > 0}
              fallback={<li class={styles.empty}>No turns yet. Send the first message to get started.</li>}
            >
              <For each={transcript()}>
                {(item) => (
                  <li class={styles.transcriptItem} data-kind={item.kind}>
                    <TranscriptRow item={item} />
                  </li>
                )}
              </For>
            </Show>
          </ul>

          <ProblemNotice error={sendError()} />
          <form
            class={styles.composer}
            onSubmit={(event) => {
              event.preventDefault();
              void submitMessage();
            }}
          >
            <textarea
              class={styles.composerInput}
              placeholder="Message the session…"
              value={messageText()}
              disabled={sending()}
              onInput={(event) => setMessageText(event.currentTarget.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && !event.shiftKey) {
                  event.preventDefault();
                  void submitMessage();
                }
              }}
            />
            <div class={styles.composerActions}>
              <button
                type="button"
                class={styles.interruptButton}
                disabled={interrupting()}
                onClick={() => void onInterrupt()}
              >
                {interrupting() ? "Interrupting…" : "Interrupt"}
              </button>
              <button type="submit" class={styles.sendButton} disabled={sending()}>
                {sending() ? "Sending…" : "Send"}
              </button>
            </div>
          </form>
        </div>

        <aside class={styles.side}>
          <ProblemNotice error={decideError()} />
          <ApprovalsPanel
            approvals={approvals()}
            onDecide={(id, decision) => {
              void onDecide(id, decision).then(() => refetchSession());
            }}
          />
          <MachinePanel sessionId={params.id} />
          <RepoStatusPanel sessionId={params.id} />
          <TerminalPanel sessionId={params.id} relay={relay} />
          <EnvEditor sessionId={params.id} />
        </aside>
      </div>
    </section>
  );
}
