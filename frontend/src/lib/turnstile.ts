/**
 * Cloudflare Turnstile: the human check guarding sign-in.
 *
 * The login page renders a non-interactive widget before it will offer
 * GitHub sign-in; the token it produces is posted to
 * `POST /v1/auth/github/start`, which verifies it with `siteverify`
 * before minting any OAuth state.
 */

/** The `data-action` the widget renders with — `siteverify` echoes it and the control plane checks it. */
export const TURNSTILE_ACTION = "login";

const SCRIPT_URL =
  "https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit";

/** Options `turnstile.render` takes, as Cloudflare documents them. */
export interface RenderOptions {
  sitekey: string;
  action?: string;
  /** `execute` keeps the widget invisible until `execute()` runs it. */
  appearance?: "always" | "execute" | "interaction-only";
  /** `execute` runs the challenge on `execute()` instead of at render. */
  execution?: "render" | "execute";
  callback?: (token: string) => void;
  "expired-callback"?: () => void;
  "error-callback"?: (errorCode?: string) => void;
}

/** The API `api.js` installs on `window`. */
export interface TurnstileApi {
  render(container: HTMLElement, options: RenderOptions): string;
  execute(widgetId?: string): void;
  reset(widgetId?: string): void;
  remove(widgetId: string): void;
}

declare global {
  interface Window {
    turnstile?: TurnstileApi;
  }
}

let loading: Promise<TurnstileApi> | null = null;

/**
 * Loads `api.js` once and answers its API.
 *
 * `render=explicit` keeps the script from scanning the DOM — the widget is
 * rendered where the login page puts it and nowhere else.
 */
export function loadTurnstile(): Promise<TurnstileApi> {
  if (window.turnstile !== undefined) {
    return Promise.resolve(window.turnstile);
  }
  loading ??= new Promise<TurnstileApi>((resolve, reject) => {
    const script = document.createElement("script");
    script.src = SCRIPT_URL;
    script.async = true;
    script.defer = true;
    script.onload = () => {
      if (window.turnstile === undefined) {
        loading = null;
        reject(new Error("the Turnstile script loaded no API"));
      } else {
        resolve(window.turnstile);
      }
    };
    script.onerror = () => {
      loading = null;
      reject(new Error("the human-check script could not be loaded"));
    };
    document.head.appendChild(script);
  });
  return loading;
}

/**
 * The one hidden widget behind every token acquisition that has no visible
 * widget of its own — "Reconnect GitHub" deep in the app, for instance.
 * `execution: "execute"` means it mints nothing until asked; if Cloudflare
 * needs an interactive challenge it opens over the page where the widget
 * was rendered, hence the fixed corner anchor.
 */
interface HiddenWidget {
  api: TurnstileApi;
  id: string;
  next(token: string): void;
  fail(error: unknown): void;
}

let hidden: HiddenWidget | undefined;
let queue: Promise<unknown> = Promise.resolve();

/**
 * Proves a human is present from outside the login page. Turnstile tokens
 * are single-use, so acquisitions serialize behind each other — two callers
 * can never share one.
 */
export function acquireTurnstileToken(sitekey: string): Promise<string> {
  const run = queue.then(() => acquire(sitekey));
  queue = run.catch(() => undefined);
  return run;
}

async function acquire(sitekey: string): Promise<string> {
  const widget = (hidden ??= await renderHidden(sitekey));
  return new Promise<string>((resolve, reject) => {
    widget.next = resolve;
    widget.fail = reject;
    widget.api.reset(widget.id);
    widget.api.execute(widget.id);
  });
}

async function renderHidden(sitekey: string): Promise<HiddenWidget> {
  const api = await loadTurnstile();
  const host = document.createElement("div");
  host.style.position = "fixed";
  host.style.bottom = "0";
  host.style.right = "0";
  document.body.appendChild(host);
  const widget: HiddenWidget = {
    api,
    id: "",
    next(_token: string): void {},
    fail(_error: unknown): void {},
  };
  widget.id = api.render(host, {
    sitekey,
    action: TURNSTILE_ACTION,
    appearance: "execute",
    execution: "execute",
    callback: (token) => widget.next(token),
    "error-callback": () =>
      widget.fail(new Error("the human check could not run — try again")),
  });
  return widget;
}
