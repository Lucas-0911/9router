import { beforeEach, expect, it, vi } from "vitest";

const db = vi.hoisted(() => ({
  getProviderConnections: vi.fn(),
  updateProviderConnection: vi.fn(),
}));
vi.mock("@/lib/localDb", () => ({
  ...db,
  validateApiKey: vi.fn(),
  getSettings: vi.fn(),
  getProxyPools: vi.fn(),
}));
vi.mock("../../src/sse/utils/logger.js", () => ({
  warn: vi.fn(), debug: vi.fn(), info: vi.fn(),
}));

import { markAccountUnavailable } from "../../src/sse/services/auth.js";
import { MUSE_WEB_BUSY, MUSE_QUOTA_BADGE, SESSION_EXPIRED_BADGE } from "../../open-sse/config/museWeb.js";

beforeEach(() => {
  vi.clearAllMocks();
  db.getProviderConnections.mockResolvedValue([{
    id: "a", providerSpecificData: { cookie: "preserved", other: true },
  }]);
});

it("falls back on local capacity without touching the database", async () => {
  expect(await markAccountUnavailable("a", 429, MUSE_WEB_BUSY, "muse-web", "muse-chat"))
    .toEqual({ shouldFallback: true, cooldownMs: 0 });
  expect(db.getProviderConnections).not.toHaveBeenCalled();
  expect(db.updateProviderConnection).not.toHaveBeenCalled();
});

it.each([
  [429, MUSE_QUOTA_BADGE, "quota_exhausted"],
  [401, SESSION_EXPIRED_BADGE, "session_expired"],
])("only auto-disables a confirmed Muse error %s", async (status, message, reason) => {
  await markAccountUnavailable("a", status, message, "muse-web", "muse-chat");
  expect(db.updateProviderConnection).toHaveBeenCalledWith("a", expect.objectContaining({
    isActive: false,
    providerSpecificData: { cookie: "preserved", other: true, auto_disabled_reason: reason },
  }));
});

it("does not permanently disable from a broad quota phrase in an ordinary error", async () => {
  vi.spyOn(console, "error").mockImplementation(() => {});
  try {
    await markAccountUnavailable("a", 502, "An old reply mentions limit reached", "muse-web", "muse-chat");
    expect(db.updateProviderConnection.mock.calls[0][1]).not.toHaveProperty("isActive");
  } finally {
    vi.restoreAllMocks();
  }
});

it("does not treat another provider's 429 as local Muse capacity", async () => {
  vi.spyOn(console, "error").mockImplementation(() => {});
  try {
    await markAccountUnavailable("a", 429, MUSE_WEB_BUSY, "grok-web", "grok");
    expect(db.updateProviderConnection).toHaveBeenCalled();
  } finally {
    vi.restoreAllMocks();
  }
});
