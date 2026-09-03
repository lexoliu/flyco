import { Show, createEffect, createSignal, on } from "solid-js";
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
import { MACHINE_STATE_LABEL } from "../lib/machines";
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

  return (
    <section class={styles.panel} aria-label="Machine">
      <h2>Machine</h2>
      {/* The catalog is what the resize list is built from, so its failure
          belongs here too: without it the dropdown is empty for a reason the
          panel would otherwise never give. */}
      <ProblemNotice error={machine.error ?? catalog.error} />
      <Show when={!machine.loading}>
        <Show when={machine()} fallback={<p class={styles.empty}>No machine provisioned for this session yet.</p>}>
          {(view) => (
            <>
              <span class={styles.state} data-state={view().state}>
                {MACHINE_STATE_LABEL[view().state]}
              </span>
              <div class={styles.facts}>
                <div class={styles.factRow}>
                  <span class={styles.factLabel}>Provider</span>
                  <span>{PROVIDER_LABEL[view().spec.provider]}</span>
                </div>
                <div class={styles.factRow}>
                  <span class={styles.factLabel}>Type</span>
                  <span>{view().spec.machine_type}</span>
                </div>
                <div class={styles.factRow}>
                  <span class={styles.factLabel}>Region</span>
                  <span>{view().region}</span>
                </div>
                <div class={styles.factRow}>
                  <span class={styles.factLabel}>Capacity</span>
                  <span>{view().spot ? "Spot" : "On-demand"}</span>
                </div>
                <div class={styles.factRow}>
                  <span class={styles.factLabel}>Price</span>
                  <span>{view().hourly !== null && view().hourly !== undefined ? `${formatUsd(view().hourly as number)}/hr` : "No metered price"}</span>
                </div>
              </div>

              <ProblemNotice error={error()} />
              <div class={styles.actions}>
                <button type="button" disabled={busy() || view().state === "running"} onClick={() => void onStart()}>
                  Start
                </button>
                <button type="button" disabled={busy() || view().state !== "running"} onClick={() => void onStop()}>
                  Stop
                </button>
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
                  catalog={catalog() ?? []}
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
