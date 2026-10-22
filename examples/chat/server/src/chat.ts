// The chat app as a TypeScript Durable Object (design note 013, step 2). The protocol logic
// is in app.ts; this class gives it the object's storage, endpoint and alarm.
import { EndpointId, IrohDurableObject, type Protocols, type Timings } from "iroh-do";
import { ChatApp, type ChatStats } from "./app";
import { SCHEMA } from "./store";

/** The bindings the class reads. */
export interface ChatEnv {
  /** This class's namespace, to check that a key matches the object's name. */
  CHAT: DurableObjectNamespace;
  /** Shorter limits for tests; unbound in the deployment. */
  T_IDLE_MS?: string;
  T_SILENT_MS?: string;
}

export class Chat extends IrohDurableObject<ChatEnv> {
  static namespace = "CHAT";
  static app = "chat";

  /** One per node build, as the Rust host builds one app value per construction. */
  #app: ChatApp | null = null;

  protocols(): Protocols {
    const sql = this.ctx.storage.sql;
    for (const s of SCHEMA) sql.exec(s);
    this.#app = new ChatApp({
      id: () => this.endpoint.id().toZ32(),
      sql,
      peers: () => new Set(this.endpoint.peers().map((p) => p.toZ32())),
      connect: (peer, alpn) => this.endpoint.connect(EndpointId.fromZ32(peer), alpn),
      schedule: (key, delayMs, payload) => this.schedule(key, delayMs, payload),
      status: () => this.endpointStatus(),
    });
    return this.#app.protocols();
  }

  protected override onAlarm(key: string, payload: string): void {
    this.#app?.onAlarm(key, payload);
  }

  protected override appStatus(): Record<string, unknown> {
    return this.#app?.status() ?? {};
  }

  protected override timings(): Timings {
    const n = (v: string | undefined) => (v === undefined || v === "" ? undefined : Number(v));
    return { tIdleMs: n(this.env.T_IDLE_MS), tSilentMs: n(this.env.T_SILENT_MS) };
  }

  /** RPC, for tests: the app's counters since its node was built, or null before mint. */
  async chatStats(): Promise<ChatStats | null> {
    // status() builds the node on this construction's first event.
    if ((await this.status()) === null) return null;
    return this.#app ? { ...this.#app.stats } : null;
  }
}
