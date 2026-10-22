// The chat room app: a small chat room per endpoint (`iroh-do/room/1`, design note 008), and
// the diagnostic protocols that exercise the lifecycle (`iroh-do/ctl/1`, `iroh-do/echo/1`,
// `iroh-do/notify/1`, design notes 006 and 007). A port of examples/chat/app/src/{lib,room,
// ctl}.rs: the same replies, limits, error strings and log records.
//
// Members do not hold connections, so that the object can hibernate between messages. A
// member opens a connection to send a request, and the app opens one to deliver events, each
// reusing an open connection to the other side if there is one. A connection the room keeps
// for reuse has its handler's promise pending until it closes (design note 013, "Connection
// lifetime"); both kinds close when idle like any other connection.
//
// A member is present while it has an authenticated relay socket to the object: that socket
// is the only path the app can dial. The app delivers each new event to every joined, present
// member except its author, batching events produced within DELIVERY_BATCH_MS on one
// unidirectional stream. A failed delivery is not retried; members catch up with `/since`.
import type { BiStream, Connection, Protocols, RecvStream } from "iroh-do";
import * as store from "./store";
import type { LogKind, LogRow, Member } from "./store";

/** ALPN of the echo protocol: each bi stream is echoed back. */
export const ECHO_ALPN = "iroh-do/echo/1";
/** ALPN of the chat room. */
export const ROOM_ALPN = "iroh-do/room/1";
/** ALPN of the diagnostic protocol. */
export const CTL_ALPN = "iroh-do/ctl/1";
/** ALPN the app dials a peer with for a dial-back. */
export const NOTIFY_ALPN = "iroh-do/notify/1";
/**
 * Largest delay, in seconds, a peer may request with `/callback`. A dial-back needs the
 * peer's relay socket, and iroh's relay client closes a socket to a relay other than its home
 * relay 60 s after its last send.
 */
export const MAX_CALLBACK_SECS = 50;
/** How long the app collects new room events for one member before opening a stream. */
export const DELIVERY_BATCH_MS = 50;
/** Longest record text stored in the log, in bytes. */
export const MAX_LOG_TEXT = 1024;
const CALLBACK_KEY = "callback:";
/** How long after its newest record expires a quiet log is pruned. */
const PRUNE_SLACK_MS = 60_000;

// room.rs
const MAX_REQUEST = 1024;
const HISTORY_DEFAULT = 50;
const HISTORY_MAX = 200;
const SINCE_MAX = 500;
const RATE_LIMIT = 10;
/**
 * Window of the request rate limit. In memory, as in the Rust app: the object hibernates
 * only after t_idle + t_linger without connections, which is longer than the window.
 */
const RATE_WINDOW_MS = 10_000;
const NICK_MAX = 24;
const ROOM_COMMANDS = new Set(["/join", "/say", "/me", "/nick", "/who", "/history", "/since", "/quit"]);

// ctl.rs
const LOG_DEFAULT = 5;
const LOG_MAX = 50;
const MAX_LINE = 4096;
const NOTIFY_LINGER_MS = 5_000;
/** QUIC application close code 0 (`iroh_do_core::CLOSE_OK`). */
const CLOSE_OK = 0n;

/**
 * Largest `usize` of the Rust app as deployed (wasm32), which bounds the numbers `/history`
 * and `/log` accept.
 */
const USIZE_MAX = 2n ** 32n - 1n;
const U64_MAX = 2n ** 64n - 1n;

const Z32 = /^[ybndrfg8ejkmcpqxot1uwisza345h769]{52}$/;
const encoder = new TextEncoder();
// ignoreBOM keeps a leading BOM in the text, as Rust's String::from_utf8 does.
const utf8 = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });

/** Counters of the chat app since its node was built (`ChatStats` in the Rust app). */
export interface ChatStats {
  dial_backs_ok: number;
  dial_backs_failed: number;
  deliveries_ok: number;
  deliveries_failed: number;
  delivery_dials: number;
  delivery_reuses: number;
  rate_limited: number;
}

