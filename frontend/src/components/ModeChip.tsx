/**
 * How much the agent may do without asking, set from a chip rather than
 * a config file (docs/ux.md §9.3).
 *
 * One chip in the session composer, beside the model: the mode is a
 * setting of the session like the model is, and the same rule holds —
 * a change has to reach the running agent through its room, so the chip
 * shows the answer the control plane returned, never the click. What
 * the harness does with a mode is its own: Claude takes it natively,
 * Codex takes the approval/sandbox pair it maps to, and the list shows
 * only what the session's harness offers.
 */
import { For, Show } from "solid-js";
import { Dynamic } from "solid-js/web";
import { Ban, Check, Eye, FilePen, Hand, ShieldAlert, Sparkles } from "lucide-solid";
import Popover from "./Popover";
import type { PermissionMode } from "../api/wire";
import { cx } from "../lib/cx";
import { modeLabel, type ModeIcon, type ModeOption } from "../lib/modes";
import composer from "./Composer.module.css";
import styles from "./ModeChip.module.css";

export interface ModeChipProps {
  /** The modes the session's harness offers, in menu order. */
  modes: readonly ModeOption[];
  /** What the session runs under now. */
  mode: PermissionMode;
  /** The new mode, on every change. */
  onChoose: (mode: PermissionMode) => void;
  /**
   * Whether a change is still on its way to the agent.
   *
   * The chip costs a round trip like the model's does; while it waits it
   * dims rather than disables, so the reading stays legible.
   */
  saving?: boolean | undefined;
  /** Which edge of the chip the panel lines up with. */
  align?: "start" | "end" | undefined;
}

export default function ModeChip(props: ModeChipProps) {
  return (
    <Popover
      label="Mode"
      align={props.align}
      panelClass={styles.panel}
      trigger={(attrs) => (
        <button
          id={attrs.id}
          onClick={attrs.onClick}
          aria-expanded={attrs.expanded()}
          aria-haspopup="dialog"
          type="button"
          class={cx(composer.chip, props.saving === true && styles.saving)}
          title="Permission mode"
        >
          <ModeMark mode={props.mode} modes={props.modes} size={13} />
          <span class={composer.chipLabel}>{modeLabel(props.mode)}</span>
        </button>
      )}
    >
      {(close) => (
        <div class={composer.popover}>
          <p class={composer.popoverTitle}>Permission mode</p>
          <ul class={composer.options} role="listbox" aria-label="Permission mode">
            <For each={props.modes}>
              {(option) => (
                <li>
                  <button
                    type="button"
                    role="option"
                    aria-selected={option.id === props.mode}
                    class={cx(
                      composer.option,
                      styles.option,
                      option.unrestricted === true && styles.optionOpen,
                      option.id === props.mode && composer.optionChosen,
                    )}
                    onClick={() => {
                      props.onChoose(option.id);
                      close();
                    }}
                  >
                    <Dynamic
                      component={GLYPH[option.icon]}
                      class={cx(styles.optionIcon)}
                      size={16}
                      aria-hidden="true"
                    />
                    <span class={styles.optionText}>
                      <span class={styles.optionName}>{option.label}</span>
                      <span class={styles.optionDescription}>{option.description}</span>
                    </span>
                    <Show when={option.id === props.mode}>
                      <Check size={14} class={cx(styles.check)} aria-hidden="true" />
                    </Show>
                  </button>
                </li>
              )}
            </For>
          </ul>
        </div>
      )}
    </Popover>
  );
}

/**
 * The glyph each mode is drawn with — a hand for the one that stops to
 * ask, an eye for the one that only reads, an open shield for the one
 * that stops nothing.
 */
const GLYPH: Record<ModeIcon, typeof Hand> = {
  auto: Sparkles,
  ask: Hand,
  read: Eye,
  edits: FilePen,
  open: ShieldAlert,
  refuse: Ban,
};

/** The chosen mode's own mark, for the closed chip. */
function ModeMark(props: {
  mode: ModeChipProps["mode"];
  modes: ModeChipProps["modes"];
  size: number;
}) {
  const icon = (): ModeIcon =>
    props.modes.find((option) => option.id === props.mode)?.icon ?? "ask";
  return <Dynamic component={GLYPH[icon()]} size={props.size} aria-hidden="true" />;
}
