/**
 * Linking AWS: the real policy, then two fields (docs/ux.md §7.3).
 *
 * The policy is fetched rather than checked in beside this file. It is
 * rendered from the driver's own call sites
 * (`flyco_provider::aws::iam::actions`), so what the user attaches is exactly
 * what flyco calls: a policy that had been copied here would be right on the
 * day it was written and quietly wrong afterwards, and the failure mode is an
 * `UnauthorizedOperation` two minutes into someone's first session.
 *
 * The break-glass key pair is optional on AWS — EC2 creates an instance
 * perfectly well without one — so it is a name under `Advanced` rather than
 * a step, and it names a key pair that lives in the user's own account.
 */
import { Show, createSignal } from "solid-js";
import { createQuery } from "../../lib/query";
import { ExternalLink } from "lucide-solid";
import CommandBlock from "../../components/CommandBlock";
import Disclosure from "../../components/Disclosure";
import ProblemNotice from "../../components/ProblemNotice";
import { getAwsIamPolicy, type ProviderCredentials } from "../../api/client";
import styles from "./Connect.module.css";

/** Where an access key is created, deep-linked to the right console page. */
const IAM_USERS_CONSOLE = "https://console.aws.amazon.com/iam/home#/users";

export interface AwsWizardProps {
  onLink: (credentials: ProviderCredentials, label: string) => Promise<void>;
  linking: boolean;
  error: unknown;
}

export default function AwsWizard(props: AwsWizardProps) {
  const [policy, { refetch: refetchPolicy }] = createQuery(getAwsIamPolicy);
  const [accessKeyId, setAccessKeyId] = createSignal("");
  const [secretAccessKey, setSecretAccessKey] = createSignal("");
  const [sessionToken, setSessionToken] = createSignal("");
  const [keyName, setKeyName] = createSignal("");

  // The policy is part of what is being linked: an access key attached to
  // nothing would validate here and fail on the first `RunInstances`. So a
  // key typed before the policy has arrived waits for it.
  // Read as a state rather than a value: a rejected resource throws from
  // its accessor, and this is called on every keystroke.
  const ready = () =>
    policy.state === "ready" && accessKeyId().trim() !== "" && secretAccessKey().trim() !== "";

  async function link(): Promise<void> {
    const token = sessionToken().trim();
    const pair = keyName().trim();
    await props.onLink(
      {
        kind: "aws",
        access_key_id: accessKeyId().trim(),
        secret_access_key: secretAccessKey().trim(),
        ...(token === "" ? {} : { session_token: token }),
        ...(pair === "" ? {} : { key_name: pair }),
      },
      "AWS",
    );
  }

  return (
    <div class={styles.step}>
      <section class={styles.stage}>
        <p class={styles.stageTitle}>1 · Attach this policy to a new IAM user</p>
        {/*
          The error is checked before the value is read: a Solid resource
          that rejected *throws* from its accessor, so `policy()` inside the
          failure branch would take the whole wizard down with it, and the
          user would see neither a policy nor a reason.
        */}
        <Show
          when={policy.error === undefined}
          fallback={
            <ProblemNotice
              error={policy.error}
              action={{ label: "Retry", onClick: () => void refetchPolicy() }}
            />
          }
        >
          <Show
            when={policy()}
            fallback={<div class={styles.policySkeleton} aria-label="Reading the policy" />}
          >
            {(document) => (
              <>
                <CommandBlock
                  value={document().document}
                  label="Copy policy"
                  caption={`${document().actions.length} actions, and nothing else`}
                />
                <p class={styles.hint}>
                  This is generated from the calls the driver actually makes, so it grants what
                  flyco needs and no more.
                </p>
              </>
            )}
          </Show>
        </Show>
        <a
          class={styles.pill}
          href={IAM_USERS_CONSOLE}
          target="_blank"
          rel="noreferrer noopener"
        >
          Open the IAM console
          <ExternalLink size={13} aria-hidden="true" />
        </a>
      </section>

      <section class={styles.stage}>
        <p class={styles.stageTitle}>2 · Paste that user's access key</p>
        <label class={styles.field}>
          <span>Access key ID</span>
          <input
            class={styles.input}
            autocomplete="off"
            spellcheck={false}
            placeholder="AKIA…"
            value={accessKeyId()}
            onInput={(event) => setAccessKeyId(event.currentTarget.value)}
          />
        </label>
        <label class={styles.field}>
          <span>Secret access key</span>
          <input
            class={styles.input}
            type="password"
            autocomplete="off"
            value={secretAccessKey()}
            onInput={(event) => setSecretAccessKey(event.currentTarget.value)}
          />
        </label>

        <Disclosure summary="Advanced">
          <div class={styles.stage}>
            <label class={styles.field}>
              <span>Session token</span>
              <input
                class={styles.input}
                autocomplete="off"
                spellcheck={false}
                value={sessionToken()}
                onInput={(event) => setSessionToken(event.currentTarget.value)}
              />
              <span class={styles.hint}>
                Required only for a temporary credential from `sts:AssumeRole`.
              </span>
            </label>
            <label class={styles.field}>
              <span>EC2 key pair name</span>
              <input
                class={styles.input}
                autocomplete="off"
                spellcheck={false}
                value={keyName()}
                onInput={(event) => setKeyName(event.currentTarget.value)}
              />
              <span class={styles.hint}>
                A key pair in your own account, for a break-glass login. Machines start fine
                without one.
              </span>
            </label>
          </div>
        </Disclosure>
      </section>

      <ProblemNotice error={props.error} />
      <button
        type="button"
        class={styles.primary}
        disabled={!ready() || props.linking}
        onClick={() => void link()}
      >
        {props.linking ? "Linking…" : "Link AWS"}
      </button>
    </div>
  );
}
