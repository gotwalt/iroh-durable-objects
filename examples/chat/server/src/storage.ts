// The chat object's tables, in its SQLite database (`ctx.storage.sql`).
//
// - `log`: the message log of design notes 007 and 008. Room events and the diagnostic
//   records (`ctl`, `echo`, `callback`) share it and its `seq`, which numbers every record
//   ever written. A room event's `prev` is the `seq` of the room event before it.
// - `log_meta`: `log_seq`, the largest `seq` of a pruned log that was left empty, and
//   `room_seq`, the `seq` of the newest room event; together they keep `seq` and `prev`
//   increasing across pruning.
// - `members`: the room's members, joined or not.
//
// The log keeps the newest LIMITS.logRetentionRows records and none older than
// LIMITS.logMaxAgeMs. Every write prunes; a log that stops growing is pruned once more by an
// alarm when its newest record expires, which `onAppend` schedules.
//
// The schema has no versions: changing it means resetting the deployment's state.
import { LIMITS, type LogKind, type RoomKind, truncateUtf8 } from "./protocol";

/** One record of the log. */
export interface LogRecord {
  seq: number;
  /** For a room event, the `seq` of the room event before it, 0 if none; 0 otherwise. */
  prev: number;
  tsMs: number;
  /** The peer's endpoint id, z-base-32. */
  peer: string;
  kind: LogKind;
  /** The author's nick for room events (the new nick for `nick`), empty otherwise. */
  nick: string;
  /**
   * The message (`msg`, `act`), the reason (`part`), the old nick (`nick`), the request line
   * (`ctl`), `<n> bytes` (`echo`), `delivered` or `failed: <reason>` (`callback`); empty for
   * `join`.
   */
  text: string;
}

/** A stored record of a kind known from the record that was appended. */
export type StoredRecord<K extends LogKind> = LogRecord & { kind: K };
export type RoomRecord = StoredRecord<RoomKind>;

/** A record to append; `text` is cut to LIMITS.maxLogText bytes. */
export interface NewRecord<K extends LogKind = LogKind> {
  peer: string;
  kind: K;
  nick: string;
  text: string;
}

export interface Member {
  /** The member's endpoint id, z-base-32. */
  peer: string;
  nick: string;
  /** False once the member has quit; it keeps its row and its last nick. */
  joined: boolean;
}

/** Schedule key of the prune of a quiet log. */
export const PRUNE_KEY = "prune";
/** Delay of that prune after an append: when the record expires, and a minute more. */
export const PRUNE_DELAY_MS = LIMITS.logMaxAgeMs + 60_000;

const ROOM_KIND_LIST: readonly RoomKind[] = ["msg", "act", "join", "part", "nick"];
const ROOM_KINDS: ReadonlySet<string> = new Set(ROOM_KIND_LIST);
const LOG_KINDS: ReadonlySet<string> = new Set<LogKind>([...ROOM_KIND_LIST, "ctl", "echo", "callback"]);
const ROOM_KINDS_SQL = `(${ROOM_KIND_LIST.map((kind) => `'${kind}'`).join(", ")})`;

function isLogKind(kind: string): kind is LogKind {
  return LOG_KINDS.has(kind);
}

const SCHEMA = [
  `CREATE TABLE IF NOT EXISTS log(
     seq INTEGER PRIMARY KEY,
     ts_ms INTEGER NOT NULL,
     peer TEXT NOT NULL,
     kind TEXT NOT NULL,
     nick TEXT NOT NULL DEFAULT '',
     text TEXT NOT NULL,
     prev INTEGER NOT NULL DEFAULT 0)`,
  "CREATE INDEX IF NOT EXISTS log_peer ON log(peer)",
  "CREATE TABLE IF NOT EXISTS log_meta(name TEXT PRIMARY KEY, value INTEGER NOT NULL)",
  `CREATE TABLE IF NOT EXISTS members(
     peer TEXT PRIMARY KEY,
     nick TEXT NOT NULL,
     joined INTEGER NOT NULL,
     first_ms INTEGER NOT NULL,
     last_ms INTEGER NOT NULL)`,
  // The room checks that a nick is free before writing it; the index refuses the write if a
  // bug let a taken nick through.
  "CREATE UNIQUE INDEX IF NOT EXISTS members_joined_nick ON members(nick COLLATE NOCASE) WHERE joined = 1",
];

