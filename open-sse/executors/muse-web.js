// MuseWebExecutor — muse.ai Hatch (muse.ai) session provider
//
// Routes requests through muse.ai's Hatch web client using browser cookies +
// Playwright DOM automation. The Hatch agent runs in a cloud VM: chat replies
// stream into the agent bubble, and image/video requests finish as inline
// attachments that are downloaded in-page as base64.
//
// Auth: cookie-based (hatch_sess + rotating hatch_gw/hatch_vml/...) from muse.ai
// Method: /api/session renewal (plain HTTP) + Playwright DOM automation
//
// Session checks are briefly cached per context. Each account keeps one hydrated
// /thread/new page after a successful chat so the next request skips navigation.
// A page that already has a conversation is never reused.

import { createHash, randomUUID } from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";
import { BaseExecutor } from "./base.js";
import { SSE_DONE, SSE_HEADERS_NO_BUFFER } from "../utils/sseConstants.js";
import { isMissingBrowserExecutable, resolveSystemChromeExecutable } from "./browserExecutableCheck.js";
import { acquireBrowserContext, releaseBrowserContext, retainBrowserContext } from "../services/browserPool.js";
import { fetchImageAsBase64, parseDataUri } from "../translator/concerns/image.js";
import { MAX_IMAGE_BYTES, FETCH_TIMEOUT_MS } from "../config/mediaConfig.js";
import { MUSE_WEB_BUSY, MUSE_WEB_TIMEOUTS } from "../config/museWeb.js";
import {
  NEW_THREAD_URL,
  parseCookiePairs,
  toMuseCookies,
  renewSession,
  MuseWebAuthError,
  MuseWebUpstreamError,
  MuseWebQuotaError,
  MuseWebTimeoutError,
} from "./muse-web/session.js";
import { flattenMessages } from "./muse-web/prompt.js";
import * as dom from "./muse-web/dom.js";
import { decideQuotaDisable, parseQuotaPanel, MUSE_QUOTA_BADGE } from "./muse-web/windows205.js";
import { confirmSessionDead, SESSION_EXPIRED_BADGE } from "./muse-web/sessionGuard.js";

export { isMissingBrowserExecutable } from "./browserExecutableCheck.js";

// ─── Constants ──────────────────────────────────────────────────────────────

const DEFAULT_MODEL_ID = "muse-chat";

const {
  sessionReuse: SESSION_REUSE_MS,
  pageReady: PAGE_READY_TIMEOUT_MS, pollMin: POLL_INTERVAL_MS,
  pollMax: MAX_POLL_INTERVAL_MS, textStable: TEXT_STABLE_MS,
  stall: STALL_AFTER_MS, firstToken: FIRST_TOKEN_TIMEOUT_MS, chat: CHAT_TIMEOUT_MS,
  image: IMAGE_TIMEOUT_MS, video: VIDEO_TIMEOUT_MS,
} = MUSE_WEB_TIMEOUTS;
// shortcut: Account slots are process-local; use shared leases before running multiple gateway processes.
const activeAccounts = new Set();
const sessions = new Map();
const warmThreads = new Map();
const WARM_THREAD_MS = 60_000;

// muse.ai posts a completion sentence ("Here's your video…") a few seconds
// before the media attachment's blob src is ready, so a reply that is "text
// only" right now may still be finalising. Keep waiting this long for the
// attachment to appear/load before treating the reply as a refusal.
const MEDIA_GRACE = { video: 90000, image: 8000 };
// The timeout is soft while muse.ai is visibly still working (stop button
// shown or attachment loading): giving up then threw away images that landed
// a minute later, and the failover started the whole generation over.
const OVERRUN_FACTOR = 2.5;
const MEDIA_POLL_MS = 600;
const MAX_CHAT_IMAGES = 8;

const sleep = (ms, signal) => delay(ms, undefined, { signal });

// ─── Helpers ────────────────────────────────────────────────────────────────

function formatChatCompletion(content, model, finishReason = "stop") {
  return {
    id: `chatcmpl-muw-${Date.now()}`,
    object: "chat.completion",
    created: Math.floor(Date.now() / 1000),
    model,
    choices: [
      {
        index: 0,
        message: { role: "assistant", content },
        finish_reason: finishReason,
        logprobs: null,
      },
    ],
    usage: { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 },
  };
}

function formatStreamChunk(content, model, id, created, finishReason = null) {
  return {
    id,
    object: "chat.completion.chunk",
    created,
    model,
    choices: [{ index: 0, delta: content ? { content } : {}, finish_reason: finishReason }],
  };
}

function errorResponse(status, error, extraHeaders = {}) {
  return new Response(JSON.stringify({ error }), {
    status,
    headers: { "Content-Type": "application/json", ...extraHeaders },
  });
}

