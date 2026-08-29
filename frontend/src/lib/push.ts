/**
 * Web push: subscribes the browser via `PushManager`, then registers the
 * subscription with the control plane (`POST /v1/push/subscriptions`) so a
 * server-side event can actually reach it. The VAPID public key comes from
 * `GET /v1/push/vapid-public-key` rather than a build-time env var, since
 * the matching private key — and therefore whether push is configured at
 * all — is a property of the deployment, not the frontend build.
 *
 * The subscription id the control plane assigns is what `DELETE
 * /v1/push/subscriptions/{id}` needs to unregister it later, so it is kept
 * in `localStorage` next to the browser's own subscription state — the
 * same pattern `src/lib/session.ts` uses for the session token.
 */
import { getVapidPublicKey, subscribePush, unsubscribePush } from "../api/client";

const SUBSCRIPTION_ID_KEY = "flyco.push_subscription_id";

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
 * Requests notification permission, subscribes to push, and registers the
 * result with the control plane. Throws if push isn't supported, permission
 * is denied, or this deployment has no VAPID key configured (surfaced as a
 * `NotImplementedError` from `getVapidPublicKey`) — never returns a
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
  const vapid = await getVapidPublicKey();
  const registration = await navigator.serviceWorker.ready;
  const subscription = await registration.pushManager.subscribe({
    userVisibleOnly: true,
    applicationServerKey: urlBase64ToUint8Array(vapid.key),
  });

  const json = subscription.toJSON();
  if (json.endpoint === undefined || json.keys?.p256dh === undefined || json.keys.auth === undefined) {
    throw new Error("Browser push subscription is missing its endpoint or keys.");
  }
  const registered = await subscribePush({
    endpoint: json.endpoint,
    keys: { p256dh: json.keys.p256dh, auth: json.keys.auth },
    expirationTime: subscription.expirationTime,
  });
  localStorage.setItem(SUBSCRIPTION_ID_KEY, registered.id);
  return subscription;
}

/**
 * Unsubscribes the current push subscription, if any, from both the browser
 * and the control plane. Returns whether one existed.
 */
export async function unsubscribeFromPush(): Promise<boolean> {
  const subscription = await getPushSubscription();
  if (subscription === null) {
    return false;
  }
  const id = localStorage.getItem(SUBSCRIPTION_ID_KEY);
  if (id !== null) {
    await unsubscribePush(id);
    localStorage.removeItem(SUBSCRIPTION_ID_KEY);
  }
  return subscription.unsubscribe();
}
