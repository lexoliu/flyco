/**
 * Linking a compute account, wherever the user is standing when they do it.
 *
 * Reached from `/connect/compute`, the welcome flow, and the settings tab,
 * so it lives here rather than inside any one of them. The per-provider
 * wizards of docs/ux.md §7 are issue #61; this is the credential form that
 * already worked, unchanged in behaviour and moved so the new routes are
 * not a second copy of it.
 */
import { For, Show, createSignal } from "solid-js";
import ProblemNotice from "../ProblemNotice";
import {
  linkProvider,
  type CloudProviderKind,
  type ProviderCredentials,
} from "../../api/client";
import { PROVIDER_LABEL } from "../../lib/providers";
import styles from "../Panel.module.css";

export interface ProviderLinkFormProps {
  /** Called after a successful link, so readiness can be reloaded. */
  onLinked: () => void;
}

export default function ProviderLinkForm(props: ProviderLinkFormProps) {
  const [kind, setKind] = createSignal<CloudProviderKind>("aws");
  const [label, setLabel] = createSignal("");

  // AWS
  const [accessKeyId, setAccessKeyId] = createSignal("");
  const [secretAccessKey, setSecretAccessKey] = createSignal("");
  // Azure
  const [clientId, setClientId] = createSignal("");
  const [clientSecret, setClientSecret] = createSignal("");
  const [subscriptionId, setSubscriptionId] = createSignal("");
  const [tenantId, setTenantId] = createSignal("");
  const [resourceGroup, setResourceGroup] = createSignal("");
  const [adminSshPublicKey, setAdminSshPublicKey] = createSignal("");
  // GCP
  const [serviceAccountJson, setServiceAccountJson] = createSignal("");
  // BYO SSH
  const [host, setHost] = createSignal("");
  const [hostFingerprint, setHostFingerprint] = createSignal("");
  const [port, setPort] = createSignal(22);
  const [sshUser, setSshUser] = createSignal("");
  const [privateKey, setPrivateKey] = createSignal("");

  const [submitting, setSubmitting] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  function buildCredentials(): ProviderCredentials {
    switch (kind()) {
      case "aws":
        return { kind: "aws", access_key_id: accessKeyId(), secret_access_key: secretAccessKey() };
      case "azure":
        return {
          kind: "azure",
          client_id: clientId(),
          client_secret: clientSecret(),
          subscription_id: subscriptionId(),
          tenant_id: tenantId(),
          resource_group: resourceGroup(),
          admin_ssh_public_key: adminSshPublicKey(),
        };
      case "gcp":
        return { kind: "gcp", service_account_json: serviceAccountJson() };
      case "byo_ssh":
        return {
          kind: "byo_ssh",
          host: host(),
          host_fingerprint: hostFingerprint(),
          port: port(),
          user: sshUser(),
          private_key: privateKey(),
        };
    }
  }

  async function onSubmit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      await linkProvider({ label: label(), credentials: buildCredentials() });
      setLabel("");
      props.onLinked();
    } catch (err) {
      setError(err);
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <form class={styles.form} onSubmit={(event) => void onSubmit(event)}>
      <div class={styles.field}>
        <label for="provider-kind">Provider</label>
        <select id="provider-kind" value={kind()} onChange={(event) => setKind(event.currentTarget.value as CloudProviderKind)}>
          <For each={Object.entries(PROVIDER_LABEL) as [CloudProviderKind, string][]}>
            {([value, providerLabel]) => <option value={value}>{providerLabel}</option>}
          </For>
        </select>
      </div>
      <div class={styles.field}>
        <label for="provider-label">Label</label>
        <input id="provider-label" value={label()} onInput={(event) => setLabel(event.currentTarget.value)} required />
      </div>

      <Show when={kind() === "aws"}>
        <div class={styles.field}>
          <label for="aws-access-key">Access key ID</label>
          <input id="aws-access-key" value={accessKeyId()} onInput={(event) => setAccessKeyId(event.currentTarget.value)} />
        </div>
        <div class={styles.field}>
          <label for="aws-secret-key">Secret access key</label>
          <input
            id="aws-secret-key"
            type="password"
            value={secretAccessKey()}
            onInput={(event) => setSecretAccessKey(event.currentTarget.value)}
          />
        </div>
      </Show>

      <Show when={kind() === "azure"}>
        <div class={styles.field}>
          <label for="azure-client-id">Client ID</label>
          <input id="azure-client-id" value={clientId()} onInput={(event) => setClientId(event.currentTarget.value)} />
        </div>
        <div class={styles.field}>
          <label for="azure-client-secret">Client secret</label>
          <input
            id="azure-client-secret"
            type="password"
            value={clientSecret()}
            onInput={(event) => setClientSecret(event.currentTarget.value)}
          />
        </div>
        <div class={styles.field}>
          <label for="azure-subscription-id">Subscription ID</label>
          <input
            id="azure-subscription-id"
            value={subscriptionId()}
            onInput={(event) => setSubscriptionId(event.currentTarget.value)}
          />
        </div>
        <div class={styles.field}>
          <label for="azure-tenant-id">Tenant ID</label>
          <input id="azure-tenant-id" value={tenantId()} onInput={(event) => setTenantId(event.currentTarget.value)} />
        </div>
        <div class={styles.field}>
          <label for="azure-resource-group">Resource group</label>
          <input
            id="azure-resource-group"
            value={resourceGroup()}
            onInput={(event) => setResourceGroup(event.currentTarget.value)}
            placeholder="Where session machines are created"
          />
        </div>
        <div class={styles.field}>
          <label for="azure-admin-ssh-key">Admin SSH public key</label>
          <textarea
            id="azure-admin-ssh-key"
            rows="2"
            value={adminSshPublicKey()}
            onInput={(event) => setAdminSshPublicKey(event.currentTarget.value)}
            placeholder="ssh-ed25519 AAAA..."
          />
        </div>
      </Show>

      <Show when={kind() === "gcp"}>
        <div class={styles.field}>
          <label for="gcp-service-account">Service account JSON</label>
          <textarea
            id="gcp-service-account"
            rows="4"
            value={serviceAccountJson()}
            onInput={(event) => setServiceAccountJson(event.currentTarget.value)}
          />
        </div>
      </Show>

      <Show when={kind() === "byo_ssh"}>
        <div class={styles.field}>
          <label for="ssh-host">Host</label>
          <input id="ssh-host" value={host()} onInput={(event) => setHost(event.currentTarget.value)} />
        </div>
        <div class={styles.field}>
          <label for="ssh-host-fingerprint">Host key fingerprint (ssh-keygen -lf, SHA256:…)</label>
          <input
            id="ssh-host-fingerprint"
            value={hostFingerprint()}
            onInput={(event) => setHostFingerprint(event.currentTarget.value)}
            placeholder="SHA256:..."
          />
        </div>
        <div class={styles.field}>
          <label for="ssh-port">Port</label>
          <input id="ssh-port" type="number" value={port()} onInput={(event) => setPort(Number(event.currentTarget.value))} />
        </div>
        <div class={styles.field}>
          <label for="ssh-user">User (must be able to run Podman)</label>
          <input id="ssh-user" value={sshUser()} onInput={(event) => setSshUser(event.currentTarget.value)} />
        </div>
        <div class={styles.field}>
          <label for="ssh-private-key">Private key (PEM)</label>
          <textarea
            id="ssh-private-key"
            rows="4"
            value={privateKey()}
            onInput={(event) => setPrivateKey(event.currentTarget.value)}
          />
        </div>
      </Show>

      <ProblemNotice error={error()} />
      <button type="submit" class={styles.primaryButton} disabled={submitting()}>
        {submitting() ? "Linking…" : "Link provider"}
      </button>
    </form>
  );
}
