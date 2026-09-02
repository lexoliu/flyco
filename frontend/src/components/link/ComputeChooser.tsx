/**
 * Choosing a cloud, and linking it — the whole of docs/ux.md §7 minus the
 * page it sits on.
 *
 * A component rather than a route body, because it is needed in two places:
 * `/connect/compute` wraps it in a heading, and the welcome flow's third
 * screen shows it inline (§4). Those have to be the same chooser — a second
 * copy is a second set of wizards to keep in step with three providers'
 * consoles.
 *
 * The fourth card is the one that takes no credential. The control plane
 * runs on Cloudflare Workers and has no TCP sockets, so it cannot dial a
 * machine the user owns: that machine is *enrolled* rather than dialled, and
 * its wizard is one command and a wait rather than a form.
 */
import { For, Match, Show, Switch, createMemo, createResource, createSignal } from "solid-js";
import { A } from "@solidjs/router";
import { ArrowLeft, ChevronRight, Server } from "lucide-solid";
import ComputeCard from "../ComputeCard";
import Logomark, { AWS_MARK, AZURE_MARK, GOOGLE_CLOUD_MARK, type Mark } from "../Logomark";
import { useReadiness } from "../Readiness";
import HostWizard from "./HostWizard";
import {
  linkProvider,
  listCloudUsage,
  type CloudProviderKind,
  type ProviderAccountView,
  type ProviderCredentials,
} from "../../api/client";
import { setSpotPreference, spotPreference } from "../../lib/localPreferences";
import AwsWizard from "../../routes/connect/AwsWizard";
import AzureWizard from "../../routes/connect/AzureWizard";
import BonusProgrammes from "../../routes/connect/BonusProgrammes";
import GcpWizard from "../../routes/connect/GcpWizard";
import styles from "../../routes/connect/Connect.module.css";

/** One card in the chooser: what it is, and the one line describing it. */
interface Choice {
  readonly kind: CloudProviderKind;
  readonly title: string;
  readonly line: string;
  readonly mark: Mark | null;
}

const CHOICES: Choice[] = [
  {
    kind: "azure",
    title: "Azure",
    line: "One command in Cloud Shell, then paste what it printed.",
    mark: AZURE_MARK,
  },
  {
    kind: "aws",
    title: "AWS",
    line: "An IAM user carrying the policy flyco actually calls.",
    mark: AWS_MARK,
  },
  {
    kind: "gcp",
    title: "Google Cloud",
    line: "A Compute Admin service account, dropped in as its key file.",
    mark: GOOGLE_CLOUD_MARK,
  },
  {
    kind: "host",
    title: "Your own machine",
    line: "Enroll a Linux machine you own with one command.",
    mark: null,
  },
];

/** A wizard that is open, and how far into it the user is. */
interface OpenStage {
  readonly at: "bonus" | "credentials";
  readonly kind: CloudProviderKind;
}

/** Where the chooser is: choosing, or somewhere inside one wizard. */
type Stage = { readonly at: "choosing" } | OpenStage;

export interface ComputeChooserProps {
  /** Called after a link succeeds, once readiness has been reloaded. */
  onLinked?: (() => void) | undefined;
}

