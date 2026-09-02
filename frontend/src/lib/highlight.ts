/**
 * Syntax highlighting for the read-only code view.
 *
 * `highlight.js` does the tokenizing — its common bundle covers every
 * language a session is plausibly working in, and a hand-rolled tokenizer
 * would be a worse one of those. What this module owns is the two
 * decisions the library leaves open: which language a path is, and what
 * happens when the answer is "no idea".
 *
 * # An unknown file is not auto-detected
 *
 * `highlightAuto` guesses, and on a short file — a `.env`, a two-line
 * config, a fixture — it guesses confidently and wrongly, which reads as a
 * bug in the product rather than a limit of the guesser. So a file whose
 * extension names no language is rendered as plain text, and the view says
 * so by simply not colouring anything.
 *
 * # The output is sanitized anyway
 *
 * `highlight.js` escapes the source it is given, so its output is already
 * safe; it is put through DOMPurify regardless, because this is the one
 * place in the app where the *contents of a session's disk* become markup,
 * and a second lock on that door costs a microsecond.
 */
import DOMPurify from "dompurify";
import hljs from "highlight.js/lib/common";

/**
 * Extensions that name a language in the common bundle.
 *
 * Only mappings that are not already the language's own name: `hljs`
 * resolves `rust`, `python`, `json` and their documented aliases itself, so
 * listing them here would be a second copy of its alias table to keep
 * correct.
 */
const BY_EXTENSION: Readonly<Record<string, string>> = {
  cjs: "javascript",
  jsx: "javascript",
  mjs: "javascript",
  mts: "typescript",
  tsx: "typescript",
  htm: "xml",
  html: "xml",
  svg: "xml",
  vue: "xml",
  yml: "yaml",
  toml: "ini",
  cfg: "ini",
  conf: "ini",
  lock: "ini",
  rs: "rust",
  py: "python",
  rb: "ruby",
  sh: "bash",
  zsh: "bash",
  fish: "bash",
  kt: "kotlin",
  md: "markdown",
  markdown: "markdown",
  patch: "diff",
  h: "c",
  hpp: "cpp",
  cc: "cpp",
  m: "objectivec",
};

/** Filenames that name a language without having an extension at all. */
const BY_NAME: Readonly<Record<string, string>> = {
  makefile: "makefile",
  dockerfile: "bash",
  "cargo.lock": "ini",
};

/**
 * The language a path is written in, or `null` when nothing here knows.
 *
 * `null` is a real answer rather than a failure: the view renders the file
 * as plain text and nothing is guessed at.
 */
export function languageOf(path: string): string | null {
  const name = (path.split("/").pop() ?? "").toLowerCase();
  const byName = BY_NAME[name];
  if (byName !== undefined) {
    return byName;
  }

  const extension = name.includes(".") ? (name.split(".").pop() ?? "") : "";
  if (extension === "") {
    return null;
  }
  const mapped = BY_EXTENSION[extension] ?? extension;
  return hljs.getLanguage(mapped) === undefined ? null : mapped;
}

/** Highlights `code` as `language`, as sanitized HTML. */
export function highlightHtml(code: string, language: string): string {
  const { value } = hljs.highlight(code, { language, ignoreIllegals: true });
  return DOMPurify.sanitize(value);
}
