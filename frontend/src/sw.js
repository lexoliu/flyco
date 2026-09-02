self.addEventListener("push", (event) => {
  event.waitUntil(
    (async () => {
      const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
      if (windows.some((client) => client.visibilityState === "visible")) {
        return;
      }

      const notification = event.data.json();
      await self.registration.showNotification(notification.title, {
        body: notification.body,
        tag: notification.tag,
        data: { url: notification.url },
        icon: "/icons/icon.svg",
        badge: "/icons/maskable.svg",
      });
    })(),
  );
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  event.waitUntil(
    (async () => {
      const target = new URL(event.notification.data.url, self.location.origin).href;
      const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
      const existing = windows.find((client) => client.url === target);
      if (existing !== undefined) {
        await existing.focus();
        return;
      }
      await self.clients.openWindow(target);
    })(),
  );
});

// Flyco is not used offline and caches nothing: the service worker exists
// for web push. A new version therefore takes over immediately instead of
// waiting behind a "reload" prompt.
self.addEventListener("install", () => {
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(self.clients.claim());
});
