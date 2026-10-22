// Bundles test/fixture/worker.ts with the wasm module's test-panic build (pkg-test/, from
// `build-wasm.sh --test-panic`) into test/fixture/dist, with a manifest.json in the format of
// scripts/bundle.mjs. The package's own pkg/ is never touched, and never used here.
import { createHash } from "node:crypto";
import { copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const here = dirname(fileURLToPath(import.meta.url));
const pkgTest = resolve(here, "../../pkg-test");
const out = join(here, "dist");
if (readFileSync(join(pkgTest, "FEATURES"), "utf8").trim() !== "test-panic") {
  throw new Error(`${pkgTest} is not a test-panic build; run build-wasm.sh --test-panic`);
}
const wasmName = "iroh_do_wasm_bg.wasm";

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
await build({
  entryPoints: [join(here, "worker.ts")],
  outfile: join(out, "worker.js"),
  bundle: true,
  format: "esm",
  target: "es2023",
  platform: "neutral",
  conditions: ["workerd", "worker", "import"],
  external: ["cloudflare:*", `./${wasmName}`],
  plugins: [
    {
      name: "pkg-test",
      setup(b) {
        // src/wasm.ts imports ../pkg/...; take the test build instead.
        b.onResolve({ filter: /\/pkg\/iroh_do_wasm\.js$/ }, () => ({ path: join(pkgTest, "iroh_do_wasm.js") }));
        b.onResolve({ filter: /\/pkg\/iroh_do_wasm_bg\.wasm$/ }, () => ({ path: `./${wasmName}`, external: true }));
      },
    },
  ],
});
copyFileSync(join(pkgTest, wasmName), join(out, wasmName));
if (!existsSync(join(out, "worker.js"))) throw new Error("no bundle");
const modules = [
  { name: "worker.js", path: "worker.js", content_type: "application/javascript+module" },
  { name: wasmName, path: wasmName, content_type: "application/wasm" },
];
const sha = (p) => createHash("sha256").update(readFileSync(p)).digest("hex");
const bundle_sha256 = createHash("sha256")
  .update(modules.map((m) => sha(join(out, m.path))).join(""))
  .digest("hex");
writeFileSync(
  join(out, "manifest.json"),
  `${JSON.stringify({ main_module: "worker.js", modules, bundle_sha256 }, null, 2)}\n`,
);
console.log(`test fixture: ${out}`);
