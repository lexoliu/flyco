/**
 * One machine the user owns, in the same card every cloud account gets.
 *
 * A host is a provider account like any other (docs/host-enrollment.md), so
 * it belongs in the compute card rather than in a list of its own — but the
 * three facts a cloud account states are the three a host does not have. It
 * has no price, because the user already bought it; no spot mode, because
 * nobody is going to evict them from their own hardware; and no invoice.
 * What it has instead is what it told flyco about itself, and whether its
 * daemon is attached right now.
 *
 * `Rename` and `Remove` are here rather than in a menu because they are the
 * only two things anyone does to a machine after enrolling it. `Remove`
 * asks first — it unenrols hardware and stops whatever is on it — and a
 * removal refused by `host-has-active-sessions` is not an error: it is the
 * control plane saying how much work is still running there, and the same
 * dialog repeats the number before offering to stop it anyway.
 */
import { Show, createSignal } from "solid-js";
import { Server } from "lucide-solid";
import { CardShell, Fact, YourHardware } from "./ComputeCard";
import ConfirmDialog from "./ConfirmDialog";
import { removeHost, renameHost, type HostView } from "../api/client";
import { formatDate } from "../lib/dates";
import { inUseRefusal, sessionsStillRunning, type InUseRefusal } from "../lib/inUse";
import {
  HOST_STATE_LABEL,
  hostCapacityLabel,
  hostDiskLabel,
  hostSoftwareLabel,
} from "../lib/hosts";
import { ARCHITECTURE_LABEL } from "../lib/machines";
import { relativeTime } from "../lib/relativeTime";
import styles from "./ComputeCard.module.css";

export interface HostCardProps {
  /** The machine this card is about. */
  host: HostView;
  /** Called after a rename or a removal, so the list can be reloaded. */
  onChanged: () => unknown;
  /** Whether `Rename` and `Remove` are on offer at all. */
  editable?: boolean | undefined;
}

export default function HostCard(props: HostCardProps) {
  const [renaming, setRenaming] = createSignal(false);
  const [label, setLabel] = createSignal(props.host.label);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  /** Whether the removal has been asked about, and how it was answered. */
  const [confirming, setConfirming] = createSignal(false);
  // What a refused removal said, so `Remove anyway` states what it is about
  // to stop rather than asking for a blank confirmation.
  const [refused, setRefused] = createSignal<InUseRefusal | null>(null);

  const editable = () => props.editable !== false;

  async function rename(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const next = label().trim();
    if (next === "" || next === props.host.label) {
      setRenaming(false);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await renameHost(props.host.id, next);
      setRenaming(false);
      await props.onChanged();
    } catch (failure) {
      setError(failure);
    } finally {
      setBusy(false);
    }
  }

  async function remove(force: boolean): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      await removeHost(props.host.id, { force });
      setRefused(null);
      setConfirming(false);
      await props.onChanged();
    } catch (failure) {
      const inUse = inUseRefusal(failure);
      if (inUse === null) {
        setError(failure);
      } else {
        // Not an error: the machine is busy, and `force` is what the
        // control plane says to send once the user knows how busy.
        setRefused(inUse);
      }
    } finally {
      setBusy(false);
    }
  }

  function stopConfirming(): void {
    setConfirming(false);
    setRefused(null);
  }

  return (
    <CardShell
      mark={<Server size={16} aria-hidden="true" />}
      title={
        <Show when={renaming()} fallback={props.host.label}>
          <form class={styles.rename} onSubmit={(event) => void rename(event)}>
            <input
              class={styles.renameInput}
              aria-label={`Name for ${props.host.label}`}
              value={label()}
              onInput={(event) => setLabel(event.currentTarget.value)}
              autocomplete="off"
              spellcheck={false}
              autofocus
            />
            <button type="submit" class={styles.action} disabled={busy()}>
              Save
            </button>
            <button
              type="button"
              class={styles.action}
              onClick={() => {
                setLabel(props.host.label);
                setRenaming(false);
              }}
            >
              Cancel
            </button>
          </form>
        </Show>
      }
      meta={
        <>
          Your own machine · enrolled {formatDate(props.host.created_at_unix)}
          <Show when={props.host.state !== "online" && props.host.last_seen_unix}>
            {(seen) => <> · last seen {relativeTime(seen(), Date.now())}</>}
          </Show>
        </>
      }
      status={
        <span class={styles.state}>
          <span class={styles.dot} data-state={props.host.state} aria-hidden="true" />
          {HOST_STATE_LABEL[props.host.state]}
        </span>
      }
      actions={
        // Nothing else while the question is up: the dialog under the card
        // is the one thing being answered, and a second `Remove` beside it
        // would be two buttons with one meaning.
        <Show when={editable() && !renaming() && !confirming()}>
          <button type="button" class={styles.action} onClick={() => setRenaming(true)}>
            Rename
          </button>
          <button
            type="button"
            class={styles.danger}
            disabled={busy()}
            onClick={() => setConfirming(true)}
          >
            Remove
          </button>
        </Show>
      }
      footer={
        <>
          <Show when={confirming()}>
            <ConfirmDialog
              title={`Remove ${props.host.label}?`}
              body={
                <Show
                  when={refused()}
                  fallback={
                    <>
                      Flyco stops managing this machine and no new session can be placed on it.
                      The machine keeps its disks, and enrolling it again brings it back.
                    </>
                  }
                >
                  {(inUse) => (
                    <>
                      {sessionsStillRunning(inUse())} Removing it stops their containers and keeps
                      the disks, so the work stays on your hardware.
                    </>
                  )}
                </Show>
              }
              tone="danger"
              confirmLabel={refused() === null ? "Remove" : "Remove anyway"}
              cancelLabel="Keep it"
              busy={busy()}
              onConfirm={() => void remove(refused() !== null)}
              onCancel={stopConfirming}
            />
          </Show>
          <Show when={error()}>
            {(failure) => (
              <p class={styles.error} role="alert">
                {failure() instanceof Error ? (failure() as Error).message : String(failure())}
              </p>
            )}
          </Show>
        </>
      }
    >
      <Fact label="Machine">
        {ARCHITECTURE_LABEL[props.host.facts.architecture]}
        {" · "}
        {hostCapacityLabel(props.host.facts)}
        {/* The card opens under the machine's own hostname, so repeating it
            here would say the same thing twice. A renamed machine is the
            case where the hostname is worth stating. */}
        <Show when={props.host.facts.hostname !== props.host.label}>
          <span class={styles.dim}>
            {" · "}
            {props.host.facts.hostname}
          </span>
        </Show>
      </Fact>

      <Fact label="Disk">
        {hostDiskLabel(props.host.facts)}
        <span class={styles.dim}> · where Podman keeps containers and volumes</span>
      </Fact>

      <Fact label="Software">{hostSoftwareLabel(props.host.facts)}</Fact>

      <Fact label="What an hour costs">
        <YourHardware />
      </Fact>
    </CardShell>
  );
}