function resolveMuseCookie(credentials) {
  const direct = String(credentials?.apiKey ?? "").trim() || String(credentials?.cookie ?? "").trim();
  if (direct) return direct;
  const providerSpecificData = credentials?.providerSpecificData;
  if (providerSpecificData && typeof providerSpecificData === "object" && !Array.isArray(providerSpecificData)) {
    return String(providerSpecificData.cookie ?? "").trim();
  }
  return "";
}

/** Map a flow error to the HTTP status the fallback layer should see. */
function statusForError(error) {
  if (error instanceof MuseWebAuthError) return 401;
  if (error instanceof MuseWebQuotaError) return 429;
  if (error instanceof MuseWebTimeoutError) return 504;
  if (error?.name === "TimeoutError") return 504;
  if (error instanceof MuseWebUpstreamError) return 502;
  if (error?.message === MUSE_WEB_BUSY) return 429;
  if (error?.name === "AbortError" || /aborted/i.test(error?.message || "")) return 499;
  return 500;
}

function freshThreadPath(page, state) {
  const reported = typeof state?.path === "string" ? state.path : "";
  if (reported) return reported;
  try {
    return new URL(page.url()).pathname;
  } catch {
    return "";
  }
}

function isUsableFreshThread(page, state) {
  const head = String(state?.head || "").toLowerCase();
  if (!state?.ready || dom.LOGIN_HINTS.some((hint) => head.includes(hint))) return false;
  return freshThreadPath(page, state) === "/thread/new";
}

function isEmptyThread(chat) {
  return !chat?.generating && !chat?.agentCount && !String(chat?.lastText || "").trim();
}

function clearWarmTimer(warm) {
  if (warm?.timer) clearTimeout(warm.timer);
}

async function discardWarmThread(key) {
  const warm = warmThreads.get(key);
  if (!warm) return;
  warmThreads.delete(key);
  clearWarmTimer(warm);
  await warm.page.close().catch(() => {});
  warm.releaseContext();
}

function scheduleWarmThread(key, page, context, releaseContext, log) {
  const previous = warmThreads.get(key);
  if (previous) {
    warmThreads.delete(key);
    clearWarmTimer(previous);
    void previous.page.close().catch(() => {});
    previous.releaseContext();
  }
  const warm = { page, context, releaseContext, timer: null };
  const ready = (async () => {
    await page.goto(NEW_THREAD_URL, { waitUntil: "domcontentloaded", timeout: PAGE_READY_TIMEOUT_MS });
    await waitForThreadReady(page);
    const state = (await page.evaluate(dom.PAGE_STATE)) || {};
    if (!isUsableFreshThread(page, state)) throw new Error("warm thread is not a fresh composer");
    const chat = (await page.evaluate(dom.CHAT_STATE)) || {};
    if (!isEmptyThread(chat)) throw new Error("warm thread is not empty");
  })();
  warm.ready = ready;
  warmThreads.set(key, warm);
  warm.timer = setTimeout(() => { void discardWarmThread(key); }, WARM_THREAD_MS);
  warm.timer.unref?.();
  ready.then(() => {
    if (warmThreads.get(key) === warm) log?.info?.("MUSE-WEB", "warm thread parked");
  }, () => {
    if (warmThreads.get(key) === warm) void discardWarmThread(key);
  });
}

async function claimWarmThread(key, context) {
  const warm = warmThreads.get(key);
  if (!warm || warm.context !== context) return null;
  warmThreads.delete(key);
  clearWarmTimer(warm);
  try {
    await warm.ready;
    if (warm.page.isClosed?.()) throw new Error("closed");
    const state = (await warm.page.evaluate(dom.PAGE_STATE)) || {};
    if (!isUsableFreshThread(warm.page, state)) throw new Error("stale");
    const chat = (await warm.page.evaluate(dom.CHAT_STATE)) || {};
    if (!isEmptyThread(chat)) throw new Error("dirty");
    warm.releaseContext();
    return warm.page;
  } catch {
    await warm.page.close().catch(() => {});
    warm.releaseContext();
    return null;
  }
}

function reserveAccount(credentials) {
  const rawCookie = resolveMuseCookie(credentials);
  const sessionCookie = parseCookiePairs(rawCookie).find(({ name }) => name === "hatch_sess")?.value;
  if (!sessionCookie) {
    throw new MuseWebAuthError("Missing muse.ai hatch_sess cookie");
  }
  const key = credentials?.connectionId || createHash("sha256").update(sessionCookie).digest("hex");
  if (activeAccounts.has(key)) throw Object.assign(new Error(MUSE_WEB_BUSY), { status: 429 });
  activeAccounts.add(key);
  let released = false;
  return () => {
    if (released) return;
    released = true;
    activeAccounts.delete(key);
  };
}

