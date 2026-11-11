// MuseRouter Windows 2.0.5 contract from engine.py, store.py and the admin page.
// The Hatch browser worker stays in muse-web.js. This module is pure so the
// quota decision and the hub layout can be tested without Chrome.

export const MUSE_ROUTER_PORT = 18610;

// version.json highlight, shown when an account is switched off for quota.
import { MUSE_QUOTA_REASON } from "../../config/museWeb.js";
export { MUSE_QUOTA_BADGE, MUSE_QUOTA_REASON } from "../../config/museWeb.js";
// admin.html account field auto_disabled_reason.

// admin.html: hatch_sess is required. The other three still work when absent.
export const REQUIRED_COOKIES = ["hatch_sess"];
export const OPTIONAL_COOKIES = ["hatch_vml", "hatch_native_auth_device", "hatch_gw"];

// Bare ids the Windows dashboard documents. muse-spark is chat.
export const MUSE_MODELS = ["muse-spark", "muse-image", "muse-video"];

// engine.py poll: a phrase raises a generation error. It does not disable the
// account. Disable happens only after quota_is_exhausted on a fresh Usage panel.
const QUOTA_PROBE_RE = /额度不足|积分不足|out of credits|达到上限|token limit|out of tokens|no tokens left|ran out of tokens|0\s*tokens?\s+left|tokens?\s+(?:are\s+)?exhausted|weekly (?:limit|quota)\s+(?:reached|hit|exceeded)|you'?ve used all|hết token|hết lượt|đã dùng hết/i;

// engine.py _EXTRA_ZERO_RE. Additional tokens count as empty only in this shape.
const EXTRA_ZERO_RE = /^0(?:\.0+)?\s*(?:[BMK]\s*)?(?:tokens?|符元|符記|代幣|代币|令牌|憑證|凭证)\b/i;

const PCT_RE = /(?:(\d+(?:\.\d+)?)\s*%\s*(?:used|已使用|đã dùng)|(?:已使用|đã dùng)\s*(\d+(?:\.\d+)?)\s*%)/i;
const LEFT_RE = /(?:([\d.,]+\s*[BMK]?\s*(?:tokens?|符元|符記|代幣|代币|令牌|憑證|凭证))\s*(?:left|剩余|剩餘|còn lại)|(?:剩余|剩餘|còn lại)\s*([\d.,]+\s*[BMK]?\s*(?:tokens?|符元|符記|代幣|代币|令牌|憑證|凭证)))/i;
const RESET_RE = /(?:resets?\s+in\s+([^\n]+)|([^\n]*?(?:后重置|後重置|重置于|重置於))|(?:đặt lại sau\s+[^\n]+))/i;
const EXP_RE = /(?:expires?|将在|將在|到期|hết hạn)[^\n]*/i;
const WEEKLY_KEYS = ["weekly usage", "每周用量", "每週用量", "mức sử dụng hàng tuần", "hàng tuần"];
const EXTRA_KEYS = ["additional tokens", "额外", "額外", "附加", "token bổ sung"];

const CONN_SLOTS = 24;
const CONN_POSITIONS = [[50, 16], [14, 30], [86, 30], [14, 70], [86, 70], [50, 84], [14, 50], [86, 50]];
// [rx%, ry%, max nodes]. Outer ring 10, inner ring 8. Node 19+ stays in the list.
const CONN_RINGS = [[45, 40, 10], [27, 21, 8]];

function stripPrompt(pageText, prompt) {
  const raw = String(pageText || "");
  const needle = String(prompt || "");
  if (!needle) return raw;
  return raw.split(needle).join(" ");
}

function percentage(match) {
  if (!match) return null;
  const raw = match[1] != null ? match[1] : match[2];
  const value = Number(raw);
  if (!Number.isFinite(value) || value < 0 || value > 100) return null;
  return value;
}

function sectionOf(line) {
  const low = line.toLowerCase();
  if (WEEKLY_KEYS.some((key) => low.includes(key))) return "weekly";
  if (EXTRA_KEYS.some((key) => low.includes(key))) return "extra";
  return null;
}

