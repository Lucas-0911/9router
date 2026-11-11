import { describe, expect, it } from "vitest";
import {
  MUSE_QUOTA_BADGE,
  MUSE_ROUTER_PORT,
  assignConnectionSlots,
  connPositions,
  decideQuotaDisable,
  parseQuotaPanel,
  parseTokenBalance,
  quotaIsExhausted,
  usesConcentricRings,
} from "../../open-sse/executors/muse-web/windows205.js";
import { QUOTA_DIALOG_JS, QUOTA_SETTINGS_JS } from "../../open-sse/executors/muse-web/dom.js";

const EXHAUSTED_PANEL = [
  "Pro Plan",
  "Usage",
  "Weekly usage",
  "100% used",
  "Resets in 2 days",
  "Additional tokens",
  "0 tokens left",
].join("\n");

describe("MuseRouter Windows 2.0.5 quota decision", () => {
  it("does not disable on a reply phrase before the Usage panel is read", () => {
    const decision = decideQuotaDisable({
      replyText: "Weekly limit reached. Try next week.",
      pageText: "",
    });
    expect(decision).toMatchObject({
      disable: false,
      consumeMediaRetry: true,
      phrase: "Weekly limit reached",
      source: "phrase_unconfirmed",
      needsQuotaRead: true,
      reason: null,
    });
    expect(MUSE_QUOTA_BADGE).toBe("Out of tokens · Auto-disabled");
  });

  it("does not disable on 0 tokens left when weekly use was not read", () => {
    const decision = decideQuotaDisable({
      replyText: "",
      pageText: "Additional tokens: 0 tokens left",
      stalled: true,
    });
    expect(decision.disable).toBe(false);
    expect(decision.consumeMediaRetry).toBe(true);
    expect(decision.source).toBe("stall_unconfirmed");
    expect(decision.needsQuotaRead).toBe(false);
  });

  it("disables only when a fresh Usage panel is weekly 100% and extra is 0", () => {
    const quota = parseQuotaPanel(EXHAUSTED_PANEL);
    expect(quota).toMatchObject({
      found: true,
      plan: "Pro Plan",
      weekly_used_pct: 100,
      extra_left: "0 tokens",
      weekly_reset: "2 days",
    });
    expect(quotaIsExhausted(quota)).toBe(true);
    expect(decideQuotaDisable({ quota })).toMatchObject({
      disable: true,
      consumeMediaRetry: false,
      source: "fresh_quota",
      reason: "quota_exhausted",
      needsQuotaRead: false,
    });
    expect(decideQuotaDisable({ pageText: EXHAUSTED_PANEL }).disable).toBe(true);
  });

  it("keeps the account when weekly is full but additional tokens remain", () => {
    const quota = parseQuotaPanel("Weekly usage\n100% used\nAdditional tokens\n1.5K tokens left");
    expect(quotaIsExhausted(quota)).toBe(false);
    expect(decideQuotaDisable({ quota }).disable).toBe(false);
  });

  it("keeps the account when the panel could not be read", () => {
    expect(quotaIsExhausted(null)).toBe(false);
    expect(quotaIsExhausted({ found: false, weekly_used_pct: 100, extra_left: "0 tokens" })).toBe(false);
    expect(decideQuotaDisable({ quota: { found: false }, stalled: true })).toMatchObject({
      disable: false,
      needsQuotaRead: false,
    });
  });

  it("parses the Vietnamese usage dialog", () => {
    const quota = parseQuotaPanel("Gói Pro\nMức sử dụng\nMức sử dụng hàng tuần\nđã dùng 100%\nToken bổ sung\ncòn lại 0 token");
    expect(quota.weekly_used_pct).toBe(100);
    expect(quota.extra_left).toBe("0 token");
    expect(quotaIsExhausted(quota)).toBe(true);
  });

  it("treats a missing extra row with weekly 100% as exhausted", () => {
    const quota = parseQuotaPanel("Weekly usage\n100% used");
    expect(quota.extra_left).toBe(null);
    expect(quota.extra_used_pct).toBe(null);
    expect(quotaIsExhausted(quota)).toBe(true);
    expect(quotaIsExhausted({ ...quota, extra_used_pct: 40 })).toBe(false);
  });

  it("keeps the account when the only quota words are the user's prompt", () => {
    const prompt = "draw a sign that says out of tokens";
    const decision = decideQuotaDisable({
      replyText: "",
      pageText: prompt,
      prompt,
      stalled: true,
    });
    expect(decision).toMatchObject({ disable: false, consumeMediaRetry: true, source: "stall_unconfirmed" });
  });

  it("does not treat a broad sidebar phrase as a fresh quota panel", () => {
    const decision = decideQuotaDisable({
      replyText: "Here is the image.",
      pageText: "yesterday: out of credits",
    });
    expect(decision.disable).toBe(false);
  });

  it("keeps the Settings and dialog selectors from the Windows worker", () => {
    expect(QUOTA_SETTINGS_JS).toContain('data-testid="hatch-dock-more"');
    expect(QUOTA_SETTINGS_JS).toContain("Cài đặt");
    expect(QUOTA_DIALOG_JS).toContain("[role=dialog],[aria-modal=true]");
  });

  it("parses additional tokens the way the Windows dashboard does", () => {
    expect(parseTokenBalance({ found: true, extra_left: "1.5K tokens left" })).toBe(1500);
    expect(parseTokenBalance({ found: false, extra_left: "10" })).toBe(null);
    expect(parseTokenBalance({ found: true, extra_left: "weekly 80%" })).toBe(null);
  });
});

describe("MuseRouter Windows 2.0.5 hub layout", () => {
  it("uses one ring through 8 accounts and concentric rings after that", () => {
    expect(MUSE_ROUTER_PORT).toBe(18610);
    expect(usesConcentricRings(8)).toBe(false);
    expect(usesConcentricRings(9)).toBe(true);
    expect(connPositions(8)).toHaveLength(8);
    expect(connPositions(9)[0]).toEqual([50, 10]);
    expect(connPositions(24)).toHaveLength(18);
  });

  it("keeps an account on its slot when another account is removed", () => {
    const first = assignConnectionSlots([{ id: "a" }, { id: "b" }, { id: "c" }]);
    const second = assignConnectionSlots([{ id: "a" }, { id: "c" }, { id: "d" }], first);
    expect(second.a).toBe(first.a);
    expect(second.c).toBe(first.c);
    expect(second.d).toBe(first.b);
  });
});
