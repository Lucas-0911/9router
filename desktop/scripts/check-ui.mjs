// Run after npm --prefix desktop run build: node desktop/scripts/check-ui.mjs
import assert from "node:assert/strict";
import { readFile, readdir, access } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { connectBackend, AuthenticatedEventSource } from "../frontend/backend.js";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const read = (name) => readFile(path.join(root, name), "utf8");
async function originalPages(directory) {
  const pages = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    if (entry.name === "api") continue;
    const filename = path.join(directory, entry.name);
    if (entry.isDirectory()) pages.push(...await originalPages(filename));
    else if (entry.name === "page.js") pages.push(path.relative(root, filename).split(path.sep).join("/"));
  }
  return pages;
}
const manifest = JSON.parse(await read("desktop/ui/routes.json"));
assert.deepEqual(manifest.map(({ source }) => source).sort(), (await originalPages(path.join(root, "src/app"))).sort(), "Every original page must be included");
assert.equal(manifest.length, 28);
const routes = await read("desktop/frontend/routes.js");
for (const { route, source } of manifest) {
  assert.ok(routes.includes(JSON.stringify(route)), `Missing route ${route}`);
  if (!["/dashboard", "/dashboard/endpoint", "/dashboard/cli-tools", "/dashboard/cli-tools/[toolId]"].includes(route)) assert.ok(routes.includes("@/" + source.slice(4)), `Must import original screen: ${source}`);
}
const app = await read("desktop/frontend/app.js");
assert.ok(app.includes("@/shared/components/layouts/DashboardLayout"));
assert.ok(app.includes("@/i18n/RuntimeI18nProvider"));
assert.ok((await read("src/app/(dashboard)/dashboard/providers/[id]/page.js")).includes("WebLoginButton"));
assert.ok((await read("src/shared/components/OAuthModal.js")).includes("data.completed && data.state === authData.state"));
for (const file of ["desktop/ui/index.html", "desktop/ui/main.js", "desktop/ui/vendors.js", "desktop/ui/styles.css", "desktop/ui/fonts/inter.ttf", "desktop/ui/material-symbols-outlined.woff2", "desktop/ui/monaco/vs/loader.js"]) await access(path.join(root, file));
const bundle = await read("desktop/ui/main.js");
assert.doesNotMatch(bundle, /cdn\.jsdelivr\.net\/npm\/monaco/);
assert.doesNotMatch(await read("desktop/frontend/backend.js"), /localStorage|sessionStorage/);

// Exercise the real native bridge, including the credential boundary and SSE API.
const originalFetch = globalThis.fetch;
const sent = [], opened = [];
const origin = "http://tauri.localhost";
globalThis.window = globalThis;
globalThis.location = { origin, href: origin + "/dashboard" };
globalThis.document = { addEventListener() {} };
globalThis.open = () => null;
globalThis.__TAURI__ = { core: { invoke: async (command, args) => command === "backend_config" ? { backendUrl: "http://127.0.0.1:20128", adminToken: "fixture-admin-token" } : opened.push(args.url) } };
globalThis.fetch = async (input, options) => {
  sent.push({ url: String(input), headers: new Headers(options?.headers) });
  return new Response('{}', { headers: { "Content-Type": "application/json" } });
};
await connectBackend();
await fetch("/api/providers");
assert.equal(sent.at(-1).url, "http://127.0.0.1:20128/api/providers");
assert.equal(sent.at(-1).headers.get("Authorization"), "Bearer fixture-admin-token");
await fetch("https://provider.example/api/models");
assert.equal(sent.at(-1).headers.get("Authorization"), null);
await fetch("/i18n/literals/en.json");
assert.equal(sent.at(-1).headers.get("Authorization"), null);
await fetch("/v1/models", { headers: { Authorization: "Bearer client-key" } });
assert.equal(sent.at(-1).headers.get("Authorization"), "Bearer client-key");
window.open("https://provider.example/authorize", "_blank");
await Promise.resolve();
assert.deepEqual(opened, ["https://provider.example/authorize"]);
assert.equal(window.open("javascript:alert(1)"), null);
globalThis.fetch = async () => new Response(new ReadableStream({ start(controller) {
  controller.enqueue(new TextEncoder().encode('id: 42\nevent: usage\ndata: {"ok":\ndata: true}\n\n'));
} }), { headers: { "Content-Type": "text/event-stream" } });
await new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error("SSE event was not delivered")), 1000);
  const stream = new AuthenticatedEventSource("/api/usage/stream");
  stream.addEventListener("usage", (event) => {
    assert.deepEqual(JSON.parse(event.data), { ok: true });
    assert.equal(event.lastEventId, "42");
    stream.close(); clearTimeout(timer); resolve();
  });
});
globalThis.fetch = originalFetch;
console.log("Desktop UI checks passed: all 28 original screens, native credential boundary, local assets and authenticated SSE.");