async function waitForThreadReady(page, signal) {
  const deadline = Date.now() + PAGE_READY_TIMEOUT_MS;
  let state = {};
  while (Date.now() < deadline) {
    signal?.throwIfAborted();
    // A busy page can leave evaluate unanswered while it loads; poll again.
    try {
      state = (await page.evaluate(dom.PAGE_STATE)) || {};
      if (state.ready) return state;
    } catch {
      /* page navigating — poll again */
    }
    if (!state.hasInput && dom.LOGIN_HINTS.some((hint) => (state.head || "").toLowerCase().includes(hint))) {
      throw new MuseWebAuthError("muse.ai redirected to login; cookies are no longer valid");
    }
    await sleep(150, signal);
  }
  const head = (state.head || "").toLowerCase();
  if (dom.LOGIN_HINTS.some((h) => head.includes(h))) {
    throw new MuseWebAuthError("muse.ai redirected to login; cookies are no longer valid");
  }
  throw new MuseWebTimeoutError("muse.ai page did not become ready");
}

async function clickQuotaPoint(page, expr, timeoutMs, signal) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    signal?.throwIfAborted();
    const raw = await page.evaluate(expr);
    const pt = typeof raw === "string" && raw.startsWith("{") ? JSON.parse(raw) : null;
    if (pt && Number.isFinite(pt.x) && Number.isFinite(pt.y)) {
      await page.mouse.click(pt.x, pt.y);
      return true;
    }
    await sleep(200, signal);
  }
  return false;
}

async function dismissQuotaUi(page, signal) {
  signal?.throwIfAborted();
  for (let i = 0; i < 2; i++) {
    await page.keyboard.press("Escape").catch(() => {});
    await sleep(100, signal);
  }
  if (await page.evaluate(dom.QUOTA_STILL_OPEN_JS)) {
    const raw = await page.evaluate(dom.QUOTA_CLOSE_JS);
    const pt = typeof raw === "string" && raw.startsWith("{") ? JSON.parse(raw) : null;
    if (pt && Number.isFinite(pt.x)) await page.mouse.click(pt.x, pt.y);
    else await page.mouse.click(20, 20).catch(() => {});
  }
}

// engine.py quota(), then the runner keeps the account unless the panel is exhausted.
// A dead tab or a missing dialog returns null. That is not proof of remaining tokens.
async function readQuotaPanel(page, signal) {
  signal?.throwIfAborted();
  try {
    await dismissQuotaUi(page, signal);
    if (!(await clickQuotaPoint(page, dom.QUOTA_SETTINGS_JS, 8000, signal))) return null;
    if (!(await clickQuotaPoint(page, dom.QUOTA_MENU_JS, 8000, signal))) return null;
    const deadline = Date.now() + 12000;
    while (Date.now() < deadline) {
      signal?.throwIfAborted();
      const text = await page.evaluate(dom.QUOTA_DIALOG_JS);
      const parsed = parseQuotaPanel(typeof text === "string" ? text : "");
      if (parsed.found) return parsed;
      await sleep(200, signal);
    }
    return null;
  } catch (error) {
    if (signal?.aborted || error?.name === "AbortError") throw error;
    return null;
  } finally {
    await dismissQuotaUi(page, signal).catch(() => {});
  }
}

async function throwIfQuotaConfirmed(page, decision, signal) {
  if (decision.disable) {
    throw new MuseWebQuotaError(`${MUSE_QUOTA_BADGE} (weekly 100% + 0 extra)`);
  }
  if (!decision.needsQuotaRead) return;
  const quota = await readQuotaPanel(page, signal);
  if (decideQuotaDisable({ quota: quota || { found: false } }).disable) {
    throw new MuseWebQuotaError(`${MUSE_QUOTA_BADGE} (weekly 100% + 0 extra)`);
  }
}

async function readChatState(page, prompt = "", media = false) {
  const state = (await page.evaluate(media ? dom.MEDIA_STATE : dom.CHAT_STATE)) || {};
  // A phrase in the bubble is not the Usage panel. Disable only when that
  // panel was already parsed into the page text and the numbers are exhausted.
  if (!state.generating) {
    const decision = decideQuotaDisable({
      replyText: state.lastText || "",
      pageText: state.tail || "",
      prompt,
    });
    if (decision.disable) {
      throw new MuseWebQuotaError(`${MUSE_QUOTA_BADGE} (weekly 100% + 0 extra)`);
    }
  }
  return state;
}

async function denyApprovals(page) {
  await page.evaluate(dom.DENY_APPROVALS);
}

async function sendPrompt(page, prompt, signal) {
  signal?.throwIfAborted();
  const res = await page.evaluate(dom.fillInputScript(prompt));
  if (!res?.ok) throw new MuseWebUpstreamError(`cannot fill muse.ai chat input: ${res?.err}`);
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    if ((await page.evaluate(dom.CLICK_SEND)) === "clicked") return;
    await sleep(100, signal);
  }
  await page.keyboard.press("Enter");
  await sleep(500, signal);
  if (!(await page.evaluate(dom.INPUT_EMPTY))) {
    throw new MuseWebUpstreamError("send button not found and Enter did not submit");
  }
}

