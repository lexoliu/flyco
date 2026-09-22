/**
 * Settings → Account (docs/ux.md §10).
 *
 * Who flyco acts as, and the way out. The credentials that call the API
 * without a browser are their own section, and so is everything about how
 * the app looks and when it interrupts: neither is a fact about the
 * identity, and both were making this page a drawer of leftovers.
 */
import { Show } from "solid-js";
import { createQuery } from "../../lib/query";
import { useNavigate } from "@solidjs/router";
import { LogOut } from "lucide-solid";
import Logomark, { GITHUB_MARK } from "../../components/Logomark";
import { initialsOf } from "../../components/AppShell";
import ProblemNotice from "../../components/ProblemNotice";
import { getMe } from "../../api/client";
import { signOut } from "../../lib/signOut";
import styles from "./Settings.module.css";

export default function AccountSection() {
  const navigate = useNavigate();

  return (
    <section class={styles.section}>
      <header class={styles.sectionHead}>
        <h2>Account</h2>
      </header>

      <Identity />

      <div>
        <button type="button" class={styles.pill} onClick={() => signOut(navigate)}>
          <LogOut size={14} aria-hidden="true" />
          Sign out
        </button>
      </div>
    </section>
  );
}

/* ── Identity ─────────────────────────────────────────────────────────── */

function Identity() {
  const [me] = createQuery(getMe);

  return (
    <div class={styles.group}>
      <ProblemNotice error={me.error} />
      <Show when={me()}>
        {(user) => (
          <article class={styles.card}>
            <div class={styles.cardTop}>
              <span class={styles.mark} aria-hidden="true">
                {initialsOf(user().login)}
              </span>
              <div class={styles.identity}>
                <span class={styles.cardTitle}>{user().login}</span>
                <span class={styles.metaWithMark}>
                  <Logomark mark={GITHUB_MARK} size={12} />
                  Signed in with GitHub · {user().session_cap} sessions at once
                </span>
              </div>
            </div>
          </article>
        )}
      </Show>
    </div>
  );
}

