// Public executor defaults only; Rust never executes JavaScript at runtime.
import assert from 'node:assert/strict';
import { readFile, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import provider from '../../open-sse/providers/registry/codex.js';
import { CODEX_DEFAULT_INSTRUCTIONS } from '../../open-sse/config/codexInstructions.js';
const sources = {};
for (const name of ['open-sse/executors/codex.js', 'open-sse/config/codexInstructions.js', 'open-sse/providers/registry/codex.js']) {
  sources[name] = createHash('sha256').update(await readFile(new URL('../../' + name, import.meta.url))).digest('hex');
}
const content = JSON.stringify({ version: 1, sources, instructions: CODEX_DEFAULT_INSTRUCTIONS, transport: provider.transport, models: provider.models }, null, 2) + '\n';
const target = new URL('../codex-contract.json', import.meta.url);
if (process.argv.includes('--check')) assert.equal(await readFile(target, 'utf8'), content, 'Regenerate the Codex public contract');
else await writeFile(target, content);
console.log(`Codex public executor contract ${process.argv.includes('--check') ? 'verified' : 'written'}.`);
