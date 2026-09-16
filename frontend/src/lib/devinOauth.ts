/**
 * What Devin's redirect leaves in the address bar, and how much of it the
 * page needs to read.
 *
 * Devin's OAuth client admits only localhost-shaped redirect URIs, so a
 * flyco grant is bound to a dead `127.0.0.1` address: after approval the
 * page cannot load, and the code stays in the address bar of the browser's
 * own error page. The user copies the whole thing —
 * `http://127.0.0.1:59653/callback?code=…&state=…` — or just the code out
 * of it, and the paste field takes either.
 *
 * This reads only enough to know whether the field holds something worth
 * sending: the control plane is the authoritative parser, and what is
 * sent is the paste itself, untouched.
 */

/** What one paste of the redirect's address (or the code alone) is. */
export type DevinPaste =
  /** A code — the redirect's `code`, or a bare one pasted alone. */
  | { readonly kind: "code" }
  /**
   * The redirect carried `error` instead of `code`: the consent was
   * declined, or Devin refused the grant before issuing one. Still worth
   * sending — the refusal it names is the answer the page shows.
   */
  | { readonly kind: "refused" }
  /** Nothing Devin issued: an empty field, or the dead address alone. */
  | { readonly kind: "empty" };

/**
 * Reads the paste field, or rather just far enough into it.
 *
 * `empty` is what the primary button reads to stay disabled: nothing was
 * pasted, or the paste is the redirect's bare address — the page that
 * could not load carries no code without its query.
 */
export function parseDevinPaste(pasted: string): DevinPaste {
  const trimmed = pasted.trim();
  if (trimmed === "") {
    return { kind: "empty" };
  }

  const query = trimmed.includes("?")
    ? (trimmed.split("?")[1] ?? "").split("#")[0] ?? ""
    : trimmed;
  const isQuery =
    trimmed.includes("?") ||
    query.startsWith("code=") ||
    query.startsWith("error=");
  if (!isQuery) {
    // A URL with no query — the dead address alone — carries no code;
    // anything else stands alone as the bare code.
    return trimmed.includes("://") ? { kind: "empty" } : { kind: "code" };
  }

  const params = new URLSearchParams(query);
  if (params.get("error") !== null) {
    return { kind: "refused" };
  }
  return params.get("code") ? { kind: "code" } : { kind: "empty" };
}
