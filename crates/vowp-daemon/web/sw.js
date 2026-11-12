// VOWP shell service worker. Hand-written; no build step.
//
// Only the static page shells, the manifest and the icon are cached. API responses,
// SSE streams and event data always go to the network and are never cached here.
"use strict";

// Bumped from v1 when the cost page gained its chart; activate deletes old caches.
const CACHE_NAME = "vowp-shell-v2";
const SHELL = [
  "/",
  "/board",
  "/sessions",
  "/cost",
  "/inbox",
  "/nodes",
  "/manifest.webmanifest",
  "/icon.svg",
];

// API traffic and event streams bypass the cache entirely.
function isBypassed(request, url) {
  if (url.pathname.startsWith("/api/")) return true;
  if (url.pathname.endsWith("/stream")) return true;
  const accept = request.headers.get("Accept") || "";
  return accept.includes("text/event-stream");
}

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE_NAME)
      .then((cache) => cache.addAll(SHELL))
      .then(() => self.skipWaiting())
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) =>
        Promise.all(keys.filter((key) => key !== CACHE_NAME).map((key) => caches.delete(key)))
      )
      .then(() => self.clients.claim())
  );
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  if (isBypassed(request, url)) return;

  if (request.mode === "navigate") {
    event.respondWith(networkFirst(request));
    return;
  }
  event.respondWith(cacheFirst(request));
});

// Navigations prefer the live daemon and fall back to a cached page shell offline.
async function networkFirst(request) {
  const url = new URL(request.url);
  try {
    const response = await fetch(request);
    if (response && response.ok && SHELL.includes(url.pathname)) {
      const cache = await caches.open(CACHE_NAME);
      await cache.put(request, response.clone());
    }
    return response;
  } catch (error) {
    const cached = await caches.match(request);
    if (cached) return cached;
    const shellPath = url.pathname.startsWith("/sessions") ? "/sessions" : "/";
    const shell = await caches.match(shellPath);
    if (shell) return shell;
    throw error;
  }
}

// Static shell assets are served from the cache and refreshed in the background.
async function cacheFirst(request) {
  const cached = await caches.match(request);
  if (cached) {
    fetch(request)
      .then((response) => {
        if (response && response.ok) {
          return caches.open(CACHE_NAME).then((cache) => cache.put(request, response.clone()));
        }
        return undefined;
      })
      .catch(() => {});
    return cached;
  }
  const response = await fetch(request);
  if (response && response.ok) {
    const cache = await caches.open(CACHE_NAME);
    await cache.put(request, response.clone());
  }
  return response;
}
