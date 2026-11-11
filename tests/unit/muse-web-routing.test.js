import { beforeEach, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  execute: vi.fn(),
  generateMedia: vi.fn(),
  markAccountUnavailable: vi.fn(async () => ({ shouldFallback: true, cooldownMs: 0 })),
  clearAccountError: vi.fn(),
}));
vi.mock("../../open-sse/executors/index.js", () => ({
  getExecutor: () => ({ generateMedia: mocks.generateMedia, execute: mocks.execute }),
}));
vi.mock("../../src/sse/services/auth.js", () => ({
  getProviderCredentials: async (_provider, excluded) => {
    const connectionId = ["a", "b"].find((id) => !excluded?.has(id));
    return connectionId ? { connectionId, apiKey: "hatch_sess=placeholder" } : null;
  },
  markAccountUnavailable: mocks.markAccountUnavailable,
  clearAccountError: mocks.clearAccountError,
  extractApiKey: () => null,
  isValidApiKey: async () => true,
}));
vi.mock("@/lib/localDb", () => ({
  getSettings: async () => ({ requireApiKey: false }),
  getProviderConnectionById: vi.fn(),
}));
vi.mock("../../src/sse/services/model.js", () => ({
  getModelInfo: async () => ({ provider: "muse-web", model: "muse-image" }),
  getComboModels: async () => null,
}));
vi.mock("../../src/sse/services/keyAccess.js", () => ({
  getKeyAccessContext: async () => null,
  enforceKeyAccess: async () => null,
  enforceKeyAccessResolved: async () => null,
}));
vi.mock("../../src/sse/services/tokenRefresh.js", () => ({
  updateProviderCredentials: vi.fn(),
  checkAndRefreshToken: async (_provider, credentials) => credentials,
}));
vi.mock("@/lib/usageDb.js", () => ({
  saveRequestUsage: vi.fn(async () => {}), trackPendingRequest: vi.fn(),
  appendRequestLog: vi.fn(async () => {}), saveRequestDetail: vi.fn(async () => {}),
}));
vi.mock("../../open-sse/utils/requestLogger.js", () => ({
  createRequestLogger: async () => ({
    logClientRawRequest: vi.fn(), logRawRequest: vi.fn(), logTargetRequest: vi.fn(), logError: vi.fn(),
    logProviderResponse: vi.fn(), logConvertedResponse: vi.fn(),
  }),
}));

import { handleImageGeneration } from "../../src/sse/handlers/imageGeneration.js";
import { handleVideoCreate } from "../../src/sse/handlers/videoGeneration.js";
import { handleChatCore } from "../../open-sse/handlers/chatCore.js";
import { MUSE_WEB_BUSY } from "../../open-sse/config/museWeb.js";

const media = [{ b64: "aW1hZ2U=", mime: "image/png" }];
const busy = () => Object.assign(new Error(MUSE_WEB_BUSY), { status: 429 });
const makeRequest = (kind, signal) => new Request(`http://localhost/v1/${kind}/generations`, {
  method: "POST", headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ model: `muse-web/muse-${kind === "images" ? "image" : "video"}`, prompt: "draw" }),
  signal,
});
beforeEach(() => {
  vi.clearAllMocks();
  mocks.generateMedia.mockReset();
});

it.each(["images", "videos"])("routes %s around a busy account and forwards client cancellation", async (kind) => {
  mocks.generateMedia.mockRejectedValueOnce(busy()).mockResolvedValueOnce(media);
  const request = makeRequest(kind);
  const result = kind === "images"
    ? await handleImageGeneration(request)
    : await handleVideoCreate(request, "generations");
  expect(result.status).toBe(200);
  expect(mocks.generateMedia.mock.calls.map(([args]) => args.credentials.connectionId)).toEqual(["a", "b"]);
  expect(mocks.generateMedia.mock.calls[1][0].signal).toBe(request.signal);
  expect(mocks.markAccountUnavailable).toHaveBeenCalledTimes(1);
});

it.each(["images", "videos"])("returns 429 when every %s account is busy", async (kind) => {
  mocks.generateMedia.mockRejectedValue(busy());
  const request = makeRequest(kind);
  const result = kind === "images"
    ? await handleImageGeneration(request)
    : await handleVideoCreate(request, "generations");
  expect(result.status).toBe(429);
  expect(mocks.generateMedia).toHaveBeenCalledTimes(2);
});

