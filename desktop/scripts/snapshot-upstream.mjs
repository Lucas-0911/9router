import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFile, readdir, writeFile } from 'node:fs/promises';
import { basename, dirname, join, relative, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import registry from '../../open-sse/providers/registry/index.js';
import { normalizeModel } from '../../open-sse/providers/models/schema.js';
import { SCHEMA_VERSION, TABLES, buildCreateTableSql } from '../../src/lib/db/schema.js';
import { resolveProviderAlias, parseModel, getModelInfoCore } from '../../open-sse/services/model.js';

const root = fileURLToPath(new URL('../../', import.meta.url));
const output = join(root, 'desktop');
async function files(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  const groups = await Promise.all(entries.map(entry => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? files(path) : [path];
  }));
  return groups.flat().sort();
}
const sha = data => createHash('sha256').update(data).digest('hex');
const portable = path => path.split(sep).join('/');
const executorSource = await readFile(join(root, 'open-sse/executors/index.js'), 'utf8');
const specialExecutors = new Set([...executorSource.split('const executors = {')[1].split('\n};')[0]
  .matchAll(/^\s*["']?([\w-]+)["']?: new /gm)].map(match => match[1]));
const apiFiles = (await files(join(root, 'src/app/api'))).filter(p => basename(p) === 'route.js');
const routes = await Promise.all(apiFiles.map(async path => {
  const text = await readFile(path, 'utf8');
  return { source: portable(relative(root, path)), path: '/' + portable(relative(join(root, 'src/app'), dirname(path))),
    sha256: sha(text) };
}));
const providers = registry.map(provider => ({
  id: provider.id, alias: provider.alias ?? provider.id,
  name: provider.display?.name ?? provider.id, category: provider.category,
  serviceKinds: provider.serviceKinds ?? [],
  format: provider.transport ? provider.transport.format ?? 'openai' : null,
  transport: provider.transport ? {
    ...Object.fromEntries(['baseUrl', 'urlSuffix', 'headers', 'auth', 'forceStream']
      .filter(key => provider.transport[key] !== undefined).map(key => [key, provider.transport[key]])),
    executor: specialExecutors.has(provider.id) ? 'specialized' : 'default',
  } : null,
  models: (provider.models ?? []).map(normalizeModel),
})).sort((a, b) => a.id.localeCompare(b.id, 'en'));
assert.equal(new Set(providers.map(p => p.id)).size, providers.length, 'Duplicate provider IDs');
const tables = Object.keys(TABLES).sort();
const providerAliases = Object.fromEntries([...new Set(['el', 'jina', 'polly', 'aws-polly', 'jina-ai',
  ...registry.flatMap(p => [p.id, p.alias, ...(p.aliases ?? [])]).filter(Boolean)])]
  .sort().map(alias => [alias, resolveProviderAlias(alias)]));
const schema = Object.entries(TABLES).flatMap(([name, definition]) =>
  [buildCreateTableSql(name, definition), ...(definition.indexes ?? [])]).join(';\n') + ';\n';
const pages = (await files(join(root, 'src/app'))).filter(p => /^page\.[jt]sx?$/.test(basename(p)))
  .map(p => portable(relative(root, p)));
const manifest = {
  upstream: 'https://github.com/decolua/9router',
  revision: execFileSync('git', ['rev-parse', 'upstream/master'], { cwd: root, encoding: 'utf8' }).trim(),
  schemaVersion: SCHEMA_VERSION, schemaSha256: sha(schema), tables,
  columns: Object.fromEntries(Object.entries(TABLES).map(([name, definition]) => [name, Object.keys(definition.columns)])),
  providerAliases,
  routes, pages, providers,
};
const modelInputs = [...new Set([null, '', '/nested/model', 'unknown/vendor/model',
  'grok-build', 'coding', 'object-alias', 'invalid-alias', 'codex-auto-review',
  'gpt-6-preview', 'gpt-5.6-terra', 'gpt-daybreak-test', 'gpt-reserve-test', 'o1-test',
  'o3-test', 'o4-test', 'DEEPSEEK-Test', 'us.anthropic.claude-sonnet-4-6',
  'global.amazon.nova', 'eu.meta.llama', 'ap.mistral.model', 'us.xai.grok',
  ...Object.keys(providerAliases).map(alias => alias + '/vendor/model'),
  ...providers.flatMap(provider => provider.models.map(model => model.id)),
])];
const aliases = { coding: 'openai/gpt-5.5', 'object-alias': { provider: 'hf', model: 'vendor/model' },
  'invalid-alias': { unrelated: true } };
const modelCases = await Promise.all(modelInputs.map(async input => ({ input,
  parsed: parseModel(input), resolved: await getModelInfoCore(input, aliases) })));
for (const [name, content] of [['upstream-contract.json', JSON.stringify(manifest, null, 2) + '\n'],
                              ['upstream-schema.sql', schema],
                              ['upstream-model-cases.json', JSON.stringify({ aliases, cases: modelCases }, null, 2) + '\n']]) {
  const destination = join(output, name);
  if (process.argv.includes('--check')) assert.equal(await readFile(destination, 'utf8'), content,
    `${name} changed; review the upstream contract before regenerating`);
  else await writeFile(destination, content, 'utf8');
}
console.log(`${process.argv.includes('--check') ? 'Verified' : 'Captured'}: ` +
  `${routes.length} API routes, ${pages.length} pages, ${providers.length} providers, ${tables.length} tables, ${modelCases.length} model cases.`);
