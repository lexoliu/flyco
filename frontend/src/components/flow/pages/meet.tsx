/** Stage A: the one page that says what flyco is (docs/ux.md §4 A). */
import Logomark, {
  ANTHROPIC_MARK,
  AWS_MARK,
  AZURE_MARK,
  GOOGLE_CLOUD_MARK,
  OPENAI_MARK,
} from "../../Logomark";
import { NEXT, type PageComponent } from "../page";
import styles from "./pages.module.css";

export const Meet: PageComponent<{ id: "meet" }> = (props) => ({
  title: "Meet flyco",
  body: (
    <>
      <p class={styles.lede}>
        Flyco runs the official Claude Code and Codex on a computer you own. You
        bring the agent and the machine; flyco runs the session, keeps the
        budget, and gets out of the way.
      </p>
      <div class={styles.marks}>
        <Logomark mark={ANTHROPIC_MARK} size={20} labelled />
        <Logomark mark={OPENAI_MARK} size={20} labelled />
        <Logomark mark={AZURE_MARK} size={20} labelled />
        <Logomark mark={AWS_MARK} size={15} labelled />
        <Logomark mark={GOOGLE_CLOUD_MARK} size={20} labelled />
      </div>
    </>
  ),
  primary: () => NEXT(() => props.advance()),
});
