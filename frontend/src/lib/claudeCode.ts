/**
 * What Anthropic shows at the end of the Claude sign-in, and how to read it.
 *
 * The authorize page renders `CODE#STATE`. People paste it with a trailing
 * newline, with the quotation marks a terminal added, or — because the two
 * halves look like two things — with only the first half selected. All of
 * those are the same intent, so this normalizes them into the one string the
 * control plane is asked to redeem.
 *
 * The state is *not* dropped when it is there: the control plane checks it
 * against the state it minted, which is what stops a code from one sign-in
 * being pasted into another. This only tidies what the browser sends.
 */

/** The two halves Anthropic's code carries, once read. */
export interface PastedCode {
  /** The authorization code itself. */
  code: string;
  /** The state Anthropic echoed, when the paste carried one. */
  state: string | null;
}

/** Characters people paste around a copied value without meaning to. */
const SURROUNDING = /^["'`\s]+|["'`\s]+$/g;

/**
 * Reads a pasted code, or `null` when there is nothing to redeem yet.
 *
 * `null` is what the button reads to stay disabled: an empty field, a stray
 * `#`, or a paste that is only a state are all "not a code" rather than a
 * code that will be refused a round trip later.
 */
export function parsePastedCode(pasted: string): PastedCode | null {
  const trimmed = pasted.replace(SURROUNDING, "");
  if (trimmed === "") {
    return null;
  }

  const separator = trimmed.indexOf("#");
  if (separator === -1) {
    return { code: trimmed, state: null };
  }

  const code = trimmed.slice(0, separator).trim();
  const state = trimmed.slice(separator + 1).trim();
  if (code === "") {
    return null;
  }
  return { code, state: state === "" ? null : state };
}

/**
 * The value `POST .../claude/oauth/complete` is given.
 *
 * Normalized rather than split apart: the endpoint takes one field and
 * validates the state itself, so sending the halves back joined keeps the
 * check on the server where it belongs.
 */
export function pastedCodeForExchange(pasted: PastedCode): string {
  return pasted.state === null ? pasted.code : `${pasted.code}#${pasted.state}`;
}

/**
 * The two secrets Anthropic hands out that flyco can run Claude Code on,
 * told apart by the prefix Anthropic gives them.
 *
 * `claude setup-token` prints a long-lived OAuth token, `sk-ant-oat01-…`;
 * the console issues API keys, `sk-ant-api03-…`. The one field on the
 * API-key page (docs/ux.md §4 B2″) takes either, and this decides which
 * credential kind the control plane is sent — the tokens describe
 * themselves, so nothing has to be asked.
 */
export type ClaudeSecret =
  | { readonly kind: "claude_setup_token"; readonly token: string }
  | { readonly kind: "claude_api_key"; readonly key: string };

/** The prefix each secret carries, as Anthropic issues them. */
const SETUP_TOKEN_PREFIX = "sk-ant-oat";
const API_KEY_PREFIX = "sk-ant-api";

/** The two forms, as the field's error names them. */
export const CLAUDE_SECRET_FORMS = `${SETUP_TOKEN_PREFIX}01-… or ${API_KEY_PREFIX}03-…`;

/**
 * Reads a pasted secret, or `null` when it is neither an API key nor a
 * setup token.
 *
 * `null` is what the page reads to say which two forms it takes rather
 * than guess: a value with neither prefix is not a credential Anthropic
 * issued, and sending it as one would fail later with a worse message.
 */
export function parseClaudeSecret(pasted: string): ClaudeSecret | null {
  const value = pasted.replace(SURROUNDING, "");
  if (value.startsWith(SETUP_TOKEN_PREFIX)) {
    return { kind: "claude_setup_token", token: value };
  }
  if (value.startsWith(API_KEY_PREFIX)) {
    return { kind: "claude_api_key", key: value };
  }
  return null;
}
