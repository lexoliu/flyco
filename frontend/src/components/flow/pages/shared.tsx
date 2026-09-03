/**
 * The pieces more than one page of the first run is built from.
 *
 * The choice cards carry radio semantics because a choice page asks one
 * question with several answers, and the answer must be visible before
 * `Next` is pressed: a card that navigated on click would be a second
 * primary in disguise.
 */
import { For, Show, type JSX } from "solid-js";
import { ArrowUpRight, Check } from "lucide-solid";
import { cx } from "../../../lib/cx";
import styles from "./pages.module.css";

export interface Choice<K extends string> {
  readonly kind: K;
  readonly title: string;
  /** One line under the title; a linked card says so here. */
  readonly line: string;
  readonly linked: boolean;
  readonly mark: JSX.Element;
}

export interface ChoiceCardsProps<K extends string> {
  /** The question, which names the radio group. */
  readonly question: string;
  readonly choices: readonly Choice<K>[];
  readonly value: K | null;
  readonly onChange: (kind: K) => void;
}

/** Selectable cards, one line each, exactly one of which can be chosen. */
export function ChoiceCards<K extends string>(props: ChoiceCardsProps<K>) {
  return (
    <ul class={styles.choices} role="radiogroup" aria-label={props.question}>
      <For each={props.choices}>
        {(choice) => {
          const chosen = () => props.value === choice.kind;
          return (
            <li>
              <button
                type="button"
                role="radio"
                aria-checked={chosen()}
                class={styles.choice}
                onClick={() => props.onChange(choice.kind)}
              >
                <span class={styles.choiceMark}>{choice.mark}</span>
                <span class={styles.choiceText}>
                  <span class={styles.choiceTitle}>{choice.title}</span>
                  <span class={cx(styles.choiceLine, choice.linked && styles.choiceLinked)}>
                    {choice.line}
                  </span>
                </span>
                <Show when={chosen()}>
                  <Check size={16} aria-hidden="true" class={styles.choiceCheck ?? ""} />
                </Show>
              </button>
            </li>
          );
        }}
      </For>
    </ul>
  );
}

/** A quiet link that opens a vendor's page in a tab of its own. */
export function ExternalLink(props: { href: string; children: JSX.Element }) {
  return (
    <a class={styles.link} href={props.href} target="_blank" rel="noreferrer noopener">
      {props.children}
      <ArrowUpRight size={13} aria-hidden="true" />
    </a>
  );
}

/** A quiet link that leads to another page of the flow, or swaps this one. */
export function QuietLink(props: { onClick: () => void; children: JSX.Element }) {
  return (
    <button type="button" class={styles.link} onClick={() => props.onClick()}>
      {props.children}
    </button>
  );
}

/** One read-only confirmation row. */
export function ConfirmRow(props: { label: string; value: string }) {
  return (
    <div class={styles.confirmRow}>
      <dt>{props.label}</dt>
      <dd>{props.value}</dd>
    </div>
  );
}

/** The live dot and the sentence naming what is being waited for. */
export function Waiting(props: { children: JSX.Element }) {
  return (
    <p class={styles.waiting} role="status">
      <span class={styles.pulse} aria-hidden="true" />
      {props.children}
    </p>
  );
}

/** Opens a vendor's page in a new tab, from a click the browser will allow. */
export function openInNewTab(url: string): void {
  window.open(url, "_blank", "noopener,noreferrer");
}
