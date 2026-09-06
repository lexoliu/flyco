/**
 * Settings: one section at a time, in a column of its own.
 *
 * The sections themselves live in the shell's rail
 * ({@link ../../components/SettingsNav}) rather than in a second nav column
 * beside it, so this is only the page the reader asked for.
 *
 * The nine tabs the five sections replaced were a map of the database — one
 * tab per table — and asked the user to know that "harness accounts" and
 * "harness features" were different things before they could find either.
 * The five of docs/ux.md §10 are named after what the user came to change:
 * the agent, the computer, the tools, the instructions, the account.
 */
import { type JSX } from "solid-js";
import styles from "./Settings.module.css";

export default function SettingsLayout(props: { children?: JSX.Element }) {
  return (
    <div class={styles.page}>
      {/* The rail already says Settings; repeating it in 28px type would
          push the section the user asked for below the fold on a laptop. */}
      <h1 class="visually-hidden">Settings</h1>
      <div class={styles.content}>{props.children}</div>
    </div>
  );
}
