import { afterEach, expect, it, vi } from "vitest";
import { renewSession, MuseWebAuthError } from "../../open-sse/executors/muse-web/session.js";
import { confirmSessionDead } from "../../open-sse/executors/muse-web/sessionGuard.js";

afterEach(() => vi.unstubAllGlobals());

function pendingFetch(_url, { signal }) {
  return new Promise((_resolve, reject) => {
    signal.addEventListener("abort", () => reject(signal.reason), { once: true });
  });
}

it("bounds the session HTTP request even when a client signal is supplied", async () => {
  vi.stubGlobal("fetch", vi.fn(pendingFetch));
  await expect(renewSession("hatch_sess=test", {
    signal: new AbortController().signal, timeoutMs: 10,
  })).rejects.toMatchObject({ name: "TimeoutError" });
});

it("bounds the wake request using the same deadline", async () => {
  vi.stubGlobal("fetch", vi.fn()
    .mockResolvedValueOnce(Response.json({ vm_id: "vm", vm_state: "RUNNING" }))
    .mockImplementationOnce(pendingFetch));
  await expect(renewSession("hatch_sess=test", { timeoutMs: 10 })).rejects.toMatchObject({ name: "TimeoutError" });
});

it("cancels the 401 confirmation delay without another attempt", async () => {
  const controller = new AbortController();
  const renew = vi.fn().mockRejectedValue(new MuseWebAuthError("401"));
  const pending = confirmSessionDead({ renew, signal: controller.signal });
  await Promise.resolve();
  controller.abort();
  await expect(pending).rejects.toMatchObject({ name: "AbortError" });
  expect(renew).toHaveBeenCalledTimes(1);
});

it("treats HTTP 403 and a login redirect as a dead session", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => new Response("", { status: 403 })));
  await expect(renewSession("hatch_sess=test")).rejects.toMatchObject({ name: "MuseWebAuthError", status: 403 });
  vi.stubGlobal("fetch", vi.fn(async () => new Response("", { status: 302, headers: { Location: "https://muse.ai/login" } })));
  await expect(renewSession("hatch_sess=test")).rejects.toMatchObject({ name: "MuseWebAuthError", status: 302 });
});

it("rejects a signed-out session and a cleared hatch_sess cookie", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ authenticated: false })));
  await expect(renewSession("hatch_sess=test")).rejects.toMatchObject({ name: "MuseWebAuthError" });
  const headers = new Headers({ "Content-Type": "application/json" });
  headers.append("Set-Cookie", "hatch_sess=; Max-Age=0; Path=/");
  vi.stubGlobal("fetch", vi.fn(async () => new Response("{}", { status: 200, headers })));
  await expect(renewSession("hatch_sess=old")).rejects.toMatchObject({ name: "MuseWebAuthError" });
});

it("does not call muse.ai when hatch_sess is missing", async () => {
  const fetchMock = vi.fn();
  vi.stubGlobal("fetch", fetchMock);
  await expect(renewSession("hatch_gw=only")).rejects.toMatchObject({ name: "MuseWebAuthError" });
  expect(fetchMock).not.toHaveBeenCalled();
});

it("keeps the account when the session endpoint returns HTTP 500", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => new Response("down", { status: 500 })));
  await expect(renewSession("hatch_sess=test")).rejects.toMatchObject({ name: "MuseWebUpstreamError" });
});

it("keeps a hatch_vml cookie that is cleared and replaced in the same response", async () => {
  const headers = new Headers({ "Content-Type": "application/json" });
  headers.append("Set-Cookie", "hatch_vml=; Max-Age=0; Path=/");
  headers.append("Set-Cookie", "hatch_vml=rotated; Max-Age=172800; Path=/; Domain=.muse.ai");
  vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify({ status: "assigned" }), { headers })));
  const result = await renewSession("hatch_sess=keep; hatch_vml=old");
  expect(result.cookieHeader).toContain("hatch_sess=keep");
  expect(result.cookieHeader).toContain("hatch_vml=rotated");
});

it("keeps rotated cookies and removes cookies the upstream expires", async () => {
  const headers = new Headers({ "Content-Type": "application/json" });
  headers.append("Set-Cookie", "hatch_sess=next; Path=/");
  headers.append("Set-Cookie", "hatch_gw=; Max-Age=0; Path=/");
  vi.stubGlobal("fetch", vi.fn(async () => new Response("{}", { headers })));
  const result = await renewSession("hatch_sess=old; hatch_gw=old");
  expect(result.cookieHeader).toBe("hatch_sess=next");
});