async function resolveImageRef(ref, signal) {
  if (typeof ref !== "string") return null;
  let parsed = parseDataUri(ref);
  if (!parsed) {
    const deadline = AbortSignal.timeout(FETCH_TIMEOUT_MS);
    const result = await fetchImageAsBase64(ref, { signal: signal ? AbortSignal.any([signal, deadline]) : deadline });
    parsed = result && parseDataUri(result.url);
  }
  signal?.throwIfAborted();
  if (!parsed || !parsed.mimeType.startsWith("image/")) return null;
  if (parsed.base64.length > Math.ceil(MAX_IMAGE_BYTES / 3) * 4 || !/^[a-zA-Z0-9+/\r\n]*={0,2}$/.test(parsed.base64)) return null;
  if (Buffer.byteLength(parsed.base64, "base64") > MAX_IMAGE_BYTES) return null;
  return { b64: parsed.base64, mime: parsed.mimeType };
}

async function attachImages(page, images, signal) {
  if (!images?.length) return 0;
  let attached = 0;
  let failed = 0;
  for (const [idx, ref] of images.slice(0, MAX_CHAT_IMAGES).entries()) {
    signal?.throwIfAborted();
    const img = await resolveImageRef(ref, signal);
    if (!img) {
      failed++;
      continue;
    }
    const ext = (img.mime.split("/")[1] || "png").replace("jpeg", "jpg");
    const res = await page.evaluate(dom.attachFileScript(img.b64, img.mime, `input_${idx}.${ext}`));
    if (!res?.ok) throw new MuseWebUpstreamError(`failed to attach image: ${res?.err}`);
    attached++;
  }
  // Every ref failed to resolve — the caller asked for images that never
  // arrived. Fail fast instead of sending a text-only prompt the agent
  // answers with "which image?" (which burns a multi-minute video turn).
  if (!attached && failed > 0) {
    throw new MuseWebUpstreamError(
      `could not download any input image (${failed} failed — check the URLs)`
    );
  }
  if (attached) await sleep(1000, signal);
  return attached;
}

/**
 * Poll the agent bubble and emit text deltas until the reply is stable.
 *
 * Unchanged text completes only after generating stops. Poll less while idle.
 */
async function pollChat({ page, base, signal, onDelta, prompt = "" }) {
  const baseCount = base.agentCount || 0;
  const baseText = base.lastText || "";
  const started = Date.now();
  let emitted = "";
  let last = null;
  let doneSince = null;
  let pollMs = POLL_INTERVAL_MS;
  let gotFirst = false;
  let quotaChecked = false;

  const confirmQuota = async (state, stalled = false) => {
    if (quotaChecked) return;
    const decision = decideQuotaDisable({
      replyText: state?.lastText || "",
      pageText: state?.tail || "",
      prompt,
      stalled,
    });
    if (!decision.disable && !decision.needsQuotaRead) return;
    quotaChecked = true;
    await throwIfQuotaConfirmed(page, decision, signal);
  };

  while (true) {
    signal?.throwIfAborted();
    const elapsed = Date.now() - started;
    if (elapsed > CHAT_TIMEOUT_MS) {
      await confirmQuota({ lastText: last || "" }, true);
      throw new MuseWebTimeoutError("assistant reply timed out");
    }

    const st = await readChatState(page, prompt);
    if (!st.generating) await confirmQuota(st);
    if (st.approval) {
      await denyApprovals(page);
      await sleep(pollMs, signal);
      continue;
    }
    const text = st.lastText || "";
    const isNew = (st.agentCount || 0) > baseCount || (text && text !== baseText);
    if (!isNew || !text) {
      if (!gotFirst && !st.generating && elapsed > FIRST_TOKEN_TIMEOUT_MS) {
        throw new MuseWebTimeoutError("no first token from assistant");
      }
      const tail = st.tail || "";
      if (elapsed > STALL_AFTER_MS && dom.STALL_HINTS.some((h) => tail.includes(h))) {
        throw new MuseWebTimeoutError("upstream workspace is stuck connecting");
      }
      pollMs = Math.min(MAX_POLL_INTERVAL_MS, pollMs + POLL_INTERVAL_MS);
      await sleep(pollMs, signal);
      continue;
    }
    gotFirst = true;
    if (text !== last) {
      const delta = text.startsWith(emitted) ? text.slice(emitted.length) : text;
      if (delta) {
        emitted = text;
        onDelta?.(delta);
      }
      last = text;
      doneSince = null;
      pollMs = POLL_INTERVAL_MS;
      await page.evaluate(dom.SCROLL_LAST_REPLY);
    } else {
      pollMs = Math.min(MAX_POLL_INTERVAL_MS, pollMs + POLL_INTERVAL_MS);
    }
    if (st.generating) doneSince = null;
    else {
      doneSince ??= Date.now();
      if (Date.now() - doneSince >= TEXT_STABLE_MS) return emitted;
    }
    const remaining = doneSince === null ? pollMs : TEXT_STABLE_MS - (Date.now() - doneSince);
    await sleep(Math.min(pollMs, remaining), signal);
  }
}

