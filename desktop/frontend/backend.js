// IPC credentials stay in this closure. Only the public loopback origin is exposed
// for the original dashboard's endpoint examples and OAuth redirect URLs.
export async function connectBackend() {
  const invoke = window.__TAURI__?.core?.invoke;
  if (!invoke) throw new Error("Open this dashboard from the 9Router desktop application");
  const { backendUrl, adminToken } = await invoke("backend_config");
  const backend = new URL(backendUrl);
  if (backend.protocol !== "http:" || !["127.0.0.1", "[::1]"].includes(backend.hostname) || backend.pathname !== "/" || backend.search || backend.hash || backend.username || backend.password || !adminToken) {
    throw new Error("Invalid local backend configuration");
  }
  globalThis.__9routerBackendOrigin = backend.origin;
  globalThis.__9routerBackendLocation = { hostname: backend.hostname, port: backend.port, protocol: backend.protocol };
  const nativeFetch = window.fetch.bind(window);
  const apiUrl = (input) => {
    const url = new URL(input instanceof Request ? input.url : input, window.location.href);
    return [globalThis.location.origin, backend.origin].includes(url.origin) && /^\/(api(?:\/|$)|v1(?:\/|$)|health$)/.test(url.pathname)
      ? new URL(url.pathname + url.search, backend.origin) : null;
  };
  const pendingCallbacks = new Set();
  async function pollCallback(state) {
    if (!state || pendingCallbacks.has(state)) return;
    pendingCallbacks.add(state);
    try {
      for (let attempts = 0; attempts < 300; attempts++) {
        await new Promise((resolve) => setTimeout(resolve, 1000));
        if (!pendingCallbacks.has(state)) return;
        const response = await window.fetch(`/api/oauth/callback-status?state=${encodeURIComponent(state)}`);
        if (!response.ok) continue;
        const result = await response.json();
        if (result.status === "done" || result.status === "error") {
          window.dispatchEvent(new MessageEvent("message", {
            origin: globalThis.location.origin,
            data: { type: "oauth_callback", data: { state, completed: result.status === "done", error: result.error } },
          }));
          return;
        }
        if (result.status === "unknown") return;
      }
    } catch { /* The original modal retains manual callback input. */ }
    finally { pendingCallbacks.delete(state); }
  }
  window.fetch = async (input, init = {}) => {
    const url = apiUrl(input);
    if (!url) return nativeFetch(input, init);
    const headers = new Headers(init.headers ?? (input instanceof Request ? input.headers : undefined));
    if (!headers.has("Authorization")) headers.set("Authorization", `Bearer ${adminToken}`);
    const request = input instanceof Request ? new Request(url, input) : url;
    const response = await nativeFetch(request, { ...init, headers, credentials: "include" });
    if (/^\/api\/oauth\/[^/]+\/authorize$/.test(url.pathname) && response.ok) {
      response.clone().json().then((data) => data.authUrl && pollCallback(data.state)).catch(() => {});
    }
    return response;
  };
  window.EventSource = AuthenticatedEventSource;
  const nativeOpen = window.open.bind(window);
  window.open = (href, target, features) => {
    const url = new URL(href, window.location.href);
    if (!["http:", "https:"].includes(url.protocol)) return null;
    if (url.origin === globalThis.location.origin) return nativeOpen(href, target, features);
    invoke("open_external", { url: url.href }).catch(() => {
      window.dispatchEvent(new MessageEvent("message", {
        origin: globalThis.location.origin,
        data: { type: "oauth_callback", data: { error: "Unable to open the system browser. Copy the authorization URL and open it manually." } },
      }));
    });
    // OAuthModal expects a popup handle while the native backend receives callbacks.
    return { closed: false, close() { this.closed = true; }, focus() {} };
  };
  document.addEventListener("click", (event) => {
    const anchor = event.target.closest?.("a[href]");
    if (!anchor || anchor.hasAttribute("download")) return;
    const url = new URL(anchor.href, window.location.href);
    if (["http:", "https:"].includes(url.protocol) && url.origin !== globalThis.location.origin) {
      event.preventDefault();
      window.open(url.href, "_blank");
    }
  }, true);
}

// Native EventSource cannot send Authorization headers. Preserve the browser SSE
// interface with a fetch stream so usage and console-log screens keep live updates.
export class AuthenticatedEventSource extends EventTarget {
  static CONNECTING = 0; static OPEN = 1; static CLOSED = 2;
  CONNECTING = 0; OPEN = 1; CLOSED = 2;
  readyState = 0;
  onopen = null; onmessage = null; onerror = null;
  constructor(url) {
    super();
    this.url = String(url);
    this.controller = new AbortController();
    this.read();
  }
  emit(event) { this.dispatchEvent(event); this[`on${event.type}`]?.(event); }
  async read() {
    let retry = 3000;
    while (this.readyState !== this.CLOSED) {
      try {
        const response = await fetch(this.url, { headers: { Accept: "text/event-stream", ...(this.lastEventId ? { "Last-Event-ID": this.lastEventId } : {}) }, signal: this.controller.signal });
        if (!response.ok || !response.body) throw new Error("Event stream unavailable");
        this.readyState = this.OPEN;
        this.emit(new Event("open"));
        const reader = response.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "", data = [], type = "message";
        while (this.readyState !== this.CLOSED) {
          const { done, value } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          let end;
          while ((end = buffer.indexOf("\n")) !== -1) {
            const line = buffer.slice(0, end).replace(/\r$/, "");
            buffer = buffer.slice(end + 1);
            if (!line) {
              if (data.length) this.emit(new MessageEvent(type, { data: data.join("\n"), lastEventId: this.lastEventId || "" }));
              data = []; type = "message";
              continue;
            }
            if (line.startsWith(":")) continue;
            const colon = line.indexOf(":"), field = colon < 0 ? line : line.slice(0, colon);
            const value = colon < 0 ? "" : line.slice(colon + 1).replace(/^ /, "");
            if (field === "data") data.push(value);
            if (field === "event") type = value || "message";
            if (field === "id" && !value.includes("\0")) this.lastEventId = value;
            if (field === "retry" && /^\d+$/.test(value)) retry = Number(value);
          }
        }
      } catch (error) {
        if (this.readyState === this.CLOSED) return;
      }
      if (this.readyState === this.CLOSED) return;
      this.readyState = this.CONNECTING;
      this.emit(new Event("error"));
      await new Promise((resolve) => { this.retryTimer = setTimeout(resolve, retry); });
    }
  }
  close() { this.readyState = this.CLOSED; this.controller.abort(); clearTimeout(this.retryTimer); }
}
