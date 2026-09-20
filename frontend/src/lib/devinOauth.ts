/**
 * What the Devin paste page reads out of its field.
 *
 * Devin's page shows the bare authorization code after sign-in — flyco
 * runs the CLI's port-free flow, the one `devin auth login
 * --force-manual-token-flow` runs — so the paste is the code with whatever
 * whitespace the copy picked up. This reads only enough to know whether
 * the field holds something worth sending; the control plane is the
 * authoritative parser, and what is sent is the paste itself, untouched.
 */

/**
 * The code the field holds, or `null` when it holds nothing — which is
 * what the primary button reads to stay disabled.
 */
export function pastedDevinCode(pasted: string): string | null {
  const trimmed = pasted.trim();
  return trimmed === "" ? null : trimmed;
}
