import { For, Show, createSignal } from "solid-js";
import { createQuery } from "../lib/query";
import ProblemNotice from "./ProblemNotice";
import {
  getMachineCatalog,
  getSessionMachine,
  resizeSessionMachine,
  startSessionMachine,
  stopSessionMachine,
  type MachineCatalogEntry,
} from "../api/client";
import { MACHINE_STATE_LABEL } from "../lib/machines";
import { formatUsd } from "../lib/money";
import { PROVIDER_LABEL } from "../lib/providers";
import styles from "./MachinePanel.module.css";

/**
 * The machine a session runs on: type, region, lifecycle state, whether it
 * actually holds spot capacity, and what it costs per hour — plus
 * start/stop/resize. There is no dedicated relay event for machine state
 * changes, so this refetches after every action rather than listening for
 * one.
 */
export default function MachinePanel(props: { sessionId: string }) {
  const [machine, { refetch }] = createQuery(() => props.sessionId, getSessionMachine);
  const [catalog] = createQuery(() => getMachineCatalog());
  const [resizing, setResizing] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  const resizeOptions = () => {
    const provider = machine()?.spec.provider;
    if (provider === undefined) {
      return [];
    }
    return (catalog() ?? []).filter((entry: MachineCatalogEntry) => entry.provider === provider);
  };

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
                <Show
                  when={resizing()}
                  fallback={
                    <button type="button" disabled={busy()} onClick={() => setResizing(true)}>
                      Resize
                    </button>
                  }
                >
                  <select
                    disabled={busy()}
                    value=""
                    onChange={(event) => {
                      if (event.currentTarget.value !== "") {
                        void onResize(event.currentTarget.value);
                      }
                    }}
                  >
                    <option value="">Choose a new type…</option>
                    <For each={resizeOptions()}>
                      {(entry) => (
                        <option value={entry.machine_type}>
                          {entry.region} · {entry.machine_type}
                        </option>
                      )}
                    </For>
                  </select>
                  <button type="button" disabled={busy()} onClick={() => setResizing(false)}>
                    Cancel
                  </button>
                </Show>
              </div>
            </>
          )}
        </Show>
      </Show>
    </section>
  );
}
