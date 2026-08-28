/**
 * Session token storage: in-memory for the life of the tab, mirrored to
 * `localStorage` so a reload doesn't force a fresh sign-in. Every other
 * module reads/writes the session token through here rather than touching
 * `localStorage` directly.
 */

const STORAGE_KEY = "flyco.session_token";

let memoryToken: string | null = null;

function assertValidToken(token: string): void {
  if (!token.startsWith("fs_")) {
    throw new Error(`Session token has an unexpected shape: ${token}`);
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
}

/**
 * Returns the current session token, falling back to `localStorage` on the
 * first read of a fresh tab (e.g. after a reload). Returns `null` when the
 * caller is signed out — that is a normal state, not an error.
 */
export function getSessionToken(
  storage: Pick<Storage, "getItem"> = localStorage,
): string | null {
  if (memoryToken !== null) {
    return memoryToken;
  }
  const stored = storage.getItem(STORAGE_KEY);
  if (stored === null) {
    return null;
  }
  assertValidToken(stored);
  memoryToken = stored;
  return stored;
}

export function clearSessionToken(
  storage: Pick<Storage, "removeItem"> = localStorage,
): void {
  memoryToken = null;
  storage.removeItem(STORAGE_KEY);
}

export function isSignedIn(): boolean {
  return getSessionToken() !== null;
}

/** Test-only: forgets the in-memory cache so a fresh `getSessionToken` re-reads storage. */
export function resetSessionTokenCacheForTests(): void {
  memoryToken = null;
}
