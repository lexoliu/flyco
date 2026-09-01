import { defineConfig } from "vitest/config";
import solid from "vite-plugin-solid";
import { VitePWA } from "vite-plugin-pwa";

export default defineConfig({
  plugins: [
    solid(),
    VitePWA({
      registerType: "prompt",
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
      workbox: {
        globPatterns: ["**/*.{js,css,html,svg}"],
        navigateFallback: "/index.html",
        importScripts: ["/push-sw.js"],
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
