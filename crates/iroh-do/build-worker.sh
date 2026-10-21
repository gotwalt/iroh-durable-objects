#!/usr/bin/env bash
# Build the iroh-do Worker bundle into <repo>/build/worker/ (see BUILD.md).
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
out="$repo/build/worker"

# ring's C code must be compiled with clang and archived with llvm-ar for
# wasm32; Apple's ar silently writes empty archives (docs/verification/iroh-endpoint.md V7).
if [[ "$(uname)" == "Darwin" ]]; then
  llvm="${LLVM_PREFIX:-$(brew --prefix llvm 2>/dev/null || echo /opt/homebrew/opt/llvm)}"
  export CC_wasm32_unknown_unknown="${CC_wasm32_unknown_unknown:-$llvm/bin/clang}"
  export AR_wasm32_unknown_unknown="${AR_wasm32_unknown_unknown:-$llvm/bin/llvm-ar}"
fi

want="0.8.7"
have="$(worker-build --version 2>/dev/null || true)"
if [[ "$have" != *"$want"* ]]; then
  echo "worker-build $want required (found: ${have:-none}); cargo install worker-build --version $want --locked" >&2
  exit 1
fi

cd "$here"
worker-build --release --out-dir "$out"
rm -rf "$out/worker" "$out/package.json" # back-compat shim alias + npm metadata: not uploaded

# Every import must come from wasm-bindgen; a stray `env.*` import means ring's
# archive came out empty (wrong `ar`).
if command -v node >/dev/null; then
  node -e '
    const m = new WebAssembly.Module(require("fs").readFileSync(process.argv[1]));
    const bad = WebAssembly.Module.imports(m).filter(i => !i.module.startsWith("./index_bg") && !i.module.startsWith("__wbindgen") && i.module !== "./snippets");
    if (bad.length) { console.error("unexpected wasm imports:", bad.slice(0, 5)); process.exit(1); }
  ' "$out/index_bg.wasm" || { echo "bad wasm imports" >&2; exit 1; }
fi

sha() { shasum -a 256 "$1" | cut -d" " -f1; }
raw=$(wc -c < "$out/index_bg.wasm" | tr -d ' ')
gz=$(gzip -9 -c "$out/index_bg.wasm" | wc -c | tr -d ' ')
js=$(wc -c < "$out/index.js" | tr -d ' ')
bundle=$(printf '%s%s' "$(sha "$out/index.js")" "$(sha "$out/index_bg.wasm")" | shasum -a 256 | cut -d" " -f1)
echo "$bundle" > "$out/BUNDLE_SHA256"
echo "bundle: $out"
echo "  index.js       $js B"
echo "  index_bg.wasm  $raw B raw, $gz B gzip -9"
echo "  BUNDLE_SHA256  $bundle"
