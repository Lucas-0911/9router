import { useMemo, useSyncExternalStore } from "react";

const snapshot = () => window.location.pathname + window.location.search;
const subscribe = (listener) => {
  window.addEventListener("popstate", listener);
  return () => window.removeEventListener("popstate", listener);
};
export function navigate(href, { replace = false, scroll = true } = {}) {
  const url = new URL(href, window.location.href);
  const current = new URL(window.location.href);
  if (url.protocol !== current.protocol || url.host !== current.host) return window.open(url.href, "_blank");
  window.history[replace ? "replaceState" : "pushState"]({}, "", url);
  window.dispatchEvent(new PopStateEvent("popstate"));
  if (scroll) window.scrollTo(0, 0);
}
const router = {
  push: (href, options) => navigate(href, options),
  replace: (href, options) => navigate(href, { ...options, replace: true }),
  back: () => window.history.back(),
  forward: () => window.history.forward(),
  refresh: () => window.location.reload(),
  prefetch: () => Promise.resolve(),
};
export const useRouter = () => router;
export const usePathname = () => useSyncExternalStore(subscribe, snapshot).split("?")[0];
export function useSearchParams() {
  const location = useSyncExternalStore(subscribe, snapshot);
  // Preserve identity across renders, as Next does, for callback effects.
  return useMemo(() => new URLSearchParams(location.includes("?") ? location.slice(location.indexOf("?") + 1) : ""), [location]);
}
export function useParams() {
  const path = usePathname().split("/").filter(Boolean).map(decodeURIComponent);
  if (path[1] === "providers") return { id: path[2] };
  if (path[1] === "cli-tools") return { toolId: path[2] };
  if (path[1] === "media-providers") return path[2] === "combo" ? { id: path[3] } : { kind: path[2], id: path[3] };
  return {};
}
export function notFound() { throw new Error("NEXT_NOT_FOUND"); }
export function redirect(href) { navigate(href, { replace: true }); return null; }
