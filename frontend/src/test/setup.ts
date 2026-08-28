import "@testing-library/jest-dom/vitest";
import { afterEach, vi } from "vitest";
import { cleanup } from "@solidjs/testing-library";

afterEach(() => {
  cleanup();
  localStorage.clear();
});

// jsdom doesn't implement scrollTo; the router calls it after navigation
// (e.g. following a hash) and logs a noisy "not implemented" error otherwise.
window.scrollTo = () => {
  /* no-op in tests */
};

// vite-plugin-pwa's virtual module only exists inside a real Vite build;
// components that call registerSW() need a stand-in for it under Vitest.
vi.mock("virtual:pwa-register", () => ({
  registerSW: () => async () => {
    /* no-op in tests */
  },
}));
