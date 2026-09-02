/**
 * Connecting an agent (docs/ux.md §8), wherever the user is standing.
 *
 * Two cards, Claude Code and Codex, each expanding in place. It is reached
 * from `/connect/harness`, from the welcome flow, and — as a single card —
 * from Settings › Agents' `Relink`, so the flow lives here once and those
 * three places frame it rather than reimplementing it.
 *
 * Claude Code is two steps because Anthropic's flow is two steps: flyco
 * opens the authorize page it was given, and the user brings back the code
 * Anthropic shows them. Everything that was the old credential form — the
 * setup token, the API key — is still there, under `Advanced`, because a
 * person who already has one should not have to run a browser flow to use
 * it.
 */
import { For, Match, Show, Switch, createResource, createSignal, type JSX } from "solid-js";
import { ArrowUpRight } from "lucide-solid";
import Logomark, { HARNESS_MARK } from "../Logomark";
import Disclosure from "../Disclosure";
import HarnessUsage from "../HarnessUsage";
import ProblemNotice from "../ProblemNotice";
import { useReadiness } from "../Readiness";
import {
  completeClaudeOauth,
  linkHarnessAccount,
  listLlmUsage,
  startClaudeOauth,
  type ClaudeOauthStart,
  type HarnessAccountView,
  type HarnessCredentialInput,
  type HarnessKind,
} from "../../api/client";
import { parsePastedCode, pastedCodeForExchange } from "../../lib/claudeCode";
import { formatDate } from "../../lib/dates";
import { cx } from "../../lib/cx";
import styles from "./HarnessChooser.module.css";

/** Where an `OpenAI` key is made. */
const OPENAI_KEYS_URL = "https://platform.openai.com/api-keys";

/** The two agents flyco runs, in the order the chooser lists them. */
const HARNESSES: readonly { kind: HarnessKind; label: string; runsOn: string }[] = [
  {
    kind: "claude_code",
    label: "Claude Code",
    runsOn: "Runs on your Claude subscription. Sign in and flyco never sees your password.",
  },
  {
    kind: "codex",
    label: "Codex",
    runsOn: "Runs on your OpenAI API key. Usage is billed to your own account.",
  },
];

export interface HarnessChooserProps {
  /** Called after a successful link, so readiness can be reloaded. */
  onLinked: () => unknown;
}

/**
 * The two-card chooser.
 *
 * A card that is already linked shows what is linked, when its credential
 * expires, and what the harness has reported using — the readout docs/ux.md
 * §8 asks for — and can still be reconnected in place.
 */
export default function HarnessChooser(props: HarnessChooserProps) {
  const readiness = useReadiness();
  const [usage] = createResource(listLlmUsage);
  const [open, setOpen] = createSignal<HarnessKind | null>(null);

  const accountFor = (kind: HarnessKind): HarnessAccountView | undefined =>
    readiness.harness().find((account) => account.harness === kind);

  async function onLinked(): Promise<void> {
    setOpen(null);
    await props.onLinked();
  }

  return (
    <div class={styles.cards}>
      <For each={HARNESSES}>
        {(harness) => {
          const account = () => accountFor(harness.kind);
          const expanded = () => open() === harness.kind;
          return (
            <article class={cx(styles.card, expanded() && styles.cardOpen)}>
              <div class={styles.head}>
                <span class={styles.mark}>
                  <Logomark mark={HARNESS_MARK[harness.kind]} size={17} />
                </span>
                <div class={styles.identity}>
                  <span class={styles.title}>{harness.label}</span>
                  <span class={styles.meta}>{account()?.label ?? harness.runsOn}</span>
                </div>
                <span class={cx(styles.status, account() !== undefined && styles.statusOn)}>
                  {account() === undefined ? "Not linked" : "Linked"}
                </span>
              </div>

              <Show when={account()}>
                {(linked) => (
                  <div class={styles.linked}>
                    <p class={styles.meta}>
                      Linked {formatDate(linked().linked_at_unix)}
                      <Show when={linked().expires_at_unix}>
                        {(expires) => <> · expires {formatDate(expires())}</>}
                      </Show>
                    </p>
                    {/* A grant with an end is renewed before a session is
                        given it, so the date above is a fact rather than a
                        deadline the user has to act on. */}
                    <Show when={linked().expires_at_unix}>
                      <p class={styles.faint}>Renewed automatically before a session uses it.</p>
                    </Show>
                    <HarnessUsage row={usage()?.find((row) => row.account === linked().id)} />
                  </div>
                )}
              </Show>

              <Show
                when={expanded()}
                fallback={
                  <div class={styles.actions}>
                    <button
                      type="button"
                      class={account() === undefined ? styles.pillPrimary : styles.pill}
                      onClick={() => setOpen(harness.kind)}
                    >
                      {account() === undefined
                        ? `Connect ${harness.label}`
                        : `Reconnect ${harness.label}`}
                    </button>
                  </div>
                }
              >
                <HarnessConnect
                  harness={harness.kind}
                  onLinked={() => void onLinked()}
                  onCancel={() => setOpen(null)}
                />
              </Show>
            </article>
          );
        }}
      </For>
    </div>
  );
}

