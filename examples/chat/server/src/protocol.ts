// The wire format of the chat protocols, as examples/chat/common/PROTOCOL.md specifies it:
// ALPNs, limits, request parsing, and the reply, event and ctl lines. Nothing here does I/O or
// imports the platform, so the unit tests run it in Node.

export const ROOM_ALPN = "iroh-do/room/1";
export const CTL_ALPN = "iroh-do/ctl/1";
export const ECHO_ALPN = "iroh-do/echo/1";
export const NOTIFY_ALPN = "iroh-do/notify/1";

/** The limits of PROTOCOL.md; `test/protocol.test.ts` checks them against constants.json. */
export const LIMITS = {
  maxRequest: 1024,
  historyDefault: 50,
  historyMax: 200,
  sinceMax: 500,
  rateLimitRequests: 10,
  rateLimitWindowMs: 10_000,
  nickMax: 24,
  maxLogText: 1024,
  maxCallbackSecs: 50,
  deliveryBatchMs: 50,
  ctlMaxLine: 4096,
  ctlLogDefault: 5,
  ctlLogMax: 50,
  logRetentionRows: 10_000,
  logMaxAgeMs: 14 * 24 * 60 * 60 * 1000,
} as const;

// ---- Text --------------------------------------------------------------------------------

const encoder = new TextEncoder();
// `ignoreBOM` keeps a leading U+FEFF in the text: PROTOCOL.md makes it part of the line.
const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });

/** `bytes` as text, or null if they are not valid UTF-8. */
export function decodeUtf8(bytes: Uint8Array): string | null {
  try {
    return decoder.decode(bytes);
  } catch (e) {
    // A fatal TextDecoder reports malformed input, and only that, as a TypeError.
    if (e instanceof TypeError) return null;
    throw e;
  }
}

export function encodeUtf8(text: string): Uint8Array {
  return encoder.encode(text);
}

/** The longest prefix of `text` that is at most `maxBytes` of UTF-8. */
export function truncateUtf8(text: string, maxBytes: number): string {
  const { read } = encoder.encodeInto(text, new Uint8Array(maxBytes));
  // encodeInto stops before a character that does not fit, so `read` is a code point boundary.
  return text.slice(0, read);
}

const ASCII_WHITESPACE = /^[\t\n\f\r ]+|[\t\n\f\r ]+$/g;

/** `arg` without leading and trailing ASCII white space (PROTOCOL.md, "White space"). */
export function trimArgument(arg: string): string {
  return arg.replace(ASCII_WHITESPACE, "");
}

const DIGITS = /^[0-9]+$/;

/**
 * A number argument (PROTOCOL.md, "number"): its value, or `max` if the value is larger; null
 * if `arg` is not one or more ASCII digits. `max` is at most `Number.MAX_SAFE_INTEGER`.
 */
export function parseNumber(arg: string, max: number): number | null {
  if (!DIGITS.test(arg)) return null;
  // Rounding a decimal to a double is monotonic, so a value above `max` never rounds below it,
  // and values up to 2^53 are exact.
  return Math.min(Number(arg), max);
}

// ---- iroh-do/room/1 requests -------------------------------------------------------------

export const ROOM_ERRORS = {
  unreadable: "request too long or unreadable",
  tooLong: "request too long",
  onePerStream: "one request per stream",
  notUtf8: "request is not utf-8",
  rateLimited: `rate limit: at most ${LIMITS.rateLimitRequests} requests per ${LIMITS.rateLimitWindowMs / 1000}s`,
  notJoined: "not joined: send /join [nick] first",
  unknown: "unknown request",
  sayUsage: "usage: /say <text>",
  meUsage: "usage: /me <text>",
  historyUsage: `usage: /history [n], n <= ${LIMITS.historyMax}`,
  sinceUsage: "usage: /since <seq>",
  nickUnchanged: "nick unchanged",
  invalidNick: `invalid nick: 1-${LIMITS.nickMax} characters of A-Z a-z 0-9 _ . -`,
  storage: "storage error",
} as const;

export function nickTakenError(nick: string): string {
  return `nick ${nick} is taken`;
}

/** The bytes a member may send on a request stream: the request, `\r\n` and nothing more. */
export const MAX_REQUEST_STREAM = LIMITS.maxRequest + 2;

/** The request line of a request stream, or the framing error to reply with. */
export type RequestFrame = { ok: true; line: string } | { ok: false; error: string };