/** What the app needs from its object. */
export interface ChatHost {
  /** This endpoint's id, z-base-32. */
  id(): string;
  sql: SqlStorage;
  /** The peers holding a relay socket to the object, z-base-32. */
  peers(): Set<string>;
  connect(peer: string, alpn: string): Promise<Connection>;
  schedule(key: string, delayMs: number, payload: string): void;
  /** The endpoint's status, as `/status` returns it. */
  status(): Record<string, unknown>;
}

type Level = "error" | "warn" | "info";

/** One log line in the format of design note 010, with the Rust app's module as target. */
function log(level: Level, target: string, message: string, fields: Record<string, unknown> = {}): void {
  const line = { level, target: `iroh_do_chat_app${target}`, message, ...fields };
  if (level === "error") console.error(line);
  else if (level === "warn") console.warn(line);
  else console.log(line);
}

const event = (level: Level, target: string, name: string, fields: Record<string, unknown>) =>
  log(level, target, name, { event: name, ...fields });

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

const RUST_WS = "[\\t\\n\\v\\f\\r \\u0085\\u00a0\\u1680\\u2000-\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000]";
const RUST_TRIM = new RegExp(`^${RUST_WS}+|${RUST_WS}+$`, "g");

/** Rust's `str::trim`: Unicode White_Space, which differs from JavaScript's `trim`. */
function rustTrim(s: string): string {
  return s.replace(RUST_TRIM, "");
}

/** Rust's `str::parse` for an unsigned integer type with maximum `max`: digits, an optional `+`. */
function parseUnsigned(s: string, max: bigint): number | null {
  if (!/^\+?[0-9]+$/.test(s)) return null;
  const v = BigInt(s.startsWith("+") ? s.slice(1) : s);
  return v > max ? null : Number(v);
}

/** The longest prefix of `s` that is at most `max` UTF-8 bytes and ends on a char boundary. */
export function truncate(s: string, max: number): string {
  const b = encoder.encode(s);
  if (b.length <= max) return s;
  let end = max;
  while (end > 0 && (b[end] & 0xc0) === 0x80) end--;
  return utf8.decode(b.subarray(0, end));
}

/** `s` as the inside of a JSON string, escaped as the Rust app's `json_escape_into` does. */
export function jsonEscape(s: string): string {
  let out = "";
  for (const c of s) {
    const code = c.codePointAt(0)!;
    if (c === '"') out += '\\"';
    else if (c === "\\") out += "\\\\";
    else if (code < 0x20) out += `\\u${code.toString(16).padStart(4, "0")}`;
    else out += c;
  }
  return out;
}

function err(text: string): string {
  return `{"t":"err","text":"${jsonEscape(text)}"}`;
}

/** Encodes a stored room event as note 008's JSON line. */
export function eventJson(r: LogRow, hist: boolean): string {
  let out = `{"t":"${r.kind}","seq":${r.seq},"prev":${r.prev},"ts":${r.ts_ms},"peer":"${r.peer}"`;
  const field = (name: string, value: string) => (out += `,"${name}":"${jsonEscape(value)}"`);
  switch (r.kind) {
    case "nick":
      field("old", r.text);
      field("new", r.nick);
      break;
    case "join":
      field("nick", r.nick);
      break;
    case "part":
      field("nick", r.nick);
      field("reason", r.text);
      break;
    default:
      field("nick", r.nick);
      field("text", r.text);
  }
  if (hist) out += ',"hist":true';
  return out + "}";
}

