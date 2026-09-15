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
import {
  Match,
  Show,
  Switch,
  createEffect,
  createMemo,
  createSignal,
  on,
  onCleanup,
} from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { createQuery } from "../lib/query";
import { AlertTriangle, Server, Wallet } from "lucide-solid";
import { BudgetRaise } from "../components/BudgetPicker";
import GoalChip from "../components/GoalChip";
import ConfirmDialog from "../components/ConfirmDialog";
import EffortChip from "../components/EffortChip";
import ModelChip from "../components/ModelChip";
import ModeChip from "../components/ModeChip";
import MachinePanel from "../components/MachinePanel";
import Popover from "../components/Popover";
import ProblemNotice from "../components/ProblemNotice";
import { useReadiness } from "../components/Readiness";
import ContextRing, { type SessionTotals } from "../components/ContextRing";
import SessionComposer, { type SessionCommand } from "../components/SessionComposer";
import SessionDrawer from "../components/SessionDrawer";
import SessionHeader from "../components/SessionHeader";
import Transcript, { ProvisioningTimeline } from "../components/Transcript";
import composerStyles from "../components/Composer.module.css";
import {
  archiveSession,
  decideApproval,
  getSession,
  getSessionMachine,
  resumeSession,
  startSessionMachine,
  stopSessionMachine,
  updateSession,
  type ModelChoice,
  type ModelOption,
  type PermissionMode,
} from "../api/client";
import { ApiProblem } from "../api/problem";
import type { ContextUsage, ContextWindow, UsageWindow } from "../api/wire";
import { createSessionRelay } from "../api/relay";
import type { HarnessCommand } from "../api/wire";
import { formatTimeOfDay } from "../lib/dates";
import { PROVIDER_LABEL } from "../lib/providers";
import { machineChip } from "../lib/machines";
import { modesFor } from "../lib/modes";
import { orderedWindows } from "../lib/planUsage";
import { dollarsToUsdMicros, usdMicrosToDollars } from "../lib/money";
import { shellCommandIn } from "../lib/shell";
import {
  REFUSING,
  deriveStatus,
  liveSignalsFrom,
  sessionNotice,
  type StatusView,
} from "../lib/status";
import {
  foldTranscript,
  pendingApprovals,
  type TranscriptItem,
} from "../lib/transcript";
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
  const readiness = useReadiness();
  const [session, { refetch: refetchSession, mutate: mutateSession }] = createQuery(
    () => params.id,
    getSession,
  );
  const [machine, { refetch: refetchMachine }] = createQuery(() => params.id, getSessionMachine);

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
      relay
        .events()
        .filter(
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
   * When the session stopped being built, for the timeline to stop at.
   *
   * A session that is no longer provisioning and never reached `ready` did
   * not finish: failing stamps `last_active_unix` as it records the
   * reason, and archiving stamps it as it releases the machine, so either
   * way that is the instant the build ended. A spinner past it is the page
   * claiming something is still happening.
   */
  const stoppedAtUnix = createMemo(() => {
    const current = session();
    if (current === undefined || current.state === "provisioning") {
      return null;
    }
    return current.state === "active" || current.state === "paused"
      ? null
      : current.last_active_unix;
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
      !transcript.some((item) => item.kind === "provisioning"),
  );

  /**
   * The transcript as a store, reconciled into place once per frame.
   *
   * Two things about why it is built this way rather than as a memo of
   * `foldTranscript(relay.events())`:
   *
   * - A fresh array every delta means `<For>` keys nothing: every row is a
   *   new object, so the whole column unmounts and remounts on every token.
   *   That is what made streaming expensive, and — because remounting
   *   resets the scroll position — what threw a reading user back to the
   *   top. `reconcile` keys rows by `TranscriptItem.key` and patches them
   *   in place, so a streamed paragraph is the same DOM node before and
   *   after its next delta.
   * - Folding is work, and the relay can deliver a burst of deltas inside
   *   one frame. Batching the fold onto `requestAnimationFrame` keeps it
   *   to once a frame without dropping a single event: the callback reads
   *   the event list as it stands when the frame fires.
   */
  const [transcript, setTranscript] = createStore<TranscriptItem[]>([]);
  // The empty-state copy below reads `transcript`, which lags the event
  // list by the frame the fold waits on. A fold in flight means items are
  // coming, so it counts as non-empty — otherwise the "queued" line would
  // flash for one frame over a prompt that already arrived.
  const [foldQueued, setFoldQueued] = createSignal(false);
  let foldFrame = 0;
  createEffect(() => {
    // The effect subscribes to the event list; the fold itself runs in the
    // animation frame so a burst costs one fold, not one per event. The
    // first run, on an empty list, folds nothing into nothing — skip it so
    // the empty state does not blink out for a frame on mount.
    const events = relay.events();
    if (foldFrame !== 0 || (events.length === 0 && transcript.length === 0)) {
      return;
    }
    setFoldQueued(true);
    foldFrame = requestAnimationFrame(() => {
      foldFrame = 0;
      setTranscript(reconcile(foldTranscript(relay.events()), { key: "key" }));
      setFoldQueued(false);
    });
  });
  onCleanup(() => {
    if (foldFrame !== 0) {
      cancelAnimationFrame(foldFrame);
    }
  });

  /*
   * The transcript scrolls inside the page, not with it, so the composer
   * stays at the foot of the window. That makes following the agent the
   * page's job: a reader at the bottom is kept there as lines arrive, and
   * one who has scrolled up to reread something is left where they are.
   *
   * Growth is watched, not counted: a streamed delta changes a row's
   * height without changing the item count, so a `ResizeObserver` on the
   * column's contents is what notices. "At the bottom" is a few lines of
   * slack, so the last line's own height never counts as having scrolled
   * away from it — and a reader who *did* scroll away keeps their place,
   * because nothing here runs when `pinned` is false.
   */
  let scroller: HTMLDivElement | undefined;
  let transcriptBody: HTMLDivElement | undefined;
  let pinned = true;
  const PIN_SLACK_PX = 96;
  function noteScroll(): void {
    if (scroller === undefined) {
      return;
    }
    pinned = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight <= PIN_SLACK_PX;
  }
  createEffect(() => {
    const watched = transcriptBody;
    if (watched === undefined) {
      return;
    }
    const observer = new ResizeObserver(() => {
      if (scroller !== undefined && pinned) {
        scroller.scrollTop = scroller.scrollHeight;
      }
    });
    observer.observe(watched);
    onCleanup(() => observer.disconnect());
  });
  const waiting = createMemo(() => pendingApprovals(transcript));
  const signals = createMemo(() => liveSignalsFrom(relay.events()));

  const status = createMemo((): StatusView | undefined => {
    const current = session();
    // No session, no status: a page whose request 404'd has no lifecycle to
    // report, and a pill reading `Loading` over a session that will never
    // arrive is the header claiming something the notice below it denies.
    return current === undefined ? undefined : deriveStatus(current, now(), signals());
  });

  // The status is also what decides whether the composer offers Stop.
  // `turnInFlight` alone is a fold over frames that *stopped arriving*: an
  // archived session whose last turn never completed goes on offering to
  // stop it, and so does one whose machine fell off the room, where the
  // interrupt would reach nobody. `working` is the one state where there
  // is genuinely something running to stop.

  /**
   * The one failure that makes the whole page moot: the session could not
   * be read, or the relay stopped for good (a 404, a 403 — see
   * `isDefinitiveFailure`). Both mean there is nothing here to look at, so
   * the notice carries the way out rather than leaving the reader on a dead
   * page.
   */
  const fatal = createMemo(() => session.error ?? relay.failure());

  /**
   * The newest token accounting the room has reported.
   *
   * A standalone `usage` frame and a turn's closing `turn_completed.usage`
   * are the same reading on different schedules, so the newest of either
   * wins. (The context ring below reads `latestContext`, which adds the
   * `context_usage` answer as a third source.)
   */
  const latestUsage = createMemo(() => {
    const events = relay.events();
    for (let i = events.length - 1; i >= 0; i -= 1) {
      const entry = events[i];
      if (entry === undefined) {
        continue;
      }
      const event = entry.event;
      if (event.type === "usage") {
        return event.usage;
      }
      if (event.type === "harness" && event.event.type === "turn_completed") {
        return event.event.usage;
      }
    }
    return null;
  });

  /**
   * How full the context window is, from wherever said so last.
   *
   * Three frames carry a window reading, newest wins: a `usage` report, a
   * completed turn's usage, and a `context_usage` answer — the last of
   * which is what makes the ring move when a breakdown is asked for
   * mid-turn.
   */
  const latestContext = createMemo((): ContextWindow | null => {
    const events = relay.events();
    for (let i = events.length - 1; i >= 0; i -= 1) {
      const entry = events[i];
      if (entry === undefined) {
        continue;
      }
      const event = entry.event;
      if (event.type === "usage" && event.usage.context !== null && event.usage.context !== undefined) {
        return event.usage.context;
      }
      if (event.type !== "harness") {
        continue;
      }
      const harness = event.event;
      if (harness.type === "context_usage" && harness.usage.window !== undefined) {
        return harness.usage.window;
      }
      if (
        harness.type === "turn_completed" &&
        harness.usage.context !== null &&
        harness.usage.context !== undefined
      ) {
        return harness.usage.context;
      }
    }
    return null;
  });

  /**
   * The newest `context_usage` answer itself, where one has been asked for.
   *
   * `latestContext` above keeps only the window out of it; the usage
   * panel's compaction threshold, category bar and breakdown need the
   * whole frame, which is what this hands them. `null` until a breakdown has been
   * requested once this page — and that is the honest state, not a
   * loading skeleton: the panel draws what it has.
   */
  const latestContextUsage = createMemo((): ContextUsage | null => {
    const events = relay.events();
    for (let i = events.length - 1; i >= 0; i -= 1) {
      const entry = events[i];
      if (
        entry !== undefined &&
        entry.event.type === "harness" &&
        entry.event.event.type === "context_usage"
      ) {
        return entry.event.event.usage;
      }
    }
    return null;
  });

  /**
   * The models this session's agent offers.
   *
   * The list the agent itself reported over the relay is the newest and
   * wins; until it has said, the linked account's list — which is what the
   * last session on it reported, or flyco's built-in one — stands in.
   */
  const models = createMemo<ModelOption[]>(() => {
    const events = relay.events();
    for (let i = events.length - 1; i >= 0; i -= 1) {
      const entry = events[i];
      if (entry !== undefined && entry.event.type === "models") {
        return entry.event.models;
      }
    }
    const harness = session()?.harness;
    return readiness.harness().find((account) => account.harness === harness)?.models ?? [];
  });

  /**
   * How much of the plan behind this session's harness account is spent.
   *
   * The newest snapshot the agent reported over the relay wins; until it
   * has reported one, the linked account's — which is what the last session
   * on it filed — stands in, so the rings are right on a page opened before
   * the machine says anything. Empty until *something* has asked the
   * vendor, which is the honest state: nothing is drawn.
   */
  const planUsage = createMemo<UsageWindow[]>(() => {
    const events = relay.events();
    for (let i = events.length - 1; i >= 0; i -= 1) {
      const entry = events[i];
      if (entry !== undefined && entry.event.type === "plan_usage") {
        return orderedWindows(entry.event.windows);
      }
    }
    const harness = session()?.harness;
    return orderedWindows(
      readiness.harness().find((account) => account.harness === harness)?.usage ?? [],
    );
  });

  /**
   * The slash commands this session's agent offers.
   *
   * Per session and never per account, unlike the model list above it: the
   * set carries the checkout's own skills, so the last session's answer
   * says nothing about this one's. Until the daemon has reported one the
   * palette shows flyco's own three and nothing else.
   */
  const commands = createMemo<HarnessCommand[]>(() => {
    const events = relay.events();
    for (let i = events.length - 1; i >= 0; i -= 1) {
      const entry = events[i];
      if (entry !== undefined && entry.event.type === "commands") {
        return entry.event.commands;
      }
    }
    return [];
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
          usageLimit: session()?.usage_limit,
          now: now(),
        });
  });

  /**
   * What the composer says about a message it will not deliver yet.
   *
   * Only the plan wait has anything to say: every other state that holds a
   * message — a machine still being built, a daemon reconnecting — delivers
   * it within the minute, and a line about it would be chrome that appears
   * and disappears. This wait is measured in hours, so the field says where
   * the message goes before it is typed rather than after it is sent.
   */
  const deferredNote = createMemo(() => {
    if (status()?.status !== "usage_limit") {
      return undefined;
    }
    const resets = session()?.usage_limit?.resets_at_unix;
    return resets === undefined
      ? "Sent when the plan's window resets"
      : `Sent when the window resets, at ${formatTimeOfDay(resets)}`;
  });

  /**
   * Whether this session can be written to at all.
   *
   * A composer that would refuse every message is not a composer, so the
   * state notice takes its place rather than sitting above a box nobody
   * can use (issue #133).
   */
  const refused = createMemo(() => {
    const view = status();
    return view !== undefined && REFUSING.has(view.status);
  });

  /**
   * Whether a daemon is holding the room — what a `!` command, a
   * `/compact`, a terminal keystroke or a context breakdown needs.
   *
   * Two facts answer it, because each sees a failure the other cannot: the
   * machine's own state, which knows a stopped or still-building machine
   * cannot be holding a daemon; and the room's `machine_connection`
   * frames, which know a running machine whose daemon fell off the
   * network. Prompts are deliberately not gated on this — the mailbox
   * holds them — only what is delivered-or-nothing is.
   */
  const machineUp = createMemo(
    () => machine()?.state === "running" && signals().machineOffline !== true,
  );

  const [error, setError] = createSignal<unknown>(null);
  const [deciding, setDeciding] = createSignal(false);
  const [resuming, setResuming] = createSignal(false);
  const [archiving, setArchiving] = createSignal(false);
  const [settingBudget, setSettingBudget] = createSignal(false);
  const [settingModel, setSettingModel] = createSignal(false);
  const [settingMode, setSettingMode] = createSignal(false);
  const [pendingDirtySummary, setPendingDirtySummary] = createSignal<string | null>(null);
  const [drawerOpen, setDrawerOpen] = createSignal(false);
  /**
   * Requests to open the machine's popover — `⋯` Resize, `/resize`, the
   * drawer's offline notice — or the drawer's `.env` tab (`Edit .env`).
   * Each carries its instant so that asking twice is two requests.
   */
  const [machinePanelAt, setMachinePanelAt] = createSignal<number>();
  const [machineResizeAt, setMachineResizeAt] = createSignal<number>();
  const [envPanelAt, setEnvPanelAt] = createSignal<number>();
  /**
   * The instant the agent last stepped onto the screen — `desktop_active`
   * is the daemon's once-per-burst announcement, and it is what opens the
   * drawer's `Screen` tab without the user going looking for it.
   */
  const [screenPanelAt, setScreenPanelAt] = createSignal<number>();
  let screenSeen = 0;
  createEffect(() => {
    const events = relay.events();
    for (let i = screenSeen; i < events.length; i += 1) {
      const entry = events[i];
      // `desktop_active` is recorded, so catch-up replays it — a minute-old
      // announcement from a session being opened now is history, not a
      // knock on the drawer's door.
      if (
        entry?.event.type === "desktop_active" &&
        entry.atUnix > Date.now() / 1000 - 60
      ) {
        setScreenPanelAt(Date.now());
      }
    }
    screenSeen = events.length;
  });

  function requestPanel(request: { panel: "machine" | "env"; resize?: boolean }): void {
    const at = Date.now();
    if (request.panel === "machine") {
      setMachinePanelAt(at);
      if (request.resize === true) {
        setMachineResizeAt(at);
      }
      return;
    }
    setEnvPanelAt(at);
  }

  /**
   * Runs one control, showing whatever it throws instead of losing it.
   */
  async function attempt(action: () => Promise<void>): Promise<void> {
    setError(null);
    try {
      await action();
    } catch (failure) {
      setError(failure);
    }
  }

  /**
   * Sends what was typed to whichever of the two it was addressed to.
   *
   * A message beginning with `!` is for the machine's bash, not for the
   * agent (docs/ux.md §9.3). A user message always goes — the room's
   * command log holds it for a daemon that is away or a machine still
   * being built, and the `/messages` handler holds it against a plan
   * window's wait (docs/ux.md §9.8) — but a shell command recorded now and
   * run whenever the daemon comes back would run against a working tree
   * the user is no longer looking at. So `!` asks that the stream be
   * live, and the composer says so when it is not rather than swallowing
   * the command.
   */
  function onSend(text: string): void {
    const command = shellCommandIn(text);
    if (command === null) {
      void attempt(() => relay.send({ type: "user_message", text }));
      return;
    }
    setError(null);
    if (relay.state() !== "live") {
      setError(new Error("Reconnecting to the session — a shell command needs a live connection."));
      return;
    }
    void attempt(() => relay.send({ type: "shell_command", command }));
  }

  function onStop(): void {
    void attempt(() => relay.send({ type: "interrupt" }));
  }

  /**
   * Asks the daemon what the context window holds.
   *
   * The usage panel's "detailed breakdown" is the only caller — there is
   * no `/context` in the palette; the ring is the door (docs/ux.md §9.3).
   * The question goes to the daemon, never to the model, and its answer
   * comes back on the stream as a `context_usage` event, which
   * `latestContextUsage` picks up for the panel.
   */
  function requestContextBreakdown(): void {
    void attempt(() => relay.send({ type: "context_usage" }));
  }

  function onCommand(command: SessionCommand): void {
    switch (command) {
      case "compact":
        void attempt(() => relay.send({ type: "compact" }));
        break;
      case "archive":
        void onArchive(false);
        break;
      case "resize":
        // Resizing is a choice among machine types, and the machine chip's
        // panel is where that choice is made; the request carries the
        // intent so the panel opens on the control rather than beside it
        // (issue #138).
        requestPanel({ panel: "machine", resize: true });
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
      mutateSession(
        await updateSession(params.id, {
          budgetLimit: dollarsToUsdMicros(dollars),
        }),
      );
    } catch (failure) {
      setError(failure);
    } finally {
      setSettingBudget(false);
    }
  }

  /**
   * Moves the session onto another model.
   *
   * Like the budget, not shown before the answer lands: the change has to
   * reach the running agent through its room, and a chip that read the new
   * model while the agent was still on the old one would be a claim the
   * next turn could contradict. The answer is the session already on it.
   */
  async function onSetModel(choice: ModelChoice): Promise<void> {
    if (settingModel()) {
      return;
    }
    setError(null);
    setSettingModel(true);
    try {
      mutateSession(await updateSession(params.id, { model: choice }));
    } catch (failure) {
      setError(failure);
    } finally {
      setSettingModel(false);
    }
  }

  /**
   * Moves the session onto another permission mode.
   *
   * On the model's terms exactly: the change has to reach the running
   * agent through its room, so the chip shows the answer the control
   * plane returned rather than the click that asked for it.
   */
  async function onSetMode(mode: PermissionMode): Promise<void> {
    if (settingMode()) {
      return;
    }
    setError(null);
    setSettingMode(true);
    try {
      mutateSession(await updateSession(params.id, { permissionMode: mode }));
    } catch (failure) {
      setError(failure);
    } finally {
      setSettingMode(false);
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
      await (action === "start" ? startSessionMachine(params.id) : stopSessionMachine(params.id));
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

  /**
   * The session's cumulative accounting, for the ring panel's `This
   * session` — the answer a `/usage` command used to spell out: what the
   * last usage report counted, plus how long the turns have run. `null`
   * while nothing has been metered and no turn has finished.
   */
  const sessionTotals = createMemo((): SessionTotals | null => {
    const usage = latestUsage();
    const clock = now() / 1000;
    let worked = 0;
    for (const item of transcript) {
      if (item.kind !== "turn") {
        continue;
      }
      worked += Math.max(0, (item.endedAtUnix ?? clock) - item.startedAtUnix);
    }
    if (usage === null && worked === 0) {
      return null;
    }
    return {
      inputTokens: usage?.input_tokens ?? 0,
      outputTokens: usage?.output_tokens ?? 0,
      costMicros: usage?.estimated_cost ?? null,
      workedSeconds: worked,
    };
  });

  return (
    <section class={styles.page}>
      <SessionHeader
        session={session()}
        sessionId={params.id}
        connection={relay.state()}
        machine={machine()}
        onRename={(title) => void onRename(title)}
        onArchive={() => void onArchive(false)}
        archiving={archiving()}
        onStartMachine={() => void onMachine("start")}
        onStopMachine={() => void onMachine("stop")}
        drawerOpen={drawerOpen()}
        onToggleDrawer={() => setDrawerOpen((was) => !was)}
        onOpenPanel={requestPanel}
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
          <div class={styles.scroller} ref={scroller} onScroll={noteScroll}>
            <div ref={transcriptBody}>
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
              when={transcript.length > 0 || foldQueued()}
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
                items={transcript}
                repo={session()?.repo ?? "the repository"}
                provider={providerLabel()}
                models={models()}
                onDecide={(id, decision) => {
                  void onDecide(id, decision).then(() => refetchSession());
                }}
                deciding={deciding()}
                now={now()}
                stoppedAtUnix={stoppedAtUnix()}
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
                  steps={[
                    {
                      key: "awaiting",
                      stage: "reserving",
                      atUnix: current().created_at_unix,
                    },
                  ]}
                  recovery={status()?.status === "migrating"}
                  attempt={1}
                  endedAtUnix={null}
                  repo={current().repo}
                  provider={providerLabel()}
                  now={now()}
                  stoppedAtUnix={stoppedAtUnix()}
                />
              )}
            </Show>

            {/*
            The agent at work, said where the work appears (docs/ux.md §6).
            A pill in the header said `Working` from across the page; this
            says it at the foot of the transcript, where the next line will
            land, and goes away the moment it does.
          */}
            <Show when={status()?.status === "working"}>
              <p class={styles.working} aria-live="polite">
                <span class={styles.workingDot} aria-hidden="true" />
                Working…
              </p>
            </Show>
            </div>
          </div>

          {/*
            The state notice and the composer are the same slot, because
            they answer the same question — what can I do next. A session
            that cannot be written to shows why instead of showing a box
            that will refuse; one that can, and has something to say about
            itself first, says it directly above the box.
          */}
          <div class={styles.composer}>
            <Show when={notice()}>
              {(state) => (
                <section
                  class={styles.stateNotice}
                  data-tone={state().tone}
                  aria-label="Session state"
                >
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
            <Show when={!refused()}>
              <SessionComposer
                turnInFlight={status()?.status === "working"}
                commands={commands()}
                onSend={onSend}
                onStop={onStop}
                onCommand={onCommand}
                machineUp={machineUp()}
                deferred={deferredNote()}
                controls={
                  /*
                    The session's own row (docs/ux.md §9.3): what it runs
                    on, what that costs, and what it may spend — each a
                    readout that opens the control that changes it, where
                    the official composers keep the same things.
                  */
                  <div class={composerStyles.chips}>
                    <Show when={machine()}>
                      {(view) => (
                        <Popover
                          label="Machine"
                          panelClass={composerStyles.popoverWide}
                          openAt={machinePanelAt()}
                          trigger={(attrs) => (
                            <button
                              id={attrs.id}
                              onClick={attrs.onClick}
                              aria-expanded={attrs.expanded()}
                              aria-haspopup="dialog"
                              type="button"
                              class={composerStyles.chip}
                              title="Machine"
                            >
                              <Server size={13} aria-hidden="true" />
                              <span class={composerStyles.chipLabel}>{machineChip(view())}</span>
                            </button>
                          )}
                        >
                          {() => (
                            <MachinePanel
                              sessionId={params.id}
                              machine={view()}
                              openResize={machineResizeAt()}
                              onChanged={() => void refetchMachine()}
                              embedded
                            />
                          )}
                        </Popover>
                      )}
                    </Show>
                    <Show when={session()}>
                      {(current) => (
                        <BudgetRaise
                          limitUsd={usdMicrosToDollars(current().budget.limit)}
                          spentUsd={usdMicrosToDollars(current().budget.spent)}
                          saving={settingBudget()}
                          onSet={(dollars) => void onSetBudget(dollars)}
                          trigger={(attrs) => (
                            <button
                              id={attrs.id}
                              onClick={attrs.onClick}
                              aria-expanded={attrs.expanded()}
                              aria-haspopup="dialog"
                              type="button"
                              class={composerStyles.chip}
                              title="Set the compute budget"
                            >
                              <Wallet size={13} aria-hidden="true" />
                              <span class={composerStyles.chipLabel}>
                                ${(budgetSpentUsd() ?? 0).toFixed(2)} / $
                                {(budgetLimitUsd() ?? 0).toFixed(0)}
                              </span>
                            </button>
                          )}
                        />
                      )}
                    </Show>
                    {/*
                      The goal is a setting of the session, not a line in
                      it — a chip like the model's, offered only where the
                      running harness says it takes one.
                    */}
                    <Show when={commands().find((command) => command.name === "goal")}>
                      {(command) => (
                        <GoalChip
                          description={command().description}
                          onSet={(condition) => onSend(`/goal ${condition}`)}
                        />
                      )}
                    </Show>
                  </div>
                }
                trailing={
                  <>
                    {/*
                      At the right, beside send, where both official apps
                      keep their model: the last thing checked before a
                      message goes out. These stay in the box when the
                      chips island — the chips say where the session runs,
                      these say what the next turn runs under.
                    */}
                    {/*
                      The mode, beside the model: both say what the next
                      turn runs under, and both reach the agent the same
                      way. The list is the harness's own — Codex has no
                      `dontAsk` worth a second row (src/lib/modes.ts).
                    */}
                    <Show when={session()}>
                      {(current) => (
                        <ModeChip
                          modes={modesFor(current().harness)}
                          mode={current().permission_mode}
                          saving={settingMode()}
                          align="end"
                          onChoose={(mode) => void onSetMode(mode)}
                        />
                      )}
                    </Show>
                    <Show when={session() !== undefined && models().length > 0 && session()}>
                      {(current) => (
                        <>
                          <ModelChip
                            models={models()}
                            choice={current().model}
                            saving={settingModel()}
                            align="end"
                            onChoose={(choice) => void onSetModel(choice)}
                          />
                          <EffortChip
                            models={models()}
                            choice={current().model}
                            saving={settingModel()}
                            align="end"
                            onChoose={(choice) => void onSetModel(choice)}
                          />
                        </>
                      )}
                    </Show>
                    {/*
                      The usage ring, beside send, where the official
                      composers keep it: how full the context window is,
                      and one tap opens the panel with the plan windows
                      beside it (docs/ux.md §9.3). Nothing is drawn until a
                      harness has reported something — a ring at zero over
                      a context flyco has never been told is an invention.
                    */}
                    <Show when={latestContext() !== null || planUsage().length > 0}>
                      <ContextRing
                        context={latestContext()}
                        usage={latestContextUsage()}
                        windows={planUsage()}
                        session={sessionTotals()}
                        now={now()}
                        machineUp={machineUp()}
                        onBreakdown={requestContextBreakdown}
                      />
                    </Show>
                  </>
                }
              />
            </Show>
          </div>
        </div>

        <SessionDrawer
          sessionId={params.id}
          relay={relay}
          machineUp={machineUp()}
          liveRepoSummary={liveRepoSummary()}
          open={drawerOpen()}
          onOpenChange={setDrawerOpen}
          openEnv={envPanelAt()}
          openScreen={screenPanelAt()}
          computerUse={session()?.computer_use === true}
          onError={setError}
        />
      </div>
    </section>
  );
}