/**
 * engine.py quota_is_exhausted. True only for a parsed panel: weekly at 100%
 * and additional tokens at 0, or no extra row. Missing numbers are not exhaustion.
 */
export function quotaIsExhausted(quota) {
  if (!quota || typeof quota !== "object" || !quota.found) return false;
  const weekly = Number(quota.weekly_used_pct);
  if (quota.weekly_used_pct == null || !Number.isFinite(weekly) || weekly < 100) return false;
  const extraLeft = String(quota.extra_left || "").trim();
  if (extraLeft) return EXTRA_ZERO_RE.test(extraLeft);
  const extraPct = quota.extra_used_pct;
  if (extraPct == null) return true;
  const extra = Number(extraPct);
  return Number.isFinite(extra) && extra >= 100;
}

/** engine.py _parse_quota. Usage dialog text in en / zh-Hans / zh-Hant / vi. */
export function parseQuotaPanel(txt) {
  const text = String(txt || "").normalize("NFKC");
  const lines = text.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
  let plan = null;
  for (let i = 0; i < lines.length; i++) {
    if (["usage", "用量", "mức sử dụng"].includes(lines[i].toLowerCase()) && i > 0) {
      const prev = lines[i - 1].toLowerCase();
      if (["free", "pro", "plus", "max", "plan", "方案", "gói"].some((key) => prev.includes(key))) {
        plan = lines[i - 1];
      }
      break;
    }
  }
  if (!plan) {
    for (const line of lines) {
      if (/\b(free|pro|plus|max)\s*(plan|方案)?\b/i.test(line)) {
        plan = line;
        break;
      }
    }
  }

  let weeklyPct = null;
  let weeklyReset = null;
  let extraLeft = null;
  let extraPct = null;
  let extraExpire = null;
  let section = null;
  for (const line of lines) {
    const next = sectionOf(line);
    if (next) {
      section = next;
      continue;
    }
    const pct = percentage(line.match(PCT_RE));
    const left = line.match(LEFT_RE);
    const reset = line.match(RESET_RE);
    const expire = line.match(EXP_RE);
    if (section === "weekly") {
      if (pct != null && weeklyPct == null) weeklyPct = pct;
      if (reset && weeklyReset == null) weeklyReset = (reset[1] || reset[2] || reset[0]).trim();
    } else if (section === "extra") {
      if (left && extraLeft == null) extraLeft = (left[1] || left[2]).trim();
      if (pct != null && extraPct == null) extraPct = pct;
      if (expire && extraExpire == null) extraExpire = expire[0].trim();
    }
  }
  if (weeklyPct == null) weeklyPct = percentage(text.match(PCT_RE));
  if (extraLeft == null) {
    const left = text.match(LEFT_RE);
    if (left) extraLeft = (left[1] || left[2]).trim();
  }
  const found = weeklyPct != null || extraLeft != null;
  return {
    found,
    plan,
    weekly_used_pct: weeklyPct,
    weekly_remaining_pct: weeklyPct != null ? Math.round(Math.max(0, 100 - weeklyPct) * 100) / 100 : null,
    weekly_reset: weeklyReset,
    extra_left: extraLeft,
    extra_used_pct: extraPct,
    extra_expire: extraExpire,
    checked_at: Math.floor(Date.now() / 1000),
    raw: lines.slice(0, 30).join("\n"),
  };
}

function looksLikeUsagePanel(text) {
  return String(text || "").split(/\r?\n/).some((line) => sectionOf(line.trim()));
}

function probeHit({ replyText, pageText, prompt }) {
  const hit = `${replyText || ""}\n${stripPrompt(pageText, prompt)}`.match(QUOTA_PROBE_RE);
  return hit ? hit[0] : null;
}

function quotaDecision({ disable, phrase = null, source, parsed = null, needsQuotaRead = false }) {
  return {
    disable,
    consumeMediaRetry: !disable,
    phrase,
    source,
    reason: disable ? MUSE_QUOTA_REASON : null,
    needsQuotaRead,
    quota: parsed,
  };
}

