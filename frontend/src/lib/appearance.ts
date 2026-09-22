/**
 * What this browser looks like: how large the interface is drawn, and
 * what a transcript is set in.
 *
 * Both are per-browser preferences with no server side, like the theme
 * beside them ({@link ./theme.ts}) — they change nothing about the
 * session, so a round trip for them would be a round trip for nothing.
 * Each is applied as a data attribute on the document root, where
 * `src/styles/tokens.css` answers it; "default" removes the attribute so
 * the stylesheet's own value stands.
 *
 * Every read is guarded the way `localPreferences.ts` guards its own: a
 * private window and blocked site data both make these calls throw, and a
 * font preference is never a reason to fail a page.
 */

/** How large the whole interface is drawn. The scale is rem-based, so this is one number. */
export type TextSize = "small" | "default" | "large";

/** What a transcript's prose is set in. */
export type ReadingFont = "sans" | "serif" | "mono";

const TEXT_SIZE_KEY = "flyco.text_size";
const READING_FONT_KEY = "flyco.reading_font";

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
    // A browser that will not remember still renders; it simply opens at
    // the defaults every time.
  }
}

function isTextSize(value: string): value is TextSize {
  return value === "small" || value === "default" || value === "large";
}

function isReadingFont(value: string): value is ReadingFont {
  return value === "sans" || value === "serif" || value === "mono";
}

/** The stored interface size, or `default` where nothing valid is stored. */
export function textSize(): TextSize {
  const raw = read(TEXT_SIZE_KEY);
  return raw !== null && isTextSize(raw) ? raw : "default";
}

/** The stored reading font, or `sans` where nothing valid is stored. */
export function readingFont(): ReadingFont {
  const raw = read(READING_FONT_KEY);
  return raw !== null && isReadingFont(raw) ? raw : "sans";
}

function apply(root: HTMLElement, attribute: string, value: string, standard: string): void {
  if (value === standard) {
    root.removeAttribute(attribute);
  } else {
    root.setAttribute(attribute, value);
  }
}

/** Records the interface size and applies it at once. */
export function setTextSize(size: TextSize, root: HTMLElement = document.documentElement): void {
  write(TEXT_SIZE_KEY, size);
  apply(root, "data-text-size", size, "default");
}

/** Records the reading font and applies it at once. */
export function setReadingFont(
  font: ReadingFont,
  root: HTMLElement = document.documentElement,
): void {
  write(READING_FONT_KEY, font);
  apply(root, "data-reading-font", font, "sans");
}

/** Applies what this browser remembers, on startup, beside `initTheme`. */
export function initAppearance(root: HTMLElement = document.documentElement): void {
  apply(root, "data-text-size", textSize(), "default");
  apply(root, "data-reading-font", readingFont(), "sans");
}
