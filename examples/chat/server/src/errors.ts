// Errors the chat object tells apart.
//
// A wasm trap (`WebAssembly.RuntimeError`) leaves the wasm instance, which every object in the
// isolate shares, unusable. The base class replaces the instance when an event sees the trap,
// so a handler that catches errors to treat a closed connection or stream as an outcome must
// not catch a trap with them. In a task that no event awaits, the rethrown trap reaches
// `background`, which logs it.

/** Rethrows `error` if it is a wasm trap; returns otherwise. */
export function rethrowTrap(error: unknown): void {
  if (error instanceof WebAssembly.RuntimeError) throw error;
}

/** A promise rejection handler that rethrows a wasm trap and otherwise resolves with `value`. */
export function unlessTrap<T>(value: T): (error: unknown) => T {
  return (error) => {
    rethrowTrap(error);
    return value;
  };
}
