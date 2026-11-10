// Run with: node desktop/scripts/check-ui.mjs
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { parseAliases, parseModelLines, connectionPayload, settingsPayload, isRunnableProvider } from "../ui/app.mjs";

assert.deepEqual(parseModelLines("openai/gpt-4.1\n\nclaude/sonnet\nopenai/gpt-4.1\n"), ["openai/gpt-4.1", "claude/sonnet"]);
assert.throws(() => parseModelLines(""));
assert.throws(() => parseModelLines("openai/model extra"));
assert.deepEqual({ ...parseAliases("fast = openai/gpt-4.1-mini\nsmart=claude/sonnet") }, { fast: "openai/gpt-4.1-mini", smart: "claude/sonnet" });
assert.throws(() => parseAliases("broken"));
assert.throws(() => parseAliases("fast=x\nfast=y"));
assert.equal(Object.getPrototypeOf(parseAliases("__proto__=safe/model")), null);
const fields = { provider: "openai", name: " Personal ", apiKey: " test-key ", priority: "0", baseUrl: "https://api.example.com/v1" };
assert.deepEqual(connectionPayload(fields), { provider: "openai", name: "Personal", apiKey: "test-key", priority: 0, isActive: true, providerSpecificData: { baseUrl: fields.baseUrl } });
for (const baseUrl of ["file:///tmp/test", "https://user:pass@example.com/v1", "https://example.com/v1?key=secret", "https://example.com/v1#secret"]) {
  assert.throws(() => connectionPayload({ ...fields, baseUrl }));
}
assert.throws(() => connectionPayload({ ...fields, priority: "-1" }));
assert.deepEqual(settingsPayload('{"requireApiKey":true}'), { requireApiKey: true });
for (const value of ["[]", "null", "false", "{"]) assert.throws(() => settingsPayload(value));
const provider = { category: "freeTier", format: "gemini", serviceKinds: ["llm"], transport: { executor: "default" } };
assert.equal(isRunnableProvider(provider), true);
assert.equal(isRunnableProvider({ ...provider, category: "oauth" }), false);
assert.equal(isRunnableProvider({ ...provider, serviceKinds: [] }), false);
assert.equal(isRunnableProvider({ ...provider, transport: { executor: "specialized" } }), false);
assert.equal(isRunnableProvider({ ...provider, transport: { executor: "default", auth: { hooks: ["custom"] } } }), false);
const source = await readFile(new URL("../ui/app.mjs", import.meta.url), "utf8");
const html = await readFile(new URL("../ui/index.html", import.meta.url), "utf8");
for (const [, id] of source.matchAll(/\$\("([a-z-]+)"\)/g)) assert.ok(html.includes(`id="${id}"`), `Missing HTML ID: ${id}`);
assert.doesNotMatch(source, /innerHTML|localStorage|sessionStorage|console\./);
assert.doesNotMatch(html, /\son[a-z]+=/);
console.log("Desktop UI checks passed: validation, prototype safety, secret handling, DOM bindings.");
