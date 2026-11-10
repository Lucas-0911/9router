// Browser smoke against disposable fixtures, or a live Rust backend supplied via
// NINE_ROUTER_SMOKE_URL and NINE_ROUTER_SMOKE_TOKEN_FILE (never private user data).
import assert from "node:assert/strict";
import http from "node:http";
import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright-core";

const desktop = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const output = path.join(desktop, ".data/ui-smoke");
await fs.mkdir(output, { recursive: true });
const csp = JSON.parse(await fs.readFile(path.join(desktop, "tauri.conf.json"))).app.security.csp;
const mime = { ".js": "text/javascript", ".css": "text/css", ".json": "application/json", ".svg": "image/svg+xml", ".png": "image/png", ".woff2": "font/woff2", ".ttf": "font/ttf", ".html": "text/html" };
const liveBackend = process.env.NINE_ROUTER_SMOKE_URL;
const server = http.createServer(async (request, response) => {
  if (liveBackend && /^\/(api|v1)(?:\/|$)/.test(request.url)) {
    try {
      const headers = new Headers();
      for (const [key, value] of Object.entries(request.headers)) if (!["host", "origin", "referer", "connection", "content-length"].includes(key)) headers.set(key, value);
      const body = [];
      for await (const chunk of request) body.push(chunk);
      const upstream = await fetch(new URL(request.url, liveBackend), { method: request.method, headers, ...(body.length ? { body: Buffer.concat(body) } : {}) });
      response.writeHead(upstream.status, { "Content-Type": upstream.headers.get("Content-Type") || "application/json" });
      response.end(Buffer.from(await upstream.arrayBuffer()));
    } catch { response.writeHead(502); response.end(); }
    return;
  }
  const pathname = decodeURIComponent(new URL(request.url, "http://localhost").pathname);
  const safe = path.resolve(desktop, "ui", "." + pathname);
  if (safe !== path.join(desktop, "ui") && !safe.startsWith(path.join(desktop, "ui") + path.sep)) { response.writeHead(403); response.end(); return; }
  try {
    const filename = path.extname(safe) ? safe : path.join(desktop, "ui/index.html");
    const content = await fs.readFile(filename);
    response.writeHead(200, { "Content-Type": mime[path.extname(filename)] || "application/octet-stream", "Content-Security-Policy": csp });
    response.end(content);
  } catch { response.writeHead(404); response.end(); }
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const origin = `http://127.0.0.1:${server.address().port}`;
const backendUrl = liveBackend ? origin : "http://127.0.0.1:20128";
const adminToken = process.env.NINE_ROUTER_SMOKE_TOKEN_FILE ? (await fs.readFile(process.env.NINE_ROUTER_SMOKE_TOKEN_FILE, "utf8")).trim() : "disposable-browser-fixture-admin";
const fixture = !process.env.NINE_ROUTER_SMOKE_URL;
const connections = [{ id: "fixture-muse", provider: "muse-web", name: "Muse Web", authType: "cookie", isActive: true, testStatus: "active", priority: 1 }];
const empty = { connections: [], nodes: [], pools: [], models: [], ids: [], aliases: {}, combos: [], keys: [], providers: [], logs: [], entries: [], data: [], results: [], items: [], sessions: [], total: 0, running: false, installed: false };
let page;
let authenticated = true;
let webLoginStarted = false;
function fixtureBody(url, method = "GET") {
  switch (url.pathname) {
    case "/api/auth/status": return { authenticated: authenticated && !page.url().endsWith("/login"), requireLogin: true, hasPassword: true, authMode: "password" };
    case "/api/auth/login": authenticated = true; return { success: true };
    case "/api/auth/logout": authenticated = false; return { success: true };
    case "/api/machine-id": return { machineId: "disposable-fixture-machine" };
    case "/api/version": return { currentVersion: "0.5.116", latestVersion: "0.5.116", version: "0.5.116", hasUpdate: false };
    case "/api/settings": return { requireLogin: false, requireApiKey: true, hasPassword: true, rtkEnabled: true, cloudEnabled: false };
    case "/api/providers": return { connections };
    case "/api/keys": return { keys: [{ id: "fixture-key", name: "Default", key: "sk-disposable-fixture", isActive: true, createdAt: "2026-01-01T00:00:00Z" }] };
    case "/api/combos/muse-web": return { id: "muse-web", name: "muse-web", kind: "image", models: ["muse-web"] };
    case "/api/usage/chart":
    case "/api/usage/logs": return [];
    case "/api/combos": return { combos: [{ id: "muse-web", name: "muse-web", models: ["openai/gpt-4o"] }] };
    case "/api/providers/web-login":
      if (method === "POST") { webLoginStarted = true; return { status: "opening" }; }
      return webLoginStarted && url.searchParams.has("provider") ? { status: "success", email: "fixture@example.test" } : { providers: ["muse-web"], profiles: ["default"], available: true, status: "idle" };
    case "/api/oauth/claude/authorize": return { authUrl: "https://authorization.example.test/authorize", state: "fixture-oauth-state", codeVerifier: "fixture-verifier" };
    case "/api/oauth/callback-status": return { status: "done" };
    case "/api/usage/stats": return { totalRequests: 0, promptTokens: 0, completionTokens: 0, totalCost: 0, byProvider: {}, byModel: {}, byApiKey: {}, byCombo: {}, byConnection: {}, recentRequests: [], activeRequests: [], pending: {} };
    case "/api/usage/stream": return { activeRequests: [], recentRequests: [], pending: {} };
    case "/api/tunnel/status": return { enabled: false, running: false, tailscale: { enabled: false } };
    default: return empty;
  }
}
const browser = await chromium.launch({ executablePath: process.env.NINE_ROUTER_CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", headless: true });
try {
  const context = await browser.newContext({ viewport: { width: 1180, height: 780 } });
  await context.addInitScript(({ backendUrl, adminToken }) => {
    window.__9routerTestOpened = [];
    window.__TAURI__ = { core: { invoke: async (command, args) => command === "backend_config" ? { backendUrl, adminToken } : window.__9routerTestOpened.push(args?.url) } };
  }, { backendUrl, adminToken });
  const errors = [], failedRequests = [], missingAssets = [], calls = new Set();
  page = await context.newPage();
  page.on("pageerror", (error) => errors.push({ page: page.url(), error: error.stack || error.message }));
  page.on("console", (message) => { if (message.type() === "error" || message.type() === "warning") errors.push({ page: page.url(), error: message.text() }); });
  page.on("response", (response) => {
    if (response.status() >= 400) (response.url().includes("/api/") ? failedRequests : missingAssets).push({ page: page.url(), url: response.url(), status: response.status() });
  });
  if (fixture) await context.route("**/api/**", async (route) => {
    const url = new URL(route.request().url());
    calls.add(url.pathname);
    if (route.request().method() !== "OPTIONS") assert.equal(route.request().headers().authorization, `Bearer ${adminToken}`, "Management requests must use the native credential");
    const stream = url.pathname.endsWith("/stream");
    await route.fulfill({ status: 200, headers: { "access-control-allow-origin": origin, "access-control-allow-credentials": "true", "access-control-allow-headers": "authorization,content-type", "content-type": stream ? "text/event-stream" : "application/json" }, body: stream ? `data: ${JSON.stringify(fixtureBody(url))}\n\n` : JSON.stringify(fixtureBody(url, route.request().method())) });
  });
  await page.goto(origin + "/index.html");
  await page.waitForURL("**/dashboard", { timeout: 10000 });
  const manifest = JSON.parse(await fs.readFile(path.join(desktop, "ui/routes.json")));
  const routes = fixture ? manifest.map(({ route }) => route.replace("[toolId]", "claude").replace("[kind]", "image").replace("[id]", route.includes("combo/") ? "muse-web" : "muse-web")) : ["/dashboard/providers", "/dashboard/providers/muse-web", "/dashboard/endpoint", "/dashboard/profile", "/dashboard/combos"];
  const results = [];
  for (const pathname of routes) {
    await page.goto(origin + pathname);
    await page.waitForFunction(() => document.getElementById("root")?.innerText.trim().length > 20, null, { timeout: 10000 }).catch(async (error) => { console.log(JSON.stringify({ pathname, errors, failedRequests, missingAssets, body: await page.locator("body").innerText() })); await page.screenshot({ path: path.join(output, "failure.png") }); throw error; });
    await page.waitForTimeout(pathname.includes("translator") ? 1500 : 250);
    await page.evaluate(() => document.fonts.ready);
    const text = await page.locator("body").innerText();
    const failed = /Unable to load this page|Page not found|Invalid local backend configuration|Unable to connect to the local Rust backend/.test(text);
    results.push({ route: pathname, rendered: !failed, title: (await page.locator("h1").allTextContents()).join(" | "), textLength: text.length });
    if (pathname === "/dashboard/providers/muse-web") {
      await page.getByRole("button", { name: /Login via Browser/ }).click();
      await page.getByRole("button", { name: "Open Browser & Login", exact: true }).waitFor();
      await page.screenshot({ path: path.join(output, "muse-browser-login-dialog.png"), fullPage: true });
      if (fixture) {
        await page.getByRole("button", { name: "Open Browser & Login", exact: true }).click();
        await page.getByText(/Signed in as.*fixture@example.test/).waitFor({ timeout: 5000 });
      }
      await page.keyboard.press("Escape");
    }
    if (["/dashboard/providers", "/dashboard/providers/muse-web", "/dashboard/endpoint", "/login", "/dashboard/translator"].includes(pathname)) await page.screenshot({ path: path.join(output, pathname.replaceAll("/", "_") + ".png"), fullPage: true });
  }
  // Exercise original navigation and password-login interactions in fixture mode.
  if (fixture) {
    await page.goto(origin + "/dashboard/providers/claude");
    await page.getByRole("button", { name: /Add Connection/ }).click();
    await page.waitForResponse((response) => response.url().includes("/api/oauth/callback-status"), { timeout: 5000 });
    await page.waitForFunction(() => !document.querySelector(".fixed.inset-0.z-50.flex.items-center.justify-center"), null, { timeout: 5000 });
    assert.deepEqual(await page.evaluate(() => window.__9routerTestOpened), ["https://authorization.example.test/authorize"]);
    await page.screenshot({ path: path.join(output, "native-oauth-callback-success.png"), fullPage: true });
    await page.goto(origin + "/login");
    await page.getByPlaceholder("Enter password").fill("fixture-password");
    await page.getByRole("button", { name: "Login", exact: true }).click();
    await page.waitForURL("**/dashboard");
    await page.getByRole("link", { name: "dns Providers" }).first().click();
    await page.waitForURL("**/dashboard/providers");
  }
  const report = { fixture, routes: results, errors, failedRequests, missingAssets, apiPaths: [...calls].sort() };
  await fs.writeFile(path.join(output, fixture ? "fixture-report.json" : "native-report.json"), JSON.stringify(report, null, 2));
  console.log(JSON.stringify({ fixture, routes: results.length, rendered: results.filter(({ rendered }) => rendered).length, errors, failedRequests, missingAssets }, null, 2));
  assert.ok(results.every(({ rendered }) => rendered), "Every original route must render");
  assert.deepEqual(errors, [], "Browser console must be clean");
  assert.deepEqual(failedRequests, [], "Dashboard APIs must succeed");
  assert.deepEqual(missingAssets, [], "All local assets must resolve");
} finally { await browser.close(); await new Promise((resolve) => server.close(resolve)); }
