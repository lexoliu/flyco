/**
 * Web push scaffolding.
 *
 * This wires the standard `PushManager` subscribe/unsubscribe flow so the
 * pieces exist, but it is **not yet wired to a backend endpoint**: flyco's
 * control plane has no `/v1/push/*` route today (see openapi.json at the
 * repo root). `subscribeToPush` will successfully register a subscription
 * with the browser's push service and return it, but nothing sends that
 * subscription to flyco, so no notification will ever be delivered until a
 * caller also POSTs the result somewhere the backend defines. Do the same
 * for `unsubscribeFromPush` when a "tell the backend to forget this
 * subscription" endpoint exists.
 */

function vapidPublicKey(): string {
  const key = import.meta.env.VITE_VAPID_PUBLIC_KEY;
  if (key === undefined || key.length === 0) {
    throw new Error(
      "VITE_VAPID_PUBLIC_KEY is not set; push notifications need a VAPID public key to subscribe.",
    );
  }
  return key;
}

/** Converts the URL-safe base64 VAPID key into the raw bytes `PushManager.subscribe` expects. */
function urlBase64ToUint8Array(base64: string): Uint8Array<ArrayBuffer> {
  const padding = "=".repeat((4 - (base64.length % 4)) % 4);
  const normalized = (base64 + padding).replace(/-/g, "+").replace(/_/g, "/");
  const binary = atob(normalized);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

export function isPushSupported(): boolean {
  return "serviceWorker" in navigator && "PushManager" in window;
}

export async function getPushSubscription(): Promise<PushSubscription | null> {
  if (!isPushSupported()) {
    return null;
  }
  const registration = await navigator.serviceWorker.ready;
  return registration.pushManager.getSubscription();
}

/**
 * Requests notification permission and subscribes to push. Throws if push
 * isn't supported or permission is denied, rather than returning a
 * half-subscribed state.
 */
export async function subscribeToPush(): Promise<PushSubscription> {
  if (!isPushSupported()) {
    throw new Error("This browser does not support web push.");
  }
  const permission = await Notification.requestPermission();
  if (permission !== "granted") {
    throw new Error(`Notification permission was "${permission}", not granted.`);
  }
  const registration = await navigator.serviceWorker.ready;
  const subscription = await registration.pushManager.subscribe({
    userVisibleOnly: true,
    applicationServerKey: urlBase64ToUint8Array(vapidPublicKey()),
  });
  // NOT YET WIRED — see module doc comment: nothing sends `subscription`
  // to the control plane yet.
  return subscription;
}

/** Unsubscribes the current push subscription, if any. Returns whether one existed. */
export async function unsubscribeFromPush(): Promise<boolean> {
  const subscription = await getPushSubscription();
  if (subscription === null) {
    return false;
  }
  // NOT YET WIRED — see module doc comment: nothing tells the control
  // plane this subscription is gone.
  return subscription.unsubscribe();
}
