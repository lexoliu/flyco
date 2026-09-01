/**
 * Session token storage: in-memory for the life of the tab, mirrored to
 * `localStorage` so a reload doesn't force a fresh sign-in. Every other
 * module reads/writes the session token through here rather than touching
 * `localStorage` directly.
 */

const STORAGE_KEY = "flyco.session_token";
const SESSION_CHANGED_EVENT = "flyco:session-changed";

let memoryToken: string | null = null;

function notifySessionChanged(): void {
  window.dispatchEvent(new Event(SESSION_CHANGED_EVENT));
}

/** Re-runs a listener whenever this tab's signed-in state changes. */
export function onSessionChanged(listener: () => void): () => void {
  window.addEventListener(SESSION_CHANGED_EVENT, listener);
  return () => window.removeEventListener(SESSION_CHANGED_EVENT, listener);
}

/** Whether a string looks like a session token this build would have issued. */
function looksLikeSessionToken(token: string): boolean {
  return token.startsWith("fs_");
}

/**
 * Rejects a token this code should never have produced.
 *
 * The offending value is deliberately absent from the message: it is a live
 * credential, and an error message ends up in consoles and log drains.
 */
function assertValidToken(token: string): void {
  if (!looksLikeSessionToken(token)) {
    throw new Error("Session token has an unexpected shape");
  }
}

/** Stores a freshly-obtained session token, replacing any previous one. */
export function setSessionToken(
  token: string,
  storage: Pick<Storage, "setItem"> = localStorage,
): void {
  assertValidToken(token);
  memoryToken = token;
  storage.setItem(STORAGE_KEY, token);
  notifySessionChanged();
}

/**
 * Returns the current session token, falling back to `localStorage` on the
 * first read of a fresh tab (e.g. after a reload). Returns `null` when the
 * caller is signed out — that is a normal state, not an error.
 */
export function getSessionToken(
  storage: Pick<Storage, "getItem" | "removeItem"> = localStorage,
): string | null {
  if (memoryToken !== null) {
    return memoryToken;
  }
  const stored = storage.getItem(STORAGE_KEY);
  if (stored === null) {
    return null;
  }
  // Storage is outside this application's control — another script, an
  // older build, or the user's own devtools can put anything here. A value
  // that is not a session token means "not signed in", not "crash": the
  // alternative bricks the app on every read with no way back for someone
  // who cannot clear site data.
  if (!looksLikeSessionToken(stored)) {
    storage.removeItem(STORAGE_KEY);
    return null;
  }
  memoryToken = stored;
  return stored;
}

export function clearSessionToken(
  storage: Pick<Storage, "removeItem"> = localStorage,
): void {
  memoryToken = null;
  storage.removeItem(STORAGE_KEY);
  notifySessionChanged();
}

export function isSignedIn(): boolean {
  return getSessionToken() !== null;
}

/** Test-only: forgets the in-memory cache so a fresh `getSessionToken` re-reads storage. */
export function resetSessionTokenCacheForTests(): void {
  memoryToken = null;
}