/** The highest `seq` ever written, kept through pruning by `log_seq`. */
const LOG_TOTAL = `max(coalesce((SELECT max(seq) FROM log), 0),
                       coalesce((SELECT value FROM log_meta WHERE name = 'log_seq'), 0))`;
const RECORD_COLUMNS = "seq, prev, ts_ms, peer, kind, nick, text";

type Row = Record<string, SqlStorageValue>;

// The decoders below turn rows into typed values and throw on anything the schema rules
// out, so a schema change that the code does not follow fails at the first read.

function integer(row: Row, column: string): number {
  const value = row[column];
  if (typeof value !== "number" || !Number.isSafeInteger(value)) {
    throw new TypeError(`column ${column}: expected a safe integer, found ${String(value)}`);
  }
  return value;
}

function text(row: Row, column: string): string {
  const value = row[column];
  if (typeof value !== "string") throw new TypeError(`column ${column}: expected text, found ${typeof value}`);
  return value;
}

function logRecord(row: Row): LogRecord {
  const kind = text(row, "kind");
  if (!isLogKind(kind)) throw new TypeError(`column kind: unknown kind ${kind}`);
  return {
    seq: integer(row, "seq"),
    prev: integer(row, "prev"),
    tsMs: integer(row, "ts_ms"),
    peer: text(row, "peer"),
    kind,
    nick: text(row, "nick"),
    text: text(row, "text"),
  };
}

function roomRecord(row: Row): RoomRecord {
  const record = logRecord(row);
  if (!isRoomRecord(record)) throw new TypeError(`expected a room event, found kind ${record.kind}`);
  return record;
}

export function isRoomRecord(record: LogRecord): record is RoomRecord {
  return ROOM_KINDS.has(record.kind);
}

function member(row: Row): Member {
  const joined = integer(row, "joined");
  if (joined !== 0 && joined !== 1) throw new TypeError(`column joined: expected 0 or 1, found ${joined}`);
  return { peer: text(row, "peer"), nick: text(row, "nick"), joined: joined === 1 };
}

export class ChatStorage {
  readonly #storage: DurableObjectStorage;
  readonly #sql: SqlStorage;
  readonly #onAppend: () => void;

  /** `onAppend` runs after every committed append; the object schedules the prune there. */
  constructor(storage: DurableObjectStorage, onAppend: () => void) {
    this.#storage = storage;
    this.#sql = storage.sql;
    this.#onAppend = onAppend;
  }

  createSchema(): void {
    for (const statement of SCHEMA) this.#sql.exec(statement);
  }

  /** Appends a record at `tsMs`. */
  append<K extends LogKind>(record: NewRecord<K>, tsMs: number): StoredRecord<K> {
    const stored = this.#storage.transactionSync(() => this.#insert(record, tsMs));
    this.#onAppend();
    return stored;
  }

