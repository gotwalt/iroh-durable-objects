#!/usr/bin/env node
// Builds a Worker bundle for upload and describes it in manifest.json.
//
//   iroh-do-bundle [--config wrangler.toml] [--outdir dist]
//
// Run from the app's directory (npm runs package scripts there). The wasm module is the one
// this package ships in pkg/ (in the repository: `npm run build -w iroh-do`). Steps:
//
// 1. `wrangler deploy --dry-run --outdir <outdir> --metafile`: esbuild bundles the app, this
//    package and the wasm-bindgen glue into one module, and copies the wasm next to it under
//    a name that carries its content hash.
// 2. <outdir>/manifest.json:
//
//    {
//      "main_module": "worker.js",          (named after the config's `main`)
//      "modules": [
//        { "name": "worker.js", "path": "worker.js", "content_type": "application/javascript+module" },
//        { "name": "<hash>-iroh_do_wasm_bg.wasm", "path": "<hash>-iroh_do_wasm_bg.wasm", "content_type": "application/wasm" }
//      ],
//      "bundle_sha256": "<hex>"
//    }
//
//    `name` is the module's name as the main module imports it, which is the part name an
//    upload must use; `path` is relative to the outdir. The main module comes first, the
//    others follow sorted by name. `bundle_sha256` is the SHA-256 of the concatenated
//    lowercase hex SHA-256 digests of the module files, in `modules` order, the scheme
//    deploy/build-worker.sh uses for the Rust bundle.
//
// The build refuses a wasm module built with features (pkg/FEATURES): test builds go to
// pkg-test/ and are bundled by the package's own test scripts, never by this one. It also
// refuses a module the main module does not import, according to esbuild's metafile.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const pkgDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const { values } = parseArgs({
  options: {
    config: { type: "string", default: "wrangler.toml" },
    outdir: { type: "string", default: "dist" },
  },
  strict: true,
});
const fail = (msg) => {
  console.error(`iroh-do-bundle: ${msg}`);
  process.exit(1);
};
const outdir = resolve(values.outdir);
const metafile = join(outdir, "bundle-meta.json");

const featuresFile = join(pkgDir, "pkg", "FEATURES");
if (!existsSync(featuresFile)) fail(`${join(pkgDir, "pkg")} is missing; build it with \`npm run build -w iroh-do\``);
const features = readFileSync(featuresFile, "utf8").trim();
if (features) fail(`pkg/ was built with features "${features}"; rebuild it with build-wasm.sh`);

rmSync(outdir, { recursive: true, force: true });
execFileSync(
  "npx",
  [
    "--no-install",
    "wrangler",
    "deploy",
    "--dry-run",
    "--config",
    values.config,
    "--outdir",
    outdir,
    "--metafile",
    metafile,
  ],
  { stdio: "inherit" },
);

const entries = readdirSync(outdir);
for (const f of entries) {
  if (statSync(join(outdir, f)).isDirectory()) fail(`unexpected directory in the bundle: ${f}`);
}
// wrangler names the bundled module after the config's `main`, with a .js extension.
const mains = entries.filter((f) => f.endsWith(".js"));
if (mains.length !== 1) fail(`expected one .js module in ${outdir}, found ${JSON.stringify(mains)}`);
const MAIN = mains[0];

// The modules the main module imports, from esbuild's metafile: external imports keep their
// specifier, which is the part name.
const meta = JSON.parse(readFileSync(metafile, "utf8"));
const output = Object.entries(meta.outputs ?? {}).find(([p]) => p.endsWith(`/${MAIN}`) || p === MAIN)?.[1];
if (!output) fail(`${metafile} has no output ${MAIN}`);
const imported = new Set((output.imports ?? []).filter((i) => i.external).map((i) => i.path.replace(/^\.\//, "")));

const types = {
  ".js": "application/javascript+module",
  ".mjs": "application/javascript+module",
  ".wasm": "application/wasm",
};
const others = entries.filter((f) => f !== MAIN && !f.endsWith(".map") && f !== "README.md" && f !== "bundle-meta.json").sort();
const modules = [MAIN, ...others].map((f) => {
  const content_type = types[f.slice(f.lastIndexOf("."))];
  if (!content_type) fail(`unexpected file in the bundle: ${f}`);
  // The upload's part name must equal the specifier the main module imports.
  if (f !== MAIN && !imported.has(f)) fail(`${MAIN} does not import ./${f} (imports: ${[...imported].join(", ")})`);
  return { name: f, path: f, content_type };
});
const sha = (p) => createHash("sha256").update(readFileSync(p)).digest("hex");
const bundle_sha256 = createHash("sha256")
  .update(modules.map((m) => sha(join(outdir, m.path))).join(""))
  .digest("hex");
writeFileSync(
  join(outdir, "manifest.json"),
  `${JSON.stringify({ main_module: MAIN, modules, bundle_sha256 }, null, 2)}\n`,
);
for (const m of modules) console.log(`${m.path}  ${statSync(join(outdir, m.path)).size} B`);
console.log(`bundle_sha256 ${bundle_sha256}`);
console.log(`manifest: ${join(outdir, "manifest.json")}`);
