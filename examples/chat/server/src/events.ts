// Stored records as the lines of the protocols.
import type { CtlLogEntry, RoomEvent } from "./protocol";
import type { LogRecord, RoomRecord } from "./storage";

/** A stored room event as its event line. */
export function roomEvent(record: RoomRecord): RoomEvent {
  const { seq, prev, tsMs: ts, peer } = record;
  switch (record.kind) {
    case "msg":
    case "act":
      return { t: record.kind, seq, prev, ts, peer, nick: record.nick, text: record.text };
    case "join":
      return { t: "join", seq, prev, ts, peer, nick: record.nick };
    case "part":
      return { t: "part", seq, prev, ts, peer, nick: record.nick, reason: record.text };
    case "nick":
      return { t: "nick", seq, prev, ts, peer, old: record.text, new: record.nick };
  }
}

/** A stored record as an entry of a `/log` reply. */
export function logEntry(record: LogRecord): CtlLogEntry {
  return { seq: record.seq, ts_ms: record.tsMs, peer: record.peer, kind: record.kind, text: record.text };
}
