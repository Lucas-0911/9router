import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const pool = vi.hoisted(() => ({
  acquireBrowserContext: vi.fn(),
  releaseBrowserContext: vi.fn(),
  retainBrowserContext: vi.fn(),
}));
vi.mock("../../open-sse/services/browserPool.js", () => pool);
vi.mock("../../open-sse/executors/browserExecutableCheck.js", () => ({
  resolveSystemChromeExecutable: () => null,
  isMissingBrowserExecutable: () => false,
}));
vi.mock("node:timers/promises", () => ({
  setTimeout: (ms, value, { signal } = {}) => new Promise((resolve, reject) => {
    signal?.throwIfAborted();
    const abort = () => {
      clearTimeout(timer);
      reject(signal.reason);
    };
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", abort);
      resolve(value);
    }, ms);
    signal?.addEventListener("abort", abort, { once: true });
  }),
}));

import { MuseWebExecutor } from "../../open-sse/executors/muse-web.js";
import { MUSE_WEB_TIMEOUTS, SESSION_EXPIRED_BADGE } from "../../open-sse/config/museWeb.js";
import * as dom from "../../open-sse/executors/muse-web/dom.js";

let pages, context, release;
const credentials = (id = "account-1") => ({ connectionId: id, apiKey: "hatch_sess=original" });
const request = (overrides = {}) => ({
  credentials: credentials(),
  body: { messages: [{ role: "user", content: "hello" }] },
  stream: false,
  ...overrides,
});

