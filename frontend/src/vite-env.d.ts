/// <reference types="vite/client" />
/// <reference types="vite-plugin-pwa/client" />

interface ImportMetaEnv {
  /** VAPID public key used to subscribe to web push (see src/lib/push.ts). */
  readonly VITE_VAPID_PUBLIC_KEY?: string;
  /**
   * Base URL of the control plane API (see src/api/client.ts). Defaults to
   * same-origin (empty string) when unset, which is correct for the
   * production PWA served from the same host as its API.
   */
  readonly VITE_API_BASE?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
