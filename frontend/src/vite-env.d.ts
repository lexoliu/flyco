/// <reference types="vite/client" />
/// <reference types="vite-plugin-pwa/client" />

interface ImportMetaEnv {
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