/**
 * Disable only when a fresh Usage panel parses as exhausted.
 * A reply phrase or a silent stall asks for that read and leaves the account on
 * when the read is missing or still shows tokens.
 */
export function decideQuotaDisable({ quota, replyText = "", pageText = "", prompt = "", stalled = false } = {}) {
  if (quota && typeof quota === "object") {
    if (quotaIsExhausted(quota)) return quotaDecision({ disable: true, source: "fresh_quota", parsed: quota });
    const phrase = probeHit({ replyText, pageText, prompt });
    return quotaDecision({
      disable: false,
      phrase,
      source: stalled ? "stall_unconfirmed" : (phrase ? "phrase_unconfirmed" : "failure"),
      parsed: quota,
    });
  }
  const panelText = stripPrompt(pageText, prompt);
  const parsed = looksLikeUsagePanel(panelText) ? parseQuotaPanel(panelText) : null;
  if (parsed && quotaIsExhausted(parsed)) {
    return quotaDecision({ disable: true, source: "fresh_quota", parsed });
  }
  const phrase = probeHit({ replyText, pageText, prompt });
  const sawSignal = Boolean(phrase) || stalled;
  const alreadyRead = Boolean(parsed?.found);
  return quotaDecision({
    disable: false,
    phrase,
    source: stalled ? "stall_unconfirmed" : (phrase ? "phrase_unconfirmed" : "failure"),
    parsed,
    needsQuotaRead: sawSignal && !alreadyRead,
  });
}

// admin.html parseTokenBalance: additional tokens only, optional K/M/B/T suffix.
export function parseTokenBalance(quota) {
  if (!quota || !quota.found) return null;
  const match = String(quota.extra_left || "").trim().match(/^(\d+(?:,\d{3})*(?:\.\d+)?)\s*([KMBT])?\s*(?:tokens?(?:\s+(?:left|remaining))?)?$/i);
  if (!match) return null;
  const scale = { K: 1e3, M: 1e6, B: 1e9, T: 1e12 }[String(match[2] || "").toUpperCase()] || 1;
  const value = Number(match[1].replace(/,/g, "")) * scale;
  return Number.isFinite(value) && value >= 0 ? value : null;
}

export function usesConcentricRings(count) {
  return count > CONN_POSITIONS.length;
}

// admin.html connPositions. count <= 8 returns the fixed eight slots.
export function connPositions(count) {
  if (count <= CONN_POSITIONS.length) return CONN_POSITIONS;
  const out = [];
  let left = count;
  CONN_RINGS.forEach(([rx, ry, cap], ri) => {
    if (!left) return;
    const take = Math.min(left, cap);
    const phase = -90 + (ri % 2 ? 180 / take : 0);
    for (let i = 0; i < take; i++) {
      const a = (phase + (i * 360) / take) * Math.PI / 180;
      out.push([
        +(50 + rx * Math.cos(a)).toFixed(2),
        +(50 + ry * Math.sin(a)).toFixed(2),
      ]);
    }
    left -= take;
  });
  return out;
}

// admin.html connectionSlots without localStorage. Ids keep their slot.
export function assignConnectionSlots(accounts, saved = {}) {
  const cap = Math.min(accounts.length, CONN_SLOTS);
  const ids = new Set(accounts.map((account) => account.id));
  const next = {};
  const used = new Set();
  for (const [id, slot] of Object.entries(saved || {})) {
    const n = Number(slot);
    if (!ids.has(id) || !Number.isInteger(n) || n < 0 || n >= cap || used.has(n)) continue;
    next[id] = n;
    used.add(n);
  }
  for (const account of accounts) {
    if (Number.isInteger(next[account.id])) continue;
    let slot = 0;
    while (slot < cap && used.has(slot)) slot += 1;
    if (slot >= cap) continue;
    next[account.id] = slot;
    used.add(slot);
  }
  return next;
}

export { CONN_SLOTS, CONN_POSITIONS };
