/**
 * Assistant prose, rendered as Markdown (docs/ux.md §9.2).
 *
 * Two libraries, no hand-written parser: `marked` turns Markdown into HTML
 * and `dompurify` decides what of that HTML is allowed to exist. A model's
 * output is untrusted text — it quotes the web, the repository, and
 * whatever a tool returned — so it never reaches `innerHTML` unsanitized.
 * Writing either half by hand would be writing a security boundary by hand.
 *
 * Code blocks get a copy button, because the reason a code block is in a
 * transcript is that someone is about to use it. The button is added to the
 * rendered DOM afterwards rather than injected into the HTML string: a
 * `<button onclick=…>` written into the markup would have to survive
 * sanitization, and anything that survives sanitization is something an
 * attacker can also write.
 */
import { createEffect, onCleanup } from "solid-js";
import DOMPurify, { type Config } from "dompurify";
import { marked } from "marked";
import styles from "./Markdown.module.css";

/**
 * `marked` with the options this product wants, set once at module scope.
 *
 * `breaks` is on because chat prose is written with single newlines meaning
 * single newlines; `gfm` is on because every model writes GitHub-flavoured
 * Markdown — tables, fenced code, task lists.
 */
marked.use({ breaks: true, gfm: true });

/**
 * What sanitization keeps.
 *
 * An allowlist rather than a blocklist, and no `style`, `class` or `id`:
 * the stylesheet below owns the appearance of everything here, and an
 * attribute that could reach the page's own CSS would let a transcript
 * restyle the app around it.
 */
const SANITIZE: Config = {
  ALLOWED_TAGS: [
    "p", "br", "hr", "strong", "em", "del", "code", "pre", "blockquote",
    "ul", "ol", "li", "a", "h1", "h2", "h3", "h4", "h5", "h6",
    "table", "thead", "tbody", "tr", "th", "td", "img",
  ],
  ALLOWED_ATTR: ["href", "title", "src", "alt", "start", "colspan", "rowspan"],
  ALLOW_DATA_ATTR: false,
};

/** How long the copy button says it worked before going quiet again. */
const COPIED_MS = 1200;

export interface MarkdownProps {
  /** The Markdown source. Untrusted: it is a model's output. */
  text: string;
}

export default function Markdown(props: MarkdownProps) {
  let container: HTMLDivElement | undefined;
  const timers = new Set<ReturnType<typeof setTimeout>>();
  let frame = 0;
  let pending: string | null = null;

  onCleanup(() => {
    for (const timer of timers) {
      clearTimeout(timer);
    }
    if (frame !== 0) {
      cancelAnimationFrame(frame);
    }
  });

  /**
   * Parses and paints once per animation frame at most.
   *
   * A streamed paragraph's text changes on every delta, and each change
   * used to cost a full `marked.parse` + `innerHTML` — dozens of repaints
   * of the same block inside one frame, which is what made output janky.
   * The frame callback paints the newest text it was handed, so a burst of
   * deltas still costs one parse and nothing is ever rendered stale.
   */
  createEffect(() => {
    pending = props.text;
    if (frame === 0) {
      frame = requestAnimationFrame(() => {
        frame = 0;
        const host = container;
        const text = pending;
        pending = null;
        if (host === undefined || text === null) {
          return;
        }
        paint(host, text, timers);
      });
    }
  });

  return <div class={styles.markdown} ref={container} />;
}

/** One parse–sanitize–paint pass over `host`. */
function paint(
  host: HTMLDivElement,
  text: string,
  timers: Set<ReturnType<typeof setTimeout>>,
): void {
  // `marked.parse` is synchronous with these options; the async overload
  // only applies when an async extension is registered, and none is.
  const html = marked.parse(text) as string;
  host.innerHTML = DOMPurify.sanitize(html, SANITIZE);

  // Links in a transcript point outside the app and are not the app's
  // to navigate; opening them in a new tab keeps the session where it is.
  for (const link of host.querySelectorAll("a")) {
    link.setAttribute("target", "_blank");
    link.setAttribute("rel", "noreferrer noopener");
  }

  for (const block of host.querySelectorAll("pre")) {
    block.classList.add(styles.codeBlock ?? "");
    block.append(copyButton(block.textContent ?? "", timers));
  }
}

/** The copy control one code block carries. */
function copyButton(code: string, timers: Set<ReturnType<typeof setTimeout>>): HTMLButtonElement {
  const button = document.createElement("button");
  button.type = "button";
  button.className = styles.copy ?? "";
  button.textContent = "Copy";
  button.addEventListener("click", () => {
    void navigator.clipboard.writeText(code).then(
      () => {
        button.textContent = "Copied";
        const timer = setTimeout(() => {
          button.textContent = "Copy";
          timers.delete(timer);
        }, COPIED_MS);
        timers.add(timer);
      },
      () => {
        // A clipboard a browser refuses (an insecure origin, a denied
        // permission) is worth saying so rather than looking like a button
        // that does nothing.
        button.textContent = "Copy failed";
      },
    );
  });
  return button;
}
