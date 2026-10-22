// The chat's tables and the statements it runs, through `ctx.storage.sql`. Ported from
// examples/chat/app/src/store.rs; the SQL text is the same.
//
// - `log`: the message log (design notes 007 and 008), keeping the newest LOG_RETENTION rows
//   and none older than LOG_MAX_AGE_MS. Room events and the diagnostic records (`ctl`,
//   `echo`, `callback`) share it. `seq` numbers every row ever written; see LOG_TOTAL. A room
//   event's `prev` is the `seq` of the room event before it.
// - `members`: the room's members (design note 008): nick, whether the member is joined, and
//   when it was first and last seen.
// - `log_meta`: named integers. `log_seq`, the largest `seq` of a pruned log that was left
//   empty; `room_seq`, the `seq` of the newest room event, the next room event's `prev`.
//
// The schema is created as is, with no versioning or upgrade path: changing it means
// resetting the object's state.

/** Records a host retains; older ones are deleted on insert. */
export const LOG_RETENTION = 10_000;
/** Age after which a record is deleted, on insert and from the prune. */
export const LOG_MAX_AGE_MS = 14 * 24 * 3600 * 1000;
/** Schedule key of the prune of a log that stopped growing. */
export const PRUNE_KEY = "prune";

/** Kind of a message-log record. */
export type LogKind = "msg" | "act" | "join" | "part" | "nick" | "ctl" | "echo" | "callback";

/** Whether records of `kind` are room events. */
export function isRoomEvent(kind: LogKind): boolean {
  return kind === "msg" || kind === "act" || kind === "join" || kind === "part" || kind === "nick";
}

/** A member of the room. */
export interface Member {
  /** The member's EndpointId, z-base-32. */
  peer: string;
  /** 1–24 characters of `[A-Za-z0-9_.-]`, unique among joined members ignoring ASCII case. */
  nick: string;
  /** Whether it is joined. A member that sent `/quit` keeps its record, not joined. */
  joined: boolean;
}

/** One message-log record (`LogRow` in the Rust app). */
export interface LogRow {
  seq: number;
  /** For a room event, the `seq` of the room event before it (0 if none); 0 otherwise. */
  prev: number;
  ts_ms: number;
  peer: string;
  kind: string;
  /** The author's nick for room events (the new nick for `nick`), empty otherwise. */
  nick: string;
  /**
   * The message for `msg` and `act`, the reason for `part`, the old nick for `nick`, the
   * request line for `ctl`, `"<n> bytes"` for `echo`, and `"delivered"` or
   * `"failed: <reason>"` for `callback`.
   */
  text: string;
}

/** The tables and indexes, created idempotently on every node build. */
export const SCHEMA = [
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
  // Nicks are unique among joined members, ignoring ASCII case. The room checks this before
  // writing; the index keeps the table consistent if it did not.
  "CREATE UNIQUE INDEX IF NOT EXISTS members_joined_nick ON members(nick COLLATE NOCASE) WHERE joined = 1",
];

const ROOM_KINDS = "('msg', 'act', 'join', 'part', 'nick')";

/** Records ever inserted: the largest `seq`, or the one saved when pruning emptied the log. */
const LOG_TOTAL = `SELECT max(coalesce((SELECT max(seq) FROM log), 0),
    coalesce((SELECT value FROM log_meta WHERE name = 'log_seq'), 0)) AS value`;
/** Appends a record with `seq` = LOG_TOTAL + 1; returns `seq`. */
const LOG_INSERT = `INSERT INTO log(seq, ts_ms, peer, kind, nick, text, prev) VALUES (
    max(coalesce((SELECT max(seq) FROM log), 0),
        coalesce((SELECT value FROM log_meta WHERE name = 'log_seq'), 0)) + 1,
    ?1, ?2, ?3, ?4, ?5, ?6) RETURNING seq AS value`;
const ROOM_SEQ = "SELECT coalesce((SELECT value FROM log_meta WHERE name = 'room_seq'), 0) AS value";
const SET_ROOM_SEQ = `INSERT INTO log_meta(name, value) VALUES ('room_seq', ?)
 ON CONFLICT(name) DO UPDATE SET value = excluded.value`;
const LOG_SAVE_SEQ = `INSERT INTO log_meta(name, value)
    SELECT 'log_seq', seq FROM log WHERE seq = (SELECT max(seq) FROM log) AND ts_ms < ?1
    ON CONFLICT(name) DO UPDATE SET value = max(value, excluded.value)`;
const LOG_PRUNE_COUNT = "DELETE FROM log WHERE seq <= ?1";
const LOG_PRUNE_AGE = `DELETE FROM log WHERE seq < coalesce(
    (SELECT seq FROM log WHERE ts_ms >= ?1 ORDER BY seq LIMIT 1),
    (SELECT max(seq) FROM log) + 1)`;
const LOG_YOURS = "SELECT count(*) AS value FROM log WHERE peer = ?";
const LOG_PEERS = "SELECT count(DISTINCT peer) AS value FROM log";
const LOG_RECENT = `SELECT seq, prev, ts_ms, peer, kind, nick, text FROM
   (SELECT seq, prev, ts_ms, peer, kind, nick, text FROM log ORDER BY seq DESC LIMIT ?)
 ORDER BY seq`;
