/**
 * `/cli/authorize?id=<attempt>` — where `flyco login` sends the browser.
 *
 * A terminal opened a sign-in attempt (`POST /v1/cli-sessions`) and handed
 * the user this URL. The page asks the one question the CLI cannot answer
 * for itself — is the person at this browser the one at that terminal —
 * and posts the answer. What it never sees is the minted key: the API holds
 * it until the CLI's own poll collects it, so nothing secret ever passes
 * through a browser, a redirect, or a history entry.
 */
import { useSearchParams } from "@solidjs/router";
import { Show, createSignal } from "solid-js";
import { approveCliSession, denyCliSession } from "../api/client";
import frame from "../components/flow/Flow.module.css";
import styles from "./CliAuthorize.module.css";

/** The answers this page can be in; each maps to what the card says. */
type Outcome = "asking" | "approved" | "denied";

export default function CliAuthorize() {
  const [params] = useSearchParams<{ id?: string }>();
  const [outcome, setOutcome] = createSignal<Outcome>("asking");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const id = () => {
    const value = params.id;
    return value !== undefined && value !== "" ? value : null;
  };

  async function decide(approve: boolean): Promise<void> {
    const attempt = id();
    if (attempt === null || busy()) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      if (approve) {
        await approveCliSession(attempt);
      } else {
        await denyCliSession(attempt);
      }
      setOutcome(approve ? "approved" : "denied");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div class={frame.page}>
      <section class={frame.card} aria-labelledby="cli-authorize-title">
        <div class={`${frame.body} ${frame.bodyShort}`}>
          <h1 id="cli-authorize-title" class={frame.title}>
            <Show when={outcome() === "asking"} fallback={<>All done</>}>
              Sign in the flyco CLI?
            </Show>
          </h1>
          <Show
            when={outcome() === "asking"}
            fallback={
              <p class={styles.lede}>
                <Show
                  when={outcome() === "approved"}
                  fallback={
                    <>
                      The sign-in was refused. The terminal that asked has been
                      told; nothing was granted.
                    </>
                  }
                >
                  Signed in. You can close this tab — the terminal that asked
                  is already picking up its key.
                </Show>
              </p>
            }
          >
            <Show
              when={id()}
              fallback={
                <p class={styles.lede} role="alert">
                  This link names no sign-in attempt. Ask the terminal for the
                  URL again — it carries the attempt's id.
                </p>
              }
            >
              <p class={styles.lede}>
                A terminal running <code>flyco login</code> is asking to sign
                in to your account. Approving mints an API key that lives on
                that machine until you revoke it.
              </p>
            </Show>
          </Show>
          <Show when={error()}>
            <p class={styles.error} role="alert">
              {error()}
            </p>
          </Show>
        </div>
        <Show when={outcome() === "asking" && id() !== null}>
          <footer class={frame.footer}>
            <button
              type="button"
              class={frame.back}
              disabled={busy()}
              onClick={() => {
                void decide(false);
              }}
            >
              Deny
            </button>
            <button
              type="button"
              class={frame.primary}
              disabled={busy()}
              onClick={() => {
                void decide(true);
              }}
            >
              Approve sign-in
            </button>
          </footer>
        </Show>
      </section>
    </div>
  );
}
