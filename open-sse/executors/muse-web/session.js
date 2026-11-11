import { OPTIONAL_COOKIES, REQUIRED_COOKIES } from "./windows205.js";
import { MUSE_QUOTA_REASON } from "../../config/museWeb.js";

// muse.ai session facts: cookie parsing and the /api/session renewal flow.
//
// Everything here was observed from the public web client and may change
// without notice. Keep upstream-specific constants in this module so that a
// site update is a one-file fix.

const SITE = "https://muse.ai";
const NEW_THREAD_URL = `${SITE}/thread/new`;
const SESSION_API = `${SITE}/api/session`;
const VM_WAKE_API = `${SITE}/api/hatch/vm/wake`;

// hatch_sess is the only cookie Windows 2.0.5 requires. The rest are optional.
const SESSION_COOKIES = [...REQUIRED_COOKIES, ...OPTIONAL_COOKIES];

// engine.py renew_session_http pins this Chrome/141 Windows agent.
const MUSE_USER_AGENT =
  "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) " +
  "Chrome/141.0.0.0 Safari/537.36";

// Cleared by muse.ai only when the session itself is dead. hatch_gw may expire.
const ESSENTIAL_COOKIES = new Set(["hatch_sess", "hatch_vml", "hatch_native_auth_device"]);
const AUTH_STATUSES = new Set([401, 403, 301, 302, 303, 307, 308]);

// Error kinds the executor maps to HTTP statuses.
export class MuseWebAuthError extends Error {
  constructor(message, status = 401) {
    super(message);
    this.name = "MuseWebAuthError";
    this.status = status;
  }
}
export class MuseWebUpstreamError extends Error {
  constructor(message) {
    super(message);
    this.name = "MuseWebUpstreamError";
    this.status = 502;
  }
}
export class MuseWebQuotaError extends Error {
  constructor(message) {
    super(message);
    this.name = "MuseWebQuotaError";
    this.status = 429;
    this.autoDisable = true;
    this.consumeMediaRetry = false;
    this.reason = MUSE_QUOTA_REASON;
  }
}
export class MuseWebTimeoutError extends Error {
  constructor(message) {
    super(message);
    this.name = "MuseWebTimeoutError";
    this.status = 504;
  }
}

const COOKIE_ATTR_NAMES = new Set([
  "path",
  "domain",
  "expires",
  "max-age",
  "secure",
  "httponly",
  "samesite",
]);

/**
 * Parse a raw Cookie header string into [{ name, value }] pairs, skipping
 * cookie attributes (Path, Domain, Expires, ...) that are not pairs.
 */
export function parseCookiePairs(raw) {
  return String(raw || "")
    .split(";")
    .map((part) => part.trim())
    .filter(Boolean)
    .map((part) => {
      const eq = part.indexOf("=");
      if (eq === -1) return null;
      const name = part.slice(0, eq).trim();
      const value = part.slice(eq + 1).trim();
      if (!name || !value) return null;
      if (COOKIE_ATTR_NAMES.has(name.toLowerCase())) return null;
      return { name, value };
    })
    .filter(Boolean);
}

export function cookieHeaderOf(pairs) {
  return pairs
    .filter((p) => p.value)
    .map((p) => `${p.name}=${p.value}`)
    .join("; ");
}

/** Missing required session cookies. Optional hatch_* cookies are not required. */
export function missingSessionCookies(pairs) {
  const names = new Set(pairs.map((p) => p.name));
  return REQUIRED_COOKIES.filter((name) => !names.has(name));
}

/**
 * Playwright addCookies() shape for muse.ai.
 * Omit `expires`. CDP Storage.setCookies rejects `-1`, and a past expiry makes
 * Chromium drop hatch_vml. A missing expiry is a session cookie.
 * `__Host-` cookies are host-only; a Domain attribute fails the whole batch.
 */
export function toMuseCookies(header) {
  return parseCookiePairs(header).map(({ name, value }) => {
    const hostOnly = name.startsWith("__Host-");
    return {
      name,
      value,
      domain: hostOnly ? "muse.ai" : ".muse.ai",
      path: "/",
      secure: true,
      httpOnly: false,
      sameSite: "Lax",
    };
  });
}

function readSetCookies(response) {
  if (typeof response.headers.getSetCookie === "function") {
    return response.headers.getSetCookie();
  }
  const joined = response.headers.get("set-cookie");
  return joined ? [joined] : [];
}

function parseSetCookieLine(line) {
  // "name=value; Path=/; Expires=Wed, 21 Oct 2026 07:28:00 GMT" → first pair only
  const firstPair = line.split(";")[0].trim();
  const eq = firstPair.indexOf("=");
  if (eq <= 0) return null;
  return { name: firstPair.slice(0, eq).trim(), value: firstPair.slice(eq + 1).trim() };
}

function parseSetCookieExpires(line) {
  const match = line.match(/expires\s*=\s*([^;]+)/i);
  if (!match) return 0;
  const ts = Date.parse(match[1].trim());
  return Number.isFinite(ts) ? Math.floor(ts / 1000) : 0;
}