function makePage() {
  let sentAt = null;
  let closed = false;
  const page = {
    goto: vi.fn(async () => { sentAt = null; }),
    url: () => "https://muse.ai/thread/new",
    close: vi.fn(async () => { closed = true; }),
    isClosed: () => closed,
    context: () => context,
    keyboard: { press: vi.fn() },
    reply: () => ({ lastText: "hello back", generating: false }),
    evaluate: vi.fn(async (script) => {
      if (closed) throw new Error("page closed");
      if (script === dom.PAGE_STATE) return { ready: true, path: "/thread/new" };
      if (script === dom.CLICK_SEND) { sentAt = Date.now(); return "clicked"; }
      if (script === dom.CHAT_STATE || script === dom.MEDIA_STATE) {
        return sentAt === null
          ? { agentCount: 0, lastText: "", generating: false, attachments: [] }
          : { agentCount: 1, attachments: [], ...page.reply(Date.now() - sentAt) };
      }
      return { ok: true };
    }),
  };
  return page;
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  pages = [];
  release = vi.fn();
  context = {
    addCookies: vi.fn(async () => {}),
    cookies: vi.fn(async () => [{ name: "hatch_sess", value: "rotated" }]),
    once: vi.fn(),
    newPage: vi.fn(async () => {
      const page = makePage();
      pages.push(page);
      return page;
    }),
  };
  pool.acquireBrowserContext.mockResolvedValue({ context });
  pool.retainBrowserContext.mockReturnValue(release);
  vi.stubGlobal("fetch", vi.fn(async () => new Response("{}", {
    headers: { "Content-Type": "application/json", "Set-Cookie": "hatch_sess=rotated; Path=/" },
  })));
});
afterEach(() => {
  for (const [event, callback] of context.once.mock.calls) {
    if (event === "close") callback();
  }
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("Muse Web request lifetime", () => {
  it("reuses one fresh thread for the next request and retains the context", async () => {
    const executor = new MuseWebExecutor();
    const first = executor.execute(request());
    await vi.advanceTimersByTimeAsync(2000);
    expect((await first).response.status).toBe(200);
    const second = executor.execute(request());
    await vi.advanceTimersByTimeAsync(2000);
    expect((await second).response.status).toBe(200);
    expect(pages).toHaveLength(1);
    expect(pages[0].close).not.toHaveBeenCalled();
    expect(pages[0].goto).toHaveBeenCalledTimes(3);
    expect(release).toHaveBeenCalledTimes(1);
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(context.addCookies.mock.calls[1][0]).toContainEqual(expect.objectContaining({
      name: "hatch_sess", value: "rotated",
    }));
  });

  it("leaves VM wake to the web client instead of making a duplicate HTTP request", async () => {
    fetch.mockResolvedValueOnce(Response.json({ status: "assigned", vm_id: "vm", vm_state: "RUNNING" }));
    const pending = new MuseWebExecutor().execute(request());
    await vi.advanceTimersByTimeAsync(2000);
    expect((await pending).response.status).toBe(200);
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it("revalidates the session after one minute without extending the deadline on reuse", async () => {
    const executor = new MuseWebExecutor();
    const first = executor.execute(request());
    await vi.advanceTimersByTimeAsync(2000);
    await first;
    await vi.advanceTimersByTimeAsync(20000);
    const second = executor.execute(request());
    await vi.advanceTimersByTimeAsync(2000);
    await second;
    expect(fetch).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(40000);
    const third = executor.execute(request());
    await vi.advanceTimersByTimeAsync(2000);
    expect((await third).response.status).toBe(200);
    expect(fetch).toHaveBeenCalledTimes(2);
    expect(fetch.mock.calls[1][1].headers.Cookie).toContain("hatch_sess=rotated");
  });

  it("confirms an expired cached session with two 401s without waiting for page timeout", async () => {
    const executor = new MuseWebExecutor();
    const first = executor.execute(request());
    await vi.advanceTimersByTimeAsync(2000);
    await first;
    fetch.mockImplementation(async () => new Response("", { status: 401 }));
    const parked = pages[0].evaluate.getMockImplementation();
    pages[0].evaluate.mockImplementation(async (script) => script === dom.PAGE_STATE
      ? { ready: false, hasInput: false, head: "Sign in to continue" }
      : parked(script));
    context.newPage.mockImplementationOnce(async () => {
      const page = makePage();
      const evaluate = page.evaluate.getMockImplementation();
      page.evaluate.mockImplementation(async (script) => script === dom.PAGE_STATE
        ? { ready: false, hasInput: false, head: "Sign in to continue" }
        : evaluate(script));
      pages.push(page);
      return page;
    });
    const controller = new AbortController();
    let result;
    const pending = executor.execute(request({ signal: controller.signal })).then((value) => { result = value; });
    try {
      await vi.advanceTimersByTimeAsync(4000);
      expect(result?.response.status).toBe(401);
      expect(await result.response.text()).toContain(SESSION_EXPIRED_BADGE);
      expect(fetch).toHaveBeenCalledTimes(3);
      expect(pages[1].close).toHaveBeenCalledTimes(1);
    } finally {
      controller.abort();
      await vi.advanceTimersByTimeAsync(1);
      await pending;
    }
  });

  it("recovers from a login redirect when renewing the cached session succeeds", async () => {
    const executor = new MuseWebExecutor();
    const first = executor.execute(request());
    await vi.advanceTimersByTimeAsync(2000);
    await first;
    const parked = pages[0].evaluate.getMockImplementation();
    pages[0].evaluate.mockImplementation(async (script) => script === dom.PAGE_STATE
      ? { ready: false, hasInput: false, head: "Sign in to continue" }
      : parked(script));
    context.newPage.mockImplementationOnce(async () => {
      const page = makePage();
      const evaluate = page.evaluate.getMockImplementation();
      page.evaluate.mockImplementation(async (script) => script === dom.PAGE_STATE
        ? { ready: false, hasInput: false, head: "Sign in to continue" }
        : evaluate(script));
      pages.push(page);
      return page;
    });
    const second = executor.execute(request());
    await vi.advanceTimersByTimeAsync(2000);
    expect((await second).response.status).toBe(200);
    expect(fetch).toHaveBeenCalledTimes(2);
    expect(pages).toHaveLength(3);
    expect(pages[0].close).toHaveBeenCalledTimes(1);
    expect(pages[1].close).toHaveBeenCalledTimes(1);
    expect(pages[2].close).not.toHaveBeenCalled();
  });

  it("disables the account when the browser has lost hatch_sess", async () => {
    const executor = new MuseWebExecutor();
    const first = executor.execute(request());
    await vi.advanceTimersByTimeAsync(2000);
    await first;
    context.cookies.mockResolvedValueOnce([{ name: "hatch_gw", value: "other" }]);
    const pending = executor.execute(request());
    await vi.advanceTimersByTimeAsync(4000);
    const second = await pending;
    expect(second.response.status).toBe(401);
    expect(await second.response.text()).toContain(SESSION_EXPIRED_BADGE);
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it("rejects a busy account without opening another page, but permits another account", async () => {
    const executor = new MuseWebExecutor();
    const first = executor.execute(request());
    await vi.advanceTimersByTimeAsync(1);
    const busy = await executor.execute(request());
    expect(busy.response.status).toBe(429);
    expect(await busy.response.text()).toContain("MUSE_WEB_BUSY");
    const other = executor.execute(request({ credentials: credentials("account-2") }));
    await vi.advanceTimersByTimeAsync(2000);
    expect((await first).response.status).toBe(200);
    expect((await other).response.status).toBe(200);
    expect(pages).toHaveLength(2);
  });

  it("does not finish during a pause while the upstream is still generating", async () => {
    const result = new MuseWebExecutor().execute(request());
    await vi.advanceTimersByTimeAsync(1);
    pages[0].reply = (elapsed) => elapsed < 2500
      ? { lastText: "first", generating: true }
      : { lastText: "first and last", generating: false };
    await vi.advanceTimersByTimeAsync(4000);
    const body = await (await result).response.json();
    expect(body.choices[0].message.content).toBe("first and last");
  });

  it.each([false, true])("waits for a working agent past the first-token deadline (stream=%s)", async (stream) => {
    const readyAt = MUSE_WEB_TIMEOUTS.firstToken + 10000;
    context.newPage.mockImplementationOnce(async () => {
      const page = makePage();
      page.reply = (elapsed) => ({
        lastText: elapsed < readyAt ? "" : "Your video is ready",
        generating: elapsed < readyAt,
      });
      pages.push(page);
      return page;
    });
    const pending = new MuseWebExecutor().execute(request({ stream }));
    await vi.advanceTimersByTimeAsync(readyAt + 2000);
    const { response } = await pending;
    expect(response.status).toBe(200);
    const text = stream
      ? await response.text()
      : (await response.json()).choices[0].message.content;
    expect(text).toContain("Your video is ready");
    expect(text).not.toContain("no first token");
    expect(pages[0].close).not.toHaveBeenCalled();
  });

  it("finishes at the stability deadline without another full polling interval", async () => {
    let stoppedAt, completedAt;
    const pending = new MuseWebExecutor().execute(request()).then((result) => {
      completedAt = Date.now();
      return result;
    });
    await vi.advanceTimersByTimeAsync(1);
    pages[0].reply = (elapsed) => {
      const generating = elapsed < 300;
      if (!generating) stoppedAt ??= Date.now();
      return { lastText: "done", generating };
    };
    await vi.advanceTimersByTimeAsync(4000);
    expect((await pending).response.status).toBe(200);
    expect(completedAt - stoppedAt).toBe(MUSE_WEB_TIMEOUTS.textStable);
  });

  it("keeps id and created stable through the final chunk and isolates concurrent streams", async () => {
    context.newPage.mockImplementation(async () => {
      const page = makePage();
      page.reply = (elapsed) => elapsed < 1500
        ? { lastText: "hello", generating: true }
        : { lastText: "hello back", generating: false };
      pages.push(page);
      return page;
    });
    const executor = new MuseWebExecutor();
    const pending = Promise.all(["stream-1", "stream-2"].map((id) => executor.execute(request({
      stream: true, credentials: credentials(id),
    }))));
    await vi.advanceTimersByTimeAsync(4000);
    const ids = [];
    for (const { response } of await pending) {
      expect(response.status).toBe(200);
      const text = await response.text();
      expect(text.match(/data:\s*\[DONE\]/g)).toHaveLength(1);
      const chunks = text.split("\n")
        .filter((line) => line.startsWith("data: {"))
        .map((line) => JSON.parse(line.slice(6)));
      expect(chunks.length).toBeGreaterThanOrEqual(3);
      expect(chunks.map((chunk) => chunk.choices[0].delta.content || "").join("")).toBe("hello back");
      expect(chunks.at(-1).choices[0].finish_reason).toBe("stop");
      expect.soft(new Set(chunks.map((chunk) => chunk.id)).size).toBe(1);
      expect.soft(new Set(chunks.map((chunk) => chunk.created)).size).toBe(1);
      ids.push(chunks[0].id);
    }
    expect(new Set(ids).size).toBe(2);
  });

  it("closes the page and frees the slot when a stream is cancelled", async () => {
    const executor = new MuseWebExecutor();
    const pending = executor.execute(request({ stream: true }));
    await vi.advanceTimersByTimeAsync(1);
    const result = await pending;
    const reader = result.response.body.getReader();
    await reader.read();
    await reader.cancel();
    expect(pages[0].close).toHaveBeenCalledTimes(1);
    expect(release).toHaveBeenCalledTimes(1);
    const retry = executor.execute(request());
    await vi.advanceTimersByTimeAsync(2000);
    expect((await retry).response.status).toBe(200);
  });

  it("closes a page when opening the thread fails and releases the account", async () => {
    const executor = new MuseWebExecutor();
    context.newPage.mockImplementationOnce(async () => {
      const page = makePage();
      page.goto.mockRejectedValueOnce(new Error("navigation failed"));
      pages.push(page);
      return page;
    });
    expect((await executor.execute(request())).response.status).toBeGreaterThanOrEqual(400);
    expect(pages[0].close).toHaveBeenCalledTimes(1);
    const retry = executor.execute(request());
    await vi.advanceTimersByTimeAsync(2000);
    expect((await retry).response.status).toBe(200);
  });

  it("does not renew or open a page for an already cancelled request", async () => {
    const signal = AbortSignal.abort();
    const result = await new MuseWebExecutor().execute(request({ signal }));
    expect(result.response.status).toBe(499);
    expect(fetch).not.toHaveBeenCalled();
    expect(pages).toHaveLength(0);
  });

  it("holds the same account slot for chat and media and releases it on abort", async () => {
    const executor = new MuseWebExecutor();
    const controller = new AbortController();
    const pending = executor.generateMedia({
      prompt: "draw", n: 2, credentials: credentials(), signal: controller.signal,
    });
    const rejected = expect(pending).rejects.toMatchObject({ status: 499 });
    await vi.advanceTimersByTimeAsync(1);
    expect((await executor.execute(request())).response.status).toBe(429);
    controller.abort();
    await rejected;
    expect(pages).toHaveLength(1);
    expect(pages[0].close).toHaveBeenCalledTimes(1);
    const retry = executor.execute(request());
    await vi.advanceTimersByTimeAsync(2000);
    expect((await retry).response.status).toBe(200);
  });

  it("waits for an attachment that appears after the completion text", async () => {
    const pending = new MuseWebExecutor().generateMedia({
      prompt: "draw", credentials: credentials(),
    });
    await vi.advanceTimersByTimeAsync(1);
    const page = pages[0];
    const evaluate = page.evaluate.getMockImplementation();
    page.evaluate.mockImplementation(async (script) => {
      if (script === dom.MEDIA_STATE) {
        return {
          agentCount: 1, lastText: "done", generating: false,
          attachments: Date.now() >= readyAt ? [{ kind: "image", src: "blob:result", w: 10 }] : [],
        };
      }
      if (script.includes("const r = await fetch(")) return { ok: true, b64: "aW1hZ2U=", mime: "image/png" };
      return evaluate(script);
    });
    const readyAt = Date.now() + 3000;
    await vi.advanceTimersByTimeAsync(4000);
    expect(await pending).toEqual([{ b64: "aW1hZ2U=", mime: "image/png", width: 10 }]);
    expect(page.close).toHaveBeenCalledTimes(1);
  });

  it("starts a new context key when credentials are replaced", async () => {
    const executor = new MuseWebExecutor();
    const first = executor.execute(request());
    await vi.advanceTimersByTimeAsync(2000);
    await first;
    const second = executor.execute(request({
      credentials: { ...credentials(), apiKey: "hatch_sess=replacement" },
    }));
    await vi.advanceTimersByTimeAsync(2000);
    await second;
    expect(pool.acquireBrowserContext.mock.calls[0][0]).not.toBe(pool.acquireBrowserContext.mock.calls[1][0]);
    expect(fetch.mock.calls[1][1].headers.Cookie).toBe("hatch_sess=replacement");
  });

  it("removes cookies expired by HTTP renewal from the browser jar", async () => {
    fetch.mockResolvedValueOnce(new Response("{}", {
      headers: { "Content-Type": "application/json", "Set-Cookie": "hatch_gw=; Max-Age=0" },
    }));
    const pending = new MuseWebExecutor().execute(request({
      credentials: { ...credentials("cookie-delete"), apiKey: "hatch_sess=original; hatch_gw=old" },
    }));
    await vi.advanceTimersByTimeAsync(2000);
    expect((await pending).response.status).toBe(200);
    expect(context.addCookies).toHaveBeenCalledWith(expect.arrayContaining([
      expect.objectContaining({ name: "hatch_gw", expires: 1 }),
    ]));
  });

  it("reduces DOM polls during a slow first token without scanning media", async () => {
    const pending = new MuseWebExecutor().execute(request());
    await vi.advanceTimersByTimeAsync(1);
    pages[0].reply = (elapsed) => ({
      lastText: elapsed < 10000 ? "" : "done", generating: elapsed < 10000,
    });
    await vi.advanceTimersByTimeAsync(13000);
    expect((await pending).response.status).toBe(200);
    const scripts = pages[0].evaluate.mock.calls.map(([script]) => script);
    expect(scripts.filter((script) => script === dom.CHAT_STATE).length).toBeLessThan(35);
    expect(scripts).not.toContain(dom.MEDIA_STATE);
  });

  it.each([1, 2, 4, 8])("bounds tabs for a burst of %i clients using two accounts", async (count) => {
    const executor = new MuseWebExecutor();
    const requests = Array.from({ length: count }, (_, index) => executor.execute(request({
      credentials: credentials(`burst-${index % 2}`),
    })));
    await vi.advanceTimersByTimeAsync(2000);
    const results = await Promise.all(requests);
    expect(results.filter(({ response }) => response.status === 200)).toHaveLength(Math.min(count, 2));
    expect(results.filter(({ response }) => response.status === 429)).toHaveLength(Math.max(0, count - 2));
    expect(pages).toHaveLength(Math.min(count, 2));
    expect(pages.every((page) => page.close.mock.calls.length === 0)).toBe(true);
  });
});
