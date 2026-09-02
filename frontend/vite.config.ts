import { defineConfig } from "vitest/config";
import solid from "vite-plugin-solid";
import { VitePWA } from "vite-plugin-pwa";

export default defineConfig({
  plugins: [
    solid(),
    VitePWA({
      // Nothing is cached and flyco is never used offline: the worker exists
      // for web push, and a new build takes over as soon as it is fetched.
      registerType: "autoUpdate",
      strategies: "injectManifest",
      srcDir: "src",
      filename: "sw.js",
      // An explicit `undefined` is how vite-plugin-pwa is told there is no
      // precache manifest to inject; omitting the key means the default
      // `self.__WB_MANIFEST`, which this worker deliberately lacks.
      // @ts-expect-error exactOptionalPropertyTypes forbids the explicit undefined the plugin requires
      injectManifest: {
        injectionPoint: undefined,
      },
      injectRegister: null,
      manifest: {
        id: "/",
        name: "Flyco",
        short_name: "Flyco",
        description:
          "Agentic coding on the web, with flexible cloud computing.",
        start_url: "/",
        scope: "/",
        display: "standalone",
        background_color: "#0e1116",
        theme_color: "#1c2333",
        icons: [
          {
            src: "/icons/icon.svg",
            sizes: "any",
            type: "image/svg+xml",
            purpose: "any",
          },
          {
            src: "/icons/maskable.svg",
            sizes: "any",
            type: "image/svg+xml",
            purpose: "maskable",
          },
        ],
      },
      devOptions: {
        enabled: false,
      },
    }),
  ],
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./src/test/setup.ts"],
  },
  resolve: {
    conditions: ["development", "browser"],
  },
});