function applySetCookies(response, jar) {
  // muse.ai rotates hatch_vml by clearing the host cookie and setting the
  // domain cookie in the same response. A clear counts only when that name
  // is still absent after every Set-Cookie line is applied.
  for (const line of readSetCookies(response)) {
    const pair = parseSetCookieLine(line);
    if (!pair) continue;
    const low = line.toLowerCase();
    const cleared = !pair.value || low.includes("max-age=0");
    if (cleared) {
      delete jar[pair.name];
      continue;
    }
    const expires = parseSetCookieExpires(line);
    if (expires && expires <= Date.now() / 1000) {
      delete jar[pair.name];
      continue;
    }
    jar[pair.name] = pair.value;
  }
  if (!jar.hatch_sess) {
    throw new MuseWebAuthError("muse.ai đã xóa cookie hatch_sess (session hết hạn)");
  }
}

/**
 * Hit the session endpoint, which rotates short-lived cookies via Set-Cookie,
 * and optionally wake the account's cloud workspace so the first request is fast.
 *
 * @returns {Promise<{ cookieHeader: string, jar: Object<string,string>, meta: object }>}
 * @throws {MuseWebAuthError} HTTP 401/403, a redirect, a cleared essential cookie, or signed-out JSON
 * @throws {MuseWebUpstreamError} network errors and HTTP >= 500. The account stays enabled.
 */
export async function renewSession(rawCookieHeader, { wakeVm = true, timeoutMs = 15000, signal } = {}) {
  signal?.throwIfAborted();
  const deadline = AbortSignal.timeout(timeoutMs);
  signal = signal ? AbortSignal.any([signal, deadline]) : deadline;
  const jar = {};
  for (const { name, value } of parseCookiePairs(rawCookieHeader)) jar[name] = value;
  if (!jar.hatch_sess) {
    throw new MuseWebAuthError("Thiếu cookie bắt buộc: hatch_sess");
  }

  const sessionHeaders = () => ({
    "User-Agent": MUSE_USER_AGENT,
    Origin: SITE,
    Referer: `${SITE}/`,
    Accept: "application/json, text/plain, */*",
    Cookie: cookieHeaderOf(Object.entries(jar).map(([name, value]) => ({ name, value }))),
  });

  let response;
  try {
    response = await fetch(SESSION_API, {
      headers: sessionHeaders(),
      redirect: "manual",
      signal,
    });
  } catch (error) {
    if (signal?.aborted) throw error;
    throw new MuseWebUpstreamError(`muse.ai session endpoint unreachable: ${error.message}`);
  }

  applySetCookies(response, jar);
  if (AUTH_STATUSES.has(response.status)) {
    throw new MuseWebAuthError(
      `muse.ai session rejected (HTTP ${response.status}); cookies are no longer valid`,
      response.status,
    );
  }
  if (response.status >= 400 || response.status === 0) {
    throw new MuseWebUpstreamError(`muse.ai session endpoint returned HTTP ${response.status}`);
  }

  let bodyJson = {};
  try {
    const contentType = response.headers.get("content-type") || "";
    if (contentType.startsWith("application/json")) bodyJson = await response.json();
  } catch {
    signal.throwIfAborted();
    /* non-JSON body — meta stays empty */
  }
  if (
    bodyJson.authenticated === false
    || (Object.prototype.hasOwnProperty.call(bodyJson, "user") && bodyJson.user == null && !bodyJson.session)
  ) {
    throw new MuseWebAuthError("/api/session báo chưa đăng nhập (user=null)");
  }

  const meta = {
    session_status: bodyJson.status,
    vm_id: bodyJson.vm_id,
    vm_state: bodyJson.vm_state,
  };

  if (wakeVm && meta.vm_id && meta.vm_state !== "DISABLED") {
    try {
      const wake = await fetch(VM_WAKE_API, {
        method: "POST",
        headers: {
          ...sessionHeaders(),
          "Content-Type": "application/json",
        },
        body: JSON.stringify({
          vm_id: meta.vm_id,
          retry_count: 0,
          connect_attempt_id: crypto.randomUUID(),
        }),
        redirect: "manual",
        signal,
      });
      applySetCookies(wake, jar);
      if (wake.status === 401 || wake.status === 403) {
        throw new MuseWebAuthError(
          `muse.ai session rejected (HTTP ${wake.status}); cookies are no longer valid`,
          wake.status,
        );
      }
      meta.vm_wake_ok = wake.status === 200;
    } catch (error) {
      if (signal?.aborted || error instanceof MuseWebAuthError) throw error;
      meta.vm_wake_ok = false;
    }
  }

  const renewedHeader = cookieHeaderOf(Object.entries(jar).map(([name, value]) => ({ name, value })));
  if (meta.session_status && meta.session_status !== "assigned") {
    // Not fatal on its own — the page may still load — but surface it for logs.
    meta.warning = `session status "${meta.session_status}"`;
  }
  return { cookieHeader: renewedHeader, jar, meta };
}

export { SITE, NEW_THREAD_URL, SESSION_API, VM_WAKE_API, SESSION_COOKIES, MUSE_USER_AGENT };