function buildMediaPrompt(prompt, kind, size, duration, hasImage) {
  const hints = [];
  if (size) hints.push(`aspect ratio ${size}`);
  if (duration) hints.push(`${duration} seconds long`);
  const verb =
    kind === "image"
      ? "Generate an image"
      : hasImage
        ? "Animate the attached image into a video"
        : "Generate a video";
  const suffix = hints.length ? ` (${hints.join(", ")})` : "";
  return `${verb}${suffix}: ${prompt}`;
}

const VIDEO_EXT = [".mp4", ".webm", ".mov", ".m4v"];
const IMAGE_EXT = [".png", ".jpg", ".jpeg", ".webp", ".gif", ".avif"];
const MEDIA_HOSTS = [
  "videodelivery.net",
  "cloudflarestream.com",
  "cloudflarestorage.com",
  "amazonaws.com",
  "storage.googleapis.com",
  "blob.core.windows.net",
  "musecdn",
  "cdn.muse",
  "muse-cdn",
];
const MEDIA_PATHS = ["/media/", "/download", "/dl/", "/video", "/attachment", "/files/"];

/** Best media URL from a reply bubble, or null if nothing looks like one. */
function pickMediaUrl(links, kind) {
  const exts = kind === "video" ? VIDEO_EXT : IMAGE_EXT;
  const score = (u) => {
    const low = u.toLowerCase();
    const path = low.split("?")[0].split("#")[0];
    if (u.startsWith("blob:") || exts.some((e) => path.endsWith(e))) return 3;
    if (MEDIA_HOSTS.some((h) => low.includes(h))) return 2;
    if (MEDIA_PATHS.some((seg) => low.includes(seg))) return 1;
    return 0;
  };
  const cands = links.filter((u) => u && !u.startsWith("mailto:") && !u.startsWith("javascript:"));
  cands.sort((a, b) => score(b) - score(a));
  return cands.length && score(cands[0]) > 0 ? cands[0] : null;
}

/**
 * Wait for the generated media to land as an inline attachment. Falls back, in
 * order, to asking the agent to attach the file (it often leaves the final
 * file only in its workspace), then to a media link inside the reply bubble.
 */
async function waitForMedia({ page, base, kind, timeoutMs, signal, log, prompt = "" }) {
  const baseAtts = (base.attachments || []).length;
  let baseCount = base.agentCount || 0;
  const grace = MEDIA_GRACE[kind] ?? 8000;
  const started = Date.now();
  let textDoneAt = null;
  let denied = false;
  let askedToAttach = false;
  let busy = false;
  let elapsed = 0;
  let quotaChecked = false;
  let lastState = null;

  const confirmQuota = async (state, stalled = false) => {
    if (quotaChecked) return;
    const decision = decideQuotaDisable({
      replyText: state?.lastText || "",
      pageText: state?.tail || "",
      prompt,
      stalled,
    });
    if (!decision.disable && !decision.needsQuotaRead) return;
    quotaChecked = true;
    await throwIfQuotaConfirmed(page, decision, signal);
  };

  while (true) {
    elapsed = Date.now() - started;
    if (elapsed >= timeoutMs && !(busy && elapsed < timeoutMs * OVERRUN_FACTOR)) break;
    signal?.throwIfAborted();
    await sleep(MEDIA_POLL_MS, signal);

    const st = await readChatState(page, prompt, true);
    lastState = st;
    if (!st.generating) await confirmQuota(st);
    if (st.approval) {
      await denyApprovals(page);
      denied = true;
      textDoneAt = null;
      continue;
    }
    const newAtts = (st.attachments || []).slice(baseAtts);
    const fresh = newAtts.filter((a) => a.src && a.kind === kind);
    if (fresh.length) return fresh[fresh.length - 1];

    busy = Boolean(st.generating) || newAtts.some((a) => (a.tid || "").includes(kind));
    // An attachment node for this kind is on the page but its (blob) src has
    // not loaded yet -> media is still finalising, keep waiting.
    if (newAtts.some((a) => (a.tid || "").includes(kind))) {
      textDoneAt = null;
      continue;
    }
    // No attachment yet. The reply may be genuinely text-only (a refusal), or
    // the attachment may simply lag the completion sentence.
    const text = st.lastText || "";
    if ((st.agentCount || 0) > baseCount && text && !st.generating) {
      if (textDoneAt === null) {
        textDoneAt = Date.now();
      } else if (Date.now() - textDoneAt >= grace) {
        if (!askedToAttach) {
          log?.info?.(
            "MUSE-WEB",
            `${kind} not attached (${denied ? "denied approval" : "text-only reply"}); asking for it in chat`
          );
          await sendPrompt(
            page,
            `Please attach the final ${kind} file here in the chat` +
              (denied ? " instead of uploading it." : ", as a single attachment."),
            signal
          );
          askedToAttach = true;
          textDoneAt = null;
          baseCount = st.agentCount || 0;
          continue;
        }
        const probe = (await page.evaluate(dom.LAST_BUBBLE_MEDIA).catch(() => null)) || {};
        const url = pickMediaUrl(probe.links || [], kind);
        if (url) {
          log?.info?.("MUSE-WEB", `${kind} delivered as link in text bubble`);
          return { src: url, kind };
        }
        throw new MuseWebUpstreamError(`muse.ai replied with text only: ${text.slice(0, 200)}`);
      }
    } else {
      textDoneAt = null;
    }
  }
  if (busy) log?.warn?.("MUSE-WEB", `${kind} still generating after ${Math.round(elapsed / 1000)}s; giving up`);
  // A silent stall is not exhaustion. Re-read the Usage panel once; disable
  // only when that read shows weekly 100% and no additional tokens.
  if (!quotaChecked) {
    const stalled = await readChatState(page, prompt, true).catch((error) => {
      if (error instanceof MuseWebQuotaError) throw error;
      return lastState;
    });
    await confirmQuota(stalled, true);
  }
  throw new MuseWebTimeoutError(`${kind} generation timed out`);
}