/** Applies the framing rules of PROTOCOL.md, "Request framing", to a whole request stream. */
export function frameRequest(bytes: Uint8Array): RequestFrame {
  let end = bytes.length;
  if (bytes[end - 1] === 0x0a) end--;
  if (bytes[end - 1] === 0x0d) end--;
  if (end > LIMITS.maxRequest) return { ok: false, error: ROOM_ERRORS.tooLong };
  const line = bytes.subarray(0, end);
  if (line.includes(0x0a)) return { ok: false, error: ROOM_ERRORS.onePerStream };
  const text = decodeUtf8(line);
  return text === null ? { ok: false, error: ROOM_ERRORS.notUtf8 } : { ok: true, line: text };
}

export type RoomCommand = "join" | "say" | "me" | "nick" | "who" | "history" | "since" | "quit";

/**
 * A parsed room request. `invalid` is a known command whose argument breaks the grammar; it
 * is answered with `error` only after the rate limit and the joined check, as every other
 * request of a member that has not joined gets `not joined`.
 */
export type RoomRequest =
  | { command: "join"; nick: string }
  | { command: "say"; text: string }
  | { command: "me"; text: string }
  | { command: "nick"; nick: string }
  | { command: "who" }
  | { command: "history"; count: number }
  | { command: "since"; after: number }
  | { command: "quit"; reason: string }
  | { command: "invalid"; of: RoomCommand; error: string }
  | { command: "unknown" };

/** Parses a request line (PROTOCOL.md, "Request grammar"). */
export function parseRoomRequest(line: string): RoomRequest {
  const space = line.indexOf(" ");
  const name = space < 0 ? line : line.slice(0, space);
  const arg = space < 0 ? "" : trimArgument(line.slice(space + 1));
  switch (name) {
    case "/join":
      return { command: "join", nick: arg };
    case "/say":
      return arg === ""
        ? { command: "invalid", of: "say", error: ROOM_ERRORS.sayUsage }
        : { command: "say", text: arg };
    case "/me":
      return arg === "" ? { command: "invalid", of: "me", error: ROOM_ERRORS.meUsage } : { command: "me", text: arg };
    case "/nick":
      return { command: "nick", nick: arg };
    case "/who":
      return { command: "who" };
    case "/history": {
      const count = arg === "" ? LIMITS.historyDefault : parseNumber(arg, LIMITS.historyMax);
      return count === null
        ? { command: "invalid", of: "history", error: ROOM_ERRORS.historyUsage }
        : { command: "history", count };
    }
    case "/since": {
      // Every seq is below 2^53, so the largest safe integer stands for every larger value.
      const after = parseNumber(arg, Number.MAX_SAFE_INTEGER);
      return after === null
        ? { command: "invalid", of: "since", error: ROOM_ERRORS.sinceUsage }
        : { command: "since", after };
    }
    case "/quit":
      return { command: "quit", reason: arg };
    default:
      return { command: "unknown" };
  }
}

/** The command name a request is logged under: `invalid` requests under their command. */
export function requestKind(request: RoomRequest): RoomCommand | "unknown" {
  return request.command === "invalid" ? request.of : request.command;
}

// ---- Nicks -------------------------------------------------------------------------------

const NICK = new RegExp(`^[A-Za-z0-9_.-]{1,${LIMITS.nickMax}}$`);

export function isValidNick(nick: string): boolean {
  return NICK.test(nick);
}

/** The key under which two nicks collide. Valid nicks are ASCII, so this folds only A-Z. */
export function nickKey(nick: string): string {
  return nick.toLowerCase();
}

/** The nicks a member joining without one is offered, in order: `abcdefgh`, `abcdefgh-2`, ... */
export function* defaultNicks(peer: string): Generator<string, never> {
  const base = peer.slice(0, 8);
  yield base;
  for (let i = 2; ; i++) yield `${base}-${i}`;
}

// ---- iroh-do/room/1 replies and events ---------------------------------------------------

interface EventHead {
  seq: number;
  prev: number;
  ts: number;
  peer: string;
}

/** A room event; `hist` is present on the lines of a `/join`, `/history` or `/since` reply. */
export type RoomEvent =
  | ({ t: "msg" | "act" } & EventHead & { nick: string; text: string; hist?: true })
  | ({ t: "join" } & EventHead & { nick: string; hist?: true })
  | ({ t: "part" } & EventHead & { nick: string; reason: string; hist?: true })
  | ({ t: "nick" } & EventHead & { old: string; new: string; hist?: true });

