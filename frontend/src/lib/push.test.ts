import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { NotImplementedError } from "../api/problem";

const getVapidPublicKey = vi.fn();
const subscribePush = vi.fn();
const unsubscribePush = vi.fn();

vi.mock("../api/client", () => ({
  getVapidPublicKey: (...args: unknown[]) => getVapidPublicKey(...args),
  subscribePush: (...args: unknown[]) => subscribePush(...args),
  unsubscribePush: (...args: unknown[]) => unsubscribePush(...args),
}));

function notImplemented(): NotImplementedError {
  return new NotImplementedError({
    type: "https://flyco.dev/problems/not-implemented",
    title: "Not implemented",
    status: 501,
    detail: "Push is not configured on this deployment.",
  });
}

describe("web push", () => {
  let subscribeMock: ReturnType<typeof vi.fn>;
  let getSubscriptionMock: ReturnType<typeof vi.fn>;
  let unsubscribeMock: ReturnType<typeof vi.fn>;

  beforeEach(async () => {
    vi.resetModules();
    getVapidPublicKey.mockReset();
    subscribePush.mockReset();
    unsubscribePush.mockReset();
    localStorage.clear();

    vi.stubGlobal("PushManager", class {});
    vi.stubGlobal("Notification", { requestPermission: vi.fn().mockResolvedValue("granted") });

    unsubscribeMock = vi.fn().mockResolvedValue(true);
    subscribeMock = vi.fn().mockResolvedValue({
      expirationTime: null,
      toJSON: () => ({ endpoint: "https://push.example/abc", keys: { p256dh: "p-key", auth: "a-key" } }),
      unsubscribe: unsubscribeMock,
    });
    getSubscriptionMock = vi.fn().mockResolvedValue(null);

    Object.defineProperty(navigator, "serviceWorker", {
      configurable: true,
      value: {
        ready: Promise.resolve({
          pushManager: { subscribe: subscribeMock, getSubscription: getSubscriptionMock },
        }),
      },
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    // @ts-expect-error -- test-only cleanup of a property defined for this suite
    delete navigator.serviceWorker;
  });

  it("is unsupported when the browser has no PushManager", async () => {
    vi.unstubAllGlobals();
    const { isPushSupported } = await import("./push");
    expect(isPushSupported()).toBe(false);
  });

  it("subscribes end to end: fetches the VAPID key, subscribes the browser, and registers with the control plane", async () => {
    getVapidPublicKey.mockResolvedValue({ key: "BEL1" });
    subscribePush.mockResolvedValue({ id: "sub-1", endpoint: "https://push.example/abc", created_at_unix: 0 });

    const { subscribeToPush } = await import("./push");
    const subscription = await subscribeToPush();

    expect(subscription).toBeDefined();
    expect(subscribeMock).toHaveBeenCalledWith(
      expect.objectContaining({ userVisibleOnly: true, applicationServerKey: expect.any(Uint8Array) }),
    );
    expect(subscribePush).toHaveBeenCalledWith({
      endpoint: "https://push.example/abc",
      keys: { p256dh: "p-key", auth: "a-key" },
      expirationTime: null,
    });
    expect(localStorage.getItem("flyco.push_subscription_id")).toBe("sub-1");
  });

  it("propagates a not-implemented VAPID key without ever touching the browser subscription", async () => {
    getVapidPublicKey.mockRejectedValue(notImplemented());

    const { subscribeToPush } = await import("./push");
    await expect(subscribeToPush()).rejects.toBeInstanceOf(NotImplementedError);
    expect(subscribeMock).not.toHaveBeenCalled();
  });

  it("unsubscribes from both the browser and the control plane, then forgets the stored id", async () => {
    localStorage.setItem("flyco.push_subscription_id", "sub-1");
    getSubscriptionMock.mockResolvedValue({ unsubscribe: unsubscribeMock });
    unsubscribePush.mockResolvedValue(undefined);

    const { unsubscribeFromPush } = await import("./push");
    const existed = await unsubscribeFromPush();

    expect(existed).toBe(true);
    expect(unsubscribePush).toHaveBeenCalledWith("sub-1");
    expect(unsubscribeMock).toHaveBeenCalledOnce();
    expect(localStorage.getItem("flyco.push_subscription_id")).toBeNull();
  });

  it("returns false without calling the control plane when there is no browser subscription", async () => {
    getSubscriptionMock.mockResolvedValue(null);

    const { unsubscribeFromPush } = await import("./push");
    const existed = await unsubscribeFromPush();

    expect(existed).toBe(false);
    expect(unsubscribePush).not.toHaveBeenCalled();
  });
});
