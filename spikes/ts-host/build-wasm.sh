#!/usr/bin/env bash
# Builds crates/iroh-do-wasm and runs wasm-bindgen (--target web) into pkg/.
# No worker-build: wrangler bundles pkg/ with the TypeScript.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"

# ring's C code must be built with clang and archived with llvm-ar for wasm32; Apple's ar
# writes archives that link but fail at instantiation (deploy/BUILD.md).
if [[ "$(uname)" == "Darwin" ]]; then
  llvm="${LLVM_PREFIX:-$(brew --prefix llvm 2>/dev/null || echo /opt/homebrew/opt/llvm)}"
  export CC_wasm32_unknown_unknown="${CC_wasm32_unknown_unknown:-$llvm/bin/clang}"
  export AR_wasm32_unknown_unknown="${AR_wasm32_unknown_unknown:-$llvm/bin/llvm-ar}"
fi

want="0.2.129"
have="$(wasm-bindgen --version 2>/dev/null || true)"
if [[ "$have" != *"$want"* ]]; then
  echo "wasm-bindgen $want required (found: ${have:-none}); cargo install wasm-bindgen-cli --version $want --locked" >&2
  exit 1
fi

cd "$repo"
cargo build -p iroh-do-wasm --target wasm32-unknown-unknown --release
target="${CARGO_TARGET_DIR:-$repo/target}"
wasm-bindgen --target web --out-dir "$here/pkg" "$target/wasm32-unknown-unknown/release/iroh_do_wasm.wasm"

# Every import must come from the wasm-bindgen glue; a stray `env.*` import means ring's
# archive came out empty (wrong `ar`).
node -e '
  const m = new WebAssembly.Module(require("fs").readFileSync(process.argv[1]));
  const bad = WebAssembly.Module.imports(m).filter(i => i.module !== "./iroh_do_wasm_bg.js");
  if (bad.length) { console.error("unexpected wasm imports:", bad.slice(0, 5)); process.exit(1); }
' "$here/pkg/iroh_do_wasm_bg.wasm"

raw=$(wc -c < "$here/pkg/iroh_do_wasm_bg.wasm" | tr -d ' ')
gz=$(gzip -9 -c "$here/pkg/iroh_do_wasm_bg.wasm" | wc -c | tr -d ' ')
js=$(wc -c < "$here/pkg/iroh_do_wasm.js" | tr -d ' ')
echo "pkg/iroh_do_wasm_bg.wasm  $raw B raw, $gz B gzip -9"
echo "pkg/iroh_do_wasm.js       $js B"
