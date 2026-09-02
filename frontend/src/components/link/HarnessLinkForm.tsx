/**
 * Linking a harness account, wherever the user is standing when they do it.
 *
 * The same form is reached from three places — `/connect/harness`, the
 * welcome flow, and the settings tab — so it lives here rather than inside
 * any one of them. The real chooser, with Claude's OAuth flow, is issue #60;
 * this is the credential form that already worked, unchanged in behaviour
 * and moved so the new routes are not a second copy of it.
 */
import { Show, createSignal } from "solid-js";
import ProblemNotice from "../ProblemNotice";
import Logomark, { ANTHROPIC_MARK, OPENAI_MARK } from "../Logomark";
import {
  linkHarnessAccount,
  type HarnessCredentialInput,
  type HarnessKind,
} from "../../api/client";
import styles from "../Panel.module.css";

type CredentialKind = HarnessCredentialInput["kind"];

export interface HarnessLinkFormProps {
  /** Called after a successful link, so readiness can be reloaded. */
  onLinked: () => unknown;
}

export default function HarnessLinkForm(props: HarnessLinkFormProps) {
  const [harness, setHarness] = createSignal<HarnessKind>("claude_code");
  const [credentialKind, setCredentialKind] = createSignal<CredentialKind>("claude_setup_token");
  const [label, setLabel] = createSignal("");
  const [secret, setSecret] = createSignal("");
  const [submitting, setSubmitting] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  function selectHarness(next: HarnessKind): void {
    setHarness(next);
    setCredentialKind(next === "claude_code" ? "claude_setup_token" : "codex_api_key");
    setSecret("");
  }

  function credential(): HarnessCredentialInput {
    switch (credentialKind()) {
      case "claude_setup_token":
        return { kind: "claude_setup_token", token: secret() };
      case "claude_api_key":
        return { kind: "claude_api_key", key: secret() };
      case "codex_api_key":
        return { kind: "codex_api_key", key: secret() };
    }
  }

  async function onSubmit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      await linkHarnessAccount({ label: label(), credential: credential() });
      setLabel("");
      setSecret("");
      await props.onLinked();
    } catch (err) {
      setError(err);
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <form class={styles.form} onSubmit={(event) => void onSubmit(event)}>
      <div class={styles.field}>
        <label for="harness-kind">Harness</label>
        <select
          id="harness-kind"
          value={harness()}
          onChange={(event) => selectHarness(event.currentTarget.value as HarnessKind)}
        >
          <option value="claude_code">Claude Code</option>
          <option value="codex">Codex</option>
        </select>
        <p class={styles.tabDescription}>
          <Logomark mark={harness() === "claude_code" ? ANTHROPIC_MARK : OPENAI_MARK} size={13} />
          {harness() === "claude_code"
            ? " Claude Code runs on your Anthropic subscription or API key."
            : " Codex runs on your OpenAI API key."}
        </p>
      </div>

      <Show when={harness() === "claude_code"}>
        <div class={styles.field}>
          <label for="credential-kind">Credential</label>
          <select
            id="credential-kind"
            value={credentialKind()}
            onChange={(event) => {
              setCredentialKind(event.currentTarget.value as CredentialKind);
              setSecret("");
            }}
          >
            <option value="claude_setup_token">Claude subscription setup token</option>
            <option value="claude_api_key">Anthropic API key</option>
          </select>
        </div>
      </Show>

      <div class={styles.field}>
        <label for="harness-label">Label</label>
        <input
          id="harness-label"
          value={label()}
          onInput={(event) => setLabel(event.currentTarget.value)}
          placeholder="Personal"
          required
        />
      </div>

      <div class={styles.field}>
        <label for="harness-secret">
          {credentialKind() === "claude_setup_token" ? "Setup token" : "API key"}
        </label>
        <input
          id="harness-secret"
          type="password"
          value={secret()}
          onInput={(event) => setSecret(event.currentTarget.value)}
          autocomplete="off"
          required
        />
      </div>

      <Show when={credentialKind() === "claude_setup_token"}>
        <p class={styles.tabDescription}>
          Run <code>claude setup-token</code> on a trusted computer, then paste the long-lived
          token it prints. Flyco encrypts it before storage.
        </p>
      </Show>

      <ProblemNotice error={error()} />
      <button type="submit" class={styles.primaryButton} disabled={submitting()}>
        {submitting() ? "Linking…" : "Link account"}
      </button>
    </form>
  );
}
