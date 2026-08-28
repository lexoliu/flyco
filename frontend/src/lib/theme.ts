/**
 * Light/dark theme preference.
 *
 * Resolution order (see src/styles/tokens.css): `prefers-color-scheme`
 * picks a default, and an explicit preference stored here overrides it via
 * `data-theme` on the document root. "system" means "no override" — it
 * removes the attribute so the media query decides.
 */

export type ThemePreference = "light" | "dark" | "system";

const STORAGE_KEY = "flyco.theme";

function isThemePreference(value: string): value is ThemePreference {
  return value === "light" || value === "dark" || value === "system";
}

/**
 * Reads the stored preference. Defaults to "system" when nothing is
 * stored; throws on a stored value that isn't one of the three valid
 * preferences, since that can only mean the storage was corrupted or
 * written by an incompatible version of this module.
 */
export function readStoredThemePreference(
  storage: Pick<Storage, "getItem"> = localStorage,
): ThemePreference {
  const raw = storage.getItem(STORAGE_KEY);
  if (raw === null) {
    return "system";
  }
  if (!isThemePreference(raw)) {
    throw new Error(`Invalid theme preference in storage: ${JSON.stringify(raw)}`);
  }
  return raw;
}

export function storeThemePreference(
  preference: ThemePreference,
  storage: Pick<Storage, "setItem"> = localStorage,
): void {
  storage.setItem(STORAGE_KEY, preference);
}

/** Applies a preference to the document by setting or clearing `data-theme`. */
export function applyThemePreference(
  preference: ThemePreference,
  root: HTMLElement = document.documentElement,
): void {
  if (preference === "system") {
    root.removeAttribute("data-theme");
  } else {
    root.setAttribute("data-theme", preference);
  }
}

/** Reads, applies, and returns the current preference — the module's one entry point for startup. */
export function initTheme(): ThemePreference {
  const preference = readStoredThemePreference();
  applyThemePreference(preference);
  return preference;
}

/** Persists a new preference and applies it immediately. */
export function setTheme(preference: ThemePreference): void {
  storeThemePreference(preference);
  applyThemePreference(preference);
}
