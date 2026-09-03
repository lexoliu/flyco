/**
 * The one-field page behind *Use an API key instead* (docs/ux.md §4 B2″).
 *
 * The same page for both agents: what differs is the vendor, the key's
 * name, and where one is made. One field, not one credential kind: for
 * Claude Code the field also takes the token `claude setup-token` prints,
 * because Anthropic's secrets carry their kind in their prefix and nothing
 * has to be asked (`lib/claudeCode.ts`). No account name is asked for
 * either. A user holds one account per harness, so a name they invent
 * would distinguish it from nothing; the account is labelled with what the
 * credential *is*.
 */
import { Show, createSignal } from "solid-js";
import { useReadiness } from "../../Readiness";
import {
  linkHarnessAccount,
  type HarnessCredentialInput,
  type HarnessKind,
} from "../../../api/client";
import {
  CLAUDE_SECRET_FORMS,
  parseClaudeSecret,
} from "../../../lib/claudeCode";
import { HARNESS_LABEL } from "../../../lib/harnesses";
import type { PageComponent, Primary } from "../page";
import { ExternalLink } from "./shared";
import styles from "./pages.module.css";

/** What the control plane is sent for one pasted secret. */
interface Linkable {
  readonly label: string;
  readonly credential: HarnessCredentialInput;
}

/** Either what to link, or why the field's contents cannot be. */
type ReadSecret =
  | { readonly ok: true; readonly link: Linkable }
  | { readonly ok: false; readonly error: string };

/** What one vendor's secret is called, where it comes from, and how it is read. */
interface KeyVendor {
  readonly label: string;
  readonly hint: string;
  readonly keysUrl: string;
  readonly keysPage: string;
  /** Reads a non-empty, trimmed value. */
  readonly read: (value: string) => ReadSecret;
}

const VENDORS: Record<HarnessKind, KeyVendor> = {
  claude_code: {
    label: "Anthropic API key",
    hint: `Starts with sk-ant-api03-. The token claude setup-token prints (sk-ant-oat01-…) works here too.`,
    keysUrl: "https://console.anthropic.com/settings/keys",
    keysPage: "the Anthropic console",
    read: (value) => {
      const secret = parseClaudeSecret(value);
      if (secret === null) {
        return {
          ok: false,
          error: `That is neither an Anthropic API key nor a setup token: one looks like ${CLAUDE_SECRET_FORMS}.`,
        };
      }
      return {
        ok: true,
        link: {
          label:
            secret.kind === "claude_api_key"
              ? "Anthropic API key"
              : "Claude subscription",
          credential: secret,
        },
      };
    },
  },
  codex: {
    label: "OpenAI API key",
    hint: "Starts with sk-.",
    keysUrl: "https://platform.openai.com/api-keys",
    keysPage: "the OpenAI API keys page",
    read: (value) => ({
      ok: true,
      link: {
        label: "OpenAI API key",
        credential: { kind: "codex_api_key", key: value },
      },
    }),
  },
};

export const ApiKey: PageComponent<{ id: "api-key"; agent: HarnessKind }> = (
  props,
) => {
  const readiness = useReadiness();
  const vendor = VENDORS[props.page.agent];
  const [secret, setSecret] = createSignal("");

  /** What the field holds, read; `null` while it is empty. */
  const read = (): ReadSecret | null => {
    const value = secret().trim();
    return value === "" ? null : vendor.read(value);
  };
  const error = () => {
    const result = read();
    return result !== null && !result.ok ? result.error : null;
  };

  const primary = (): Primary => {
    const result = read();
    const link = result !== null && result.ok ? result.link : null;
    return {
      label: `Link ${HARNESS_LABEL[props.page.agent]}`,
      busy: "Linking…",
      disabled:
        result === null
          ? "Paste a key to continue"
          : link === null
            ? "Paste a key Anthropic issued to continue"
            : null,
      onClick: async () => {
        if (link === null) {
          return;
        }
        const account = await linkHarnessAccount(link);
        await readiness.refresh();
        props.advance({
          agents: {
            ...props.state().answers.agents,
            [props.page.agent]: account,
          },
        });
      },
    };
  };

  return {
    title: "Paste your API key",
    body: (
      <>
        <p class={styles.lede}>
          Billed per token by the vendor rather than by your subscription. Flyco
          encrypts the key before storing it.
        </p>
        <div class={styles.field}>
          <label for="harness-api-key">{vendor.label}</label>
          <input
            id="harness-api-key"
            class={`${styles.input} ${styles.mono}`}
            type="password"
            aria-invalid={error() !== null}
            value={secret()}
            onInput={(event) => setSecret(event.currentTarget.value)}
            autocomplete="off"
            autofocus
          />
          <Show
            when={error()}
            fallback={<p class={styles.hint}>{vendor.hint}</p>}
          >
            {(message) => <p class={styles.error}>{message()}</p>}
          </Show>
        </div>
        <ExternalLink href={vendor.keysUrl}>
          Create a key on {vendor.keysPage}
        </ExternalLink>
      </>
    ),
    primary,
  };
};
