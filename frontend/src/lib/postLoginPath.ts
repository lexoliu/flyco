/** The protected route to restore after GitHub OAuth returns to this tab. */
const STORAGE_KEY = "flyco.post_login_path";

function assertInternalPath(path: string): void {
  if (!path.startsWith("/") || path.startsWith("//")) {
    throw new Error("Post-login destination must be an absolute path on this Flyco origin");
  }
}

/** Remembers a router-owned path without putting it into the OAuth request. */
export function rememberPostLoginPath(
  path: string,
  storage: Pick<Storage, "setItem"> = sessionStorage,
): void {
  assertInternalPath(path);
  storage.setItem(STORAGE_KEY, path);
}

/** Returns and removes the one-shot post-login destination. */
export function consumePostLoginPath(
  storage: Pick<Storage, "getItem" | "removeItem"> = sessionStorage,
): string {
  const path = storage.getItem(STORAGE_KEY);
  storage.removeItem(STORAGE_KEY);
  if (path === null) {
    return "/";
  }
  assertInternalPath(path);
  return path;
}
