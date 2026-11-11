// session_guard.py from MuseRouter Windows 2.0.5.
//
// confirm_session_dead calls renew_session_http (engine.py). HTTP 401 and 403
// are both MuseAuthError. One retry runs after AUTH_RETRY_DELAY (3s). A second
// 401 or 403 is a dead session. A network error, HTTP 5xx, or timeout on either
// try keeps the account. pool.py replaced FairLock; this executor does not use it.

import { MuseWebAuthError } from "./session.js";
import { setTimeout as sleep } from "node:timers/promises";
import { SESSION_EXPIRED_BADGE, SESSION_EXPIRED_REASON } from "../../config/museWeb.js";

export const AUTH_RETRY_DELAY_MS = 3000;

// store.py badge and auto_disabled_reason for a confirmed dead session.
export { SESSION_EXPIRED_BADGE, SESSION_EXPIRED_REASON };

export function isSessionUnauthorized(error) {
  return error instanceof MuseWebAuthError && (error.status === 401 || error.status === 403);
}

/**
 * @param {{ renew: () => Promise<any>, delayMs?: number, sleep?: (ms: number) => Promise<void> }} args
 * @returns {Promise<{ dead: boolean, session: any, reason: string, error: Error|null }>}
 */
export async function confirmSessionDead({ renew, delayMs = AUTH_RETRY_DELAY_MS, signal, sleep = (ms) => defaultSleep(ms, signal) }) {
  signal?.throwIfAborted();
  try {
    const session = await renew();
    return { dead: false, session, reason: "alive", error: null };
  } catch (error) {
    signal?.throwIfAborted();
    if (!isSessionUnauthorized(error)) {
      return { dead: false, session: null, reason: "not_auth", error };
    }
    await sleep(delayMs);
    try {
      const session = await renew();
      return { dead: false, session, reason: `transient_${error.status}`, error: null };
    } catch (retryError) {
      signal?.throwIfAborted();
      if (isSessionUnauthorized(retryError)) {
        return { dead: true, session: null, reason: `confirmed_${retryError.status}`, error: retryError };
      }
      return { dead: false, session: null, reason: "retry_inconclusive", error: retryError };
    }
  }
}

export function decideSessionDisable(errorText) {
  const text = typeof errorText === "string" ? errorText : JSON.stringify(errorText || "");
  if (!text.includes(SESSION_EXPIRED_BADGE)) {
    return { disable: false, reason: null, badge: null };
  }
  return { disable: true, reason: SESSION_EXPIRED_REASON, badge: SESSION_EXPIRED_BADGE };
}

function defaultSleep(ms, signal) {
  return sleep(ms, undefined, { signal });
}
