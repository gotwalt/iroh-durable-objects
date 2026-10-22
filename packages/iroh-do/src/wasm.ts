// The wasm library (crates/iroh-do-wasm), loaded once per isolate, and its recovery after a
// panic.
//
// wrangler's default module rules import `*.wasm` as a compiled `WebAssembly.Module`, and
// wasm-bindgen's `--target web` glue instantiates it synchronously with `initSync`.
//
// Every object in an isolate shares this one instance. With `panic = "abort"` a panic traps
// it, and its memory is no longer consistent. The glue is generated with
// `--experimental-reset-state-function`: `__wbg_reset_state` instantiates the module again,
// and marks every object and closure of the old instance stale, so that a stale call throws
// instead of reaching the new instance. `generation` counts instances; an object whose node
// was built for an older generation rebuilds it (object.ts).
import * as glue from "../pkg/iroh_do_wasm.js";
import wasmModule from "../pkg/iroh_do_wasm_bg.wasm";
import { log } from "./log.js";

/** Marks `globalThis.setTimeout` as wrapped, so that a second copy of this module does not wrap it again. */
const ROUND_UP = Symbol.for("iroh-do.setTimeout.roundUp");
installTimerRoundUp();
glue.initSync({ module: wasmModule });

let generation = 0;
/** Why the current instance is unusable, or null. */
let trapped: string | null = null;
let configured = -1;
let logLevel: string | undefined;

/**
 * Returns the current instance's generation. Instantiates the module again first if the
 * current one trapped, and configures a new instance (log subscriber, panic handler).
 *
 * Call only with no wasm frame on the stack: at the start of an event, or in a microtask.
 */
export function instance(level?: string): number {
  if (logLevel === undefined && level !== undefined) logLevel = level;
  if (trapped !== null) {
    const reason = trapped;
    trapped = null;
    glue.__wbg_reset_state();
    generation++;
    log("warn", "wasm_reset", { generation, reason });
  }
  if (configured !== generation) {
    configured = generation;
    glue.initLogging(logLevel);
    glue.setPanicHandler(onPanic);
  }
  return generation;
}

/** Whether the instance has trapped and not yet been replaced. */
export function isTrapped(): boolean {
  return trapped !== null;
}

/**
 * Records `error` if it is a wasm trap. Returns whether it was. The instance is replaced at
 * the next `instance()` call.
 */
export function noteTrap(error: unknown): boolean {
  if (!(error instanceof WebAssembly.RuntimeError)) return false;
  if (trapped === null) {
    trapped = String(error);
    log("error", "wasm_trap", { generation, error: String(error) });
  }
  return true;
}

// Called by the wasm panic hook, before the trap. The trap may surface in a microtask or a
// timer of a node task, where no object catches it; the reset then waits for the next event,
// or for this microtask, which runs once the trapping call has unwound. Resetting inside the
// hook would let the unwinding glue code write to the new instance.
function onPanic(message: string): void {
  log("error", "wasm_panic", { generation, message });
  if (trapped === null) trapped = message;
  queueMicrotask(() => {
    if (trapped !== null) instance();
  });
}

/**
 * Makes every `setTimeout` in the isolate fire 1 ms later than asked (formerly
 * `crates/iroh-do/src/glue/timers.rs`; design.md, "Time").
 *
 * Why: n0-future's wasm `sleep` (n0-future 0.3.2, `src/time.rs`, `sleep_impl` and
 * `Sleep::reset`) passes `duration.as_millis()` to `setTimeout`, which truncates. Under
 * Cloudflare's clock, which advances only between events, a noq timer then fires before its
 * deadline, finds it not reached, and re-arms with a delay that truncates to 0: a loop that
 * never advances the clock. One more millisecond guarantees `when >= deadline`.
 *
 * Why globally: n0-future looks `setTimeout` up on `globalThis` at every call, through its
 * own wasm-bindgen import, so no stock dependency lets this module give it a private timer.
 * The Rust host patches the same property for the same reason.
 *
 * What it changes: every `setTimeout(callback, ms, ...args)` in the isolate, the app's and
 * other libraries' included, runs after `max(0, floor(ms)) + 1` ms. The handle, the callback
 * and its arguments are unchanged; `clearTimeout`, `setInterval` and `scheduler.wait` are
 * untouched. Installed once, when this module is first evaluated.
 *
 * The fix belongs upstream: n0-future rounding the delay up (`as_micros().div_ceil(1000)`)
 * instead of truncating would make this patch unnecessary.
 */
function installTimerRoundUp(): void {
  const current = globalThis.setTimeout as typeof setTimeout & { [ROUND_UP]?: true };
  if (current[ROUND_UP]) return;
  const original = current.bind(globalThis);
  const wrapped = (callback: (...args: unknown[]) => void, ms?: number, ...args: unknown[]) => {
    const d = typeof ms === "number" && Number.isFinite(ms) && ms > 0 ? Math.floor(ms) : 0;
    return original(callback, d + 1, ...args);
  };
  Object.defineProperty(wrapped, ROUND_UP, { value: true });
  globalThis.setTimeout = wrapped as unknown as typeof setTimeout;
}

/**
 * `testPanic(inTask)` if the module was built with the `test-panic` feature, else undefined.
 * Production builds never have it (build-wasm.sh, scripts/bundle.mjs).
 */
export function testPanicExport(): ((inTask: boolean) => void) | undefined {
  // Looked up at run time: the export exists only in test builds.
  const f: unknown = Reflect.get(glue, "testPanic");
  return typeof f === "function" ? (f as (inTask: boolean) => void) : undefined;
}

export const {
  EndpointAddr: GlueEndpointAddr,
  EndpointId: GlueEndpointId,
  WasmNode,
  generateSecretKey,
  liveNodes,
  pkarrConfirm,
  pkarrRecord,
  publicKeyOf,
} = glue;
export type { IngressResult, NodeHost, NodeOptions, PkarrRecord, UpgradeResult } from "../pkg/iroh_do_wasm.js";
export type WasmNode = glue.WasmNode;
export type GlueEndpoint = glue.Endpoint;
