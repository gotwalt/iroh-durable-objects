// ChatStorage on a Durable Object's SQLite: the log's numbering and retention (PROTOCOL.md,
// "Sequence numbers and gaps" and "Retention"), members, and the prune alarm.
import { describe, expect, it } from "vitest";
import { LIMITS, type LogKind } from "../../src/protocol";
import { PRUNE_DELAY_MS, StorageError } from "../../src/storage";
import { chatStorage, inObject } from "./helpers";

const DAY = 24 * 60 * 60 * 1000;
const T0 = 1_800_000_000_000;

const rec = (kind: LogKind, text = "x", peer = "ana") => ({ peer, kind, nick: kind === "ctl" ? "" : "ana", text });

describe("numbering", () => {
  it("numbers every record by one, and chains room events by prev across diagnostic records", () =>
    inObject((state) => {
      const { storage } = chatStorage(state);
      const kinds: LogKind[] = ["join", "ctl", "msg", "echo", "callback", "act"];
      const stored = kinds.map((kind, i) => storage.append(rec(kind), T0 + i));
      expect(stored.map((r) => r.seq)).toEqual([1, 2, 3, 4, 5, 6]);
      // prev: the room event before, 0 for the first; diagnostic records have 0.
      expect(stored.map((r) => r.prev)).toEqual([0, 0, 1, 0, 0, 3]);
      expect(storage.roomSeq()).toBe(6);
      expect(storage.totalRecords()).toBe(6);
    }));

  it("keeps seq and prev increasing after pruning has emptied the log", () =>
    inObject((state) => {
      const { storage } = chatStorage(state);
      storage.append(rec("msg"), T0);
      storage.append(rec("ctl"), T0 + 1);
      // Fifteen days later both records have expired; the next append prunes them first.
      const later = storage.append(rec("msg"), T0 + 15 * DAY);
      expect(later).toMatchObject({ seq: 3, prev: 1 });
      expect(storage.recentRecords(10).map((r) => r.seq)).toEqual([3]);
      // And when the alarm alone empties the log.
      expect(storage.prune(T0 + 30 * DAY)).toBeNull();
      expect(storage.recentRecords(10)).toEqual([]);
      expect(storage.append(rec("act"), T0 + 30 * DAY)).toMatchObject({ seq: 4, prev: 3 });
      expect(storage.totalRecords()).toBe(4);
    }));

  it("cuts text to the log's limit at a character boundary", () =>
    inObject((state) => {
      const { storage } = chatStorage(state);
      const stored = storage.append(rec("ctl", "é".repeat(LIMITS.maxLogText)), T0);
      expect(new TextEncoder().encode(stored.text).length).toBe(LIMITS.maxLogText);
      expect(storage.recentRecords(1)[0]?.text).toBe(stored.text);
    }));
});

describe("retention", () => {
  it(
    "keeps the newest LOG_RETENTION rows of all kinds",
    () =>
      inObject((state) => {
        const { storage } = chatStorage(state);
        const extra = 25;
        for (let i = 0; i < LIMITS.logRetentionRows + extra; i++) {
          storage.append(rec(i % 2 === 0 ? "msg" : "ctl"), T0 + i);
        }
        const oldest = storage.recentRecords(LIMITS.logRetentionRows + extra)[0];
        expect(oldest?.seq).toBe(extra + 1);
        expect(storage.recentRecords(LIMITS.logRetentionRows + extra)).toHaveLength(LIMITS.logRetentionRows);
        expect(storage.totalRecords()).toBe(LIMITS.logRetentionRows + extra);
      }),
    60_000,
  );

  it("keeps no record older than 14 days on insert", () =>
    inObject((state) => {
      const { storage } = chatStorage(state);
      storage.append(rec("msg", "old"), T0);
      storage.append(rec("msg", "newer"), T0 + 2 * DAY);
      storage.append(rec("msg", "now"), T0 + LIMITS.logMaxAgeMs + 1);
      expect(storage.recentEvents(10).map((r) => r.text)).toEqual(["newer", "now"]);
    }));

  it("history and since return only room events, oldest first, within their limits", () =>
    inObject((state) => {
      const { storage } = chatStorage(state);
      for (let i = 0; i < 6; i++) storage.append(rec(i % 3 === 0 ? "ctl" : "msg", `m${i}`), T0 + i);
      // Records 1..6: ctl, msg, msg, ctl, msg, msg.
      expect(storage.recentEvents(3).map((r) => r.seq)).toEqual([3, 5, 6]);
      expect(storage.eventsAfter(2, 2).map((r) => r.seq)).toEqual([3, 5]);
      expect(storage.eventsAfter(6, 10)).toEqual([]);
    }));

  it("reads room events through the partial index, not by scanning the log", () =>
    inObject((state) => {
      chatStorage(state);
      const plan = (query: string) =>
        state.storage.sql
          .exec(`EXPLAIN QUERY PLAN ${query}`)
          .toArray()
          .map((row) => String(row.detail))
          .join("; ");
      const kinds = "('msg', 'act', 'join', 'part', 'nick')";
      expect(plan(`SELECT seq FROM log WHERE kind IN ${kinds} ORDER BY seq DESC LIMIT 50`)).toMatch(
        /USING INDEX log_room\b/,
      );
      expect(plan(`SELECT seq FROM log WHERE seq > 5 AND kind IN ${kinds} ORDER BY seq LIMIT 500`)).toMatch(
        /USING INDEX log_room\b/,
      );
    }));
});