/** The request line without its terminator, or the error reply's text. */
export function parseRequest(bytes: Uint8Array): { line: string } | { error: string } {
  let end = bytes.length;
  if (end > 0 && bytes[end - 1] === 0x0a) end--;
  if (end > 0 && bytes[end - 1] === 0x0d) end--;
  if (end > MAX_REQUEST) return { error: "request too long" };
  const line = bytes.subarray(0, end);
  if (line.includes(0x0a)) return { error: "one request per stream" };
  try {
    return { line: utf8.decode(line) };
  } catch {
    return { error: "request is not utf-8" };
  }
}

/** The trimmed argument if `line` is `cmd` or `cmd <arg>`, else null. */
export function commandArg(line: string, cmd: string): string | null {
  if (!line.startsWith(cmd)) return null;
  const rest = line.slice(cmd.length);
  if (rest === "") return "";
  return rest.startsWith(" ") ? rustTrim(rest) : null;
}

function asciiLower(s: string): string {
  return s.replace(/[A-Z]/g, (c) => c.toLowerCase());
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  const out = new Uint8Array(a.length + b.length);
  out.set(a);
  out.set(b, a.length);
  return out;
}

/** Reads `\n`-terminated UTF-8 lines from a stream (`LineReader` in ctl.rs). */
class LineReader {
  #buf: Uint8Array = new Uint8Array(0);
  #eof = false;
  constructor(private recv: RecvStream) {}

  /**
   * The next complete line, without `\n` or `\r\n`; at the end of the stream a final
   * unterminated line counts as complete. Lines longer than MAX_LINE bytes are an error,
   * whichever read delivered their terminator.
   */
  #take(): string | null {
    const buf = this.#buf;
    let end = buf.indexOf(0x0a);
    if (end < 0) {
      // Room for a `\r` that may precede the terminator still to come.
      if (buf.length > MAX_LINE + 1) throw new Error("line too long");
      if (!(this.#eof && buf.length > 0)) return null;
      end = buf.length;
    }
    let line = buf.subarray(0, end);
    this.#buf = buf.subarray(Math.min(end + 1, buf.length));
    if (line.length > 0 && line[line.length - 1] === 0x0d) line = line.subarray(0, line.length - 1);
    if (line.length > MAX_LINE) throw new Error("line too long");
    try {
      return utf8.decode(line);
    } catch {
      throw new Error("line is not utf-8");
    }
  }

  /** The next line, or null at the end of the stream. */
  async next(): Promise<string | null> {
    for (;;) {
      const line = this.#take();
      if (line !== null) return line;
      if (this.#eof) return null;
      const chunk = await this.recv.read(1024);
      if (chunk.length === 0) this.#eof = true;
      else this.#buf = concat(this.#buf, chunk);
    }
  }
}

/** Waits for `p` or `ms`, whichever comes first, and cancels the timer. */
async function within(p: Promise<unknown>, ms: number): Promise<void> {
  let timer: ReturnType<typeof setTimeout> | null = null;
  await Promise.race([p.catch(() => undefined), new Promise((r) => (timer = setTimeout(r, ms)))]);
  if (timer !== null) clearTimeout(timer);
}

function liveStableId(c: Connection): number | null {
  try {
    return c.stableId();
  } catch {
    // A connection of a wasm instance that has since been replaced.
    return null;
  }
}

function isOpen(c: Connection): boolean {
  try {
    return c.closeReason() == null;
  } catch {
    return false;
  }
}

/** One chat app per node build (`ChatApp` in the Rust app). */
export class ChatApp {
  readonly stats: ChatStats = {
    dial_backs_ok: 0,
    dial_backs_failed: 0,
    deliveries_ok: 0,
    deliveries_failed: 0,
    delivery_dials: 0,
    delivery_reuses: 0,
    rate_limited: 0,
  };
  /** Open room connections by peer, whichever side dialed. The newest one wins. */
  #conns = new Map<string, Connection>();
  /** Event lines waiting for delivery, by recipient. Present while its delivery runs. */
  #outbox = new Map<string, string[]>();
  /** Recent request times by peer, for the rate limit. */
  #requests = new Map<string, number[]>();

  constructor(private host: ChatHost) {}

  protocols(): Protocols {
    return {
      [ROOM_ALPN]: { accept: (c) => this.#serveRoom(c) },
      [CTL_ALPN]: { accept: (c) => this.#serveCtl(c) },
      [ECHO_ALPN]: { accept: (c) => this.#serveEcho(c) },
    };
  }

  /** The app's status fields. */
  status(): Record<string, unknown> {
    const records = this.#sql("log_total", store.logTotal) ?? 0;
    const members = this.#sql("members", store.members) ?? [];
    const peers = this.host.peers();
    return { records, members: members.length, present: members.filter((m) => peers.has(m.peer)).length };
  }

  /** Runs work scheduled with `host.schedule`. */
  onAlarm(key: string, _payload: string): void {
    if (key.startsWith(CALLBACK_KEY)) {
      const peer = key.slice(CALLBACK_KEY.length);
      if (!Z32.test(peer)) event("warn", "", "bad_schedule_key", { key });
      // Spawned, as the Rust app does, so that other due work does not wait for it.
      else void this.#dialNotify(peer);
    } else if (key === store.PRUNE_KEY) {
      this.#sql("log_prune", (s) => store.logPrune(s, Date.now()));
    }
  }

  /** Runs `f` on the database; logs and returns null if it fails. */
  #sql<T>(what: string, f: (s: SqlStorage) => T): T | null {
    try {
      return f(this.host.sql);
    } catch (e) {
      event("warn", "", "chat_sql_failed", { what, error: errorText(e) });
      return null;
    }
  }

  /**
   * Appends a record, truncating `text` to MAX_LOG_TEXT bytes; returns the stored record.
   * Inserting prunes the log; a log that stops growing is pruned once more when its newest
   * record expires, from the alarm this schedules.
   */
  #append(peer: string, kind: LogKind, nick: string, text: string): LogRow | null {
    const t = truncate(text, MAX_LOG_TEXT);
    const ts = Date.now();
    const r = this.#sql("log_insert", (s) => store.logInsert(s, ts, peer, kind, nick, t));
    if (!r) return null;
    this.host.schedule(store.PRUNE_KEY, store.LOG_MAX_AGE_MS + PRUNE_SLACK_MS, "");
    return { seq: r.seq, prev: r.prev, ts_ms: ts, peer, kind, nick, text: t };
  }

  #member(peer: string): Member | null {
    return this.#sql("member", (s) => store.member(s, peer));
  }

  #members(): Member[] {
    return this.#sql("members", store.members) ?? [];
  }

  #setMember(m: Member): void {
    this.#sql("set_member", (s) => store.setMember(s, m, Date.now()));
  }

