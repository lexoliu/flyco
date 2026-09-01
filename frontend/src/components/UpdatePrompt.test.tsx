import { fireEvent, render, screen } from "@solidjs/testing-library";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { registerSW } from "virtual:pwa-register";
import UpdatePrompt, { activateWaitingWorker } from "./UpdatePrompt";

class ServiceWorkerEvents extends EventTarget {
  controllerChanged(): void {
    this.dispatchEvent(new Event("controllerchange"));
  }
}

describe("service worker activation", () => {
  it("listens before requesting activation and reloads when control changes", async () => {
    const serviceWorker = new ServiceWorkerEvents();
    const order: string[] = [];
    const reload = vi.fn(() => order.push("reload"));
    serviceWorker.addEventListener("controllerchange", () => order.push("controllerchange"));

    const activated = activateWaitingWorker(
      async () => {
        order.push("activate");
        serviceWorker.controllerChanged();
      },
      serviceWorker,
      reload,
    );

    await activated;
    expect(order).toEqual(["activate", "controllerchange", "reload"]);
    expect(reload).toHaveBeenCalledOnce();
  });

  it("removes its listener when activation fails", async () => {
    const serviceWorker = new ServiceWorkerEvents();
    const reload = vi.fn();

    await expect(
      activateWaitingWorker(
        async () => {
          throw new Error("activation failed");
        },
        serviceWorker,
        reload,
      ),
    ).rejects.toThrow("activation failed");

    serviceWorker.controllerChanged();
    expect(reload).not.toHaveBeenCalled();
  });
});

describe("UpdatePrompt", () => {
  beforeEach(() => {
    vi.mocked(registerSW).mockReset();
  });

  it("shows progress immediately when reload is clicked", async () => {
    let needRefresh: (() => void) | undefined;
    vi.mocked(registerSW).mockImplementation((options) => {
      needRefresh = options?.onNeedRefresh;
      return async () => new Promise<void>(() => undefined);
    });

    render(() => <UpdatePrompt />);
    needRefresh?.();
    const reload = await screen.findByRole("button", { name: "Reload" });
    fireEvent.click(reload);

    expect(screen.getByRole("button", { name: "Updating…" })).toBeDisabled();
  });

  it("offers a retry when activation fails", async () => {
    let needRefresh: (() => void) | undefined;
    vi.mocked(registerSW).mockImplementation((options) => {
      needRefresh = options?.onNeedRefresh;
      return async () => {
        throw new Error("activation failed");
      };
    });

    render(() => <UpdatePrompt />);
    needRefresh?.();
    fireEvent.click(await screen.findByRole("button", { name: "Reload" }));

    expect(await screen.findByText("Flyco could not activate the update.")).toBeVisible();
    expect(screen.getByRole("button", { name: "Retry" })).toBeEnabled();
  });
});
