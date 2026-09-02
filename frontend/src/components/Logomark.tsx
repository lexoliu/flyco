/**
 * Vendor logomarks, as inline SVG in the ink of whatever row they sit in.
 *
 * Identity is the one place this design system spends a graphic on: a chip
 * that says "Claude Code" or "Azure" is read faster with the mark than
 * without it, and there is no accent colour doing that job. Everything else
 * on screen is a Lucide glyph.
 *
 * Two sources, for one reason. `simple-icons` ships Anthropic, GitHub and
 * Google Cloud as single monochrome paths and is the package to use for
 * them. It has **removed** OpenAI, Amazon Web Services and Microsoft Azure,
 * so those three are checked in under `src/assets/logos/` instead, taken
 * from the CC0 SVG Logos collection and recoloured to `currentColor`; each
 * file records where it came from. A mark is sized by its height so a wide
 * lockup (the AWS one) and a square glyph sit on the same baseline.
 */
import { For } from "solid-js";
import { siAnthropic, siGithub, siGooglecloud } from "simple-icons";
import type { CloudProviderKind, HarnessKind } from "../api/client";
import awsAsset from "../assets/logos/aws.svg?raw";
import azureAsset from "../assets/logos/azure.svg?raw";
import openaiAsset from "../assets/logos/openai.svg?raw";
import styles from "./Logomark.module.css";

/** One vendor mark: what it is, the box it is drawn in, and its geometry. */
export interface Mark {
  /** Vendor name, used as the accessible label when the mark stands alone. */
  readonly title: string;
  /** `viewBox` the paths are drawn against. */
  readonly viewBox: string;
  /** `d` of every path in the mark, drawn in `currentColor`. */
  readonly paths: readonly string[];
}

/**
 * Reads a checked-in logo asset into a {@link Mark}.
 *
 * Fast fail: an asset whose `viewBox` or paths this cannot find is a broken
 * file, and rendering an empty `<svg>` would hide that until somebody
 * noticed a gap in a chip row.
 */
function assetMark(title: string, source: string): Mark {
  const viewBox = /viewBox="([^"]+)"/.exec(source)?.[1];
  if (viewBox === undefined) {
    throw new Error(`The ${title} logo asset has no viewBox.`);
  }
  const paths = [...source.matchAll(/\sd="([^"]+)"/g)].map((match) => match[1] ?? "");
  if (paths.length === 0) {
    throw new Error(`The ${title} logo asset has no paths.`);
  }
  return { title, viewBox, paths };
}

/** A `simple-icons` entry, which is always one path on a 24×24 grid. */
function iconMark(icon: { title: string; path: string }): Mark {
  return { title: icon.title, viewBox: "0 0 24 24", paths: [icon.path] };
}

export const GITHUB_MARK: Mark = iconMark(siGithub);
export const ANTHROPIC_MARK: Mark = iconMark(siAnthropic);
export const OPENAI_MARK: Mark = assetMark("OpenAI", openaiAsset);
export const AWS_MARK: Mark = assetMark("Amazon Web Services", awsAsset);
export const AZURE_MARK: Mark = assetMark("Microsoft Azure", azureAsset);
export const GOOGLE_CLOUD_MARK: Mark = iconMark(siGooglecloud);

/** The mark that stands for each harness. */
export const HARNESS_MARK: Record<HarnessKind, Mark> = {
  claude_code: ANTHROPIC_MARK,
  codex: OPENAI_MARK,
};

/**
 * The mark that stands for each compute provider.
 *
 * A machine the user owns has no vendor behind it, so it borrows GitHub's
 * mark from nobody: it is drawn with a Lucide `Server` glyph by the caller
 * instead, and is deliberately absent here.
 */
export const PROVIDER_MARK: Partial<Record<CloudProviderKind, Mark>> = {
  azure: AZURE_MARK,
  aws: AWS_MARK,
  gcp: GOOGLE_CLOUD_MARK,
};

export interface LogomarkProps {
  /** Which mark to draw. */
  mark: Mark;
  /** Height in `px`. Width follows the mark's own aspect ratio. */
  size?: number | undefined;
  /**
   * Whether the mark is the only thing naming the vendor.
   *
   * A mark beside the vendor's name is decoration and is hidden from
   * assistive technology; one standing alone carries the name itself.
   */
  labelled?: boolean | undefined;
}

export default function Logomark(props: LogomarkProps) {
  const size = () => props.size ?? 14;
  return (
    <svg
      class={styles.mark}
      viewBox={props.mark.viewBox}
      height={size()}
      fill="currentColor"
      role={props.labelled === true ? "img" : "presentation"}
      aria-label={props.labelled === true ? props.mark.title : undefined}
      aria-hidden={props.labelled === true ? undefined : "true"}
    >
      <For each={props.mark.paths}>{(path) => <path d={path} />}</For>
    </svg>
  );
}
