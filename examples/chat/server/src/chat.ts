// The chat object: an iroh endpoint that serves a chat room (`iroh-do/room/1`) and the
// diagnostic protocols `iroh-do/ctl/1` and `iroh-do/echo/1`, and dials peers back with
// `iroh-do/notify/1`. examples/chat/common/PROTOCOL.md specifies all four.
import { IrohDurableObject, type Protocols, type Timings } from "iroh-do";
import { Ctl } from "./ctl";
import { serveEcho } from "./echo";
import { background, logger } from "./log";
import { DialBack, type DialBackStats } from "./notify";
import { CTL_ALPN, ECHO_ALPN, ROOM_ALPN } from "./protocol";
import { Room, type RoomStats } from "./room";
import { ChatStorage, PRUNE_DELAY_MS, PRUNE_KEY } from "./storage";

/** The bindings the class reads. */
export interface ChatEnv {
  /** This class's namespace; the base class checks a minted key against the object's name. */
  CHAT: DurableObjectNamespace;
  /** Shorter connection limits for tests, in milliseconds; unbound in the deployment. */
  T_IDLE_MS?: string;
  T_SILENT_MS?: string;
}

export type ChatStats = RoomStats & DialBackStats;

/** Schedule keys of dial-backs: the prefix, then the peer's endpoint id in z-base-32. */
const DIAL_BACK_KEY = "callback:";

export class Chat extends IrohDurableObject<ChatEnv> {
  static override namespace = "CHAT";
  static override app = "chat";

  readonly #storage: ChatStorage;
  readonly #room: Room;
  readonly #ctl: Ctl;
  readonly #dialBack: DialBack;
  readonly #log = logger("chat", () => this.#endpointId());
  readonly #echoLog = logger("chat.echo", () => this.#endpointId());
  #id: string | undefined;

  constructor(ctx: DurableObjectState, env: ChatEnv) {
    super(ctx, env);
    this.#storage = new ChatStorage(ctx.storage, () => this.schedule(PRUNE_KEY, PRUNE_DELAY_MS));
    // `sql.exec` is synchronous, so the tables exist before the object handles its first
    // event; `blockConcurrencyWhile` is needed only for asynchronous setup. The router reaches
    // this class only for endpoints the registry lists as `chat`, so only objects that are
    // minted, or about to be, create them. The exception is a destroyed endpoint reached
    // through another isolate's stale registry cache, which gets empty tables again.
    this.#storage.createSchema();
    const endpointId = () => this.#endpointId();
    this.#room = new Room(this.#storage, () => this.endpoint, logger("chat.room", endpointId));
    this.#ctl = new Ctl({
      storage: this.#storage,
      log: logger("chat.ctl", endpointId),
      endpointId,
      status: () => this.endpointStatus(),
      scheduleDialBack: (peer, delayMs) => this.schedule(DIAL_BACK_KEY + peer, delayMs),
    });
    this.#dialBack = new DialBack({
      storage: this.#storage,
      log: logger("chat.notify", endpointId),
      endpoint: () => this.endpoint,
      wakes: () => this.#wakes(),
    });
  }

  /** Called on every node build: after a wake, and after a wasm reset. */
  protocols(): Protocols {
    this.#room.forgetConnections();
    return {
      [ROOM_ALPN]: { accept: (conn) => this.#room.serve(conn) },
      [CTL_ALPN]: { accept: (conn) => this.#ctl.serve(conn) },
      [ECHO_ALPN]: { accept: (conn) => serveEcho(conn, this.#storage, this.#echoLog) },
    };
  }

  protected override onAlarm(key: string): void {
    if (key === PRUNE_KEY) {
      this.#storage.prune(Date.now());
    } else if (key.startsWith(DIAL_BACK_KEY)) {
      // Not awaited, so that the alarm's other due work does not wait for the dial and the
      // linger. A failed dial-back is recorded, not retried.
      background(this.#dialBack.run(key.slice(DIAL_BACK_KEY.length)), this.#log, "dial-back");
    } else {
      this.#log.warn("bad_schedule_key", { key });
    }
  }

  protected override appStatus(): Record<string, unknown> {
    return { records: this.#storage.totalRecords(), ...this.#room.status() };
  }

  protected override timings(): Timings {
    const timings: Timings = {};
    const idle = milliseconds(this.env.T_IDLE_MS, "T_IDLE_MS");
    const silent = milliseconds(this.env.T_SILENT_MS, "T_SILENT_MS");
    if (idle !== undefined) timings.tIdleMs = idle;
    if (silent !== undefined) timings.tSilentMs = silent;
    return timings;
  }

  /** RPC, for tests: the counters since construction, or null before mint. */
  async chatStats(): Promise<ChatStats | null> {
    // status() builds the node on the construction's first event, and is null before mint.
    if ((await this.status()) === null) return null;
    return { ...this.#room.stats, ...this.#dialBack.stats };
  }

  /** The endpoint id, z-base-32. Builds the node if needed; throws before mint. */
  #endpointId(): string {
    this.#id ??= this.endpoint.id().toZ32();
    return this.#id;
  }

  #wakes(): number {
    const { wakes } = this.endpointStatus();
    if (typeof wakes !== "number") throw new TypeError(`status: wakes is ${typeof wakes}`);
    return wakes;
  }
}

/** A var holding a positive number of milliseconds; undefined if unset or empty. */
function milliseconds(value: string | undefined, name: string): number | undefined {
  if (value === undefined || value === "") return undefined;
  const ms = Number(value);
  if (!Number.isSafeInteger(ms) || ms <= 0) throw new RangeError(`${name} must be a positive integer, got ${value}`);
  return ms;
}
