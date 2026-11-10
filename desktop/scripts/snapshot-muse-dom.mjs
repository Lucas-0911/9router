import { readFile, writeFile } from 'node:fs/promises';
import * as dom from '../../open-sse/executors/muse-web/dom.js';

// These expressions run inside the provider page; no JavaScript backend is shipped.
const snapshot = {
  ...Object.fromEntries(Object.entries(dom).filter(([, value]) => typeof value !== 'function')),
  fillInput: dom.fillInputScript('__9ROUTER_TEXT__'),
  attachFile: dom.attachFileScript('__9ROUTER_BASE64__', '__9ROUTER_MIME__', '__9ROUTER_FILENAME__'),
  fetchMedia: dom.fetchAsBase64Script('__9ROUTER_URL__'),
};
const output = new URL('../muse-dom.json', import.meta.url);
const text = JSON.stringify(snapshot, null, 2) + '\n';
if (process.argv.includes('--check')) {
  if (await readFile(output, 'utf8') !== text) throw new Error('Muse DOM snapshot differs from the original provider; regenerate it.');
} else {
  await writeFile(output, text);
}
