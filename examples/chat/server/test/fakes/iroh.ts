// Small fakes of the iroh-do Endpoint, Connection and stream types, with the member's side of
// each connection, for tests that drive the chat protocols without a network. They implement
// what the chat object uses and behave as PROTOCOL.md and packages/iroh-do/README.md describe
// the real ones: a stream ends when its writer finishes it, a closed connection fails its
// streams and its acceptBi, and `read` resolves with an empty chunk at the end.
import type { Connection, Endpoint } from "iroh-do";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** Resolves waiters when something changes, for state that tests and fakes poll. */
class Changes {
  #waiters: (() => void)[] = [];

  notify(): void {
    for (const wake of this.#waiters.splice(0)) wake();
  }

  /** Resolves once `ready()` returns a value other than undefined, and with it. */
  async until<T>(ready: () => T | undefined): Promise<T> {
    for (;;) {
      const value = ready();
      if (value !== undefined) return value;
      await new Promise<void>((resolve) => this.#waiters.push(resolve));
    }
  }
}

/** An endpoint id; the fake `iroh-do` module exports this class as `EndpointId`. */
export class EndpointId {
  readonly #z32: string;

  constructor(z32: string) {
    this.#z32 = z32;
  }

  static fromZ32(z32: string): EndpointId {
    return new EndpointId(z32);
  }

  toZ32(): string {
    return this.#z32;
  }
}

/**
 * One direction of a stream: the writer's SendStream and the reader's RecvStream in one
 * object. A test holds the side the object does not.
 */
export class Pipe {
  readonly #changes = new Changes();
  #chunks: Uint8Array[] = [];
  #state: "open" | "finished" | "reset" = "open";
  #resetCode: bigint | undefined;
  #failure: Error | undefined;
  /** While set, writes wait for it: a reader that does not read. */
  stall: Promise<void> | undefined;

  // ---- SendStream ----

  async writeAll(buf: Uint8Array): Promise<void> {
    this.#check();
    if (this.stall !== undefined) await this.stall;
    this.#check();
    this.#chunks.push(buf.slice());
    this.#changes.notify();
  }

  async finish(): Promise<void> {
    this.#check();
    this.#state = "finished";
    this.#changes.notify();
  }

  async reset(code: bigint): Promise<void> {
    this.#check();
    this.#state = "reset";
    this.#resetCode = code;
    this.#changes.notify();
  }

  // ---- RecvStream ----

  /** Up to `limit` bytes; an empty chunk at the end. */
  async read(limit: number): Promise<Uint8Array> {
    return this.#changes.until(() => {
      if (this.#failure !== undefined) throw this.#failure;
      if (this.#state === "reset") throw new Error(`stream reset with code ${this.#resetCode}`);
      const chunk = this.#chunks[0];
      if (chunk !== undefined) {
        if (chunk.length <= limit) return this.#chunks.shift();
        this.#chunks[0] = chunk.subarray(limit);
        return chunk.subarray(0, limit);
      }
      return this.#state === "finished" ? new Uint8Array(0) : undefined;
    });
  }

  /** Everything up to the end; rejects if that is more than `limit` bytes. */
  async readToEnd(limit: number): Promise<Uint8Array> {
    const bytes = await this.#changes.until(() => {
      if (this.#failure !== undefined) throw this.#failure;
      if (this.#state === "reset") throw new Error(`stream reset with code ${this.#resetCode}`);
      return this.#state === "finished" ? concat(this.#chunks) : undefined;
    });
    if (bytes.length > limit) throw new Error(`read limit of ${limit} bytes exceeded`);
    return bytes;
  }

  // ---- For tests ----

  /** The text written once the writer has finished; rejects if it reset the stream. */
  async text(): Promise<string> {
    return decoder.decode(await this.readToEnd(Number.MAX_SAFE_INTEGER));
  }

  /** Whether the writer finished, reset, or neither, so far. */
  get state(): "open" | "finished" | "reset" {
    return this.#state;
  }

  get resetCode(): bigint | undefined {
    return this.#resetCode;
  }

  /** The text written so far, finished or not. */
  get written(): string {
    return decoder.decode(concat(this.#chunks));
  }

  /** Fails both sides, as a closed connection does. */
  fail(error: Error): void {
    this.#failure ??= error;
    this.#changes.notify();
  }

  #check(): void {
    if (this.#failure !== undefined) throw this.#failure;
    if (this.#state !== "open") throw new Error(`stream already ${this.#state}`);
  }
}

/** A bidirectional stream as the side that accepted it sees it. */
export interface BiPipes {
  send: Pipe;
  recv: Pipe;
}

/**
 * A connection between the object and the member `remote`. The Connection methods are the
 * object's side; `request`, `openBi`, `nextUni` and `closeByPeer` are the member's.
 */
export class FakeConnection {
  readonly #remote: string;
  readonly #changes = new Changes();
  readonly #incoming: BiPipes[] = [];
  readonly #pipes: Pipe[] = [];
  /** Unidirectional streams the object opened, oldest first, read or not. */
  readonly unis: Pipe[] = [];
  #unisTaken = 0;
  #closeReason: string | null = null;
  /** When set, `openUni` rejects with it: a connection that fails to open streams. */
  openUniError: Error | undefined;
  /** When set, writes on the unidirectional streams the object opens wait for it. */
  stallUnis: Promise<void> | undefined;

  constructor(remote: string) {
    this.#remote = remote;
  }

  // ---- Connection, the object's side ----

  remoteId(): EndpointId {
    return new EndpointId(this.#remote);
  }

  async acceptBi(): Promise<BiPipes> {
    return this.#changes.until(() => {
      const next = this.#incoming.shift();
      if (next !== undefined) return next;
      if (this.#closeReason !== null) throw new Error(`connection lost: ${this.#closeReason}`);
      return undefined;
    });
  }

  async openUni(): Promise<Pipe> {
    if (this.#closeReason !== null) throw new Error(`connection lost: ${this.#closeReason}`);
    if (this.openUniError !== undefined) throw this.openUniError;
    const pipe = this.#pipe();
    pipe.stall = this.stallUnis;
    this.unis.push(pipe);
    this.#changes.notify();
    return pipe;
  }

  closeReason(): string | null {
    return this.#closeReason;
  }

  close(code: bigint, reason: Uint8Array): void {
    this.#close(`closed by object: ${code} ${decoder.decode(reason)}`);
  }

  async closed(): Promise<string> {
    return this.#changes.until(() => this.#closeReason ?? undefined);
  }

  // ---- The member's side ----

  /** Opens a stream for one room request, finishes it, and resolves with the reply text. */
  async request(line: string | Uint8Array): Promise<string> {
    const { send, recv } = this.openBi();
    await send.writeAll(typeof line === "string" ? encoder.encode(line) : line);
    await send.finish();
    return recv.text();
  }

  /** Opens a bidirectional stream: the member writes `send` and reads `recv`. */
  openBi(): BiPipes {
    if (this.#closeReason !== null) throw new Error("connection closed");
    const toObject = this.#pipe();
    const toMember = this.#pipe();
    this.#incoming.push({ send: toMember, recv: toObject });
    this.#changes.notify();
    return { send: toObject, recv: toMember };
  }

  /** The text of the next unidirectional stream the object opens, once it is finished. */
  async nextUni(): Promise<string> {
    const pipe = await this.#changes.until(() => this.unis[this.#unisTaken]);
    this.#unisTaken++;
    return pipe.text();
  }

  closeByPeer(reason = "peer closed"): void {
    this.#close(reason);
  }

  #pipe(): Pipe {
    const pipe = new Pipe();
    this.#pipes.push(pipe);
    return pipe;
  }

  #close(reason: string): void {
    if (this.#closeReason !== null) return;
    this.#closeReason = reason;
    for (const pipe of this.#pipes) pipe.fail(new Error(`connection lost: ${reason}`));
    this.#changes.notify();
  }
}

/** The object's endpoint: which members hold a relay socket, and the connections it dials. */
export class FakeEndpoint {
  readonly #id: string;
  readonly #changes = new Changes();
  /** Members holding a relay socket to the object. */
  readonly present = new Set<string>();
  /** Every dial, in order, successful or not. */
  readonly dials: { peer: string; alpn: string; conn: FakeConnection | undefined }[] = [];
  /** When set, `peers` throws it. */
  peersError: Error | undefined;
  /** Called with each connection the object dials, before `connect` resolves with it. */
  onDial: ((conn: FakeConnection) => void) | undefined;

  constructor(id: string) {
    this.#id = id;
  }

  id(): EndpointId {
    return new EndpointId(this.#id);
  }

  peers(): EndpointId[] {
    if (this.peersError !== undefined) throw this.peersError;
    return [...this.present].map((peer) => new EndpointId(peer));
  }

  async connect(peer: EndpointId, alpn: string): Promise<FakeConnection> {
    const z32 = peer.toZ32();
    if (!this.present.has(z32)) {
      this.dials.push({ peer: z32, alpn, conn: undefined });
      this.#changes.notify();
      throw new Error(`no relay socket for ${z32}`);
    }
    const conn = new FakeConnection(z32);
    this.onDial?.(conn);
    this.dials.push({ peer: z32, alpn, conn });
    this.#changes.notify();
    return conn;
  }

  /** The connection of the next successful dial to `peer` with `alpn`. */
  async dialed(peer: string, alpn: string): Promise<FakeConnection> {
    return this.#changes.until(() => this.dials.find((d) => d.peer === peer && d.alpn === alpn)?.conn);
  }
}

/** The fakes as the iroh-do types the chat object takes; they implement the parts it uses. */
export function asEndpoint(endpoint: FakeEndpoint): Endpoint {
  return endpoint as unknown as Endpoint;
}

export function asConnection(conn: FakeConnection): Connection {
  return conn as unknown as Connection;
}

function concat(chunks: readonly Uint8Array[]): Uint8Array {
  const out = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
  let at = 0;
  for (const chunk of chunks) {
    out.set(chunk, at);
    at += chunk.length;
  }
  return out;
}