async function downloadMedia(page, att, kind) {
  const res = await page.evaluate(dom.fetchAsBase64Script(att.src));
  if (!res?.ok) {
    throw new MuseWebUpstreamError(`failed to download generated ${kind}: ${res?.err}`);
  }
  return { b64: res.b64, mime: res.mime || (kind === "video" ? "video/mp4" : "image/png") };
}

// ─── Executor ───────────────────────────────────────────────────────────────

export class MuseWebExecutor extends BaseExecutor {
  constructor() {
    super("muse-web", { id: "muse-web", baseUrl: "https://muse.ai" });
  }

  /**
   * testConnection — validates the cookie format without a network call or a
   * browser launch. hatch_sess is the only long-lived identifying cookie (the
   * others rotate via /api/session), so it must be present.
   */
  async testConnection(credentials) {
    try {
      const raw = resolveMuseCookie(credentials);
      if (!raw) return false;
      const pairs = parseCookiePairs(raw);
      return pairs.some((p) => p.name === "hatch_sess" && p.value.length > 0);
    } catch {
      return false;
    }
  }

  /**
   * Validate uncached sessions, acquire the pooled context with current cookies,
   * and open a ready /thread/new page.
   */
  async _openThreadPage(credentials, { signal, log }) {
    signal?.throwIfAborted();
    const rawCookie = resolveMuseCookie(credentials);
    if (!rawCookie) throw new MuseWebAuthError("Missing muse.ai cookies");

    const key = `muse-web:${credentials?.connectionId || ""}:${createHash("sha256").update(rawCookie).digest("hex")}`;
    const cached = sessions.get(key);
    let cookieHeader = cached?.cookieHeader || rawCookie;
    let reuseSession = false;
    if (cached) {
      const cookies = await cached.context.cookies(NEW_THREAD_URL).catch(() => null);
      if (cookies?.length) {
        cookieHeader = cookies.map(({ name, value }) => `${name}=${value}`).join("; ");
        reuseSession = Date.now() - cached.renewedAt < SESSION_REUSE_MS
          && cookies.some(({ name, value }) => name === "hatch_sess" && value);
      }
    }
    let session = { cookieHeader };
    if (!reuseSession) {
      const verdict = await confirmSessionDead({
        // The web client wakes the VM with its current browser auth; a plain HTTP wake duplicates it.
        renew: () => renewSession(cookieHeader, { signal, wakeVm: false }),
        signal,
      });
      if (verdict.dead) {
        await discardWarmThread(key);
        throw new MuseWebAuthError(SESSION_EXPIRED_BADGE);
      }
      if (verdict.error) throw verdict.error;
      session = verdict.session;
      log?.debug?.(
        "MUSE-WEB",
        `session renewed (status=${session.meta.session_status ?? "?"}, vm=${session.meta.vm_state ?? "?"}, check=${verdict.reason})`
      );
    }
    const renewedAt = reuseSession ? cached.renewedAt : Date.now();
    const renewedCookies = toMuseCookies(session.cookieHeader);
    const names = new Set(renewedCookies.map(({ name }) => name));
    // Plain HTTP renewal does not apply Set-Cookie deletions to the browser.
    renewedCookies.push(...toMuseCookies(cookieHeader)
      .filter(({ name }) => !names.has(name))
      .map((cookie) => ({ ...cookie, expires: 1 })));
    const executablePath = resolveSystemChromeExecutable();
    let pooled = await acquireBrowserContext(key, {
      cookieDomain: ".muse.ai",
      cookieString: session.cookieHeader,
      ...(executablePath ? { executablePath } : {}),
    });
    let releaseContext = retainBrowserContext(pooled);
    let page = null;
    let closing;
    let handedOff = false;
    const onAbort = () => { void close(); };
    const close = () => {
      if (handedOff) return Promise.resolve();
      closing ??= (async () => {
        signal?.removeEventListener("abort", onAbort);
        await page?.close().catch(() => {});
        releaseContext();
      })();
      return closing;
    };
    const keep = () => {
      if (handedOff || closing || !page) return;
      handedOff = true;
      signal?.removeEventListener("abort", onAbort);
      scheduleWarmThread(key, page, pooled.context, releaseContext, log);
    };
    try {
      signal?.throwIfAborted();
      try {
        await pooled.context.addCookies(renewedCookies);
      } catch {
        if (warmThreads.get(key)?.context === pooled.context) await discardWarmThread(key);
        releaseContext();
        await releaseBrowserContext(key);
        pooled = await acquireBrowserContext(key, {
          cookieDomain: ".muse.ai",
          cookieString: session.cookieHeader,
          ...(executablePath ? { executablePath } : {}),
        });
        releaseContext = retainBrowserContext(pooled);
      }
      const state = { context: pooled.context, cookieHeader: session.cookieHeader, renewedAt };
      sessions.set(key, state);
      if (cached?.context !== pooled.context) {
        pooled.context.once("close", () => {
          if (sessions.get(key)?.context === pooled.context) sessions.delete(key);
          const warm = warmThreads.get(key);
          if (warm?.context !== pooled.context) return;
          warmThreads.delete(key);
          clearWarmTimer(warm);
          warm.releaseContext();
        });
      }
      signal?.throwIfAborted();
      const openedAt = Date.now();
      page = await claimWarmThread(key, pooled.context);
      if (page) {
        signal?.addEventListener("abort", onAbort, { once: true });
        log?.info?.("MUSE-WEB", `warm thread ready in ${Date.now() - openedAt}ms`);
      } else {
        page = await pooled.context.newPage();
        signal?.addEventListener("abort", onAbort, { once: true });
        signal?.throwIfAborted();
        await page.goto(NEW_THREAD_URL, { waitUntil: "domcontentloaded", timeout: PAGE_READY_TIMEOUT_MS });
        await waitForThreadReady(page, signal);
        log?.info?.("MUSE-WEB", `cold thread ready in ${Date.now() - openedAt}ms`);
      }
      return { page, close, keep };
    } catch (error) {
      await close();
      if (reuseSession && error instanceof MuseWebAuthError) {
        sessions.delete(key);
        return this._openThreadPage(credentials, { signal, log });
      }
      throw error;
    }
  }

