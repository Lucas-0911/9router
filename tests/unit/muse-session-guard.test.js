import { describe, expect, it, vi } from "vitest";
import { MuseWebAuthError, MuseWebUpstreamError } from "../../open-sse/executors/muse-web/session.js";
import {
  AUTH_RETRY_DELAY_MS,
  SESSION_EXPIRED_BADGE,
  confirmSessionDead,
  decideSessionDisable,
} from "../../open-sse/executors/muse-web/sessionGuard.js";

const alive = { cookieHeader: "hatch_sess=ok", meta: {} };
const unauthorized = () => new MuseWebAuthError("muse.ai session rejected (HTTP 401)", 401);

describe("MuseRouter session_guard", () => {
  it("keeps the account when the first /api/session call succeeds", async () => {
    const renew = vi.fn(async () => alive);
    const sleep = vi.fn();
    const verdict = await confirmSessionDead({ renew, sleep });
    expect(verdict).toMatchObject({ dead: false, reason: "alive", session: alive });
    expect(sleep).not.toHaveBeenCalled();
  });

  it("keeps the account when a 401 is gone on the single retry", async () => {
    const renew = vi.fn()
      .mockRejectedValueOnce(unauthorized())
      .mockResolvedValueOnce(alive);
    const sleep = vi.fn(async () => {});
    const verdict = await confirmSessionDead({ renew, sleep });
    expect(verdict).toMatchObject({ dead: false, reason: "transient_401", session: alive });
    expect(sleep).toHaveBeenCalledWith(AUTH_RETRY_DELAY_MS);
    expect(AUTH_RETRY_DELAY_MS).toBe(3000);
  });

  it("marks the session dead only after a second 401", async () => {
    const renew = vi.fn().mockRejectedValue(unauthorized());
    const verdict = await confirmSessionDead({ renew, sleep: async () => {} });
    expect(verdict).toMatchObject({ dead: true, reason: "confirmed_401" });
    expect(decideSessionDisable(SESSION_EXPIRED_BADGE)).toMatchObject({
      disable: true,
      reason: "session_expired",
    });
  });

  it("marks the session dead when the retry is also HTTP 403", async () => {
    const forbidden = new MuseWebAuthError("muse.ai session rejected (HTTP 403)", 403);
    const renew = vi.fn()
      .mockRejectedValueOnce(unauthorized())
      .mockRejectedValueOnce(forbidden);
    const verdict = await confirmSessionDead({ renew, sleep: async () => {} });
    expect(verdict).toMatchObject({ dead: true, reason: "confirmed_403", error: forbidden });
  });

  it("does not confirm death when the retry is a network error", async () => {
    const network = new MuseWebUpstreamError("muse.ai session endpoint unreachable");
    const renew = vi.fn()
      .mockRejectedValueOnce(unauthorized())
      .mockRejectedValueOnce(network);
    const verdict = await confirmSessionDead({ renew, sleep: async () => {} });
    expect(verdict.dead).toBe(false);
    expect(verdict.reason).toBe("retry_inconclusive");
    expect(verdict.error).toBe(network);
  });

  it("does not disable on a single 401 message that was not confirmed", () => {
    expect(decideSessionDisable("muse.ai session rejected (HTTP 401); cookies are no longer valid").disable).toBe(false);
  });
});
