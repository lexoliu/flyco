/**
 * Settings → Account (docs/ux.md §10).
 *
 * Who you are, what can call the API as you, whether flyco may interrupt
 * you, and how it looks. Four small cards rather than four screens, because
 * none of them is a thing anyone visits twice.
 *
 * Push is one button. The tab this replaces asked the user to read about
 * VAPID keys before they could turn notifications on; the question they
 * actually have is "will flyco tell me when a session needs me", and the
 * answer is a button that says yes.
 */
import { For, Show, createSignal, onMount } from "solid-js";
import { createQuery } from "../../lib/query";
import { useNavigate } from "@solidjs/router";
import { BellRing, Check, LogOut, Monitor, Moon, Plus, Sun } from "lucide-solid";
import Logomark, { GITHUB_MARK } from "../../components/Logomark";
import ProblemNotice from "../../components/ProblemNotice";
import CopyButton from "../../components/CopyButton";
import { initialsOf } from "../../components/AppShell";
import {
  createApiKey,
  getMe,
  getVapidPublicKey,
  listApiKeys,
  revokeApiKey,
  type CreatedApiKey,
} from "../../api/client";
import {
  getPushSubscription,
  isPushSupported,
  subscribeToPush,
  unsubscribeFromPush,
} from "../../lib/push";
import { type ThemePreference, readStoredThemePreference, setTheme } from "../../lib/theme";
import { signOut } from "../../lib/signOut";
import { formatDate } from "../../lib/dates";
import { cx } from "../../lib/cx";
import styles from "./Settings.module.css";