export interface WhoMember {
  peer: string;
  nick: string;
  present: boolean;
}

export type RoomLine =
  | RoomEvent
  | { t: "who"; members: WhoMember[] }
  | { t: "ready"; room: string; you: { peer: string; nick: string }; last_seq: number }
  | { t: "ok"; seq: number; prev: number }
  | { t: "err"; text: string };

export function errorLine(text: string): RoomLine {
  return { t: "err", text };
}

/** The history form of `event`: `hist` is written last. */
export function historyLine(event: RoomEvent): RoomEvent {
  return { ...event, hist: true };
}

/** One JSON line per value. Keys come out in the order the objects were built. */
export function encodeLines(lines: readonly RoomLine[]): string {
  return lines.map((line) => `${JSON.stringify(line)}\n`).join("");
}

// ---- iroh-do/ctl/1 -----------------------------------------------------------------------

export type CtlRequest =
  | { command: "status" }
  | { command: "count" }
  | { command: "log"; count: number }
  | { command: "callback"; secs: number }
  | { command: "echo"; line: string }
  | { command: "invalid"; error: string };

const LOG_USAGE = `error: usage /log [n], n <= ${LIMITS.ctlLogMax}`;
const CALLBACK_USAGE = `error: usage /callback <secs>, secs <= ${LIMITS.maxCallbackSecs}`;

/** The trimmed argument if `line` is `command` or `command <argument>`, else null. */
function argumentOf(line: string, command: string): string | null {
  if (line === command) return "";
  return line.startsWith(`${command} `) ? trimArgument(line.slice(command.length + 1)) : null;
}

/** Parses one ctl line (PROTOCOL.md, `iroh-do/ctl/1` "Requests"). */
export function parseCtlRequest(line: string): CtlRequest {
  if (line === "/status") return { command: "status" };
  if (line === "/count") return { command: "count" };
  const logArg = argumentOf(line, "/log");
  if (logArg !== null) {
    const count = logArg === "" ? LIMITS.ctlLogDefault : parseNumber(logArg, LIMITS.ctlLogMax);
    return count === null ? { command: "invalid", error: LOG_USAGE } : { command: "log", count };
  }
  const callbackArg = argumentOf(line, "/callback");
  if (callbackArg !== null) {
    const secs = parseNumber(callbackArg, Number.MAX_SAFE_INTEGER);
    return secs === null || secs > LIMITS.maxCallbackSecs
      ? { command: "invalid", error: CALLBACK_USAGE }
      : { command: "callback", secs };
  }
  return { command: "echo", line };
}

/** The kinds of room events, which are also log records. */
export type RoomKind = "msg" | "act" | "join" | "part" | "nick";
/** The kinds of log records (PROTOCOL.md, `/log`). */
export type LogKind = RoomKind | "ctl" | "echo" | "callback";

/** One record of a `/log` reply. */
export interface CtlLogEntry {
  seq: number;
  ts_ms: number;
  peer: string;
  kind: LogKind;
  text: string;
}

export type CtlReply =
  | { reply: "status"; status: Readonly<Record<string, unknown>> }
  | { reply: "count"; total: number; yours: number; peers: number }
  | { reply: "log"; records: readonly CtlLogEntry[] }
  | { reply: "callback"; secs: number }
  | { reply: "echo"; line: string; seq: number; yours: number }
  | { reply: "error"; text: string };

/** The reply line, without its terminator. */
export function encodeCtlReply(reply: CtlReply): string {
  switch (reply.reply) {
    case "status":
      return JSON.stringify(reply.status);
    case "count":
      return JSON.stringify({ total: reply.total, yours: reply.yours, peers: reply.peers });
    case "log":
      return JSON.stringify(reply.records);
    case "callback":
      return `ok: callback in ${reply.secs}s`;
    case "echo":
      // `seq` numbers every record ever written, so it is also the total.
      return `echo: ${reply.line} (#${reply.seq}, total ${reply.seq}, yours ${reply.yours})`;
    case "error":
      return reply.text;
  }
}

/** The line the object writes once per ctl connection, unprompted. */
export function ctlHello(room: string, peer: string): string {
  return `hello from ${room}: you are ${peer}\n`;
}

/** The line of an `iroh-do/notify/1` dial-back. */
export function notifyLine(room: string, wakes: number, records: number): string {
  return `callback from ${room} wakes=${wakes} records=${records}\n`;
}
