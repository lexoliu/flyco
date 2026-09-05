/**
 * `/connect/return` — where a vendor's consent screen sends the browser.
 *
 * The sign-in happened in a tab the flow opened; the flow itself is still
 * waiting in the tab it started from, polling the attempt. This tab has
 * one thing to say and nothing to do, so it says it and stops. A consent
 * the control plane could not complete arrives here too, named by
 * `?problem=`, so the failure is readable where it happened.
 */
import { useSearchParams } from "@solidjs/router";
import { Show } from "solid-js";
import { PROVIDER_LABEL } from "../../lib/providers";
import styles from "../../components/flow/pages/pages.module.css";
import frame from "../../components/flow/Flow.module.css";

/** What the vendor is called on this page, when the query names one flyco knows. */
function vendorName(param: string | undefined): string {
  return param === "azure" || param === "gcp"
    ? PROVIDER_LABEL[param]
    : "the provider";
}

export default function ConnectReturn() {
  const [params] = useSearchParams<{ provider?: string; problem?: string }>();
  const failed = () => params.problem !== undefined && params.problem !== "";

  return (
    <div class={frame.page}>
      <section class={frame.card} aria-labelledby="return-title">
        <div class={frame.body}>
          <h1 id="return-title" class={frame.title}>
            <Show
              when={failed()}
              fallback={<>Signed in with {vendorName(params.provider)}</>}
            >
              {vendorName(params.provider)} could not finish the sign-in
            </Show>
          </h1>
          <Show
            when={failed()}
            fallback={
              <p class={styles.lede}>
                You can close this tab. Flyco carries on in the tab you started
                from.
              </p>
            }
          >
            <p class={styles.lede}>
              The control plane could not complete it:{" "}
              <code>{params.problem}</code>. Go back to the tab you started from
              and try again.
            </p>
          </Show>
        </div>
      </section>
    </div>
  );
}
