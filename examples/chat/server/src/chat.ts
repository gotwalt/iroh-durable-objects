// The chat object: an iroh endpoint that serves a chat room (`iroh-do/room/1`) and the
// diagnostic protocols `iroh-do/ctl/1` and `iroh-do/echo/1`, and dials peers back with
// `iroh-do/notify/1`. examples/chat/common/PROTOCOL.md specifies all four.
import { IrohDurableObject, type IrohVars, NotMintedError, type Protocols, type Timings } from "iroh-do";
import { Ctl } from "./ctl";
import { serveEcho } from "./echo";
import { rethrowTrap } from "./errors";
import { errorText, logger } from "./log";
import { DialBack } from "./notify";
import { CTL_ALPN, ECHO_ALPN, ROOM_ALPN } from "./protocol";
import { Room } from "./room";
import { ChatStorage, PRUNE_KEY } from "./storage";

/** The bindings the class reads, with the vars the base class reads (IrohVars). */
export interface ChatEnv extends IrohVars {
  /** This class's namespace; the base class checks a minted key against the object's name. */
  CHAT: DurableObjectNamespace<Chat>;
  /** Shorter connection limits for tests, in milliseconds; unbound in the deployment. */
  T_IDLE_MS?: string;
  T_SILENT_MS?: string;
}

/** Schedule keys of dial-backs: the prefix, then the peer's endpoint id in z-base-32. */
const DIAL_BACK_KEY = "callback:";

export class Chat extends IrohDurableObject<ChatEnv> {
  static override namespace = "CHAT";
  static override app = "chat";

  readonly #storage: ChatStorage;
  readonly #room: Room;
  readonly #ctl: Ctl;
  readonly #dialBack: DialBack;
  readonly #log = logger("chat", () => this.#logEndpoint());
  readonly #echoLog = logger("chat.echo", () => this.#logEndpoint());

  constructor(ctx: DurableObjectState, env: ChatEnv) {
    super(ctx, env);
    this.#storage = new ChatStorage(ctx.storage, (dueMs) => this.#schedulePrune(dueMs));
    const logEndpoint = () => this.#logEndpoint();
    const endpointId = () => this.endpointId;
    this.#room = new Room({
      storage: this.#storage,
      endpoint: () => this.endpoint,
      endpointId,
      log: logger("chat.room", logEndpoint),
    });
    this.#ctl = new Ctl({
      storage: this.#storage,
      log: logger("chat.ctl", logEndpoint),
      endpointId,
      status: () => this.endpointStatus(),
      scheduleDialBack: (peer, delayMs) => this.schedule(DIAL_BACK_KEY + peer, delayMs),
    });
    this.#dialBack = new DialBack({
      storage: this.#storage,
      log: logger("chat.notify", logEndpoint),
      endpoint: () => this.endpoint,
      endpointId,
      wakes: () => this.endpointStatus().wakes,
    });
  }

  /**
   * Called on every node build, in a minted object only (at mint, after a wake, after a wasm
   * reset), inside the build's transaction. The tables are created here, so that an object
   * that is not minted, such as a destroyed endpoint reached through a stale registry cache,
   * writes none.
   */
  protocols(): Protocols {
    this.#storage.createSchema();
    // The connections the room keeps belong to this node and are stale once it is lost.
    this.nodeSignal.addEventListener("abort", () => this.#room.forgetConnections(), { once: true });
    return {
      [ROOM_ALPN]: { accept: (conn) => this.#room.serve(conn) },
      [CTL_ALPN]: { accept: (conn) => this.#ctl.serve(conn) },
      [ECHO_ALPN]: { accept: (conn) => serveEcho(conn, this.#storage, this.#echoLog) },
    };
  }

  protected override async onAlarm(key: string): Promise<void> {
    if (key === PRUNE_KEY) {
      const due = this.#storage.prune(Date.now());
      if (due !== null) this.#schedulePrune(due);
    } else if (key.startsWith(DIAL_BACK_KEY)) {
      // Awaited until the line is written, so that the alarm event covers the dial and the
      // base class sees a wasm trap in it, and runs the dial-back again. The wait for the
      // peer's close, at most 5 s, runs on after the event. A failed dial or write is
      // recorded, not retried.
      await this.#dialBack.run(key.slice(DIAL_BACK_KEY.length));
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

  /**
   * Arms the prune alarm for `dueMs`. Runs after an append has committed, so a failure is
   * logged, not thrown: the append stands, and the next append after `dueMs` arms the alarm
   * again (ChatStorage).
   */
  #schedulePrune(dueMs: number): void {
    try {
      this.schedule(PRUNE_KEY, Math.max(0, dueMs - Date.now()));
    } catch (error) {
      rethrowTrap(error);
      this.#log.error("prune_schedule_failed", { error: errorText(error) });
    }
  }

  /** The endpoint id for a log line, or undefined before mint. */
  #logEndpoint(): string | undefined {
    try {
      return this.endpointId;
    } catch (error) {
      if (error instanceof NotMintedError) return undefined;
      throw error;
    }
  }
}

/** A var holding a positive number of milliseconds; undefined if unset or empty. */
function milliseconds(value: string | undefined, name: string): number | undefined {
  if (value === undefined || value === "") return undefined;
  const ms = Number(value);
  if (!Number.isSafeInteger(ms) || ms <= 0) throw new RangeError(`${name} must be a positive integer, got ${value}`);
  return ms;
}
