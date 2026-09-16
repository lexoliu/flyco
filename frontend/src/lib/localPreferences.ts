/**
 * The three things this browser remembers on its own: which repositories the
 * user reaches for, whether they want interruptible capacity, and whether
 * they have seen the welcome.
 *
 * Neither is worth a round trip and neither matters if it is lost, which is
 * exactly the shape `localStorage` is for. Every access is guarded: private
 * windows, cleared site data and storage-blocking settings all make these
 * calls *throw*, and a recents list is never a reason to fail a page. A
 * throw is therefore read as "this browser remembers nothing", which is the
 * one safe interpretation.
 */

const RECENT_REPOS_KEY = "flyco.recent_repos";
const WELCOME_KEY = "flyco.welcome_dismissed";
const SPOT_KEY = "flyco.spot";

/** How many repositories the picker offers before the search results. */
export const MAX_RECENT_REPOS = 5;

function read(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function write(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    // A browser that will not remember is not an error to report: the app
    // works, it simply offers no shortcuts.
  }
}

/**
 * Repositories this browser used most recently, newest first.
 *
 * Anything that is not a list of strings is discarded rather than repaired:
 * the value was written by another version of this module or by something
 * that is not flyco, and neither is worth guessing about.
 */
export function recentRepos(): string[] {
  const raw = read(RECENT_REPOS_KEY);
  if (raw === null) {
    return [];
  }
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) {
      return [];
    }
    return parsed.filter((slug): slug is string => typeof slug === "string");
  } catch {
    return [];
  }
}

/** Moves `slug` to the front of the recents list, capped at five. */
export function rememberRepo(slug: string): string[] {
  const next = [slug, ...recentRepos().filter((seen) => seen !== slug)].slice(
    0,
    MAX_RECENT_REPOS,
  );
  write(RECENT_REPOS_KEY, JSON.stringify(next));
  return next;
}

/**
 * Whether new sessions ask for interruptible capacity.
 *
 * On unless this browser has been told otherwise: spot is cheaper, flyco
 * handles eviction, and a preference nobody has expressed should be the one
 * that costs less. Stored rather than sent, because it is a default for the
 * next session rather than a fact about any session that exists.
 */
export function spotPreference(): boolean {
  return read(SPOT_KEY) !== "off";
}

/** Records whether new sessions should ask for spot capacity. */
export function setSpotPreference(spot: boolean): void {
  write(SPOT_KEY, spot ? "on" : "off");
}

/** Whether the user has already dismissed or finished the welcome. */
export function welcomeDismissed(): boolean {
  return read(WELCOME_KEY) !== null;
}

/** Records that the welcome has been seen; it is never shown again. */
export function dismissWelcome(): void {
  write(WELCOME_KEY, "1");
}
