import { Show, createEffect, createMemo, createSignal, on } from "solid-js";
import { createQuery } from "../lib/query";
import { MachineResize } from "./MachinePicker";
import ProblemNotice from "./ProblemNotice";
import {
  getMachineCatalog,
  getSessionMachine,
  listProviders,
  resizeSessionMachine,
  startSessionMachine,
  stopSessionMachine,
} from "../api/client";
import type { MachineCatalogEntry, MachineView } from "../api/client";
import {
  MACHINE_STATE_LABEL,
  capacityParts,
  hourlyLabel,
  runtimeOf,
  shortMachineType,
} from "../lib/machines";
import { formatUsd } from "../lib/money";
import { PROVIDER_LABEL } from "../lib/providers";
import styles from "./MachinePanel.module.css";

export interface MachinePanelProps {
  sessionId: string;
  /**
   * A request to open the resize control, from `/resize` in the composer.
   *
   * The instant it was asked for, so that asking twice is two requests: a
   * user who cancelled a resize and typed `/resize` again would otherwise
   * set an unchanged signal and see nothing happen.
   */
  openResize?: number | undefined;
}

/**
 * The machine a session runs on: type, region, lifecycle state, whether it
 * actually holds spot capacity, and what it costs per hour — plus
 * start/stop/resize. There is no dedicated relay event for machine state
 * changes, so this refetches after every action rather than listening for
 * one.
 */
export default function MachinePanel(props: MachinePanelProps) {
  const [machine, { refetch }] = createQuery(() => props.sessionId, getSessionMachine);
  const [catalog] = createQuery(() => getMachineCatalog());
  // The account labels the slider's filters read; the catalog itself only
  // carries account ids.
  const [providers] = createQuery(() => listProviders());
  const [resizing, setResizing] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  // `/resize` in the composer asks for this control, not for this tab: a
  // command that opened a drawer and left the user to find the button is a
  // command that did half of what it said (issue #138). Not deferred: the
  // request is usually what mounted this panel in the first place, and the
  // instant it carries is what makes a second `/resize` a second request.
  createEffect(
    on(
      () => props.openResize,
      (at) => {
        if (at !== undefined) {
          setResizing(true);
        }
      },
    ),
  );

  async function onStart(): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      await startSessionMachine(props.sessionId);
      await refetch();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  async function onStop(): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      await stopSessionMachine(props.sessionId);
      await refetch();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  async function onResize(machineType: string): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      await resizeSessionMachine(props.sessionId, machineType);
      await refetch();
      setResizing(false);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  /** The catalog row this machine was chosen from, for what the row lacks. */
  // The catalog row this machine was picked from, for the capacity a
  // container's opaque type name does not carry and the price the view has
  // not learned yet.
  const entry = createMemo<MachineCatalogEntry | undefined>(() => {
    const view = machine();
    if (view === undefined) {
      return undefined;
    }
    return catalog()?.entries.find(
      (candidate) =>
        candidate.provider === view.spec.provider &&
        candidate.region === view.region &&
        candidate.machine_type === view.spec.machine_type,
    );
  });

  return (
    <section class={styles.panel} aria-label="Machine">
      {/* The catalog is what the resize list is built from, so its failure
          belongs here too: without it the dropdown is empty for a reason the
          panel would otherwise never give. */}
      <ProblemNotice error={machine.error ?? catalog.error} />
      <Show when={!machine.loading}>
        <Show
          when={machine()}
          fallback={<p class={styles.empty}>No machine yet: one is reserved when the session starts.</p>}
        >
          {(view) => (
            <>
              {/* One machine, said once: what it is and where it is in its
                  life on the first line, where it runs and what it costs on
                  the second. A session has exactly one, so this is a fact,
                  not a list. */}
              <div class={styles.headline}>
                <span class={styles.name}>{machineName(view(), entry())}</span>
                <span class={styles.state} data-state={view().state}>
                  {MACHINE_STATE_LABEL[view().state]}
                </span>
              </div>
              <p class={styles.where}>{whereLabel(view(), entry())}</p>
              <ProblemNotice error={error()} />
              <div class={styles.actions}>
                <Show
                  when={view().state === "running"}
                  fallback={
                    <button type="button" disabled={busy()} onClick={() => void onStart()}>
                      Start
                    </button>
                  }
                >
                  <button type="button" disabled={busy()} onClick={() => void onStop()}>
                    Stop
                  </button>
                </Show>
                <Show when={!resizing()}>
                  <button type="button" disabled={busy()} onClick={() => setResizing(true)}>
                    Resize
                  </button>
                </Show>
              </div>
              {/* The same tiered slider the home composer picks a machine
                  with (docs/ux.md §7.7), on the machine this session is
                  already on. */}
              <Show when={resizing()}>
                <MachineResize
                  catalog={catalog()?.entries ?? []}
                  accounts={providers() ?? []}
                  error={catalog.error ?? providers.error}
                  current={view()}
                  saving={busy()}
                  onResize={(machineType) => void onResize(machineType)}
                  onCancel={() => setResizing(false)}
                />
              </Show>
            </>
          )}
        </Show>
      </Show>
    </section>
  );
}

/**
 * What the machine is called: the type the user picked for a virtual
 * machine, and the size for a managed container, because `aca-4x8` is
 * flyco's key and names nothing to a reader.
 */
function machineName(view: MachineView, entry: MachineCatalogEntry | undefined): string {
  if (entry !== undefined && runtimeOf(entry) === "container") {
    return ["Container", ...capacityParts(entry)].join(" · ");
  }
  return shortMachineType(view.spec.machine_type);
}

/**
 * Where the machine runs and what it costs, on one line.
 *
 * The price is the row's own hourly once the provider has answered, and
 * the catalog's before that — a machine still being reserved has no meter
 * yet, and "no metered price" on it would be a claim about a bill that has
 * not started. A machine the user owns has no meter at all, and says so.
 */
function whereLabel(view: MachineView, entry: MachineCatalogEntry | undefined): string {
  const price =
    view.hourly !== null && view.hourly !== undefined
      ? `${formatUsd(view.hourly)}/hr`
      : entry === undefined
        ? "no metered price"
        : hourlyLabel(entry, view.spot);
  return [
    PROVIDER_LABEL[view.spec.provider],
    view.region,
    view.spot ? "Spot" : "On-demand",
    price,
  ].join(" · ");
}
