#!/usr/bin/env node
// Builds a Worker bundle for upload and describes it in manifest.json.
//
//   iroh-do-bundle [--config wrangler.toml] [--outdir dist] [--no-wasm] [--allow-test-features]
//
// Run from the app's directory (npm runs package scripts there). Steps:
//
// 1. build-wasm.sh, without features (skipped with --no-wasm).
// 2. `wrangler deploy --dry-run --outdir <outdir>`: esbuild bundles the app, this package
//    and the wasm-bindgen glue into one module, and copies the wasm next to it under a name
//    that carries its content hash.
// 3. <outdir>/manifest.json:
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
// The build refuses a wasm module built with features (pkg/FEATURES), such as test-panic,
// unless --allow-test-features is given; test bundles pass it and go to a separate outdir.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const pkgDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : dflt;
};
const config = opt("--config", "wrangler.toml");
const outdir = resolve(opt("--outdir", "dist"));

if (!args.includes("--no-wasm")) {
  execFileSync(join(pkgDir, "build-wasm.sh"), [], { stdio: "inherit" });
}
const features = readFileSync(join(pkgDir, "pkg", "FEATURES"), "utf8").trim();
if (features && !args.includes("--allow-test-features")) {
  console.error(`pkg/ was built with features "${features}"; rebuild with build-wasm.sh, or pass --allow-test-features`);
  process.exit(1);
}

rmSync(outdir, { recursive: true, force: true });
execFileSync("npx", ["--no-install", "wrangler", "deploy", "--dry-run", "--config", config, "--outdir", outdir], { stdio: "inherit" });

// wrangler names the bundled module after the config's `main`, with a .js extension.
const mains = readdirSync(outdir).filter((f) => f.endsWith(".js"));
if (mains.length !== 1) {
  console.error(`expected one .js module in ${outdir}, found ${JSON.stringify(mains)}`);
  process.exit(1);
}
const MAIN = mains[0];
const main = readFileSync(join(outdir, MAIN), "utf8");
const types = { ".js": "application/javascript+module", ".mjs": "application/javascript+module", ".wasm": "application/wasm" };
const others = readdirSync(outdir)
  .filter((f) => f !== MAIN && statSync(join(outdir, f)).isFile())
  .filter((f) => !f.endsWith(".map") && f !== "README.md" && f !== "manifest.json")
  .sort();
const modules = [MAIN, ...others].map((f) => {
  const ext = f.slice(f.lastIndexOf("."));
  const content_type = types[ext];
  if (!content_type) {
    console.error(`unexpected file in the bundle: ${f}`);
    process.exit(1);
  }
  // The upload's part name must equal the specifier the main module imports.
  if (f !== MAIN && !main.includes(`"./${f}"`)) {
    console.error(`${MAIN} does not import ./${f}`);
    process.exit(1);
  }
  return { name: f, path: f, content_type };
});
const sha = (p) => createHash("sha256").update(readFileSync(p)).digest("hex");
const bundle_sha256 = createHash("sha256")
  .update(modules.map((m) => sha(join(outdir, m.path))).join(""))
  .digest("hex");
const manifest = { main_module: MAIN, modules, bundle_sha256 };
writeFileSync(join(outdir, "manifest.json"), JSON.stringify(manifest, null, 2) + "\n");
for (const m of modules) console.log(`${m.path}  ${statSync(join(outdir, m.path)).size} B`);
console.log(`bundle_sha256 ${bundle_sha256}`);
console.log(`manifest: ${join(outdir, "manifest.json")}`);
