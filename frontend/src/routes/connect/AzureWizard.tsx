/**
 * Linking Azure: one command, one paste (docs/ux.md §7.2).
 *
 * The old form asked for six fields, five of which the user had to find in
 * three different blades. This asks for the output of one command, because
 * that is what the user has in front of them — and it accepts both documents
 * that command prints, since the CLI's shape depends on a flag nobody
 * remembers.
 *
 * The resource group is gone entirely. The service principal is
 * `Contributor` on the subscription, which is the scope a resource-group
 * creation needs, so flyco makes the group itself the moment the account is
 * linked.
 *
 * The break-glass key is generated here, in the browser. Azure will not
 * create a Linux machine with neither a password nor a key, flyco sets no
 * passwords, and flyco holds no private keys — so the pair is minted on this
 * page, the private half is offered once, and only the public half is sent.
 */
import { Show, createMemo, createSignal, onMount } from "solid-js";
import { Download, KeyRound } from "lucide-solid";
import CommandBlock from "../../components/CommandBlock";
import CopyButton from "../../components/CopyButton";
import Disclosure from "../../components/Disclosure";
import ProblemNotice from "../../components/ProblemNotice";
import type { ProviderCredentials } from "../../api/client";
import { parseAzureServicePrincipal } from "../../lib/azureCredentials";
import { downloadPrivateKey, generateBreakGlassKey, type BreakGlassKey } from "../../lib/sshKey";
import styles from "./Connect.module.css";

/** The one command the wizard asks the user to run. */
const CREATE_PRINCIPAL = `az ad sp create-for-rbac --name flyco --role Contributor \\
  --scopes /subscriptions/$(az account show --query id -o tsv) --json-auth`;

/** The command that prints the subscription id, when the paste lacked one. */
const SHOW_SUBSCRIPTION = "az account show --query id -o tsv";

/** What the downloaded private key is called. */
const KEY_FILENAME = "flyco_azure_ed25519";

export interface AzureWizardProps {
  /** Links the account. Rejections surface above the button. */
  onLink: (credentials: ProviderCredentials, label: string) => Promise<void>;
  /** Whether a link request is in flight. */
  linking: boolean;
  /** Whatever the last link attempt failed with. */
  error: unknown;
}