export default function AccountSection() {
  const navigate = useNavigate();

  return (
    <section class={styles.section}>
      <header class={styles.sectionHead}>
        <h2>Account</h2>
        <p class={styles.lede}>
          The GitHub identity flyco acts as, the credentials that can call it without a browser,
          and how it reaches you.
        </p>
      </header>

      <Identity />
      <ApiKeys />
      <Notifications />
      <Appearance />

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

/* ── API keys ─────────────────────────────────────────────────────────── */

function ApiKeys() {
  const [keys, { refetch }] = createQuery(listApiKeys);
  const [label, setLabel] = createSignal("");
  const [creating, setCreating] = createSignal(false);
  const [created, setCreated] = createSignal<CreatedApiKey | null>(null);
  const [error, setError] = createSignal<unknown>(null);

  async function create(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    setCreating(true);
    setError(null);
    try {
      setCreated(await createApiKey(label()));
      setLabel("");
      await refetch();
    } catch (err) {
      setError(err);
    } finally {
      setCreating(false);
    }
  }

  async function revoke(id: string): Promise<void> {
    setError(null);
    try {
      await revokeApiKey(id);
      await refetch();
    } catch (err) {
      setError(err);
    }
  }

  return (
    <div class={styles.group}>
      <p class={styles.groupLabel}>API keys</p>
      <ProblemNotice error={keys.error ?? error()} />

      <Show when={created()}>
        {(key) => (
          <article class={styles.card}>
            <div class={styles.identity}>
              <span class={styles.cardTitle}>{key().label}</span>
              <span class={styles.cardMeta}>
                This is the only time this key is shown. Copy it before you leave the page.
              </span>
            </div>
            <div class={styles.secret}>
              <span class={styles.secretValue}>{key().token}</span>
              <div class={styles.actions}>
                <CopyButton value={key().token} class={styles.pill} label="Copy key" />
                <button type="button" class={styles.pill} onClick={() => setCreated(null)}>
                  Done
                </button>
              </div>
            </div>
          </article>
        )}
      </Show>

      <Show when={(keys() ?? []).length > 0}>
        <div class={styles.cards}>
          <For each={keys()}>
            {(key) => (
              <article class={styles.card}>
                <div class={styles.cardTop}>
                  <div class={styles.identity}>
                    <span class={styles.cardTitle}>{key.label}</span>
                    <span class={styles.cardMeta}>
                      Created {formatDate(key.created_at_unix)} ·{" "}
                      {key.last_used_unix === null || key.last_used_unix === undefined
                        ? "never used"
                        : `last used ${formatDate(key.last_used_unix)}`}
                    </span>
                  </div>
                  <div class={styles.actions}>
                    <button
                      type="button"
                      class={styles.pillDanger}
                      onClick={() => void revoke(key.id)}
                    >
                      Revoke
                    </button>
                  </div>
                </div>
              </article>
            )}
          </For>
        </div>
      </Show>

      <article class={styles.card}>
        <form class={styles.form} onSubmit={(event) => void create(event)}>
          <div class={styles.field}>
            <label for="api-key-label">New key for</label>
            <input
              id="api-key-label"
              value={label()}
              onInput={(event) => setLabel(event.currentTarget.value)}
              placeholder="CI"
              required
            />
          </div>
          <div class={styles.formActions}>
            <button type="submit" class={styles.pillPrimary} disabled={creating()}>
              <Plus size={14} aria-hidden="true" />
              {creating() ? "Creating…" : "Create key"}
            </button>
            <span class={styles.note}>Keys call the flyco API without a browser session.</span>
          </div>
        </form>
      </article>
    </div>
  );
}

/* ── Notifications ────────────────────────────────────────────────────── */

function Notifications() {
  // A browser with no `PushManager` cannot use the key, so it is not asked
  // for: firing a request whose answer can only be discarded is how the
  // unsupported case ends up reported as a failure.
  const [vapid] = createQuery(
    () => (isPushSupported() ? true : undefined),
    getVapidPublicKey,
  );
  const [subscribed, setSubscribed] = createSignal(false);
  const [checked, setChecked] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  onMount(() => {
    void getPushSubscription().then((subscription) => {
      setSubscribed(subscription !== null);
      setChecked(true);
    });
  });

  async function toggle(): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      if (subscribed()) {
        await unsubscribeFromPush();
        setSubscribed(false);
      } else {
        await subscribeToPush();
        setSubscribed(true);
      }
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div class={styles.group}>
      <p class={styles.groupLabel}>Notifications</p>
      <article class={styles.card}>
        <div class={styles.cardTop}>
          <span class={styles.mark} aria-hidden="true">
            <BellRing size={16} />
          </span>
          <div class={styles.identity}>
            <span class={styles.cardTitle}>Push notifications</span>
            <span class={styles.cardMeta}>
              Flyco tells you when a session needs a decision, or when a turn finishes while it is
              not open in a tab.
            </span>
          </div>
          <div class={styles.actions}>
            <Show
              when={isPushSupported() && vapid.error === undefined && !vapid.loading && checked()}
              fallback={
                <span class={styles.status}>
                  {isPushSupported() ? "Unavailable" : "Unsupported here"}
                </span>
              }
            >
              <button
                type="button"
                class={subscribed() ? styles.pill : styles.pillPrimary}
                disabled={busy()}
                onClick={() => void toggle()}
              >
                {busy() ? "Working…" : subscribed() ? "Turn off" : "Enable push"}
              </button>
            </Show>
          </div>
        </div>
        <ProblemNotice error={vapid.error ?? error()} />
      </article>
    </div>
  );
}

/* ── Appearance ───────────────────────────────────────────────────────── */

const THEMES: readonly { value: ThemePreference; label: string; icon: typeof Sun }[] = [
  { value: "system", label: "System", icon: Monitor },
  { value: "light", label: "Light", icon: Sun },
  { value: "dark", label: "Dark", icon: Moon },
];

function Appearance() {
  const [theme, setPreference] = createSignal<ThemePreference>(readStoredThemePreference());

  function choose(preference: ThemePreference): void {
    setTheme(preference);
    setPreference(preference);
  }

  return (
    <div class={styles.group}>
      <p class={styles.groupLabel}>Appearance</p>
      <article class={styles.card}>
        <div class={styles.cardTop}>
          <div class={styles.identity}>
            <span class={styles.cardTitle}>Theme</span>
            <span class={styles.cardMeta}>Applies on this browser, immediately.</span>
          </div>
          <div class={styles.actions}>
            <div class={styles.segmented} role="group" aria-label="Theme">
              <For each={THEMES}>
                {(option) => (
                  <button
                    type="button"
                    class={cx(styles.segment, theme() === option.value && styles.segmentOn)}
                    aria-pressed={theme() === option.value}
                    onClick={() => choose(option.value)}
                  >
                    <option.icon size={13} aria-hidden="true" />
                    {option.label}
                    <Show when={theme() === option.value}>
                      <Check size={12} aria-hidden="true" />
                    </Show>
                  </button>
                )}
              </For>
            </div>
          </div>
        </div>
      </article>
    </div>
  );
}