export default function ComputeChooser(props: ComputeChooserProps) {
  const readiness = useReadiness();
  const [usage, { refetch: refetchUsage }] = createResource(() => listCloudUsage());
  const [stage, setStage] = createSignal<Stage>({ at: "choosing" });
  const [linking, setLinking] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const [linked, setLinked] = createSignal<ProviderAccountView | undefined>();
  const [spot, setSpot] = createSignal(spotPreference());

  /** The wizard on screen, or nothing while the chooser is. */
  const open = createMemo<OpenStage | null>(() => {
    const current = stage();
    return current.at === "choosing" ? null : current;
  });

  async function link(credentials: ProviderCredentials, label: string): Promise<void> {
    setLinking(true);
    setError(null);
    try {
      const account = await linkProvider({ label, credentials });
      setLinked(account);
      setStage({ at: "choosing" });
      await readiness.refresh();
      void refetchUsage();
      props.onLinked?.();
    } catch (failure) {
      setError(failure);
    } finally {
      setLinking(false);
    }
  }

  function chooseSpot(next: boolean): void {
    setSpot(next);
    setSpotPreference(next);
  }

  /**
   * Opens one provider's wizard.
   *
   * A machine the user owns skips the bonus questions: there is no free
   * credit for hardware somebody already bought, and asking whether they
   * are new to their own computer is a question with no answer.
   */
  function choose(kind: CloudProviderKind): void {
    setError(null);
    setStage({ at: kind === "host" ? "credentials" : "bonus", kind });
  }

  /** A machine arrived: readiness has a new compute account to learn about. */
  async function enrolled(): Promise<void> {
    await readiness.refresh();
    void refetchUsage();
    props.onLinked?.();
  }

  return (
    <div class={styles.chooserRoot}>
      <Show when={linked()}>
        {(account) => (
          <div class={styles.stage}>
            <p class={styles.stageTitle}>Linked</p>
            <ComputeCard
              account={account()}
              usage={usage()?.find((row) => row.account === account().id)}
              spot={spot()}
              onSpot={chooseSpot}
            />
          </div>
        )}
      </Show>

      <Switch>
        <Match when={open() === null}>
          <ul class={styles.chooser}>
            <For each={CHOICES}>
              {(choice) => (
                <li>
                  <button type="button" class={styles.choice} onClick={() => choose(choice.kind)}>
                    <span class={styles.choiceMark}>
                      <Show
                        when={choice.mark}
                        /* A machine the user owns has no vendor behind it,
                           so it gets the generic server glyph. */
                        fallback={<Server size={18} aria-hidden="true" />}
                      >
                        {(mark) => <Logomark mark={mark()} size={18} />}
                      </Show>
                    </span>
                    <span class={styles.choiceText}>
                      <span class={styles.choiceTitle}>{choice.title}</span>
                      <span class={styles.choiceLine}>{choice.line}</span>
                    </span>
                    <ChevronRight size={15} aria-hidden="true" class={styles.choiceGlyph ?? ""} />
                  </button>
                </li>
              )}
            </For>
          </ul>
        </Match>

        <Match when={open()}>
          {(wizard) => (
            <div class={styles.wizard}>
              <div class={styles.wizardHead}>
                <button
                  type="button"
                  class={styles.back}
                  onClick={() => setStage({ at: "choosing" })}
                >
                  <ArrowLeft size={14} aria-hidden="true" />
                  All providers
                </button>
                <h2>
                  {CHOICES.find((choice) => choice.kind === wizard().kind)?.title ?? "Compute"}
                </h2>
              </div>

              <Switch>
                <Match when={wizard().at === "bonus"}>
                  <BonusProgrammes
                    provider={wizard().kind}
                    onContinue={() => setStage({ at: "credentials", kind: wizard().kind })}
                  />
                </Match>
                <Match when={wizard().at === "credentials" && wizard().kind === "azure"}>
                  <AzureWizard onLink={link} linking={linking()} error={error()} />
                </Match>
                <Match when={wizard().at === "credentials" && wizard().kind === "aws"}>
                  <AwsWizard onLink={link} linking={linking()} error={error()} />
                </Match>
                <Match when={wizard().at === "credentials" && wizard().kind === "gcp"}>
                  <GcpWizard onLink={link} linking={linking()} error={error()} />
                </Match>
                <Match when={wizard().at === "credentials" && wizard().kind === "host"}>
                  <HostWizard onEnrolled={() => void enrolled()} />
                </Match>
              </Switch>
            </div>
          )}
        </Match>
      </Switch>

      <Show when={readiness.compute().length > 0 && linked() === undefined}>
        <p class={styles.hint}>
          {readiness.compute().length === 1
            ? "One compute account is already linked."
            : `${readiness.compute().length} compute accounts are already linked.`}{" "}
          <A href="/settings/compute">Manage them in settings</A>
        </p>
      </Show>
    </div>
  );
}
