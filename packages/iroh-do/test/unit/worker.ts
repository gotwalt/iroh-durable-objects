// The Worker the unit tests run in (vitest.unit.config.ts): a test endpoint class, the
// registry, and the router with two apps. No peer reaches it; the tests drive the objects
// over RPC and through `runInDurableObject`.
import { IrohDurableObject, IrohRegistry, irohRouter, type Protocols } from "../../src/index.js";

/** What `TestEndpoint.onAlarm` does for a key, by the key's first word. */
function behaviour(key: string): "trap" | "throw" | "ok" {
  if (key.startsWith("trap")) return "trap";
  if (key.startsWith("throw")) return "throw";
  return "ok";
}

export class TestEndpoint extends IrohDurableObject {
  static override app = "test";
  /** Aborts of `nodeSignal` seen by this construction. */
  #lost: string[] = [];

  protocols(): Protocols {
    this.ctx.storage.sql.exec("CREATE TABLE IF NOT EXISTS runs (key TEXT PRIMARY KEY, n INTEGER NOT NULL)");
    const signal = this.nodeSignal;
    signal.addEventListener("abort", () => this.#lost.push(String(signal.reason)), { once: true });
    return {
      "iroh-example/echo/0": {
        accept: async (conn) => {
          const bi = await conn.acceptBi();
          await bi.send.writeAll(await bi.recv.readToEnd(65536));
          await bi.send.finish();
          await conn.closed();
        },
      },
    };
  }

  protected override onAlarm(key: string): void {
    this.ctx.storage.sql.exec(
      "INSERT INTO runs (key, n) VALUES (?, 1) ON CONFLICT(key) DO UPDATE SET n = n + 1",
      key,
    );
    switch (behaviour(key)) {
      // What a trap looks like to JavaScript; `noteTrap` cannot tell it from a real one, and
      // the instance is reset for real.
      case "trap":
        throw new WebAssembly.RuntimeError("unreachable (simulated)");
      case "throw":
        throw new Error("app failure (simulated)");
      case "ok":
        return;
    }
  }

  protected override appStatus(): Record<string, unknown> {
    return { lost: this.#lost.length };
  }

  /** RPC: schedules `key` after `delayMs`. */
  async scheduleWork(key: string, delayMs: number): Promise<void> {
    this.schedule(key, delayMs);
  }

  /** RPC: how often `onAlarm` ran each key. */
  async runs(): Promise<Record<string, number>> {
    const out: Record<string, number> = {};
    for (const r of this.ctx.storage.sql.exec("SELECT key, n FROM runs").toArray()) {
      out[String(r.key)] = Number(r.n);
    }
    return out;
  }

  /** RPC, not wrapped by the base class: reads the endpoint as a subclass method would. */
  async probe(): Promise<{ id: string; lost: string[] }> {
    return { id: this.endpoint.id().toZ32(), lost: [...this.#lost] };
  }

  /** RPC: traps inside `withNode`. */
  async trapInNode(): Promise<string> {
    try {
      await this.withNode(() => {
        throw new WebAssembly.RuntimeError("unreachable (simulated)");
      });
      return "no error";
    } catch (e) {
      return e instanceof Error ? e.name : String(e);
    }
  }

  /** RPC: `schedule` before mint. */
  async scheduleUnminted(): Promise<string> {
    try {
      this.schedule("x", 1000);
      return "scheduled";
    } catch (e) {
      return e instanceof Error ? e.name : String(e);
    }
  }
}

/** Throws from `protocols()`: the node build fails. */
export class BrokenEndpoint extends IrohDurableObject {
  static override namespace = "BROKEN";

  protocols(): Protocols {
    this.ctx.storage.sql.exec("CREATE TABLE IF NOT EXISTS app_table (x)");
    throw new Error("protocols failed (simulated)");
  }
}

export class Registry extends IrohRegistry {}

export default irohRouter({
  apps: {
    test: { binding: "ENDPOINTS", joinHint: (id) => `join ${id}` },
    broken: { binding: "BROKEN" },
  },
  registry: "REGISTRY",
});