export interface HarnessConnectProps {
  /** Which agent is being connected. */
  harness: HarnessKind;
  /** Called after a successful link. */
  onLinked: () => unknown;
  /** Offered as `Cancel` when the caller can close the flow. */
  onCancel?: (() => void) | undefined;
}

/**
 * One agent's connect flow, without the card around it.
 *
 * Settings › Agents opens this under the card it already draws; the chooser
 * above opens it inside the card it drew.
 */
export function HarnessConnect(props: HarnessConnectProps) {
  return (
    <Switch>
      <Match when={props.harness === "claude_code"}>
        <ClaudeConnect onLinked={props.onLinked} onCancel={props.onCancel} />
      </Match>
      <Match when={props.harness === "codex"}>
        <CodexConnect onLinked={props.onLinked} onCancel={props.onCancel} />
      </Match>
    </Switch>
  );
}

/** Where the Claude card is in its two-step flow. */
type ClaudeStep = "sign-in" | "paste";

function ClaudeConnect(props: Omit<HarnessConnectProps, "harness">) {
  const [step, setStep] = createSignal<ClaudeStep>("sign-in");
  const [attempt, setAttempt] = createSignal<ClaudeOauthStart | null>(null);
  const [pasted, setPasted] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  const code = () => parsePastedCode(pasted());

  /** Opens the authorize page flyco was handed, in a tab of its own. */
  function open(url: string): void {
    window.open(url, "_blank", "noopener,noreferrer");
  }

  async function signIn(): Promise<void> {
    setBusy(true);
    setError(null);
    try {
      const started = await startClaudeOauth();
      setAttempt(started);
      open(started.authorize_url);
      setStep("paste");
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  async function submit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const started = attempt();
    const parsed = code();
    if (started === null || parsed === null) {
      return;
    }

    setBusy(true);
    setError(null);
    try {
      await completeClaudeOauth({
        attempt_id: started.attempt_id,
        code: pastedCodeForExchange(parsed),
      });
      setPasted("");
      await props.onLinked();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  return (
    <div class={styles.flow}>
      <ol class={styles.steps}>
        <li class={cx(styles.step, step() === "sign-in" && styles.stepCurrent)}>
          <span class={styles.stepMark}>1</span>
          <div class={styles.stepBody}>
            <p class={styles.stepTitle}>Sign in at Anthropic</p>
            <Show when={step() === "sign-in"}>
              <p class={styles.hint}>
                Flyco opens Anthropic's own sign-in page. Your password never reaches flyco.
              </p>
              <button
                type="button"
                class={styles.pillPrimary}
                disabled={busy()}
                onClick={() => void signIn()}
              >
                {busy() ? "Opening…" : "Sign in with Claude"}
                <ArrowUpRight size={14} aria-hidden="true" />
              </button>
            </Show>
          </div>
        </li>

        <li class={cx(styles.step, step() === "paste" && styles.stepCurrent)}>
          <span class={styles.stepMark}>2</span>
          <div class={styles.stepBody}>
            <p class={styles.stepTitle}>Paste the code Anthropic shows you</p>
            <Show when={step() === "paste"}>
              <form class={styles.form} onSubmit={(event) => void submit(event)}>
                <div class={styles.field}>
                  <label for="claude-oauth-code">Code from Anthropic</label>
                  <input
                    id="claude-oauth-code"
                    class={styles.mono}
                    value={pasted()}
                    onInput={(event) => setPasted(event.currentTarget.value)}
                    autocomplete="off"
                    spellcheck={false}
                    autofocus
                  />
                  <p class={styles.hint}>
                    It looks like <code>CODE#STATE</code> — paste the whole thing.
                  </p>
                </div>
                <div class={styles.actions}>
                  <button type="submit" class={styles.pillPrimary} disabled={busy() || code() === null}>
                    {busy() ? "Linking…" : "Link Claude Code"}
                  </button>
                  <button
                    type="button"
                    class={styles.quiet}
                    onClick={() => {
                      const started = attempt();
                      if (started !== null) {
                        open(started.authorize_url);
                      }
                    }}
                  >
                    Open the sign-in page again
                  </button>
                </div>
              </form>
            </Show>
          </div>
        </li>
      </ol>

      <ProblemNotice error={error()} />

      <Disclosure summary="Advanced">
        <CredentialForm
          fields={[
            {
              kind: "claude_setup_token",
              label: "Setup token",
              hint: (
                <>
                  Run <code>claude setup-token</code> on a computer you trust and paste the
                  long-lived token it prints.
                </>
              ),
              accountLabel: "Claude subscription",
              submitLabel: "Link with a setup token",
            },
            {
              kind: "claude_api_key",
              label: "Anthropic API key",
              hint: "Billed per token by Anthropic rather than by your subscription.",
              accountLabel: "Anthropic API key",
              submitLabel: "Link with an API key",
            },
          ]}
          onLinked={props.onLinked}
        />
      </Disclosure>

      <Show when={props.onCancel}>
        {(cancel) => (
          <button type="button" class={styles.quiet} onClick={() => cancel()()}>
            Cancel
          </button>
        )}
      </Show>
    </div>
  );
}

function CodexConnect(props: Omit<HarnessConnectProps, "harness">) {
  return (
    <div class={styles.flow}>
      <p class={styles.hint}>
        Codex signs in with an OpenAI API key.{" "}
        <a class={styles.link} href={OPENAI_KEYS_URL} target="_blank" rel="noreferrer">
          Create one on the API keys page
          <ArrowUpRight size={13} aria-hidden="true" />
        </a>
      </p>
      <CredentialForm
        fields={[
          {
            kind: "codex_api_key",
            label: "OpenAI API key",
            hint: (
              <>
                Starts with <code>sk-</code>. Flyco encrypts it before storing it.
              </>
            ),
            accountLabel: "OpenAI API key",
            submitLabel: "Link Codex",
          },
        ]}
        onLinked={props.onLinked}
      />
      <Show when={props.onCancel}>
        {(cancel) => (
          <button type="button" class={styles.quiet} onClick={() => cancel()()}>
            Cancel
          </button>
        )}
      </Show>
    </div>
  );
}

/** One credential mode the plain link route accepts. */
interface CredentialField {
  kind: HarnessCredentialInput["kind"];
  /** The field's label, which is also the button's subject. */
  label: string;
  /** The sentence under the field. */
  hint: JSX.Element;
  /** What the account is called once linked. */
  accountLabel: string;
  /** What the button says, so two ways to link never share a name. */
  submitLabel: string;
}

/**
 * The credential form the API has always had: one secret, sealed on arrival.
 *
 * No account name is asked for. A user holds one account per harness, so a
 * name the user invents would distinguish it from nothing; the card is
 * labelled with what the credential *is* instead.
 */
function CredentialForm(props: { fields: readonly CredentialField[]; onLinked: () => unknown }) {
  const [selected, setSelected] = createSignal(0);
  const [secret, setSecret] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  const field = () => props.fields[selected()] ?? props.fields[0];

  function credential(kind: CredentialField["kind"], value: string): HarnessCredentialInput {
    switch (kind) {
      case "claude_setup_token":
        return { kind, token: value };
      case "claude_api_key":
      case "codex_api_key":
        return { kind, key: value };
      case "claude_oauth":
        // Minted by the sign-in flow above, never typed by hand.
        throw new Error("an OAuth grant is not a credential anybody pastes");
    }
  }

  async function submit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const chosen = field();
    if (chosen === undefined || secret().trim() === "") {
      return;
    }

    setBusy(true);
    setError(null);
    try {
      await linkHarnessAccount({
        label: chosen.accountLabel,
        credential: credential(chosen.kind, secret().trim()),
      });
      setSecret("");
      await props.onLinked();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  }

  return (
    <form class={styles.form} onSubmit={(event) => void submit(event)}>
      <Show when={props.fields.length > 1}>
        <div class={styles.segmented} role="group" aria-label="Credential">
          <For each={props.fields}>
            {(option, index) => (
              <button
                type="button"
                class={cx(styles.segment, selected() === index() && styles.segmentOn)}
                aria-pressed={selected() === index()}
                onClick={() => {
                  setSelected(index());
                  setSecret("");
                }}
              >
                {option.label}
              </button>
            )}
          </For>
        </div>
      </Show>

      <div class={styles.field}>
        <label for={`harness-secret-${field()?.kind ?? "credential"}`}>{field()?.label}</label>
        <input
          id={`harness-secret-${field()?.kind ?? "credential"}`}
          class={styles.mono}
          type="password"
          value={secret()}
          onInput={(event) => setSecret(event.currentTarget.value)}
          autocomplete="off"
        />
        <p class={styles.hint}>{field()?.hint}</p>
      </div>

      <ProblemNotice error={error()} />
      <div class={styles.actions}>
        <button type="submit" class={styles.pill} disabled={busy() || secret().trim() === ""}>
          {busy() ? "Linking…" : (field()?.submitLabel ?? "Link this account")}
        </button>
      </div>
    </form>
  );
}