describe("prune alarm", () => {
  it("is armed by the first append, not by later ones while it is pending", () =>
    inObject((state) => {
      const { storage, pruneAlarms } = chatStorage(state);
      storage.append(rec("msg"), T0);
      storage.append(rec("msg"), T0 + 1_000);
      storage.append(rec("ctl"), T0 + 2_000);
      expect(pruneAlarms).toEqual([T0 + PRUNE_DELAY_MS]);
    }));

  it("moves itself to PRUNE_DELAY_MS after the newest record when it fires", () =>
    inObject((state) => {
      const { storage, pruneAlarms } = chatStorage(state);
      storage.append(rec("msg", "first"), T0);
      storage.append(rec("msg", "last"), T0 + 3 * DAY);
      const fired = T0 + PRUNE_DELAY_MS;
      expect(storage.prune(fired)).toBe(T0 + 3 * DAY + PRUNE_DELAY_MS);
      expect(storage.recentEvents(10).map((r) => r.text)).toEqual(["last"]);
      // An append before that due time arms nothing.
      storage.append(rec("msg"), fired + 1);
      expect(pruneAlarms).toEqual([T0 + PRUNE_DELAY_MS]);
    }));

  it("is armed again by an append after a due time that passed without the alarm", () =>
    inObject((state) => {
      const { storage, pruneAlarms } = chatStorage(state);
      storage.append(rec("msg"), T0);
      const late = T0 + PRUNE_DELAY_MS + 1;
      storage.append(rec("msg"), late);
      expect(pruneAlarms).toEqual([T0 + PRUNE_DELAY_MS, late + PRUNE_DELAY_MS]);
    }));
});

describe("members", () => {
  it("writes a member and its event together", () =>
    inObject((state) => {
      const { storage } = chatStorage(state);
      storage.putMemberAndAppend({ peer: "ana", nick: "Ana", joined: true }, rec("join", ""), T0);
      storage.putMemberAndAppend({ peer: "bo", nick: "bo", joined: true }, { ...rec("join", ""), peer: "bo" }, T0);
      expect(storage.member("ana")).toEqual({ peer: "ana", nick: "Ana", joined: true });
      expect(storage.joinedMembers().map((m) => m.nick)).toEqual(["Ana", "bo"]);
      storage.putMemberAndAppend({ peer: "ana", nick: "Ana", joined: false }, rec("part", ""), T0 + 1);
      expect(storage.member("ana")?.joined).toBe(false);
      expect(storage.joinedMembers().map((m) => m.peer)).toEqual(["bo"]);
      expect(storage.member("cy")).toBeNull();
    }));

  it("refuses a joined nick that differs only in case, and stores neither row nor event", () =>
    inObject((state) => {
      const { storage } = chatStorage(state);
      storage.putMemberAndAppend({ peer: "ana", nick: "ana", joined: true }, rec("join", ""), T0);
      const before = storage.totalRecords();
      expect(() =>
        storage.putMemberAndAppend({ peer: "bo", nick: "ANA", joined: true }, { ...rec("join", ""), peer: "bo" }, T0),
      ).toThrow(StorageError);
      expect(storage.member("bo")).toBeNull();
      expect(storage.totalRecords()).toBe(before);
      // A member that quit holds no nick.
      storage.putMemberAndAppend({ peer: "ana", nick: "ana", joined: false }, rec("part", ""), T0 + 1);
      storage.putMemberAndAppend({ peer: "bo", nick: "ANA", joined: true }, { ...rec("join", ""), peer: "bo" }, T0 + 2);
      expect(storage.member("bo")?.nick).toBe("ANA");
    }));

  it("throws StorageError for a write that fails, and stores nothing of it", () =>
    inObject((state) => {
      const { storage } = chatStorage(state);
      storage.append(rec("msg"), T0);
      state.storage.sql.exec(
        "CREATE TRIGGER refuse BEFORE INSERT ON members BEGIN SELECT RAISE(ABORT, 'refused'); END",
      );
      expect(() =>
        storage.putMemberAndAppend({ peer: "bo", nick: "bo", joined: true }, { ...rec("join", ""), peer: "bo" }, T0),
      ).toThrow(StorageError);
      expect(storage.totalRecords()).toBe(1);
      expect(storage.roomSeq()).toBe(1);
    }));
});
