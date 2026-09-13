/**
 * The session's goal, set from a chip rather than typed as a command
 * (docs/ux.md §9.3).
 *
 * `/goal <condition>` is a setting of the session — the agent keeps working
 * until the condition is met — and a setting is a control, not a line in
 * the conversation: it lives on a chip beside the model and the machine,
 * and the palette stops offering the command this replaces.
 */
import { createSignal } from "solid-js";
import { Target } from "lucide-solid";
import Popover from "./Popover";
import composer from "./Composer.module.css";
import styles from "./GoalChip.module.css";

export interface GoalChipProps {
  /** What a goal does, in the harness's own words. */
  description: string;
  /** Commits the condition — sent as the harness's `/goal` command. */
  onSet: (condition: string) => void;
}

export default function GoalChip(props: GoalChipProps) {
  const [condition, setCondition] = createSignal("");

  function set(close: () => void): void {
    const goal = condition().trim();
    if (goal === "") {
      return;
    }
    setCondition("");
    close();
    props.onSet(goal);
  }

  return (
    <Popover
      label="Goal"
      trigger={(attrs) => (
        <button
          id={attrs.id}
          onClick={attrs.onClick}
          aria-expanded={attrs.expanded()}
          aria-haspopup="dialog"
          type="button"
          class={composer.chip}
          title="Set the session's goal"
        >
          <Target size={13} aria-hidden="true" />
          <span class={composer.chipLabel}>Goal</span>
        </button>
      )}
    >
      {(close) => (
        <div class={composer.popover}>
          <p class={composer.popoverTitle}>Goal</p>
          <p class={styles.note}>{props.description}</p>
          <input
            class={styles.field}
            value={condition()}
            onInput={(event) => setCondition(event.currentTarget.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                set(close);
              }
            }}
            placeholder="until the tests pass"
            aria-label="Goal condition"
            autocomplete="off"
            autocapitalize="off"
            spellcheck={false}
          />
          <button
            type="button"
            class={styles.set}
            disabled={condition().trim() === ""}
            onClick={() => set(close)}
          >
            Set the goal
          </button>
        </div>
      )}
    </Popover>
  );
}