const EVENTS_AFTER = `SELECT seq, prev, ts_ms, peer, kind, nick, text FROM log
         WHERE seq > ? AND kind IN ${ROOM_KINDS} ORDER BY seq LIMIT ?`;
const EVENTS_RECENT = `SELECT seq, prev, ts_ms, peer, kind, nick, text FROM
           (SELECT seq, prev, ts_ms, peer, kind, nick, text FROM log
            WHERE kind IN ${ROOM_KINDS} ORDER BY seq DESC LIMIT ?)
         ORDER BY seq`;
const MEMBER_GET = "SELECT peer, nick, joined FROM members WHERE peer = ?";
const MEMBERS_JOINED = "SELECT peer, nick, joined FROM members WHERE joined = 1 ORDER BY nick COLLATE NOCASE";
const MEMBER_UPSERT = `INSERT INTO members(peer, nick, joined, first_ms, last_ms) VALUES (?1, ?2, ?3, ?4, ?4)
 ON CONFLICT(peer) DO UPDATE SET nick = excluded.nick, joined = excluded.joined, last_ms = excluded.last_ms`;

const Z32 = /^[ybndrfg8ejkmcpqxot1uwisza345h769]{52}$/;

function int(sql: SqlStorage, query: string, ...params: SqlStorageValue[]): number {
  const v = sql.exec(query, ...params).toArray()[0]?.value;
  return typeof v === "number" ? v : 0;
}

function logRow(r: Record<string, SqlStorageValue>): LogRow {
  return {
    seq: Number(r.seq ?? 0),
    prev: Number(r.prev ?? 0),
    ts_ms: Number(r.ts_ms ?? 0),
    peer: String(r.peer ?? ""),
    kind: String(r.kind ?? ""),
    nick: String(r.nick ?? ""),
    text: String(r.text ?? ""),
  };
}

function memberRow(r: Record<string, SqlStorageValue>): Member | null {
  // As the Rust app, which parses the id with from_z32 and skips rows it cannot parse.
  if (typeof r.peer !== "string" || !Z32.test(r.peer)) return null;
  return { peer: r.peer, nick: String(r.nick ?? ""), joined: Number(r.joined) !== 0 };
}

/** The newest room event's `seq`, 0 if none. */
export function roomSeq(sql: SqlStorage): number {
  return int(sql, ROOM_SEQ);
}

/** Deletes records beyond LOG_RETENTION or older than LOG_MAX_AGE_MS at `nowMs`. */
export function logPrune(sql: SqlStorage, nowMs: number): void {
  const newest = int(sql, LOG_TOTAL);
  sql.exec(LOG_PRUNE_COUNT, newest - LOG_RETENTION);
  const cutoff = nowMs - LOG_MAX_AGE_MS;
  sql.exec(LOG_SAVE_SEQ, cutoff);
  sql.exec(LOG_PRUNE_AGE, cutoff);
}

/** Appends a record at `tsMs` and enforces retention; returns its `seq` and `prev`. */
export function logInsert(sql: SqlStorage, tsMs: number, peer: string, kind: LogKind, nick: string, text: string): { seq: number; prev: number } {
  const room = isRoomEvent(kind);
  const prev = room ? roomSeq(sql) : 0;
  const seq = int(sql, LOG_INSERT, tsMs, peer, kind, nick, text, prev);
  if (room) sql.exec(SET_ROOM_SEQ, seq);
  logPrune(sql, tsMs);
  return { seq, prev };
}

/** Records ever inserted. */
export function logTotal(sql: SqlStorage): number {
  return int(sql, LOG_TOTAL);
}

/** Retained records of `peer` (z-base-32). */
export function logYours(sql: SqlStorage, peer: string): number {
  return int(sql, LOG_YOURS, peer);
}

/** Distinct peers in retained records. */
export function logPeers(sql: SqlStorage): number {
  return int(sql, LOG_PEERS);
}

/** Newest `n` records of any kind, oldest first. */
export function logRecent(sql: SqlStorage, n: number): LogRow[] {
  return sql.exec(LOG_RECENT, n).toArray().map(logRow);
}

/** Up to `limit` room events after `seq`, oldest first. */
export function eventsAfter(sql: SqlStorage, seq: number, limit: number): LogRow[] {
  return sql.exec(EVENTS_AFTER, seq, limit).toArray().map(logRow);
}

/** Newest `n` room events, oldest first. */
export function eventsRecent(sql: SqlStorage, n: number): LogRow[] {
  return sql.exec(EVENTS_RECENT, n).toArray().map(logRow);
}

/** One member, joined or not. */
export function member(sql: SqlStorage, peer: string): Member | null {
  const r = sql.exec(MEMBER_GET, peer).toArray()[0];
  return r ? memberRow(r) : null;
}

/** Joined members, by nick. */
export function members(sql: SqlStorage): Member[] {
  return sql.exec(MEMBERS_JOINED).toArray().map(memberRow).filter((m): m is Member => m !== null);
}

/** Inserts or updates a member. */
export function setMember(sql: SqlStorage, m: Member, nowMs: number): void {
  sql.exec(MEMBER_UPSERT, m.peer, m.nick, m.joined ? 1 : 0, nowMs);
}
