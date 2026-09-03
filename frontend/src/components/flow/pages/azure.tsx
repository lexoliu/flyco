/**
 * Azure's pages (docs/ux.md §4 C5–C7, §7.2): one command, one paste, one
 * key.
 *
 * The old form asked for six fields, five of which the user had to find in
 * three different blades. This asks for the output of one command, because
 * that is what the user has in front of them — and it accepts both
 * documents that command prints, since the CLI's shape depends on a flag
 * nobody remembers. A block with no subscription in it gets one more page,
 * with the one command that prints the id.
 *
 * The resource group is gone entirely. The service principal is
 * `Contributor` on the subscription, which is the scope a resource-group
 * creation needs, so flyco makes the group itself the moment the account is
 * linked.
 *
 * The break-glass key is generated on the key page, in the browser. Azure
 * will not create a Linux machine with neither a password nor a key, flyco
 * sets no passwords, and flyco holds no private keys — so the pair is
 * minted here, the private half is offered once, and only the public half
 * is sent.
 */
import { Show, createMemo, createSignal, onMount } from "solid-js";
import { Download, KeyRound } from "lucide-solid";
import CommandBlock from "../../CommandBlock";
import CopyButton from "../../CopyButton";
import ProblemNotice from "../../ProblemNotice";
import { useReadiness } from "../../Readiness";
import { linkProvider } from "../../../api/client";
import { parseAzureServicePrincipal } from "../../../lib/azureCredentials";
import { downloadPrivateKey, generateBreakGlassKey, type BreakGlassKey } from "../../../lib/sshKey";
import { NEXT, type PageComponent, type Primary } from "../page";
import { ConfirmRow, QuietLink } from "./shared";
import styles from "./pages.module.css";

/** The one command the flow asks the user to run. */
const CREATE_PRINCIPAL = `az ad sp create-for-rbac --name flyco --role Contributor \\
  --scopes /subscriptions/$(az account show --query id -o tsv) --json-auth`;

/** The command that prints the subscription id, when the paste lacked one. */
const SHOW_SUBSCRIPTION = "az account show --query id -o tsv";

/** What the downloaded private key is called. */
const KEY_FILENAME = "flyco_azure_ed25519";

export const AzureCommand: PageComponent<{ id: "azure-command" }> = (props) => ({
  title: "Run this in Azure Cloud Shell",
  body: (
    <>
      <CommandBlock value={CREATE_PRINCIPAL} label="Copy" />
      <p class={styles.hint}>
        Cloud Shell, or any terminal with the Azure CLI signed in. It prints a JSON block; the next
        page asks for it.
      </p>
    </>
  ),
  primary: () => NEXT(() => props.advance()),
});

export const AzurePaste: PageComponent<{ id: "azure-paste" }> = (props) => {
  const [pasted, setPasted] = createSignal(props.state().answers.azurePaste);

  const parsed = createMemo(() =>
    pasted().trim() === "" ? null : parseAzureServicePrincipal(pasted()),
  );
  const principal = () => {
    const result = parsed();
    return result !== null && result.ok ? result.principal : null;
  };
  const parseError = () => {
    const result = parsed();
    return result !== null && !result.ok ? result.error : null;
  };

  const primary = (): Primary => {
    const found = principal();
    return {
      label: "Next",
      disabled: found === null ? "Paste the JSON block to continue" : null,
      onClick: () => {
        if (found !== null) {
          props.advance({ azurePaste: pasted(), azurePrincipal: found, azureSubscription: null });
        }
      },
    };
  };

  return {
    title: "Paste the JSON block",
    body: (
      <>
        <textarea
          class={styles.paste}
          rows="6"
          spellcheck={false}
          aria-label="The JSON block the command printed"
          aria-invalid={parseError() !== null}
          placeholder='{ "clientId": "…", "clientSecret": "…", "tenantId": "…" }'
          value={pasted()}
          onInput={(event) => setPasted(event.currentTarget.value)}
          autofocus
        />
        <Show when={parseError()}>{(message) => <p class={styles.error}>{message()}</p>}</Show>
        <Show when={principal()}>
          {(found) => (
            <dl class={styles.confirm}>
              <ConfirmRow label="Client id" value={found().clientId} />
              <ConfirmRow label="Tenant id" value={found().tenantId} />
              <ConfirmRow label="Client secret" value="held, and never shown again" />
              <Show when={found().subscriptionId}>
                {(id) => <ConfirmRow label="Subscription" value={id()} />}
              </Show>
            </dl>
          )}
        </Show>
      </>
    ),
    primary,
  };
};