  async execute({ model, body, stream, credentials, signal, log }) {
    const requestBody = body || {};
    const messages = requestBody.messages || [];
    const modelId = model || DEFAULT_MODEL_ID;

    // The muse.ai Hatch agent only uses its own built-in tools (web search,
    // image generation) and explicitly ignores prompt-level tool contracts —
    // verified live: it answers around them instead of emitting calls. Fail
    // fast with a clean 400 rather than a doomed emulation.
    const requestedTools = requestBody.tools;
    const requestedFunctions = requestBody.functions;
    if (requestedTools != null && (!Array.isArray(requestedTools) || requestedTools.length > 0)) {
      return {
        response: errorResponse(400, "Muse Web does not support OpenAI function tools"),
        url: "https://muse.ai",
        headers: {},
        transformedBody: body,
      };
    }
    if (requestedFunctions != null && (!Array.isArray(requestedFunctions) || requestedFunctions.length > 0)) {
      return {
        response: errorResponse(400, "Muse Web does not support legacy function tools"),
        url: "https://muse.ai",
        headers: {},
        transformedBody: body,
      };
    }

    const { text: flatPrompt, images } = flattenMessages(messages);

    if (!flatPrompt) {
      return {
        response: errorResponse(400, "No user message found"),
        url: "https://muse.ai",
        headers: {},
        transformedBody: body,
      };
    }

    const cancellation = new AbortController();
    signal = signal ? AbortSignal.any([signal, cancellation.signal]) : cancellation.signal;
    let opened;
    let release;
    let streaming = false;
    let keepPage = false;

    try {
      signal.throwIfAborted();
      release = reserveAccount(credentials);
      // Setup phase runs eagerly so auth/ready failures return a real HTTP
      // status instead of a stream that errors on first read.
      opened = await this._openThreadPage(credentials, { signal, log });
      const { page } = opened;

      const base = await readChatState(page, flatPrompt);
      await attachImages(page, images, signal);
      await sendPrompt(page, flatPrompt, signal);

      if (stream) {
        const encoder = new TextEncoder();
        const id = `chatcmpl-muw-${randomUUID()}`;
        const created = Math.floor(Date.now() / 1000);
        let task;
        let cancelled = false;
        const readable = new ReadableStream(
          {
            start(controller) {
              task = (async () => {
                try {
                  await pollChat({
                    page,
                    base,
                    signal,
                    prompt: flatPrompt,
                    onDelta: (delta) => {
                      controller.enqueue(
                        encoder.encode(`data: ${JSON.stringify(formatStreamChunk(delta, modelId, id, created))}\n\n`)
                      );
                    },
                  });
                  if (cancelled) return;
                  keepPage = true;
                  controller.enqueue(
                    encoder.encode(`data: ${JSON.stringify(formatStreamChunk("", modelId, id, created, "stop"))}\n\n`)
                  );
                  controller.enqueue(encoder.encode(SSE_DONE));
                  controller.close();
                } catch (error) {
                  if (cancelled) return;
                  controller.enqueue(
                    encoder.encode(
                      `data: ${JSON.stringify({ error: { message: error.message || "muse-web stream failed" } })}\n\n`
                    )
                  );
                  controller.enqueue(encoder.encode(SSE_DONE));
                  controller.close();
                } finally {
                  if (keepPage && !cancelled) opened.keep();
                  else await opened.close();
                  release();
                }
              })();
            },
            cancel() {
              cancelled = true;
              cancellation.abort();
              return task;
            },
          },
          { highWaterMark: 16384 }
        );
        streaming = true;
        return {
          response: new Response(readable, { status: 200, headers: { ...SSE_HEADERS_NO_BUFFER } }),
          url: "https://muse.ai",
          headers: {},
          transformedBody: body,
        };
      }

      const responseText = await pollChat({ page, base, signal, prompt: flatPrompt });
      keepPage = true;
      return {
        response: new Response(JSON.stringify(formatChatCompletion(responseText, modelId)), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        }),
        url: "https://muse.ai",
        headers: {},
        transformedBody: body,
      };
    } catch (error) {
      const message = error instanceof Error ? error.message : "Unknown error";

      // A missing Playwright browser is a host/config problem, not a transient
      // upstream fault. Actionable error + cooldown hint so accountFallback
      // applies a short cooldown instead of looping on a retryable 500.
      if (isMissingBrowserExecutable(message)) {
        return {
          response: errorResponse(
            503,
            "Muse Web needs a Chrome/Chromium binary. Install Google Chrome, set NINEROUTER_BROWSER_BIN " +
              "or CHROME_PATH to an existing Chrome, or run `npx playwright-core install chromium`."
          ),
          url: "https://muse.ai",
          headers: {},
          transformedBody: body,
        };
      }
      // DOM/selector timeouts are terminal — muse.ai shipped a UI change or the
      // session is so stale the page is somewhere else entirely. 400 keeps the
      // fallback layer from retrying it as a transient 5xx.
      if (
        error instanceof Error &&
        !signal.aborted && (message.includes("waitForSelector") || message.includes("actionability"))
      ) {
        return {
          response: errorResponse(400, message),
          url: "https://muse.ai",
          headers: {},
          transformedBody: body,
        };
      }
      return {
        response: errorResponse(signal.aborted ? 499 : statusForError(error), message),
        url: "https://muse.ai",
        headers: {},
        transformedBody: body,
      };
    } finally {
      if (!streaming) {
        if (keepPage) opened?.keep();
        else await opened?.close();
        release?.();
      }
    }
  }

  /**
   * Generate image (or video) media. Media always starts a fresh thread.
   * Used by the /v1/images/generations adapter (handlers/imageProviders/muse-web.js).
   *
   * @returns {Promise<Array<{ b64: string, mime: string, width?: number, height?: number }>>}
   */
  async generateMedia({ prompt, kind = "image", size, duration, n = 1, images, credentials, signal, log }) {
    if (typeof prompt !== "string" || !prompt.trim() || !["image", "video"].includes(kind) || !Number.isInteger(n) || n < 1 || n > 4) {
      throw Object.assign(new Error("Media requires a prompt, image/video kind and integer n between 1 and 4"), { status: 400 });
    }
    signal?.throwIfAborted();
    const release = reserveAccount(credentials);
    const mediaPrompt = buildMediaPrompt(prompt, kind, size, duration, Boolean(images?.length));
    const timeoutMs = kind === "video" ? VIDEO_TIMEOUT_MS : IMAGE_TIMEOUT_MS;
    const results = [];

    try {
      for (let i = 0; i < n; i++) {
        signal?.throwIfAborted();
        const opened = await this._openThreadPage(credentials, { signal, log });
        const { page } = opened;
        try {
          const base = await readChatState(page, mediaPrompt, true);
          if (images?.length) await attachImages(page, images, signal);
          await sendPrompt(page, mediaPrompt, signal);
          const att = await waitForMedia({ page, base, kind, timeoutMs, signal, log, prompt: mediaPrompt });
          const media = await downloadMedia(page, att, kind);
          results.push({
            b64: media.b64,
            mime: media.mime,
            ...(att.w ? { width: att.w } : {}),
            ...(att.h ? { height: att.h } : {}),
          });
        } finally {
          await opened.close();
        }
      }
      return results;
    } catch (error) {
      if (signal?.aborted) throw Object.assign(new Error("Request aborted"), { name: "AbortError", status: 499 });
      if (error?.name === "TimeoutError") throw new MuseWebTimeoutError("Muse Web request timed out");
      throw error;
    } finally {
      release();
    }
  }
}

export default MuseWebExecutor;
