/**
 * The one control that picks a machine.
 *
 * Two places choose one and they are the same question asked twice: the home
 * composer's compute chip, before a session exists, and the drawer's `Resize`,
 * which moves a session already running onto another type (docs/ux.md §7.7,
 * §9.5). So the tiered slider, the failure that leaves it with nothing to
 * offer, and the line under it are written once here, and each caller supplies
 * only what differs — where it is rendered, what the line says, and, where the
 * change is a request rather than a field, the action that commits it.
 *
 * Controlled, like {@link BudgetPicker}: the chosen entry lives with the
 * caller, because the caller is what renders the reading beside the control
 * and a draft hidden in here would leave the two disagreeing.
 */
import { type JSX, Show, createMemo, createSignal } from "solid-js";
import { AlertTriangle } from "lucide-solid";
import MachineSlider, { type MachineFilter } from "./MachineSlider";
import ProblemNotice from "./ProblemNotice";
import type {
  MachineCatalogEntry,
  MachineDefault,
  MachineView,
  ProviderAccountView,
} from "../api/client";
import { entryKey } from "../lib/machines";
import styles from "./MachinePicker.module.css";

export interface MachinePickerProps {
  /** The whole curated catalog, every account and region. */
  catalog: MachineCatalogEntry[];
  /** The caller's linked accounts, for the account filter's labels. */
  accounts: ProviderAccountView[];
  /** What flyco would pick on its own, where that is offered. */
  automatic?: MachineDefault | undefined;
  /** The entry whose account and region the detents open on. */
  anchor?: MachineCatalogEntry | undefined;
  /** Whether the leftmost detent hands the choice back to flyco. */
  allowAuto?: boolean | undefined;
  /** Which dimensions `Advanced` offers. */
  filters?: readonly MachineFilter[] | undefined;
  /** Whether prices are quoted against spot capacity. */
  spot: boolean;
  chosenKey: string | null;
  onChoose: (key: string | null) => void;
  onSpot?: ((spot: boolean) => void) | undefined;
  /** Why the catalog is missing, when the request for it failed. */
  error?: unknown;
  /**
   * What to say instead of the control, while flyco is still reading what
   * the account can deploy. See [`MachineSliderProps.pending`].
   */
  pending?: string | undefined;
  /** The line under the slider: what choosing here means. */
  note?: JSX.Element | undefined;
  /** What commits the choice, for a caller whose choice is a request. */
  children?: JSX.Element | undefined;
}

export default function MachinePicker(props: MachinePickerProps) {
  return (
    <div class={styles.picker}>
      {/* The slider has nothing to offer when the catalog never arrived, so
          the panel says that instead of showing an empty track with no
          explanation. */}
      <ProblemNotice error={props.error} />
      <MachineSlider
        catalog={props.catalog}
        accounts={props.accounts}
        automatic={props.automatic}
        anchor={props.anchor}
        allowAuto={props.allowAuto}
        filters={props.filters}
        spot={props.spot}
        chosenKey={props.chosenKey}
        onChoose={props.onChoose}
        onSpot={props.onSpot}
        pending={props.pending}
      />
      <Show when={props.note}>{(note) => <p class={styles.note}>{note()}</p>}</Show>
      {props.children}
    </div>
  );
}

/**
 * The dimensions a resize can carry.
 *
 * `POST /v1/sessions/{id}/machine/resize` takes a machine type and nothing
 * else: the session keeps its account, its region and its capacity mode, so
 * those three are not offered. Architecture and operating system are, because
 * both are properties of the type itself — a session on Linux/x86 can be moved
 * onto arm64, and onto a Mac if the account has one.
 */
const RESIZE_FILTERS: readonly MachineFilter[] = ["architecture", "os"];

/** What a resize does to the machine, stated before it is asked for. */
export const RESIZE_CONFIRMATION = "Restarts the machine; the disk is kept.";

export interface MachineResizeProps {
  catalog: MachineCatalogEntry[];
  accounts: ProviderAccountView[];
  /** Why the catalog is missing, when the request for it failed. */
  error?: unknown;
  /** The machine the session is on now. */
  current: MachineView;
  /** Whether a resize is in flight. */
  saving: boolean;
  /** Moves the session onto this machine type. */
  onResize: (machineType: string) => void;
  /** Abandons the resize. */
  onCancel: () => void;
}

/**
 * Moving a session already running onto another machine type.
 *
 * The type it is on is where the thumb starts, so the control opens on the
 * truth rather than on an empty `Choose a new type…`; there is no `Auto`,
 * because flyco choosing again is not one of the outcomes; and the button
 * names the machine it would move to, with the two consequences stated
 * beside it. A user who reaches the same detent it started on has changed
 * nothing, and the button says so rather than spending a restart.
 */
export function MachineResize(props: MachineResizeProps) {
  /**
   * The catalog entry the session's machine is, so the detents open on its
   * account and region.
   *
   * A machine whose exact type is no longer curated still has to open the
   * track somewhere, and the account and region it is in are the part that
   * decides which machines the resize can even reach — so the anchor falls
   * back to any entry from the same place.
   */
  const current = createMemo(() => {
    const spec = props.current.spec;
    const here = props.catalog.filter(
      (entry) => entry.provider === spec.provider && entry.region === props.current.region,
    );
    return here.find((entry) => entry.machine_type === spec.machine_type) ?? here[0];
  });

  const [draft, setDraft] = createSignal<string | null>(null);
  const chosenKey = createMemo(() => {
    const held = draft();
    if (held !== null) {
      return held;
    }
    const entry = current();
    return entry === undefined ? null : entryKey(entry);
  });

  /** The machine the button would move to, once the thumb has landed. */
  const chosen = createMemo(() =>
    props.catalog.find((entry) => entryKey(entry) === chosenKey()),
  );

  /** Whether the thumb is back where it started, which is not a resize. */
  const unchanged = createMemo(() => chosen()?.machine_type === props.current.spec.machine_type);

  /**
   * What the button says, in one memo rather than nested `Show`s: the text
   * changes with the thumb while the button itself never comes or goes, and
   * a `Show` callback that returns a string would be read once and then
   * quietly stop following the slider.
   */
  const commitLabel = createMemo(() => {
    if (props.saving) {
      return "Resizing…";
    }
    const entry = chosen();
    if (entry === undefined) {
      return "Resize";
    }
    return unchanged() ? `Already on ${entry.machine_type}` : `Resize to ${entry.machine_type}`;
  });

  return (
    <MachinePicker
      catalog={props.catalog}
      accounts={props.accounts}
      anchor={current()}
      allowAuto={false}
      filters={RESIZE_FILTERS}
      spot={props.current.spot}
      chosenKey={chosenKey()}
      onChoose={setDraft}
      error={props.error}
    >
      <p class={styles.confirm}>
        <AlertTriangle size={14} aria-hidden="true" />
        {RESIZE_CONFIRMATION}
      </p>
      <div class={styles.actions}>
        <button
          type="button"
          class={styles.commit}
          disabled={props.saving || unchanged() || chosen() === undefined}
          onClick={() => {
            const entry = chosen();
            if (entry !== undefined) {
              props.onResize(entry.machine_type);
            }
          }}
        >
          {commitLabel()}
        </button>
        <button type="button" class={styles.cancel} disabled={props.saving} onClick={() => props.onCancel()}>
          Cancel
        </button>
      </div>
    </MachinePicker>
  );
}
