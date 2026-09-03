/**
 * AWS's pages (docs/ux.md §4 C5′–C6′, §7.3): the real policy, then the key.
 *
 * The policy is fetched rather than checked in beside this file. It is
 * rendered from the driver's own call sites
 * (`flyco_provider::aws::iam::actions`), so what the user attaches is
 * exactly what flyco calls: a policy copied here would be right on the day
 * it was written and quietly wrong afterwards, and the failure mode is an
 * `UnauthorizedOperation` two minutes into someone's first session.
 *
 * The session token is the one optional field, revealed in place by a
 * quiet link: a temporary credential from `sts:AssumeRole` carries one and
 * a plain access key does not.
 */
import { Show, createSignal } from "solid-js";
import CommandBlock from "../../CommandBlock";
import ProblemNotice from "../../ProblemNotice";
import { useReadiness } from "../../Readiness";
import { getAwsIamPolicy, linkProvider } from "../../../api/client";
import { createQuery } from "../../../lib/query";
import type { PageComponent, Primary } from "../page";
import { ExternalLink, QuietLink } from "./shared";
import styles from "./pages.module.css";

/** Where an access key is created, deep-linked to the right console page. */
const IAM_USERS_CONSOLE = "https://console.aws.amazon.com/iam/home#/users";

export const AwsPolicy: PageComponent<{ id: "aws-policy" }> = (props) => {
  const [policy, { refetch }] = createQuery(getAwsIamPolicy);

  // The policy is part of what is being linked: a key attached to nothing
  // would validate and fail on the first `RunInstances`. So the page holds
  // `Next` until the document the user is meant to attach has been shown.
  const primary = (): Primary => ({
    label: "Next",
    disabled:
      policy.state === "ready"
        ? null
        : policy.error === undefined
          ? "Reading the policy…"
          : "The policy could not be read; retry it to continue",
    onClick: () => props.advance(),
  });

  return {
    title: "Create an access key",
    body: (
      <>
        <p class={styles.lede}>
          Make an IAM user, attach this policy to it, and create an access key
          for it. The policy is generated from the calls the driver actually
          makes, so it grants what flyco needs and no more.
        </p>
        <Show
          when={policy.error === undefined}
          fallback={
            <ProblemNotice
              error={policy.error}
              action={{ label: "Retry", onClick: () => void refetch() }}
            />
          }
        >
          <Show
            when={policy()}
            fallback={
              <div
                class={`${styles.skeleton} ${styles.skeletonPolicy}`}
                aria-label="Reading the policy"
              />
            }
          >
            {(document) => (
              <CommandBlock
                value={document().document}
                label="Copy"
                caption={`${document().actions.length} actions, and nothing else`}
              />
            )}
          </Show>
        </Show>
        <ExternalLink href={IAM_USERS_CONSOLE}>
          Open the IAM console
        </ExternalLink>
      </>
    ),
    primary,
  };
};

export const AwsKeys: PageComponent<{ id: "aws-keys" }> = (props) => {
  const readiness = useReadiness();
  const [accessKeyId, setAccessKeyId] = createSignal("");
  const [secretAccessKey, setSecretAccessKey] = createSignal("");
  const [sessionToken, setSessionToken] = createSignal<string | null>(null);

  const primary = (): Primary => {
    const id = accessKeyId().trim();
    const secret = secretAccessKey().trim();
    return {
      label: "Link AWS",
      busy: "Linking…",
      disabled:
        id === ""
          ? "Enter the access key ID to continue"
          : secret === ""
            ? "Enter the secret access key to continue"
            : null,
      onClick: async () => {
        if (id === "" || secret === "") {
          return;
        }
        const token = sessionToken()?.trim() ?? "";
        await linkProvider({
          label: "AWS",
          credentials: {
            kind: "aws",
            access_key_id: id,
            secret_access_key: secret,
            ...(token === "" ? {} : { session_token: token }),
          },
        });
        await readiness.refresh();
        props.advance();
      },
    };
  };

  return {
    title: "Enter the access key",
    body: (
      <>
        <label class={styles.field}>
          <span>Access key ID</span>
          <input
            class={`${styles.input} ${styles.mono}`}
            autocomplete="off"
            spellcheck={false}
            placeholder="AKIA…"
            value={accessKeyId()}
            onInput={(event) => setAccessKeyId(event.currentTarget.value)}
            autofocus
          />
        </label>
        <label class={styles.field}>
          <span>Secret access key</span>
          <input
            class={`${styles.input} ${styles.mono}`}
            type="password"
            autocomplete="off"
            value={secretAccessKey()}
            onInput={(event) => setSecretAccessKey(event.currentTarget.value)}
          />
        </label>
        <Show
          when={sessionToken() !== null}
          fallback={
            <QuietLink onClick={() => setSessionToken("")}>
              I have a session token
            </QuietLink>
          }
        >
          <div class={styles.field}>
            <label for="aws-session-token">Session token</label>
            <input
              id="aws-session-token"
              class={`${styles.input} ${styles.mono}`}
              autocomplete="off"
              spellcheck={false}
              value={sessionToken() ?? ""}
              onInput={(event) => setSessionToken(event.currentTarget.value)}
              autofocus
            />
            <p class={styles.hint}>
              Only a temporary credential from <code>sts:AssumeRole</code>{" "}
              carries one.
            </p>
          </div>
        </Show>
      </>
    ),
    primary,
  };
};