export const AzureSubscription: PageComponent<{ id: "azure-subscription" }> = (props) => {
  const [subscription, setSubscription] = createSignal(
    props.state().answers.azureSubscription ?? "",
  );

  const primary = (): Primary => {
    const id = subscription().trim();
    return {
      label: "Next",
      disabled: id === "" ? "Paste the subscription id to continue" : null,
      onClick: () => {
        if (id !== "") {
          props.advance({ azureSubscription: id });
        }
      },
    };
  };

  return {
    title: "Which subscription?",
    body: (
      <>
        <p class={styles.lede}>
          That block carries no subscription. Run this and paste the id it prints.
        </p>
        <CommandBlock value={SHOW_SUBSCRIPTION} label="Copy" />
        <input
          class={`${styles.input} ${styles.mono}`}
          aria-label="Subscription id"
          placeholder="00000000-0000-0000-0000-000000000000"
          spellcheck={false}
          autocomplete="off"
          value={subscription()}
          onInput={(event) => setSubscription(event.currentTarget.value)}
          autofocus
        />
      </>
    ),
    primary,
  };
};

export const AzureKey: PageComponent<{ id: "azure-key" }> = (props) => {
  const readiness = useReadiness();
  const answers = props.state().answers;
  const principal = answers.azurePrincipal;
  if (principal === null) {
    throw new Error("the Azure key page was reached without a service principal");
  }
  const subscriptionId = principal.subscriptionId ?? answers.azureSubscription;
  if (subscriptionId === null) {
    throw new Error("the Azure key page was reached without a subscription");
  }

  const [generated, setGenerated] = createSignal<BreakGlassKey | null>(answers.azureKey);
  const [keyError, setKeyError] = createSignal<unknown>(null);
  const [own, setOwn] = createSignal<string | null>(null);

  // Minted as the page opens rather than on submit: the user has to be
  // given the private half *before* they commit, and a key that appeared
  // after the button was pressed would arrive on a page they had left.
  // Minted once: the key is recorded in the flow, so `Back` and forward
  // again shows the same key the user may already have saved.
  onMount(() => {
    if (generated() !== null) {
      return;
    }
    try {
      const key = generateBreakGlassKey("flyco");
      setGenerated(key);
      props.record({ azureKey: key });
    } catch (failure) {
      setKeyError(failure);
    }
  });

  /** The public key that will be sent: the user's own, or the one minted here. */
  const publicKey = () => {
    const pasted = own();
    return pasted === null ? (generated()?.publicKey ?? "") : pasted.trim();
  };

  const primary = (): Primary => {
    const key = publicKey();
    return {
      label: "Link Azure",
      busy: "Linking…",
      disabled:
        key === ""
          ? own() === null
            ? "Waiting for the key to be generated"
            : "Paste a public key to continue"
          : null,
      onClick: async () => {
        if (key === "") {
          return;
        }
        const account = await linkProvider({
          label: "Azure",
          credentials: {
            kind: "azure",
            tenant_id: principal.tenantId,
            client_id: principal.clientId,
            client_secret: principal.clientSecret,
            subscription_id: subscriptionId,
            admin_ssh_public_key: key,
          },
        });
        await readiness.refresh();
        props.advance({ computeAccount: account });
      },
    };
  };

  return {
    title: "Save the machine's admin SSH key",
    body: (
      <Show
        when={own() === null}
        fallback={
          <>
            <p class={styles.lede}>
              Paste the public half of a key you already hold. Azure installs it as the login key of
              every machine flyco builds.
            </p>
            <textarea
              class={styles.paste}
              rows="3"
              spellcheck={false}
              aria-label="Your own public key"
              placeholder="ssh-ed25519 AAAA…"
              value={own() ?? ""}
              onInput={(event) => setOwn(event.currentTarget.value)}
              autofocus
            />
            <QuietLink onClick={() => setOwn(null)}>Use the generated key instead</QuietLink>
          </>
        }
      >
        <p class={styles.lede}>
          Azure requires an SSH login key for every Linux machine it builds. Flyco made one in this
          browser and keeps only the public half. Save the private key now if you ever want to SSH
          into a session machine yourself; it is not shown again.
        </p>
        <ProblemNotice error={keyError()} />
        <Show when={generated()}>
          {(key) => (
            <>
              <div class={styles.pills}>
                <button
                  type="button"
                  class={styles.pill}
                  onClick={() => downloadPrivateKey(key(), KEY_FILENAME)}
                >
                  <Download size={13} aria-hidden="true" />
                  Download
                </button>
                <CopyButton value={key().privateKey} label="Copy" class={styles.pill} />
              </div>
              <span class={styles.fingerprint}>
                <KeyRound size={13} aria-hidden="true" />
                Fingerprint <code>{key().fingerprint}</code>
              </span>
            </>
          )}
        </Show>
        <QuietLink onClick={() => setOwn("")}>Use my own public key instead</QuietLink>
      </Show>
    ),
    primary,
  };
};
