/**
 * The session's scrolling column (docs/ux.md §9.2).
 *
 * Everything that happened, in the order it happened: the user's messages
 * as bubbles, the agent's prose as Markdown, its tool calls as one-line
 * rows that expand, the machine being built as a timeline, approvals as
 * action cards, and anything else worth reading as a notice.
 *
 * The whole column is driven by one folded list (src/lib/transcript.ts).
 * Nothing here reaches back into the relay for a second opinion, so a row
 * can never disagree with the row above it.
 */
import { For, Match, Show, Switch } from "solid-js";
import {
  AlertTriangle,
  Check,
  ChevronRight,
  CircleDashed,
  Cpu,
  Info,
  Loader,
  Wrench,
  X,
} from "lucide-solid";
import Markdown from "./Markdown";
import type { ProvisioningStage } from "../api/wire";
import { operation } from "../lib/approvals";
import { cx } from "../lib/cx";
import { formatDuration } from "../lib/duration";
import { summarizeTool } from "../lib/toolSummary";
import type { ProvisioningStep, ToolCall, TranscriptItem } from "../lib/transcript";
import { machineChangePrice, machineChangeSummary } from "../lib/transcript";
import styles from "./Transcript.module.css";

/** How a stage reads, given what the session is actually provisioning. */
function stageLabel(stage: ProvisioningStage, provider: string | null, repo: string): string {
  switch (stage) {
    case "reserving":
      return provider === null ? "Reserving a machine" : `Reserving a machine on ${provider}`;
    case "booting":
      return "Booting";
    case "installing":
      return "Installing flycod";
    case "cloning":
      return `Cloning ${repo}`;
    case "ready":
      return "Agent ready";
  }
}

/**
 * How long a finished turn took, or `null` while it is still running.
 *
 * A separate function because `endedAtUnix` is a nullable number and
 * `<Show when={…}>` treats `0` as absent; asking for the whole string keeps
 * the falsy-epoch case out of the JSX.
 */
function workedFor(turn: Extract<TranscriptItem, { kind: "turn" }>): string | null {
  const ended = turn.endedAtUnix;
  return ended === null ? null : formatDuration(ended - turn.startedAtUnix);
}

export interface TranscriptProps {
  items: TranscriptItem[];
  /** The repository the session works in, for the timeline's clone step. */
  repo: string;
  /** The provider the machine came from, when it is known. */
  provider: string | null;
  /** Decides one approval. Absent when there is nothing live to decide against. */
  onDecide?: ((id: string, decision: "approved" | "denied") => void) | undefined;
  /** Whether a decision is in flight, which disables both buttons. */
  deciding?: boolean | undefined;
  /** The instant elapsed times are measured against. */
  now: number;
}

export default function Transcript(props: TranscriptProps) {
  return (
    <ol class={styles.transcript} aria-label="Transcript">
      <For each={props.items}>
        {(item) => (
          <li class={styles.item} data-kind={item.kind}>
            <Switch>
              <Match when={item.kind === "user_message" && item}>
                {(message) => <p class={styles.userMessage}>{message().text}</p>}
              </Match>

              <Match when={item.kind === "turn" && item}>
                {(turn) => (
                  <div class={styles.turn}>
                    <Show when={turn().text !== ""}>
                      <Markdown text={turn().text} />
                    </Show>
                    <Show when={turn().tools.length > 0}>
                      <ul class={styles.tools}>
                        <For each={turn().tools}>
                          {(tool) => <ToolRow tool={tool} />}
                        </For>
                      </ul>
                    </Show>
                    <Show when={turn().status === "failed"}>
                      <p class={styles.turnError}>{turn().error}</p>
                    </Show>
                    <Show when={workedFor(turn())}>
                      {(worked) => <p class={styles.workedFor}>Worked for {worked()}</p>}
                    </Show>
                  </div>
                )}
              </Match>

              <Match when={item.kind === "approval" && item}>
                {(approval) => (
                  <ApprovalCard
                    approval={approval()}
                    onDecide={props.onDecide}
                    deciding={props.deciding ?? false}
                  />
                )}
              </Match>

              <Match when={item.kind === "machine_change" && item}>
                {(change) => <MachineChangeRow change={change()} />}
              </Match>

              <Match when={item.kind === "provisioning" && item}>
                {(timeline) => (
                  <ProvisioningTimeline
                    steps={timeline().steps}
                    repo={props.repo}
                    provider={props.provider}
                    now={props.now}
                  />
                )}
              </Match>

              <Match when={item.kind === "notice" && item}>
                {(item_) => (
                  <p class={styles.notice} data-tone={item_().tone}>
                    <Switch fallback={<Info size={14} aria-hidden="true" />}>
                      <Match when={item_().tone === "warning"}>
                        <AlertTriangle size={14} aria-hidden="true" />
                      </Match>
                      <Match when={item_().tone === "danger"}>
                        <AlertTriangle size={14} aria-hidden="true" />
                      </Match>
                    </Switch>
                    {item_().text}
                  </p>
                )}
              </Match>
            </Switch>
          </li>
        )}
      </For>
    </ol>
  );
}

/**
 * One tool call: a line to read, and the raw call behind a disclosure.
 *
 * `<details>` rather than a signal and a conditional: the browser owns the
 * open state, the keyboard already works, and a transcript with four
 * hundred rows in it does not need four hundred signals.
 */
