import { describe, expect, it, vi } from "vitest";
import source from "../../public/push-sw.js?raw";

type WorkerEvent = { waitUntil: (work: Promise<unknown>) => void };

function loadWorker(windows: Array<{ url: string; visibilityState: string; focus: () => Promise<void> }> = []) {
  const listeners = new Map<string, (event: WorkerEvent & Record<string, unknown>) => void>();
  const showNotification = vi.fn().mockResolvedValue(undefined);
  const openWindow = vi.fn().mockResolvedValue(undefined);
  const worker = {
    addEventListener: (type: string, listener: (event: WorkerEvent & Record<string, unknown>) => void) => {
      listeners.set(type, listener);
    },
    clients: {
      matchAll: vi.fn().mockResolvedValue(windows),
      openWindow,
    },
    registration: { showNotification },
    location: { origin: "https://dev.flyco.dev" },
  };
  new Function("self", source)(worker);
  return { listeners, showNotification, openWindow };
}

describe("push service worker", () => {
  it("shows a server notification when Flyco is not visible", async () => {
    const worker = loadWorker();
    let work: Promise<unknown> | undefined;
    worker.listeners.get("push")?.({
      data: { json: () => ({ title: "Turn completed", body: "Done", tag: "turn-1", url: "/sessions/1" }) },
      waitUntil: (promise) => { work = promise; },
    });
    await work;

    expect(worker.showNotification).toHaveBeenCalledWith("Turn completed", expect.objectContaining({
      body: "Done",
      tag: "turn-1",
      data: { url: "/sessions/1" },
    }));
  });

  it("suppresses push while a Flyco window is visible", async () => {
    const worker = loadWorker([{ url: "https://dev.flyco.dev/", visibilityState: "visible", focus: vi.fn() }]);
    let work: Promise<unknown> | undefined;
    worker.listeners.get("push")?.({
      data: { json: () => ({ title: "Approval required" }) },
      waitUntil: (promise) => { work = promise; },
    });
    await work;
    expect(worker.showNotification).not.toHaveBeenCalled();
  });

  it("opens the session when the notification is clicked", async () => {
    const worker = loadWorker();
    let work: Promise<unknown> | undefined;
    const close = vi.fn();
    worker.listeners.get("notificationclick")?.({
      notification: { close, data: { url: "/sessions/1" } },
      waitUntil: (promise) => { work = promise; },
    });
    await work;
    expect(close).toHaveBeenCalledOnce();
    expect(worker.openWindow).toHaveBeenCalledWith("https://dev.flyco.dev/sessions/1");
  });
});
