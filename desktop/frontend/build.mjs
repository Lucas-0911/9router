import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import postcss from "postcss";
import tailwind from "@tailwindcss/postcss";

const require = createRequire(import.meta.url);
await require("next/dist/build/swc").loadBindings();
const { webpack } = require("next/dist/compiled/webpack/webpack");
const frontend = path.dirname(fileURLToPath(import.meta.url));
const desktop = path.dirname(frontend), root = path.dirname(desktop), output = path.join(desktop, "ui");
async function pages(directory) {
  const results = [];
  for (const entry of await fs.readdir(directory, { withFileTypes: true })) {
    if (entry.name === "api") continue;
    const file = path.join(directory, entry.name);
    if (entry.isDirectory()) results.push(...await pages(file));
    else if (entry.name === "page.js") results.push(file);
  }
  return results;
}
const manifest = (await pages(path.join(root, "src/app"))).map((file) => ({
  source: path.relative(root, file).split(path.sep).join("/"),
  route: "/" + path.relative(path.join(root, "src/app"), path.dirname(file)).split(path.sep).filter((segment) => segment && !segment.startsWith("(")).join("/"),
})).sort((a, b) => (a.route.match(/\[/g)?.length || 0) - (b.route.match(/\[/g)?.length || 0) || b.route.length - a.route.length);
const serverPages = new Set(["/dashboard", "/dashboard/endpoint", "/dashboard/cli-tools", "/dashboard/cli-tools/[toolId]"]);
await fs.writeFile(path.join(frontend, "routes.js"), `// Generated from every original src/app page by build.mjs.\nimport { lazy } from "react";\nimport { MachinePage } from "./app";\nexport const routes = [\n${manifest.map(({ source, route }) => {
  const pattern = "^" + route.replace(/\[[^\]]+\]/g, "[^/]+") + "/?$";
  const page = serverPages.has(route) ? `() => <MachinePage tool={${route.startsWith("/dashboard/cli-tools")}} />` : `lazy(() => import(${JSON.stringify("@/" + source.slice(4))}))`;
  return `  { route: ${JSON.stringify(route)}, pattern: new RegExp(${JSON.stringify(pattern)}), page: ${page} },`;
}).join("\n")}\n];\n`);
await fs.rm(output, { recursive: true, force: true });
await fs.mkdir(output, { recursive: true });
await fs.cp(path.join(root, "public"), output, { recursive: true });
await fs.cp(path.join(desktop, "node_modules/monaco-editor/min/vs"), path.join(output, "monaco/vs"), { recursive: true });
await fs.writeFile(path.join(output, "routes.json"), JSON.stringify(manifest, null, 2) + "\n");
const aliases = Object.fromEntries(["navigation", "link", "image", "dynamic"].map((name) => [`next/${name}$`, path.join(frontend, `${name}.js`)]));
const stats = await new Promise((resolve, reject) => {
  webpack({
    mode: "production", context: root, target: ["web", "es2020"], entry: path.join(frontend, "app.js"),
    output: { path: output, filename: "[name].js", chunkFilename: "chunks/[name].[contenthash:12].js", publicPath: "/" },
    resolve: { extensions: [".js", ".jsx", ".json"], modules: [path.join(desktop, "node_modules"), "node_modules"], alias: { ...aliases, "@/shared/constants/monaco$": path.join(frontend, "monaco.js"), "os$": path.join(frontend, "os.js"), "@": path.join(root, "src"), "open-sse": path.join(root, "open-sse") } },
    module: { rules: [
      { test: /\.jsx?$/, resolve: { fullySpecified: false }, exclude: /node_modules/, use: path.join(frontend, "loader.cjs") },
      { test: /\.css$/, type: "asset/source" },
    ] },
    plugins: [new webpack.DefinePlugin({
      "process.env": JSON.stringify({ NODE_ENV: "production", NEXT_PUBLIC_CLOUD_URL: process.env.NEXT_PUBLIC_CLOUD_URL || "https://9router.com" }),
      "process.env.NODE_ENV": JSON.stringify("production"),
      "process.env.NEXT_PUBLIC_BASE_URL": "globalThis.__9routerBackendOrigin",
      "window.location.origin": "(globalThis.__9routerBackendOrigin || window.location.origin)",
      ...Object.fromEntries(["hostname", "port", "protocol"].map((key) => [`window.location.${key}`, `(globalThis.__9routerBackendLocation?.${key} || window.location.${key})`])),
    })],
    performance: { hints: false }, optimization: { minimize: false, splitChunks: { chunks: "all", cacheGroups: { defaultVendors: { name: "vendors", test: /[\\/]node_modules[\\/]/ } } } },
    stats: { all: false, errors: true, warnings: true, assets: true },
  }, (error, stats) => error ? reject(error) : stats.hasErrors() ? reject(new Error(stats.toString({ all: false, errors: true }))) : resolve(stats));
});
console.log(stats.toString({ all: false, errors: true, warnings: true, assets: true }));
const css = await fs.readFile(path.join(root, "src/app/globals.css"), "utf8");
const built = await postcss([tailwind({ base: root, optimize: true })]).process(css.replace('source("../../")', 'source(none)') + `\n@source "../../src";\n`, { from: path.join(frontend, "styles.css") });
const symbols = await fs.readFile(path.join(desktop, "node_modules/material-symbols/outlined.css"), "utf8");
await fs.copyFile(path.join(desktop, "node_modules/material-symbols/material-symbols-outlined.woff2"), path.join(output, "material-symbols-outlined.woff2"));
const flow = await fs.readFile(path.join(desktop, "node_modules/@xyflow/react/dist/style.css"), "utf8");
await fs.writeFile(path.join(output, "styles.css"), built.css + "\n" + symbols + "\n" + flow);
await fs.cp(path.join(frontend, "assets"), path.join(output, "fonts"), { recursive: true });
await fs.appendFile(path.join(output, "styles.css"), '\n@font-face{font-family:Inter;font-style:normal;font-weight:100 900;font-display:swap;src:url("/fonts/inter.ttf") format("truetype")}\n');
for (const [name, source] of [["Material-Symbols", "material-symbols/LICENSE"], ["Monaco", "monaco-editor/LICENSE"], ["Monaco-ThirdPartyNotices", "monaco-editor/ThirdPartyNotices.txt"]]) {
  await fs.mkdir(path.join(output, "licenses"), { recursive: true });
  await fs.copyFile(path.join(desktop, "node_modules", source), path.join(output, "licenses", name + ".txt"));
}
// Next's bundled webpack references a removed Terser plugin; use its own SWC minifier.
const swc = require("next/dist/build/swc");
for (const name of Object.keys(stats.compilation.assets).filter((name) => name.endsWith(".js"))) {
  const filename = path.join(output, name);
  const { code } = await swc.minify(await fs.readFile(filename, "utf8"), { compress: true, mangle: true });
  await fs.writeFile(filename, code);
}
const scripts = stats.toJson({ all: false, entrypoints: true }).entrypoints.main.assets.filter(({ name }) => name.endsWith(".js")).map(({ name }) => `<script src="/${name}" defer></script>`).join("");
await fs.writeFile(path.join(output, "index.html"), `<!doctype html><html lang="en"><head><meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="theme-color" content="#0a0a0a"><title>9Router - AI Infrastructure Management</title><link rel="icon" href="/favicon.svg"><link rel="stylesheet" href="/styles.css"><script src="/theme.js"></script>${scripts}</head><body class="font-sans antialiased"><div id="root"></div></body></html>\n`);
await fs.copyFile(path.join(frontend, "theme.js"), path.join(output, "theme.js"));
console.log(`Built ${manifest.length} original page routes in desktop/ui (static assets; no Node runtime).`);