function ToolRow(props: { tool: ToolCall }) {
  const duration = () => {
    const ended = props.tool.endedAtUnix;
    return ended === null ? null : formatDuration(ended - props.tool.startedAtUnix);
  };

  return (
    <li>
      <details class={styles.toolRow}>
        <summary class={styles.toolSummary}>
          <ChevronRight class={cx(styles.toolChevron)} size={13} aria-hidden="true" />
          <Wrench size={13} class={cx(styles.toolIcon)} aria-hidden="true" />
          <span class={styles.toolText}>{summarizeTool(props.tool.tool, props.tool.input)}</span>
          <Show when={duration()}>
            {(elapsed) => <span class={styles.toolDuration}>{elapsed()}</span>}
          </Show>
          <span class={styles.toolStatus} data-ok={props.tool.ok === null ? "running" : props.tool.ok}>
            <Switch>
              <Match when={props.tool.ok === null}>
                <Loader size={13} class={cx(styles.spin)} aria-label="Running" />
              </Match>
              <Match when={props.tool.ok === true}>
                <Check size={13} aria-label="Done" />
              </Match>
              <Match when={props.tool.ok === false}>
                <X size={13} aria-label="Failed" />
              </Match>
            </Switch>
          </span>
        </summary>
        <div class={styles.toolDetail}>
          <p class={styles.toolDetailLabel}>{props.tool.tool}</p>
          <pre class={styles.toolInput}>{JSON.stringify(props.tool.input, null, 2)}</pre>
        </div>
      </details>
    </li>
  );
}

/**
 * The machine being built, as a list of milestones with the time each one
 * took (docs/ux.md §9.2).
 *
 * Elapsed time is measured between consecutive stages, so each line says
 * how long *that* step took rather than how long the whole thing has run.
 * The step still in progress counts against the clock passed in, which is
 * what makes the timeline move while the user watches it.
 */
function ProvisioningTimeline(props: {
  steps: ProvisioningStep[];
  repo: string;
  provider: string | null;
  now: number;
}) {
  const done = () => props.steps.some((step) => step.stage === "ready");

  return (
    <div class={styles.timeline} aria-label="Provisioning">
      <For each={props.steps}>
        {(step, index) => {
          const next = () => props.steps[index() + 1];
          const last = () => index() === props.steps.length - 1;
          const took = () => {
            const following = next();
            if (following !== undefined) {
              return formatDuration(following.atUnix - step.atUnix);
            }
            if (done()) {
              return null;
            }
            return formatDuration(Math.floor(props.now / 1000) - step.atUnix);
          };

          return (
            <div class={styles.stage} data-active={last() && !done()}>
              <span class={styles.stageMark} aria-hidden="true">
                <Show when={last() && !done()} fallback={<Check size={12} />}>
                  <CircleDashed size={12} class={cx(styles.spin)} />
                </Show>
              </span>
              <span class={styles.stageLabel}>
                {stageLabel(step.stage, props.provider, props.repo)}
              </span>
              <Show when={took()}>
                {(elapsed) => <span class={styles.stageElapsed}>{elapsed()}</span>}
              </Show>
            </div>
          );
        }}
      </For>
    </div>
  );
}

/**
 * The session moving onto another machine (docs/ux.md §9.5).
 *
 * One line, because that is what it is: what the machine is now, that it
 * restarted, and that the disk came across. The price sits beside it rather
 * than inside the sentence — the sentence is about what happened, and the
 * rate is what it costs from here on.
 */
function MachineChangeRow(props: {
  change: Extract<TranscriptItem, { kind: "machine_change" }>;
}) {
  return (
    <p class={styles.machineChange}>
      <Cpu size={14} aria-hidden="true" />
      {machineChangeSummary(props.change)}
      <Show when={machineChangePrice(props.change)}>
        {(price) => <span class={styles.machineChangePrice}>{price()}</span>}
      </Show>
    </p>
  );
}

/**
 * One approval, inline where it happened (docs/ux.md §9.2).
 *
 * It keeps its place in the transcript after it is decided rather than
 * disappearing: what was approved, and when, is part of the record of the
 * session — and a card that vanished on click would leave the user unsure
 * which button they pressed.
 */
function ApprovalCard(props: {
  approval: Extract<TranscriptItem, { kind: "approval" }>;
  onDecide?: ((id: string, decision: "approved" | "denied") => void) | undefined;
  deciding: boolean;
}) {
  const asked = () => operation(props.approval.payload);

  return (
    <section class={styles.approval} data-state={props.approval.state} aria-label="Approval">
      <p class={styles.approvalTitle}>{asked().title}</p>
      <pre class={styles.approvalDetail}>{asked().detail}</pre>
      <Show
        when={props.approval.state === "pending" && props.onDecide}
        fallback={
          <p class={styles.approvalDecided}>
            {props.approval.state === "approved" ? "Approved" : "Denied"}
          </p>
        }
      >
        {(decide) => (
          <div class={styles.approvalActions}>
            <button
              type="button"
              class={styles.approve}
              disabled={props.deciding}
              onClick={() => decide()(props.approval.id, "approved")}
            >
              Approve
            </button>
            <button
              type="button"
              class={styles.deny}
              disabled={props.deciding}
              onClick={() => decide()(props.approval.id, "denied")}
            >
              Deny
            </button>
          </div>
        )}
      </Show>
    </section>
  );
}
