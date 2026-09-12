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
import { For, Match, Show, Switch, createMemo } from "solid-js";
import {
  AlertTriangle,
  Check,
  ChevronRight,
  CircleDashed,
  Cpu,
  Info,
  Loader,
  Sparkles,
  TerminalSquare,
  Wrench,
  X,
} from "lucide-solid";
import Markdown from "./Markdown";
import { shellOutcomeLabel, shellSucceeded } from "../lib/shell";
import type { ModelOption, ProvisioningStage } from "../api/wire";
import { operation, type OperationDetail } from "../lib/approvals";
import { cx } from "../lib/cx";
import { formatDuration } from "../lib/duration";
import { highlightHtml } from "../lib/highlight";
import { choiceLabel } from "../lib/models";
import { detailOfTool } from "../lib/toolDetail";
import { summarizeTool } from "../lib/toolSummary";
import type { ProvisioningStep, ToolCall, TranscriptItem } from "../lib/transcript";
import { machineChangePrice, machineChangeSummary, TURN_FAILED_NOTE } from "../lib/transcript";
import styles from "./Transcript.module.css";

/** How a stage reads, given what the session is actually provisioning. */
function stageLabel(
  stage: ProvisioningStage,
  provider: string | null,
  repo: string,
  recovery: boolean,
): string {
  switch (stage) {
    case "reserving":
      return provider === null ? "Reserving a machine" : `Reserving a machine on ${provider}`;
    case "booting":
      // One stage covers boot and install, because nothing can see where
      // one ends and the other begins. A recovered machine installs
      // nothing — `flycod` is already on its disk — so it does not claim
      // to (issue #225).
      return recovery ? "Starting the machine" : "Booting and installing flycod";
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
  /**
   * The models the agent offers, for the row that says the session moved
   * onto one of them: the stream carries the id, and a person reads the
   * name.
   */
  models: readonly ModelOption[];
  /** Decides one approval. Absent when there is nothing live to decide against. */
  onDecide?: ((id: string, decision: "approved" | "denied") => void) | undefined;
  /** Whether a decision is in flight, which disables both buttons. */
  deciding?: boolean | undefined;
  /** The instant elapsed times are measured against. */
  now: number;
  /**
   * When the session stopped being built, so a timeline still waiting on a
   * stage stops there — with the time that stage had run — instead of
   * counting on under a `Failed` or `Archived` pill.
   */
  stoppedAtUnix: number | null;
}

export default function Transcript(props: TranscriptProps) {
  return (
    <ol class={styles.transcript} aria-label="Transcript">
      <For each={props.items}>
        {(item) => (
          <li class={styles.item} data-kind={item.kind}>
            <Switch>
              <Match when={item.kind === "user_message" && item}>
                {(message) => (
                  <p class={styles.userMessage} data-origin={message().origin}>
                    {/*
                      A message flyco sent on the user's behalf says so. It
                      sits in the user's own bubble because that is what it
                      is in the conversation — the agent was told this by
                      the user's side — and a reader coming back to a
                      session that carried on through the night has to be
                      able to tell it from a sentence they typed.
                    */}
                    <Show when={message().origin === "flyco"}>
                      <span class={styles.userMessageAuthor}>Sent by flyco</span>
                    </Show>
                    {message().text}
                  </p>
                )}
              </Match>

              <Match when={item.kind === "turn" && item}>
                {(turn) => (
                  <div class={styles.turn}>
                    {/*
                      In the order the agent worked, not prose first and
                      commands after: a turn that ran something and then
                      explained what it found used to show the explanation
                      above the command it came from.
                    */}
                    <For each={turn().parts}>
                      {(part) => (
                        <Switch>
                          <Match when={part.kind === "text" && part}>
                            {(text) => <Markdown text={text().text} />}
                          </Match>
                          <Match when={part.kind === "tools" && part}>
                            {(group) => (
                              <ul class={styles.tools}>
                                <For each={group().calls}>
                                  {(tool) => <ToolRow tool={tool} />}
                                </For>
                              </ul>
                            )}
                          </Match>
                        </Switch>
                      )}
                    </For>
                    {/*
                      A turn that stopped says so, and says the session is
                      still the user's to continue; the harness's own words
                      follow as the reason rather than standing alone
                      (issue #136).
                    */}
                    <Show when={turn().status === "failed"}>
                      <div class={styles.turnFailed}>
                        <p class={styles.turnFailedNote}>
                          <AlertTriangle size={14} aria-hidden="true" />
                          {TURN_FAILED_NOTE}
                        </p>
                        <Show when={turn().error}>
                          {(error) => <p class={styles.turnError}>{error()}</p>}
                        </Show>
                      </div>
                    </Show>
                    <Show when={workedFor(turn())}>
                      {(worked) => <p class={styles.workedFor}>Worked for {worked()}</p>}
                    </Show>
                  </div>
                )}
              </Match>

              <Match when={item.kind === "shell" && item}>
                {(shell) => <ShellBlock shell={shell()} />}
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

              <Match when={item.kind === "model_change" && item}>
                {(change) => (
                  <p class={styles.machineChange}>
                    <Sparkles size={14} aria-hidden="true" />
                    Switched to {choiceLabel(props.models, change().model)}
                  </p>
                )}
              </Match>

              <Match when={item.kind === "provisioning" && item}>
                {(timeline) => (
                  <ProvisioningTimeline
                    steps={timeline().steps}
                    recovery={timeline().recovery}
                    attempt={timeline().attempt}
                    endedAtUnix={timeline().endedAtUnix}
                    repo={props.repo}
                    provider={props.provider}
                    now={props.now}
                    stoppedAtUnix={props.stoppedAtUnix}
                  />
                )}
              </Match>

              <Match when={item.kind === "command_output" && item}>
                {(output) => (
                  <div class={styles.commandOutput} role="region" aria-label="Command output">
                    <Markdown text={output().text} />
                  </div>
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
          <span class={styles.toolText}>
            {summarizeTool(props.tool.tool, props.tool.input, props.tool.ok)}
          </span>
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
          <ToolDetail tool={props.tool.tool} input={props.tool.input} />
        </div>
      </details>
    </li>
  );
}

/**
 * What is behind a tool row's disclosure.
 *
 * The call as the agent made it, never as the wire carried it: the command
 * highlighted as shell, the patch as a diff, and everything else as named
 * values. A person opening a row wants to see what ran, and a JSON object
 * makes them read past the syntax of a protocol to find it.
 */
function ToolDetail(props: { tool: string; input: unknown }) {
  const detail = createMemo(() => detailOfTool(props.tool, props.input));

  return (
    <Show
      when={detail().code !== null || detail().fields.length > 0}
      fallback={<p class={styles.toolBare}>This call carried no arguments.</p>}
    >
      <Show when={detail().code}>
        {(code) => (
          <pre class={styles.toolCode}>
            <code
              // Sanitized by `highlightHtml`, which is the only thing that
              // ever produces the markup here.
              // eslint-disable-next-line solid/no-innerhtml
              innerHTML={highlightHtml(code().text, code().language)}
            />
          </pre>
        )}
      </Show>
      <Show when={detail().fields.length > 0}>
        <dl class={styles.toolFields}>
          <For each={detail().fields}>
            {(field) => (
              <>
                <dt class={styles.toolFieldLabel}>{field.label}</dt>
                <dd class={styles.toolFieldValue}>{field.value}</dd>
              </>
            )}
          </For>
        </dl>
      </Show>
    </Show>
  );
}

/**
 * One `!` command and what the machine printed (docs/ux.md §9.3).
 *
 * Mono throughout, which docs/ux.md §2 reserves for text a person is meant
 * to read as the machine wrote it — and the whole point of a `!` command is
 * that the output is verbatim. The status line is always present, including
 * while the command runs and including when it never ran at all: a block
 * with no last line would leave the user waiting on a command that already
 * has its answer.
 */
function ShellBlock(props: { shell: Extract<TranscriptItem, { kind: "shell" }> }) {
  const outcome = () => props.shell.outcome;
  /** Running, finished well, or finished badly — which is what colours it. */
  const state = () => {
    const ended = outcome();
    return ended === null ? "running" : String(shellSucceeded(ended));
  };
  const ran = () => {
    const ended = props.shell.endedAtUnix;
    return ended === null ? null : formatDuration(ended - props.shell.atUnix);
  };

  return (
    <section class={styles.shell} aria-label="Shell command">
      <p class={styles.shellCommand}>
        <TerminalSquare size={13} class={cx(styles.shellIcon)} aria-hidden="true" />
        <span class={styles.shellText}>{props.shell.command}</span>
      </p>
      <Show when={props.shell.output.length > 0}>
        <pre class={styles.shellOutput}>
          <For each={props.shell.output}>
            {(chunk) => <span data-stream={chunk.stream}>{chunk.data}</span>}
          </For>
        </pre>
      </Show>
      <Show when={props.shell.truncated}>
        <p class={styles.shellTruncated}>
          Output past this session's limit was dropped. Run it in the terminal to see all of it.
        </p>
      </Show>
      <p
        class={styles.shellStatus}
        data-ok={state()}
      >
        <Show
          when={outcome()}
          fallback={
            <>
              <Loader size={13} class={cx(styles.spin)} aria-hidden="true" />
              Running
            </>
          }
        >
          {(ended) => <span>{shellOutcomeLabel(ended())}</span>}
        </Show>
        <Show when={ran()}>{(elapsed) => <span class={styles.shellElapsed}>{elapsed()}</span>}</Show>
      </p>
    </section>
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
export function ProvisioningTimeline(props: {
  steps: ProvisioningStep[];
  recovery: boolean;
  attempt: number;
  repo: string;
  provider: string | null;
  now: number;
  /** When the build stopped with this timeline still open; see {@link TranscriptProps}. */
  stoppedAtUnix: number | null;
  /** When a later attempt superseded this one; see the transcript's `Provisioning`. */
  endedAtUnix: number | null;
}) {
  const done = () => props.steps.some((step) => step.stage === "ready");
  /**
   * The stage in progress is where the failure landed: a timeline that
   * reached `ready` belongs to an earlier, finished episode and keeps its
   * ticks. One a later attempt superseded ended when that attempt began,
   * whatever the session is doing now.
   */
  const stoppedAt = () => (done() ? null : (props.endedAtUnix ?? props.stoppedAtUnix));
  const label = () => {
    const what = props.recovery ? "Migrating" : "Provisioning";
    // What stopped it — failed, archived — is the notice's sentence, not
    // this one's: the timeline says only that it did.
    return stoppedAt() === null ? what : `${what} stopped`;
  };

  /**
   * How long the whole build took, once it is over.
   *
   * A finished timeline folds to this one line (docs/ux.md §9.2): four
   * ticked rows above the first turn are a record nobody rereads, and the
   * one number worth keeping is the wait. The rows stay behind a
   * disclosure for the reader who wants to know where the minutes went.
   */
  const took = () => {
    const first = props.steps[0];
    const ready = props.steps.find((step) => step.stage === "ready");
    return first === undefined || ready === undefined
      ? null
      : formatDuration(ready.atUnix - first.atUnix);
  };

  const rows = () => (
    <div class={styles.stages}>
      <For each={props.steps}>
        {(step, index) => {
          const next = () => props.steps[index() + 1];
          const last = () => index() === props.steps.length - 1;
          const stopped = () => last() && stoppedAt() !== null;
          const took = () => {
            const following = next();
            if (following !== undefined) {
              return formatDuration(following.atUnix - step.atUnix);
            }
            if (done()) {
              return null;
            }
            // A stage that never finished says how long it ran before the
            // session stopped, not how long ago that session was opened.
            const until = stoppedAt() ?? Math.floor(props.now / 1000);
            return formatDuration(Math.max(0, until - step.atUnix));
          };

          return (
            <div
              class={styles.stage}
              data-active={last() && !done() && !stopped()}
              data-failed={stopped()}
            >
              <span class={styles.stageMark} aria-hidden="true">
                <Switch fallback={<Check size={12} />}>
                  <Match when={stopped()}>
                    <X size={12} />
                  </Match>
                  <Match when={last() && !done()}>
                    <CircleDashed size={12} class={cx(styles.spin)} />
                  </Match>
                </Switch>
              </span>
              <span class={styles.stageLabel}>
                {stageLabel(step.stage, props.provider, props.repo, props.recovery)}
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

  return (
    <Show
      when={!done()}
      fallback={
        <details class={styles.timeline} aria-label={label()}>
          <summary class={styles.timelineSummary}>
            <ChevronRight size={13} class={cx(styles.toolChevron)} aria-hidden="true" />
            <span class={styles.stageLabel}>
              {props.recovery ? "Machine restarted" : "Machine ready"}
            </span>
            <Show when={took()}>
              {(elapsed) => <span class={styles.stageElapsed}>{elapsed()}</span>}
            </Show>
          </summary>
          {rows()}
        </details>
      }
    >
      <div class={styles.timeline} aria-label={label()}>
        <Show when={props.recovery}>
          <p class={styles.timelineHeading}>
            Migrating · the machine was reclaimed and is being restarted on its own disk
          </p>
        </Show>
        <Show when={props.attempt > 1}>
          <p class={styles.timelineHeading}>
            Attempt {props.attempt} · the last machine never came up, so flyco asked for another
          </p>
        </Show>
        {rows()}
      </div>
    </Show>
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
 * Exactly what is being approved.
 *
 * A tool call's arguments are read out one per row — the name of the
 * argument, and the value as it was written — because the value is what the
 * decision is about, and finding a shell command inside pretty-printed JSON
 * is work the reader should not be doing (issue #136). A one-line value
 * sits beside its name; anything that runs over gets a block under it.
 */
function OperationDetailView(props: { detail: OperationDetail }) {
  return (
    <Switch>
      <Match when={props.detail.kind === "text" && props.detail}>
        {(detail) => <pre class={styles.approvalDetail}>{detail().text}</pre>}
      </Match>
      <Match when={props.detail.kind === "fields" && props.detail}>
        {(detail) => (
          <dl class={styles.approvalFields}>
            <For each={detail().fields}>
              {(field) => (
                <div class={styles.approvalField} data-block={field.block}>
                  <dt class={styles.approvalFieldName}>{field.name}</dt>
                  <dd class={styles.approvalFieldValue}>
                    <code>{field.value}</code>
                  </dd>
                </div>
              )}
            </For>
          </dl>
        )}
      </Match>
    </Switch>
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
      <OperationDetailView detail={asked().detail} />
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


