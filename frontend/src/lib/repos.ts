/**
 * Reading a session's repository list out loud.
 *
 * A session works across one or more checkouts, and the places that name
 * them — the header, the provisioning timeline's clone step, the drawer —
 * all read the same shape: the primary repository named in full, the rest
 * counted. `flyco`, or `flyco +2`. Naming every slug would fill the header
 * with what is one click away in its popover.
 */
import type { SessionRepo } from "../api/client";

/** `flyco`, `flyco +2`, or the placeholder when nothing is recorded yet. */
export function reposLabel(repos: readonly SessionRepo[]): string {
  const first = repos[0];
  if (first === undefined) {
    return "the repositories";
  }
  const extra = repos.length - 1;
  return extra === 0 ? first.slug : `${first.slug} +${extra}`;
}
