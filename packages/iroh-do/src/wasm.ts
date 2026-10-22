/// <reference path="./wasm-module.d.ts" />
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
import wasmModule from "../pkg/iroh_do_wasm_bg.wasm";
import * as glue from "../pkg/iroh_do_wasm.js";
import { log } from "./log";

glue.initSync({ module: wasmModule });

let generation = 0;
/** Why the current instance is unusable, or null. */
let trapped: string | null = null;
let configured = -1;
let logLevel: string | undefined;
let timersInstalled = false;

/**
 * Returns the current instance's generation. Instantiates the module again first if the
 * current one trapped, and configures a new instance (log subscriber, panic handler).
 *
 * Call only at the start of an event, with no wasm frame on the stack.
 */
export function instance(level?: string): number {
  if (logLevel === undefined && level !== undefined) logLevel = level;
  installTimers();
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
 * Adds 1 ms to every `setTimeout` delay (formerly `crates/iroh-do/src/glue/timers.rs`).
 *
 * n0-future truncates timer delays to whole milliseconds before calling `setTimeout`. Under
 * Cloudflare's clock a noq timer then fires before its deadline, sees it not yet reached, and
 * re-arms with a zero delay that never advances the clock (design.md, "Time"). Adding 1 ms
 * guarantees `when >= deadline`. n0-future looks `setTimeout` up on `globalThis` at every
 * call, so replacing the property covers every iroh and noq timer.
 */
function installTimers(): void {
  if (timersInstalled) return;
  timersInstalled = true;
  const original = globalThis.setTimeout;
  const wrapped = (callback: (...args: unknown[]) => void, ms?: number, ...args: unknown[]) => {
    const d = typeof ms === "number" && Number.isFinite(ms) && ms > 0 ? Math.floor(ms) : 0;
    return original(callback, d + 1, ...args);
  };
  globalThis.setTimeout = wrapped as typeof setTimeout;
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

export {
  BiStream,
  Connection,
  Endpoint,
  EndpointAddr,
  EndpointId,
  RecvStream,
  SendStream,
  WasmNode,
  generateSecretKey,
  pkarrConfirm,
  pkarrRecord,
  publicKeyOf,
} from "../pkg/iroh_do_wasm.js";
export type { IngressResult, NodeHost, NodeOptions, PkarrRecord, UpgradeResult } from "../pkg/iroh_do_wasm.js";
