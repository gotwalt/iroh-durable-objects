import { expect, it } from "vitest";
import { logEntry, roomEvent } from "../src/events";
import { encodeLines, historyLine } from "../src/protocol";
import type { RoomRecord } from "../src/storage";

const record = (kind: RoomRecord["kind"], nick: string, text: string): RoomRecord => ({
  seq: 7,
  prev: 5,
  tsMs: 1000,
  peer: "p",
  kind,
  nick,
  text,
});

it("encodes stored room events as PROTOCOL.md shows them", () => {
  const lines = encodeLines([
    roomEvent(record("msg", "ana", "hi")),
    roomEvent(record("act", "ana", "waves")),
    historyLine(roomEvent(record("join", "bo", ""))),
    roomEvent(record("part", "bo", "bye")),
    roomEvent(record("nick", "bob", "bo")),
  ]);
  expect(lines.split("\n")).toEqual([
    '{"t":"msg","seq":7,"prev":5,"ts":1000,"peer":"p","nick":"ana","text":"hi"}',
    '{"t":"act","seq":7,"prev":5,"ts":1000,"peer":"p","nick":"ana","text":"waves"}',
    '{"t":"join","seq":7,"prev":5,"ts":1000,"peer":"p","nick":"bo","hist":true}',
    '{"t":"part","seq":7,"prev":5,"ts":1000,"peer":"p","nick":"bo","reason":"bye"}',
    '{"t":"nick","seq":7,"prev":5,"ts":1000,"peer":"p","old":"bo","new":"bob"}',
    "",
  ]);
});

it("encodes /log entries with the record's text", () => {
  expect(logEntry({ seq: 3, prev: 0, tsMs: 9, peer: "p", kind: "echo", nick: "", text: "4 bytes" })).toEqual({
    seq: 3,
    ts_ms: 9,
    peer: "p",
    kind: "echo",
    text: "4 bytes",
  });
});