  #eventsRecent(n: number): LogRow[] {
    return this.#sql("events_recent", (s) => store.eventsRecent(s, n)) ?? [];
  }

  // ---- iroh-do/room/1 -------------------------------------------------------------------

  /** Serves requests on a room connection, accepted or dialed, until it closes. */
  async #serveRoom(conn: Connection): Promise<void> {
    const peer = conn.remoteId().toZ32();
    const id = conn.stableId();
    this.#conns.set(peer, conn);
    for (;;) {
      let bi: BiStream;
      try {
        bi = await conn.acceptBi();
      } catch {
        break;
      }
      void this.#requestStream(peer, bi);
    }
    const cur = this.#conns.get(peer);
    if (cur && liveStableId(cur) === id) this.#conns.delete(peer);
  }

  /** Reads one request, handles it and writes the reply. */
  async #requestStream(peer: string, bi: BiStream): Promise<void> {
    let replies: string[];
    let bytes: Uint8Array | null = null;
    try {
      // Room for a `\r\n` terminator.
      bytes = await bi.recv.readToEnd(MAX_REQUEST + 2);
    } catch {
      replies = [err("request too long or unreadable")];
    }
    if (bytes !== null) {
      const p = parseRequest(bytes);
      replies = "line" in p ? this.#handle(peer, p.line) : [err(p.error)];
    }
    const out = replies!.map((r) => r + "\n").join("");
    try {
      await bi.send.writeAll(encoder.encode(out));
      await bi.send.finish();
    } catch {
      // The member went away; nothing to tell it.
    }
  }

  /** Handles one request and logs it as `room_request`: the kind and nick, never the text. */
  #handle(peer: string, line: string): string[] {
    const replies = this.#handleRequest(peer, line);
    const cmd = line.split(" ")[0];
    const kind = ROOM_COMMANDS.has(cmd) ? cmd.slice(1) : "unknown";
    const error = replies[0]?.startsWith('{"t":"err"') ?? false;
    const nick = this.#member(peer)?.nick ?? "";
    event("info", "::room", "room_request", { peer, kind, nick, ok: !error });
    return replies;
  }

  #handleRequest(peer: string, line: string): string[] {
    if (!this.#allow(peer)) {
      this.stats.rate_limited++;
      return [err(`rate limit: at most ${RATE_LIMIT} requests per ${RATE_WINDOW_MS / 1000}s`)];
    }
    const m = this.#member(peer);
    const me = m?.joined ? m : null;
    const sp = line.indexOf(" ");
    const [cmd, arg] = sp >= 0 ? [line.slice(0, sp), rustTrim(line.slice(sp + 1))] : [line, ""];
    if (cmd === "/join") return this.#join(peer, me, arg);
    if (!me) return [err("not joined: send /join [nick] first")];
    switch (cmd) {
      case "/say":
      case "/me":
        if (arg === "") return [err(`usage: ${cmd} <text>`)];
        return this.#storeAndPublish(peer, cmd === "/say" ? "msg" : "act", me.nick, arg);
      case "/nick": {
        const bad = this.#checkNick(peer, arg);
        if (bad) return [err(bad)];
        if (arg === me.nick) return [err("nick unchanged")];
        const old = me.nick;
        this.#setMember({ ...me, nick: arg });
        return this.#storeAndPublish(peer, "nick", arg, old);
      }
      case "/who":
        return [this.#who()];
      case "/history": {
        const n = arg === "" ? HISTORY_DEFAULT : parseUnsigned(arg, USIZE_MAX);
        if (n === null) return [err(`usage: /history [n], n <= ${HISTORY_MAX}`)];
        return this.#eventsRecent(Math.min(n, HISTORY_MAX)).map((r) => eventJson(r, true));
      }
      case "/since": {
        const seq = parseUnsigned(arg, U64_MAX);
        if (seq === null) return [err("usage: /since <seq>")];
        return (this.#sql("events_after", (s) => store.eventsAfter(s, seq, SINCE_MAX)) ?? []).map((r) => eventJson(r, true));
      }
      case "/quit":
        this.#setMember({ ...me, joined: false });
        this.#outbox.delete(peer);
        return this.#storeAndPublish(peer, "part", me.nick, arg);
      default:
        return [err("unknown request")];
    }
  }

  /** Records a request from `peer`; returns whether it is within the rate limit. */
  #allow(peer: string): boolean {
    const now = Date.now();
    let times = this.#requests.get(peer);
    if (!times) this.#requests.set(peer, (times = []));
    while (times.length > 0 && now - times[0] >= RATE_WINDOW_MS) times.shift();
    if (times.length >= RATE_LIMIT) return false;
    times.push(now);
    return true;
  }

  #join(peer: string, me: Member | null, arg: string): string[] {
    let you: Member;
    if (me) {
      if (arg === "" || arg === me.nick) {
        you = me;
      } else {
        // Already joined: `/join <nick>` with another nick renames.
        const bad = this.#checkNick(peer, arg);
        if (bad) return [err(bad)];
        const old = me.nick;
        you = { ...me, nick: arg };
        this.#setMember(you);
        this.#storeAndPublish(peer, "nick", arg, old);
      }
    } else {
      let nick: string;
      if (arg === "") {
        nick = this.#defaultNick(peer);
      } else {
        const bad = this.#checkNick(peer, arg);
        if (bad) return [err(bad)];
        nick = arg;
      }
      you = { peer, nick, joined: true };
      this.#setMember(you);
      const stored = this.#storeAndPublish(peer, "join", nick, "");
      if (stored[0]?.startsWith('{"t":"err"')) return stored;
    }
    const replies = this.#eventsRecent(HISTORY_DEFAULT).map((r) => eventJson(r, true));
    const roomSeq = this.#sql("room_seq", store.roomSeq) ?? 0;
    replies.push(`{"t":"ready","room":"${this.host.id()}","you":{"peer":"${peer}","nick":"${jsonEscape(you.nick)}"},"last_seq":${roomSeq}}`);
    return replies;
  }

  /** A previous nick if it is still free, else a prefix of the member's EndpointId. */
  #defaultNick(peer: string): string {
    const m = this.#member(peer);
    if (m && this.#checkNick(peer, m.nick) === null) return m.nick;
    const base = peer.slice(0, 8);
    if (this.#checkNick(peer, base) === null) return base;
    for (let i = 2; ; i++) {
      const n = `${base}-${i}`;
      if (this.#checkNick(peer, n) === null) return n;
    }
  }

  /** Null if `nick` is valid and not used by another joined member, else the error. */
  #checkNick(peer: string, nick: string): string | null {
    if (!/^[A-Za-z0-9_.-]{1,24}$/.test(nick)) return `invalid nick: 1-${NICK_MAX} characters of A-Z a-z 0-9 _ . -`;
    const lower = asciiLower(nick);
    if (this.#members().some((m) => m.peer !== peer && asciiLower(m.nick) === lower)) return `nick ${nick} is taken`;
    return null;
  }

  /** Stores an event, queues it for every other joined, present member; the `ok` reply. */
  #storeAndPublish(peer: string, kind: LogKind, nick: string, text: string): string[] {
    const row = this.#append(peer, kind, nick, text);
    if (!row) return [err("storage error")];
    const line = eventJson(row, false);
    const present = this.host.peers();
    let recipients = 0;
    for (const m of this.#members()) {
      if (m.peer === peer || !present.has(m.peer)) continue;
      recipients++;
      const queue = this.#outbox.get(m.peer);
      if (queue) {
        queue.push(line);
      } else {
        this.#outbox.set(m.peer, [line]);
        this.#spawnDelivery(m.peer);
      }
    }
    event("info", "::room", "room_event", { peer, kind, seq: row.seq, recipients });
    return [`{"t":"ok","seq":${row.seq},"prev":${row.prev}}`];
  }

  /**
   * Starts the delivery of `peer`'s queue after the batch window. The endpoint's linger (2 s
   * by default) outlasts the window, so it stays bound for the dial.
   */
  #spawnDelivery(peer: string): void {
    setTimeout(async () => {
      for (;;) {
        const q = this.#outbox.get(peer);
        if (!q || q.length === 0) {
          this.#outbox.delete(peer);
          break;
        }
        const lines = q.splice(0);
        const t0 = Date.now();
        const [ok, via] = await this.#deliver(peer, lines);
        const fields = { peer, events: lines.length, via, ok, ms: Date.now() - t0 };
        event(ok ? "info" : "warn", "::room", "room_delivery", fields);
        if (ok) this.stats.deliveries_ok++;
        else this.stats.deliveries_failed++;
      }
    }, DELIVERY_BATCH_MS);
  }

  /**
   * Writes `lines` to `peer` on one uni stream; returns whether the stream was finished, and
   * the connection used: `open` (reused), `dial` (new) or `none` (no path).
   */
  async #deliver(peer: string, lines: string[]): Promise<[boolean, string]> {
    const payload = lines.map((l) => l + "\n").join("");
    const open = this.#conns.get(peer);
    if (open && isOpen(open)) {
      if (await this.#writeUni(open, payload)) {
        this.stats.delivery_reuses++;
        return [true, "open"];
      }
    }
    if (!this.host.peers().has(peer)) return [false, "none"];
    let conn: Connection;
    try {
      conn = await this.host.connect(peer, ROOM_ALPN);
    } catch (e) {
      log("warn", "::room", `room: dial failed: ${errorText(e)}`, { peer: peer.slice(0, 10) });
      return [false, "dial"];
    }
    this.stats.delivery_dials++;
    // The member may send requests on this connection too.
    void this.#serveRoom(conn);
    return [await this.#writeUni(conn, payload), "dial"];
  }

  async #writeUni(conn: Connection, payload: string): Promise<boolean> {
    try {
      const s = await conn.openUni();
      await s.writeAll(encoder.encode(payload));
      await s.finish();
      return true;
    } catch {
      return false;
    }
  }

  #who(): string {
    const present = this.host.peers();
    const items = this.#members().map((m) => `{"peer":"${m.peer}","nick":"${jsonEscape(m.nick)}","present":${present.has(m.peer)}}`);
    return `{"t":"who","members":[${items.join(",")}]}`;
  }

  // ---- iroh-do/echo/1 -------------------------------------------------------------------

  async #serveEcho(conn: Connection): Promise<void> {
    const peer = conn.remoteId().toZ32();
    for (;;) {
      let bi: BiStream;
      try {
        bi = await conn.acceptBi();
      } catch {
        break;
      }
      void this.#echoStream(bi).then((n) => this.#append(peer, "echo", "", `${n} bytes`));
    }
  }

  /** Echoes one stream; returns the number of bytes echoed. */
  async #echoStream(bi: BiStream): Promise<number> {
    let total = 0;
    for (;;) {
      let chunk: Uint8Array;
      try {
        chunk = await bi.recv.read(16 * 1024);
      } catch {
        return total;
      }
      if (chunk.length === 0) break;
      try {
        await bi.send.writeAll(chunk);
      } catch {
        return total;
      }
      total += chunk.length;
    }
    try {
      await bi.send.finish();
    } catch {
      // Already finished or reset by the peer's stop.
    }
    return total;
  }

  // ---- iroh-do/ctl/1 --------------------------------------------------------------------

  async #serveCtl(conn: Connection): Promise<void> {
    const hello = { sent: false };
    for (;;) {
      let bi: BiStream;
      try {
        bi = await conn.acceptBi();
      } catch {
        break;
      }
      void this.#ctlStream(conn, hello, bi);
    }
  }

  async #ctlStream(conn: Connection, hello: { sent: boolean }, bi: BiStream): Promise<void> {
    const lines = new LineReader(bi.recv);
    for (;;) {
      let line: string | null;
      try {
        line = await lines.next();
      } catch (e) {
        try {
          await bi.send.writeAll(encoder.encode(`error: ${errorText(e)}\n`));
        } catch {
          // The peer is gone.
        }
        break;
      }
      if (line === null) break;
      const reply = this.#respond(conn, line) + "\n";
      try {
        await bi.send.writeAll(encoder.encode(reply));
      } catch {
        return;
      }
      if (!hello.sent) {
        hello.sent = true;
        void this.#sendHello(conn);
      }
    }
    try {
      await bi.send.finish();
    } catch {
      // Already finished or reset.
    }
  }

  /**
   * Opens a uni stream to the peer and writes one greeting line. Sent once per ctl
   * connection, to show that the object can open streams on a connection the peer dialed.
   */
  async #sendHello(conn: Connection): Promise<void> {
    try {
      const line = `hello from ${this.host.id()}: you are ${conn.remoteId().toZ32()}\n`;
      const s = await conn.openUni();
      await s.writeAll(encoder.encode(line));
      await s.finish();
    } catch {
      // The connection closed first.
    }
  }

  /** The reply line, without `\n`, to one ctl request line. */
  #respond(conn: Connection, line: string): string {
    const peer = conn.remoteId().toZ32();
    if (line === "/status") return JSON.stringify(this.host.status());
    if (line === "/count") {
      const c = this.#sql("count", (s) => [store.logTotal(s), store.logYours(s, peer), store.logPeers(s)]) ?? [0, 0, 0];
      return `{"total":${c[0]},"yours":${c[1]},"peers":${c[2]}}`;
    }
    const logArg = commandArg(line, "/log");
    if (logArg !== null) {
      const n = logArg === "" ? LOG_DEFAULT : parseUnsigned(logArg, USIZE_MAX);
      if (n === null) return `error: usage /log [n], n <= ${LOG_MAX}`;
      const rows = this.#sql("log_recent", (s) => store.logRecent(s, Math.min(n, LOG_MAX))) ?? [];
      const items = rows.map((r) => `{"seq":${r.seq},"ts_ms":${r.ts_ms},"peer":"${jsonEscape(r.peer)}","kind":"${jsonEscape(r.kind)}","text":"${jsonEscape(r.text)}"}`);
      return `[${items.join(",")}]`;
    }
    const cbArg = commandArg(line, "/callback");
    if (cbArg !== null) {
      const secs = parseUnsigned(cbArg, U64_MAX);
      if (secs === null || secs > MAX_CALLBACK_SECS) return `error: usage /callback <secs>, secs <= ${MAX_CALLBACK_SECS}`;
      this.host.schedule(CALLBACK_KEY + peer, secs * 1000, "");
      return `ok: callback in ${secs}s`;
    }
    const seq = this.#append(peer, "ctl", "", line)?.seq ?? 0;
    const yours = this.#sql("log_yours", (s) => store.logYours(s, peer)) ?? 0;
    // `seq` numbers every record ever inserted, so it is also the total.
    return `echo: ${line} (#${seq}, total ${seq}, yours ${yours})`;
  }

  // ---- iroh-do/notify/1 -----------------------------------------------------------------

  /**
   * Dials `peer` with the notify ALPN over its relay socket and writes one line on a uni
   * stream. Every attempt is logged, including those that fail before dialing. Waits up to
   * NOTIFY_LINGER_MS for the peer to close the connection, then closes it.
   */
  async #dialNotify(peer: string): Promise<void> {
    const started = Date.now();
    const fail = (error: string) => {
      event("warn", "::ctl", "dial_back", { peer, ok: false, ms: Date.now() - started, error });
      this.stats.dial_backs_failed++;
    };
    let conn: Connection;
    try {
      conn = await this.host.connect(peer, NOTIFY_ALPN);
    } catch (e) {
      fail(`connect: ${errorText(e)}`);
      this.#append(peer, "callback", "", `failed: ${errorText(e)}`);
      return;
    }
    const records = this.#sql("log_total", store.logTotal) ?? 0;
    const line = `callback from ${this.host.id()} wakes=${Number(this.host.status().wakes ?? 0)} records=${records}\n`;
    let outcome: string;
    try {
      const s = await conn.openUni();
      await s.writeAll(encoder.encode(line));
      await s.finish();
      event("info", "::ctl", "dial_back", { peer, ok: true, ms: Date.now() - started });
      this.stats.dial_backs_ok++;
      outcome = "delivered";
    } catch (e) {
      fail(`stream: ${errorText(e)}`);
      outcome = `failed: ${errorText(e)}`;
    }
    this.#append(peer, "callback", "", outcome);
    await within(conn.closed(), NOTIFY_LINGER_MS);
    try {
      conn.close(CLOSE_OK, encoder.encode("done"));
    } catch {
      // Already closed.
    }
  }
}
