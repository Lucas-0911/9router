import { afterEach, beforeEach, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  browser: { newContext: vi.fn(), close: vi.fn(), isConnected: () => true },
}));
vi.mock("../../open-sse/services/lazyRuntime.js", () => ({
  loadRuntimeDep: async () => ({ chromium: {} }),
}));
vi.mock("../../open-sse/services/obscura.js", () => ({
  connectObscuraBrowser: async () => null,
}));
vi.mock("../../open-sse/services/virtualDisplay.js", () => ({
  ensureVirtualDisplay: async () => null,
  stopVirtualDisplay: () => {},
}));
vi.mock("../../open-sse/executors/browserExecutableCheck.js", () => ({
  launchChromiumWithFallback: async () => mocks.browser,
}));

import {
  acquireBrowserContext, retainBrowserContext, shutdownPool, getBrowserPoolStatus,
} from "../../open-sse/services/browserPool.js";

let context;
beforeEach(() => {
  vi.useFakeTimers();
  context = { close: vi.fn(async () => {}), once: vi.fn() };
  mocks.browser.newContext.mockResolvedValue(context);
});
afterEach(async () => {
  await shutdownPool("test-cleanup");
  vi.useRealTimers();
  vi.clearAllMocks();
});

it("keeps a busy context beyond video duration, then expires it after release", async () => {
  const pooled = await acquireBrowserContext("muse-test", {});
  const release = retainBrowserContext(pooled);
  await vi.advanceTimersByTimeAsync(16 * 60 * 1000);
  expect(context.close).not.toHaveBeenCalled();
  expect(getBrowserPoolStatus().contexts).toBe(1);
  release();
  release();
  await vi.advanceTimersByTimeAsync(5 * 60 * 1000);
  expect(context.close).toHaveBeenCalledTimes(1);
});

it("still permits explicit shutdown while a context is active", async () => {
  const pooled = await acquireBrowserContext("muse-test", {});
  const release = retainBrowserContext(pooled);
  await shutdownPool("manual");
  expect(context.close).toHaveBeenCalledTimes(1);
  release();
});

it("closes a newly created context if cookie seeding fails", async () => {
  context.addCookies = vi.fn().mockRejectedValue(new Error("invalid cookie"));
  await expect(acquireBrowserContext("invalid-cookie", {
    cookieString: "test=value", cookieDomain: ".muse.ai",
  })).rejects.toThrow("invalid cookie");
  expect(context.close).toHaveBeenCalledTimes(1);
  expect(getBrowserPoolStatus().contexts).toBe(0);
});
