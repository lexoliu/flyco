/**
 * Azure's Cloud Shell pages (docs/ux.md §4 C5–C6, §7.2): one command, one
 * paste.
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
 * There is no key page. Azure will not create a Linux machine with neither
 * a password nor a key, and flyco sets no passwords; but the user has a
 * browser and signs in from anywhere, so the key is flyco's — minted and
 * kept by the control plane when the account is linked, and never shown.
 */
import { Show, createMemo, createSignal } from "solid-js";
import CommandBlock from "../../CommandBlock";
import { useReadiness } from "../../Readiness";
import { linkProvider } from "../../../api/client";
import {
  parseAzureServicePrincipal,
  type AzureServicePrincipal,
} from "../../../lib/azureCredentials";
import { NEXT, type PageComponent, type Primary } from "../page";
import { ConfirmRow, ExternalLink } from "./shared";
import styles from "./pages.module.css";

/**
 * Azure's terminal in the browser, signed in as the user already. The one
 * place the flow sends anyone to run a command: a user is assumed to have
 * a browser and nothing else installed.
 */
const AZURE_CLOUD_SHELL = "https://shell.azure.com";

/** The one command the flow asks the user to run. */
const CREATE_PRINCIPAL = `az ad sp create-for-rbac --name flyco --role Contributor \\
  --scopes /subscriptions/$(az account show --query id -o tsv) --json-auth`;

/** The command that prints the subscription id, when the paste lacked one. */
const SHOW_SUBSCRIPTION = "az account show --query id -o tsv";

export const AzureCommand: PageComponent<{ id: "azure-command" }> = (
  props,
) => ({
  title: "Run this in Azure Cloud Shell",
  body: (
    <>
      <p class={styles.lede}>
        Cloud Shell is a terminal in your browser, already signed in to your
        Azure account. Nothing to install.
      </p>
      <CommandBlock value={CREATE_PRINCIPAL} label="Copy" wrap />
      <ExternalLink href={AZURE_CLOUD_SHELL}>
        Open Azure Cloud Shell
      </ExternalLink>
      <p class={styles.hint}>
        Paste the command there and press Enter. It prints a JSON block; the
        next page asks for it.
      </p>
    </>
  ),
  primary: () => NEXT(() => props.advance()),
});

/** The link itself, shared by the two pages a paste can end on. */
function useLinkAzure(): (
  principal: AzureServicePrincipal,
  subscriptionId: string,
  then: () => void,
) => Primary {
  const readiness = useReadiness();
  return (principal, subscriptionId, then) => ({
    label: "Link Azure",
    busy: "Linking…",
    disabled: null,
    onClick: async () => {
      await linkProvider({
        label: "Azure",
        credentials: {
          kind: "azure",
          tenant_id: principal.tenantId,
          client_id: principal.clientId,
          client_secret: principal.clientSecret,
          subscription_id: subscriptionId,
        },
      });
      await readiness.refresh();
      then();
    },
  });
}

export const AzurePaste: PageComponent<{ id: "azure-paste" }> = (props) => {
  const linkAzure = useLinkAzure();
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
    if (found === null) {
      return {
        label: "Next",
        disabled: "Paste the JSON block to continue",
        onClick: () => undefined,
      };
    }
    // A block that names its subscription links from here; one that does
    // not gets the subscription page first.
    if (found.subscriptionId === null) {
      return {
        label: "Next",
        disabled: null,
        onClick: () => {
          props.advance({
            azurePaste: pasted(),
            azurePrincipal: found,
            azureSubscription: null,
          });
        },
      };
    }
    return linkAzure(found, found.subscriptionId, () =>
      props.advance({
        azurePaste: pasted(),
        azurePrincipal: found,
        azureSubscription: null,
      }),
    );
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
        <Show when={parseError()}>
          {(message) => <p class={styles.error}>{message()}</p>}
        </Show>
        <Show when={principal()}>
          {(found) => (
            <dl class={styles.confirm}>
              <ConfirmRow label="Client id" value={found().clientId} />
              <ConfirmRow label="Tenant id" value={found().tenantId} />
              <ConfirmRow
                label="Client secret"
                value="held, and never shown again"
              />
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

export const AzureSubscription: PageComponent<{ id: "azure-subscription" }> = (
  props,
) => {
  const linkAzure = useLinkAzure();
  const principal = props.state().answers.azurePrincipal;
  if (principal === null) {
    throw new Error(
      "the Azure subscription page was reached without a service principal",
    );
  }
  const [subscription, setSubscription] = createSignal(
    props.state().answers.azureSubscription ?? "",
  );

  const primary = (): Primary => {
    const id = subscription().trim();
    if (id === "") {
      return {
        label: "Link Azure",
        disabled: "Paste the subscription id to continue",
        onClick: () => undefined,
      };
    }
    return linkAzure(principal, id, () =>
      props.advance({ azureSubscription: id }),
    );
  };

  return {
    title: "Which subscription?",
    body: (
      <>
        <p class={styles.lede}>
          That block carries no subscription. Run this in Cloud Shell and paste
          the id it prints.
        </p>
        <CommandBlock value={SHOW_SUBSCRIPTION} label="Copy" wrap />
        <ExternalLink href={AZURE_CLOUD_SHELL}>
          Open Azure Cloud Shell
        </ExternalLink>
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
