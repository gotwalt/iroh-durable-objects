// Log lines of the TypeScript host, in the format of design note 010: one flat object per
// event, with `level`, `target`, `message` and a stable `event` field. Workers Logs indexes
// the fields of a logged object. The wasm side writes iroh's and the node's events in the
// same format; `LOG_LEVEL` filters only those.

export type Level = "error" | "warn" | "info";

const TARGET = "iroh_do::host";

/** Writes one event. Apps pass their own `target`, as a Rust app's module path would be. */
export function log(level: Level, event: string, fields: Record<string, unknown> = {}, target = TARGET): void {
  const line = { level, target, message: event, event, ...fields };
  if (level === "error") console.error(line);
  else if (level === "warn") console.warn(line);
  else console.log(line);
}