export default function AzureWizard(props: AzureWizardProps) {
  const [pasted, setPasted] = createSignal("");
  const [subscription, setSubscription] = createSignal("");
  const [ownKey, setOwnKey] = createSignal("");
  const [generated, setGenerated] = createSignal<BreakGlassKey | undefined>();
  const [keyError, setKeyError] = createSignal<unknown>(null);

  // Minted as the step opens rather than on submit: the user has to be given
  // the private half *before* they commit, and a key that appeared after the
  // button was pressed would arrive on the screen they have already left.
  onMount(() => {
    try {
      setGenerated(generateBreakGlassKey("flyco"));
    } catch (failure) {
      setKeyError(failure);
    }
  });

  const parsed = createMemo(() =>
    pasted().trim() === "" ? null : parseAzureServicePrincipal(pasted()),
  );
  const principal = createMemo(() => {
    const result = parsed();
    return result !== null && result.ok ? result.principal : null;
  });
  const parseError = createMemo(() => {
    const result = parsed();
    return result !== null && !result.ok ? result.error : null;
  });

  const needsSubscription = createMemo(
    () => principal() !== null && principal()?.subscriptionId === null,
  );
  const subscriptionId = createMemo(
    () => principal()?.subscriptionId ?? (subscription().trim() || null),
  );

  /** The public key that will be sent: the user's, or the one minted here. */
  const publicKey = createMemo(() => {
    const own = ownKey().trim();
    return own === "" ? (generated()?.publicKey ?? "") : own;
  });

  const ready = createMemo(
    () => principal() !== null && subscriptionId() !== null && publicKey() !== "",
  );

  async function link(): Promise<void> {
    const found = principal();
    const subscriptionUsed = subscriptionId();
    if (found === null || subscriptionUsed === null) {
      return;
    }
    await props.onLink(
      {
        kind: "azure",
        tenant_id: found.tenantId,
        client_id: found.clientId,
        client_secret: found.clientSecret,
        subscription_id: subscriptionUsed,
        admin_ssh_public_key: publicKey(),
      },
      "Azure",
    );
  }

  return (
    <div class={styles.step}>
      <section class={styles.stage}>
        <p class={styles.stageTitle}>1 · Create a service principal</p>
        <CommandBlock value={CREATE_PRINCIPAL} label="Copy command" />
        <p class={styles.hint}>
          Run this in Azure Cloud Shell, or in a terminal with the Azure CLI signed in. It prints a
          JSON block.
        </p>
      </section>

      <section class={styles.stage}>
        <p class={styles.stageTitle}>2 · Paste what it printed</p>
        <textarea
          class={styles.paste}
          rows="6"
          spellcheck={false}
          aria-label="The JSON block the command printed"
          aria-invalid={parseError() !== null}
          placeholder='{ "clientId": "…", "clientSecret": "…", "tenantId": "…" }'
          value={pasted()}
          onInput={(event) => setPasted(event.currentTarget.value)}
        />
        <Show when={parseError()}>{(message) => <p class={styles.error}>{message()}</p>}</Show>

        <Show when={principal()}>
          {(found) => (
            <dl class={styles.confirm}>
              <Row label="Client id" value={found().clientId} />
              <Row label="Tenant id" value={found().tenantId} />
              <Row label="Client secret" value="held, and never shown again" />
              <Show when={found().subscriptionId}>
                {(id) => <Row label="Subscription" value={id()} />}
              </Show>
            </dl>
          )}
        </Show>

        <Show when={needsSubscription()}>
          <div class={styles.stage}>
            <p class={styles.hint}>
              That block carries no subscription. Run this and paste the id it prints.
            </p>
            <CommandBlock value={SHOW_SUBSCRIPTION} label="Copy command" />
            <input
              class={styles.input}
              aria-label="Subscription id"
              placeholder="00000000-0000-0000-0000-000000000000"
              value={subscription()}
              onInput={(event) => setSubscription(event.currentTarget.value)}
            />
          </div>
        </Show>
      </section>

      <section class={styles.stage}>
        <p class={styles.stageTitle}>3 · Save the machine's admin SSH key (optional)</p>
        <ProblemNotice error={keyError()} />
        <Show when={generated()}>
          {(key) => (
            <>
              <p class={styles.hint}>
                Azure requires an SSH login key for every Linux machine it builds. Flyco made one in
                this browser and keeps only the public half. The private key is only needed if you
                ever want to SSH into a session machine yourself, so save it now or skip this.
              </p>
              <div class={styles.keyRow}>
                <span class={styles.fingerprint}>
                  <KeyRound size={13} aria-hidden="true" />
                  <span class={styles.fingerprintLabel}>Fingerprint</span>
                  <code>{key().fingerprint}</code>
                </span>
                <button
                  type="button"
                  class={styles.pill}
                  onClick={() => downloadPrivateKey(key(), KEY_FILENAME)}
                >
                  <Download size={13} aria-hidden="true" />
                  Download private key
                </button>
                <CopyButton
                  value={key().privateKey}
                  label="Copy private key"
                  class={styles.pill ?? ""}
                />
              </div>
            </>
          )}
        </Show>
        <Disclosure summary="Advanced">
          <label class={styles.field}>
            <span>Use my own public key instead</span>
            <textarea
              class={styles.paste}
              rows="2"
              spellcheck={false}
              placeholder="ssh-ed25519 AAAA…"
              value={ownKey()}
              onInput={(event) => setOwnKey(event.currentTarget.value)}
            />
          </label>
        </Disclosure>
      </section>

      <ProblemNotice error={props.error} />
      <button
        type="button"
        class={styles.primary}
        disabled={!ready() || props.linking}
        onClick={() => void link()}
      >
        {props.linking ? "Linking…" : "Link Azure"}
      </button>
    </div>
  );
}

/** One read-only confirmation row. */
function Row(props: { label: string; value: string }) {
  return (
    <div class={styles.confirmRow}>
      <dt>{props.label}</dt>
      <dd>{props.value}</dd>
    </div>
  );
}
