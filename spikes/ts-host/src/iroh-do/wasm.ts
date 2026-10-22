// Loads the wasm library once per isolate. wrangler's default module rules import `*.wasm`
// as a compiled `WebAssembly.Module`; wasm-bindgen's `--target web` glue instantiates it
// synchronously with `initSync`. No worker-build and no JS shim class are involved.
import wasmModule from "../../pkg/iroh_do_wasm_bg.wasm";
import { initLogging, initSync } from "../../pkg/iroh_do_wasm.js";

initSync({ module: wasmModule });

let installed = false;

/**
 * Per-isolate setup that needs `env` or must not run at module load: the JSON log
 * subscriber, and the `setTimeout` round-up.
 *
 * The round-up (formerly `crates/iroh-do/src/glue/timers.rs`): n0-future truncates timer
 * delays to whole milliseconds before calling `setTimeout`. Under Cloudflare's clock a noq
 * timer then fires before its deadline, sees it not yet reached, and re-arms with a zero
 * delay that never advances the clock (design.md, "Time"). Adding 1 ms to every delay
 * guarantees `when >= deadline`. n0-future looks `setTimeout` up on `globalThis` at every
 * call, so replacing the property covers every iroh and noq timer.
 */
export function installOnce(logLevel: string | undefined): void {
  if (installed) return;
  installed = true;
  initLogging(logLevel);
  const original = globalThis.setTimeout;
  const wrapped = (callback: (...args: unknown[]) => void, ms?: number, ...args: unknown[]) => {
    const d = typeof ms === "number" && Number.isFinite(ms) && ms > 0 ? Math.floor(ms) : 0;
    return original(callback, d + 1, ...args);
  };
  globalThis.setTimeout = wrapped as typeof setTimeout;
}

export * from "../../pkg/iroh_do_wasm.js";