  /** Writes `member` and appends `record`, both or neither. */
  putMemberAndAppend<K extends LogKind>(update: Member, record: NewRecord<K>, tsMs: number): StoredRecord<K> {
    const stored = this.#storage.transactionSync(() => {
      this.#sql.exec(
        `INSERT INTO members(peer, nick, joined, first_ms, last_ms) VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(peer) DO UPDATE SET nick = excluded.nick, joined = excluded.joined, last_ms = excluded.last_ms`,
        update.peer,
        update.nick,
        update.joined ? 1 : 0,
        tsMs,
      );
      return this.#insert(record, tsMs);
    });
    this.#onAppend();
    return stored;
  }

  /** Deletes the records beyond the retention limits at `nowMs`. */
  prune(nowMs: number): void {
    this.#storage.transactionSync(() => this.#prune(nowMs));
  }

  // `seq` is explicit because SQLite would number the first row of an emptied table 1 again.
  #insert<K extends LogKind>(record: NewRecord<K>, tsMs: number): StoredRecord<K> {
    const room = ROOM_KINDS.has(record.kind);
    const prev = room ? this.roomSeq() : 0;
    const body = truncateUtf8(record.text, LIMITS.maxLogText);
    const seq = integer(
      this.#sql
        .exec(
          `INSERT INTO log(seq, ts_ms, peer, kind, nick, text, prev)
           VALUES (${LOG_TOTAL} + 1, ?, ?, ?, ?, ?, ?) RETURNING seq`,
          tsMs,
          record.peer,
          record.kind,
          record.nick,
          body,
          prev,
        )
        .one(),
      "seq",
    );
    if (room) {
      this.#sql.exec(
        `INSERT INTO log_meta(name, value) VALUES ('room_seq', ?)
         ON CONFLICT(name) DO UPDATE SET value = excluded.value`,
        seq,
      );
    }
    this.#prune(tsMs);
    return { seq, prev, tsMs, peer: record.peer, kind: record.kind, nick: record.nick, text: body };
  }

  #prune(nowMs: number): void {
    this.#sql.exec("DELETE FROM log WHERE seq <= ?", this.totalRecords() - LIMITS.logRetentionRows);
    const cutoff = nowMs - LIMITS.logMaxAgeMs;
    // If every record is about to expire, keep the newest `seq` so the next one continues it.
    this.#sql.exec(
      `INSERT INTO log_meta(name, value)
       SELECT 'log_seq', seq FROM log WHERE seq = (SELECT max(seq) FROM log) AND ts_ms < ?
       ON CONFLICT(name) DO UPDATE SET value = max(value, excluded.value)`,
      cutoff,
    );
    // Records are appended in time order, so the expired ones are a prefix of `seq`: walking
    // from the oldest to the first record to keep reads one row more than it deletes, where a
    // scan of `ts_ms` would read the whole log. A record whose timestamp went back with the
    // clock goes once the records before it have.
    this.#sql.exec(
      `DELETE FROM log WHERE seq < coalesce(
         (SELECT seq FROM log WHERE ts_ms >= ? ORDER BY seq LIMIT 1),
         (SELECT max(seq) FROM log) + 1)`,
      cutoff,
    );
  }

  /** The `seq` of the newest room event, 0 if none. */
  roomSeq(): number {
    const row = this.#sql.exec("SELECT value FROM log_meta WHERE name = 'room_seq'").toArray()[0];
    return row === undefined ? 0 : integer(row, "value");
  }

  /** The highest `seq` ever written: the number of records ever written. */
  totalRecords(): number {
    return integer(this.#sql.exec(`SELECT ${LOG_TOTAL} AS total`).one(), "total");
  }

  /** Retained records of `peer`. */
  recordsOf(peer: string): number {
    return integer(this.#sql.exec("SELECT count(*) AS n FROM log WHERE peer = ?", peer).one(), "n");
  }

  /** Distinct peers among the retained records. */
  peersInLog(): number {
    return integer(this.#sql.exec("SELECT count(DISTINCT peer) AS n FROM log").one(), "n");
  }

  /** The newest `n` records of any kind, oldest first. */
  recentRecords(n: number): LogRecord[] {
    return this.#sql
      .exec(`SELECT * FROM (SELECT ${RECORD_COLUMNS} FROM log ORDER BY seq DESC LIMIT ?) ORDER BY seq`, n)
      .toArray()
      .map(logRecord);
  }

  /** The newest `n` room events, oldest first. */
  recentEvents(n: number): RoomRecord[] {
    return this.#sql
      .exec(
        `SELECT * FROM (SELECT ${RECORD_COLUMNS} FROM log WHERE kind IN ${ROOM_KINDS_SQL} ORDER BY seq DESC LIMIT ?)
         ORDER BY seq`,
        n,
      )
      .toArray()
      .map(roomRecord);
  }

  /** The oldest `limit` room events after `seq`. */
  eventsAfter(seq: number, limit: number): RoomRecord[] {
    return this.#sql
      .exec(
        `SELECT ${RECORD_COLUMNS} FROM log WHERE seq > ? AND kind IN ${ROOM_KINDS_SQL} ORDER BY seq LIMIT ?`,
        seq,
        limit,
      )
      .toArray()
      .map(roomRecord);
  }

  /** The member `peer`, joined or not, or null if it never joined. */
  member(peer: string): Member | null {
    const row = this.#sql.exec("SELECT peer, nick, joined FROM members WHERE peer = ?", peer).toArray()[0];
    return row === undefined ? null : member(row);
  }

  /** The joined members, ordered by nick ignoring case. */
  joinedMembers(): Member[] {
    return this.#sql
      .exec("SELECT peer, nick, joined FROM members WHERE joined = 1 ORDER BY nick COLLATE NOCASE")
      .toArray()
      .map(member);
  }
}
