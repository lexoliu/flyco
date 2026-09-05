/**
 * `/connect/return` — where a vendor's consent screen sends the browser.
 *
 * The sign-in happened in a tab the flow opened; the flow itself is still
 * waiting in the tab it started from, polling the attempt. When the
 * sign-in went through, this tab has one thing to say and nothing to do.
 * When it did not, this tab is the one the user is looking at, so it says
 * what the vendor said, names the way round it, and carries one action
 * back into the flow to pick another road.
 */
import { useNavigate, useSearchParams } from "@solidjs/router";
import { Show } from "solid-js";
import { PROVIDER_LABEL } from "../../lib/providers";
import styles from "../../components/flow/pages/pages.module.css";
import frame from "../../components/flow/Flow.module.css";

/** Where "Try another way" goes: the compute stage, from its first page. */
const COMPUTE_STAGE = "/connect/compute";

/** The vendors this page can name; anything else is "the provider". */
function vendorName(param: string | undefined): string {
  return param === "azure" || param === "gcp"
    ? PROVIDER_LABEL[param]
    : "the provider";
}

/** What each problem slug means to the person reading it. */
function explain(problem: string, vendor: string): string {
  switch (problem) {
    case "microsoft-rejected":
    case "google-rejected":
      return `${vendor} did not grant the sign-in. Either the consent was declined, or your organization needs an administrator to approve apps like flyco first. Cloud Shell needs no approval, and flyco can link through it instead.`;
    case "unknown-oauth-state":
      return "This link was already used, or the sign-in it belongs to has expired.";
    default:
      return "The control plane could not complete the sign-in.";
  }
}

export default function ConnectReturn() {
  const [params] = useSearchParams<{
    provider?: string;
    problem?: string;
    reason?: string;
  }>();
  const navigate = useNavigate();
  const vendor = () => vendorName(params.provider);
  const problem = () => {
    const slug = params.problem;
    return slug !== undefined && slug !== "" ? slug : null;
  };

  return (
    <div class={frame.page}>
      <section class={frame.card} aria-labelledby="return-title">
        <div class={`${frame.body} ${frame.bodyShort}`}>
          <h1 id="return-title" class={frame.title}>
            <Show when={problem()} fallback={<>Signed in with {vendor()}</>}>
              {vendor()} did not complete the sign-in
            </Show>
          </h1>
          <Show
            when={problem()}
            fallback={
              <p class={styles.lede}>
                You can close this tab. Flyco carries on in the tab you started
                from.
              </p>
            }
          >
            {(slug) => (
              <>
                <p class={styles.lede}>{explain(slug(), vendor())}</p>
                <Show when={params.reason}>
                  {(reason) => (
                    <p class={styles.hint}>
                      {vendor()} said: <code>{reason()}</code>
                    </p>
                  )}
                </Show>
              </>
            )}
          </Show>
        </div>
        <Show when={problem()}>
          <footer class={frame.footer}>
            <button
              type="button"
              class={frame.primary}
              onClick={() => navigate(COMPUTE_STAGE)}
            >
              Try another way
            </button>
          </footer>
        </Show>
      </section>
    </div>
  );
}
