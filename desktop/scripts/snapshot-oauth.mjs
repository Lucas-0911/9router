// Build-time snapshot of public upstream client descriptors; Rust never starts JavaScript.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import registry from '../../open-sse/providers/registry/index.js';
const root = new URL('../../', import.meta.url);
async function constants(path) {
  const source = (await readFile(new URL(path, root), 'utf8')).replace(/from "(open-sse\/[^"]+)"/g,
    (_, name) => `from ${JSON.stringify(new URL(name, root).href)}`);
  return import('data:text/javascript;base64,' + Buffer.from(source).toString('base64'));
}
const c = await constants('src/lib/oauth/constants/oauth.js');
const x = await constants('src/lib/oauth/constants/xai.js');
const overrides = { claude:c.CLAUDE_CONFIG, codex:c.CODEX_CONFIG, 'gemini-cli':c.GEMINI_CONFIG,
  antigravity:c.ANTIGRAVITY_CONFIG, github:c.GITHUB_CONFIG, iflow:c.IFLOW_CONFIG,
  kimi:{...c.KIMI_CONFIG,clientId:registry.find(p=>p.id==='kimi').oauth.clientId},
  nous:c.NOUS_CONFIG, xai:x.XAI_CONFIG, trae:c.TRAE_CONFIG, windsurf:c.WINDSURF_CONFIG,
  zed:c.ZED_HOSTED_CONFIG, 'xiaomi-mimo':c.XIAOMI_MIMO_CONFIG };
const providers = {}, sources = {};
for (const p of registry) {
  let source;
  try { source = await readFile(new URL(`src/lib/oauth/providers/${p.id}.js`, root), 'utf8'); }
  catch { continue; }
  sources[p.id] = createHash('sha256').update(source).digest('hex');
  const flowType = source.match(/flowType:\s*"([^"]+)"/)?.[1] ||
    (['qoder','qoder-cn','minimax-code','minimax-code-global'].includes(p.id)?'device_code':null);
  const config = { ...(overrides[p.id] || p.oauth || {}) };
  // Rust computes OS/architecture metadata at runtime; it is not public static configuration.
  delete config.loadCodeAssistClientMetadata;
  providers[p.id] = { flowType, config };
}
providers['minimax-code'].config = {clientId:'mcode-public',scope:'agent.default',audience:'agent-backend',
  deviceCodeUrl:'https://account.minimax.cn/oauth2/device/code',tokenUrl:'https://account.minimax.cn/oauth2/token'};
providers['minimax-code-global'].config = {...providers['minimax-code'].config,
  deviceCodeUrl:'https://account.minimax.io/oauth2/device/code',tokenUrl:'https://account.minimax.io/oauth2/token'};
const content = JSON.stringify({version:1,sources,providers}, null, 2)+'\n';
const output = new URL('../oauth-contract.json', import.meta.url);
if(process.argv.includes('--check')) assert.equal(await readFile(output,'utf8'),content,'Upstream OAuth contract changed');
else await writeFile(output,content);
console.log(`${process.argv.includes('--check')?'Verified':'Captured'} ${Object.keys(providers).length} public OAuth descriptors at ${fileURLToPath(output)}`);