it.each(["images", "videos"])("does not rotate %s accounts after cancellation", async (kind) => {
  const controller = new AbortController();
  mocks.generateMedia.mockImplementationOnce(async () => {
    controller.abort();
    throw Object.assign(new Error("Request aborted"), { status: 499 });
  });
  const request = makeRequest(kind, controller.signal);
  const result = kind === "images"
    ? await handleImageGeneration(request)
    : await handleVideoCreate(request, "generations");
  expect(result.status).toBe(499);
  expect(mocks.markAccountUnavailable).not.toHaveBeenCalled();
  expect(mocks.generateMedia).toHaveBeenCalledTimes(1);
});

it("forwards client cancellation through chatCore to the executor during setup", async () => {
  const controller = new AbortController();
  mocks.execute.mockImplementationOnce(async ({ signal }) => {
    controller.abort();
    signal.throwIfAborted();
  });
  const result = await handleChatCore({
    body: { model: "muse-chat", stream: false, messages: [{ role: "user", content: "hi" }] },
    modelInfo: { provider: "muse-web", model: "muse-chat" },
    credentials: { apiKey: "hatch_sess=placeholder" },
    signal: controller.signal,
    log: { info: vi.fn(), debug: vi.fn(), warn: vi.fn() },
  });
  expect(result.status).toBe(499);
  expect(mocks.execute.mock.calls[0][0].signal.aborted).toBe(true);
});

it.each([
  [false, "data: [DONE]\n\n"],
  [true, "data: [DONE]\n\n"],
  [undefined, "data: [DONE]\n\n"],
  [true, "data:[DONE]\r\n\r\n"],
  [true, "data: [DONE]"],
  [true, "data: [DONE]\n\ndata: [DONE]"],
  [true, ""],
])("honors stream=%s with terminator %j for Muse chat responses", async (stream, ending) => {
  const chunks = [
    { choices: [{ index: 0, delta: { content: "hello" }, finish_reason: null }] },
    { choices: [{ index: 0, delta: { content: " back" }, finish_reason: null }] },
    { choices: [{ index: 0, delta: {}, finish_reason: "stop" }], usage: { prompt_tokens: 12, completion_tokens: 3, total_tokens: 15 } },
  ];
  const raw = chunks.map((chunk) => `data: ${JSON.stringify(chunk)}\n\n`).join("");
  const upstream = new ReadableStream({
    start(controller) {
      for (const part of [raw, ending.slice(0, 8), ending.slice(8)]) {
        controller.enqueue(new TextEncoder().encode(part));
      }
      controller.close();
    },
  });
  mocks.execute.mockResolvedValueOnce({
    response: new Response(upstream, { headers: { "Content-Type": "text/event-stream" } }),
  });
  const result = await handleChatCore({
    body: { model: "muse-spark", ...(stream === undefined ? {} : { stream }), messages: [{ role: "user", content: "hi" }] },
    modelInfo: { provider: "muse-web", model: "muse-spark" },
    credentials: { apiKey: "hatch_sess=placeholder" },
  });
  expect(result.success).toBe(true);
  expect(mocks.execute.mock.calls[0][0].stream).toBe(stream !== false);
  if (stream === false) {
    expect(result.response.headers.get("content-type")).toContain("application/json");
    expect(await result.response.json()).toMatchObject({
      object: "chat.completion",
      choices: [{ message: { role: "assistant", content: "hello back" }, finish_reason: "stop" }],
      usage: { completion_tokens: 3 },
    });
  } else {
    expect(result.response.headers.get("content-type")).toContain("text/event-stream");
    const text = await result.response.text();
    expect(text.match(/data:\s*\[DONE\]/g)).toHaveLength(1);
    expect(text).toMatch(/data:\s*\[DONE\]\r?\n\r?\n$/);
    const received = text.split("\n").filter((line) => line.startsWith("data: {")).map((line) => JSON.parse(line.slice(6)));
    expect(received.map((chunk) => chunk.choices[0].delta.content || "").join("")).toBe("hello back");
    expect(received.at(-1).choices[0].finish_reason).toBe("stop");
  }
});
