// Shared pieces of the storage-backed tests: a fresh object's storage, a ChatStorage on it,
// and a logger that keeps what it logs.
import { runInDurableObject } from "cloudflare:test";
import { env } from "cloudflare:workers";
import type { EventName, Fields, Logger } from "../../src/log";
import { ChatStorage } from "../../src/storage";

/** Runs `body` in a new Durable Object, with its own empty SQLite database. */
export async function inObject<T>(body: (state: DurableObjectState) => Promise<T> | T): Promise<T> {
  const stub = env.TEST_OBJECT.get(env.TEST_OBJECT.newUniqueId());
  return runInDurableObject(stub, (_instance, state) => body(state));
}

/** A ChatStorage on `state` with its tables, and the prune alarms it asked for. */
export function chatStorage(state: DurableObjectState): { storage: ChatStorage; pruneAlarms: number[] } {
  const pruneAlarms: number[] = [];
  const storage = new ChatStorage(state.storage, (dueMs) => pruneAlarms.push(dueMs));
  storage.createSchema();
  return { storage, pruneAlarms };
}

export interface LogLine {
  level: "info" | "warn" | "error";
  event: EventName;
  fields: Fields;
}

/** A Logger that keeps its lines in `lines`. */
export function testLogger(): Logger & { lines: LogLine[]; events(event: EventName): Fields[] } {
  const lines: LogLine[] = [];
  return {
    lines,
    events: (event) => lines.filter((l) => l.event === event).map((l) => l.fields),
    info: (event, fields) => lines.push({ level: "info", event, fields }),
    warn: (event, fields) => lines.push({ level: "warn", event, fields }),
    error: (event, fields) => lines.push({ level: "error", event, fields }),
  };
}

/** Resolves once `ready()` is true, checking every few milliseconds; rejects after `ms`. */
export async function eventually(ready: () => boolean, ms = 2_000): Promise<void> {
  const deadline = Date.now() + ms;
  while (!ready()) {
    if (Date.now() > deadline) throw new Error("condition not met in time");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

export const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));
