import { describe, expect, it } from "vitest";
import constants from "../../common/constants.json";
import {
  CTL_ALPN,
  type CtlRequest,
  defaultNicks,
  ECHO_ALPN,
  encodeCtlReply,
  encodeLines,
  frameRequest,
  historyLine,
  isValidNick,
  LIMITS,
  NOTIFY_ALPN,
  nickKey,
  parseCtlRequest,
  parseNumber,
  parseRoomRequest,
  ROOM_ALPN,
  ROOM_ERRORS,
  type RoomEvent,
  type RoomRequest,
  trimArgument,
  truncateUtf8,
} from "../src/protocol";

const bytes = (text: string) => new TextEncoder().encode(text);

describe("constants", () => {
  it("match examples/chat/common/constants.json", () => {
    expect({ room: ROOM_ALPN, ctl: CTL_ALPN, echo: ECHO_ALPN, notify: NOTIFY_ALPN }).toEqual(constants.alpn);
    expect({
      max_request: LIMITS.maxRequest,
      history_default: LIMITS.historyDefault,
      history_max: LIMITS.historyMax,
      since_max: LIMITS.sinceMax,
      rate_limit_requests: LIMITS.rateLimitRequests,
      rate_limit_window_secs: LIMITS.rateLimitWindowMs / 1000,
      nick_max: LIMITS.nickMax,
      max_log_text: LIMITS.maxLogText,
      max_callback_secs: LIMITS.maxCallbackSecs,
      delivery_batch_ms: LIMITS.deliveryBatchMs,
      ctl_max_line: LIMITS.ctlMaxLine,
      ctl_log_default: LIMITS.ctlLogDefault,
      ctl_log_max: LIMITS.ctlLogMax,
      log_retention_rows: LIMITS.logRetentionRows,
      log_max_age_secs: LIMITS.logMaxAgeMs / 1000,
    }).toEqual(constants.limits);
  });
});

describe("request framing", () => {
  it("accepts \\n, \\r\\n or no terminator", () => {
    for (const raw of ["/who\n", "/who\r\n", "/who"])
      expect(frameRequest(bytes(raw))).toEqual({ ok: true, line: "/who" });
    expect(frameRequest(new Uint8Array())).toEqual({ ok: true, line: "" });
  });

  it("refuses more than one line", () => {
    expect(frameRequest(bytes("/who\n/who"))).toEqual({ ok: false, error: ROOM_ERRORS.onePerStream });
    expect(frameRequest(bytes("/who\n\n"))).toEqual({ ok: false, error: ROOM_ERRORS.onePerStream });
  });

  it("limits the line, not its terminator, to max_request bytes", () => {
    expect(frameRequest(bytes(`${"a".repeat(LIMITS.maxRequest)}\r\n`)).ok).toBe(true);
    expect(frameRequest(bytes("a".repeat(LIMITS.maxRequest + 1)))).toEqual({ ok: false, error: ROOM_ERRORS.tooLong });
  });

  it("decodes strictly and keeps a byte order mark", () => {
    expect(frameRequest(Uint8Array.of(0xff))).toEqual({ ok: false, error: ROOM_ERRORS.notUtf8 });
    // An encoded surrogate (U+D800) is not UTF-8.
    expect(frameRequest(Uint8Array.of(0xed, 0xa0, 0x80))).toEqual({ ok: false, error: ROOM_ERRORS.notUtf8 });
    expect(frameRequest(Uint8Array.of(0xef, 0xbb, 0xbf, 0x2f, 0x77, 0x68, 0x6f))).toEqual({ ok: true, line: "﻿/who" });
  });
});

describe("arguments", () => {
  it("trims ASCII white space only", () => {
    expect(trimArgument(" \t\r\f\nhi \t")).toBe("hi");
    expect(trimArgument(" hi ")).toBe(" hi ");
    expect(trimArgument("﻿hi")).toBe("﻿hi");
    expect(trimArgument("\vhi")).toBe("\vhi");
  });

  it("parses numbers as digits only, saturating at the bound", () => {
    expect(parseNumber("0", 200)).toBe(0);
    expect(parseNumber("007", 200)).toBe(7);
    expect(parseNumber("201", 200)).toBe(200);
    expect(parseNumber("9".repeat(400), 200)).toBe(200);
    expect(parseNumber("9007199254740993", Number.MAX_SAFE_INTEGER)).toBe(Number.MAX_SAFE_INTEGER);
    for (const bad of ["", "+1", "-1", "1.5", "1e3", " 1", "0x1", "١"]) expect(parseNumber(bad, 200)).toBeNull();
  });
});

describe("room requests", () => {
  const cases: [string, RoomRequest][] = [
    ["/join", { command: "join", nick: "" }],
    ["/join \t ana \t", { command: "join", nick: "ana" }],
    ["/say  hi there ", { command: "say", text: "hi there" }],
    ["/say  hi", { command: "say", text: " hi" }],
    ["/say", { command: "invalid", of: "say", error: ROOM_ERRORS.sayUsage }],
    ["/say   ", { command: "invalid", of: "say", error: ROOM_ERRORS.sayUsage }],
    ["/me waves", { command: "me", text: "waves" }],
    ["/me", { command: "invalid", of: "me", error: ROOM_ERRORS.meUsage }],
    ["/nick", { command: "nick", nick: "" }],
    ["/who extra", { command: "who" }],
    ["/history", { command: "history", count: 50 }],
    ["/history 0", { command: "history", count: 0 }],
    ["/history 99999999999999999999", { command: "history", count: 200 }],
    ["/history +2", { command: "invalid", of: "history", error: ROOM_ERRORS.historyUsage }],
    ["/history 3 4", { command: "invalid", of: "history", error: ROOM_ERRORS.historyUsage }],
    ["/since 12", { command: "since", after: 12 }],
    ["/since 18446744073709551616", { command: "since", after: Number.MAX_SAFE_INTEGER }],
    ["/since", { command: "invalid", of: "since", error: ROOM_ERRORS.sinceUsage }],
    ["/quit", { command: "quit", reason: "" }],
    ["/quit  bye ", { command: "quit", reason: "bye" }],
    ["/SAY hi", { command: "unknown" }],
    ["/say\thi", { command: "unknown" }],
    ["﻿/who", { command: "unknown" }],
    ["hello", { command: "unknown" }],
    ["", { command: "unknown" }],
  ];
  it.each(cases)("%j", (line, request) => {
    expect(parseRoomRequest(line)).toEqual(request);
  });
});

describe("nicks", () => {
  it("are 1-24 characters of A-Z a-z 0-9 _ . -", () => {
    for (const nick of ["a", "Ana_1.-", ".", "..", "n".repeat(24)]) expect(isValidNick(nick)).toBe(true);
    for (const nick of ["", "n".repeat(25), "ana bo", "é", "ana ", "a/b"]) expect(isValidNick(nick)).toBe(false);
  });

  it("collide ignoring case", () => {
    expect(nickKey("BoB")).toBe(nickKey("bob"));
  });

  it("default to a prefix of the endpoint id, then numbered", () => {
    const nicks = defaultNicks("ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1u");
    expect([nicks.next().value, nicks.next().value, nicks.next().value]).toEqual([
      "ybndrfg8",
      "ybndrfg8-2",
      "ybndrfg8-3",
    ]);
  });
});

describe("room lines", () => {
  const msg: RoomEvent = { t: "msg", seq: 7, prev: 5, ts: 1000, peer: "p", nick: "ana", text: 'hi "there"' };

  it("encode as one JSON line each, keys in PROTOCOL.md's order", () => {
    expect(encodeLines([msg, { t: "ok", seq: 7, prev: 5 }])).toBe(
      '{"t":"msg","seq":7,"prev":5,"ts":1000,"peer":"p","nick":"ana","text":"hi \\"there\\""}\n{"t":"ok","seq":7,"prev":5}\n',
    );
  });

  it("put hist last", () => {
    expect(Object.keys(historyLine(msg)).at(-1)).toBe("hist");
  });

  it("keep text outside ASCII as UTF-8", () => {
    expect(encodeLines([{ t: "err", text: "é " }])).toBe('{"t":"err","text":"é "}\n');
  });
});

describe("ctl", () => {
  const cases: [string, CtlRequest][] = [
    ["/status", { command: "status" }],
    ["/status x", { command: "echo", line: "/status x" }],
    ["/count", { command: "count" }],
    ["/log", { command: "log", count: 5 }],
    ["/log \t7 ", { command: "log", count: 7 }],
    ["/log 500", { command: "log", count: 50 }],
    ["/log +1", { command: "invalid", error: "error: usage /log [n], n <= 50" }],
    ["/logx", { command: "echo", line: "/logx" }],
    ["/callback 0", { command: "callback", secs: 0 }],
    ["/callback 50", { command: "callback", secs: 50 }],
    ["/callback 51", { command: "invalid", error: "error: usage /callback <secs>, secs <= 50" }],
    ["/callback 99999999999999999999", { command: "invalid", error: "error: usage /callback <secs>, secs <= 50" }],
    ["/callback", { command: "invalid", error: "error: usage /callback <secs>, secs <= 50" }],
    ["hello", { command: "echo", line: "hello" }],
  ];
  it.each(cases)("%j", (line, request) => {
    expect(parseCtlRequest(line)).toEqual(request);
  });

  it("encodes replies", () => {
    expect(encodeCtlReply({ reply: "count", total: 3, yours: 2, peers: 1 })).toBe('{"total":3,"yours":2,"peers":1}');
    expect(encodeCtlReply({ reply: "echo", line: "hi", seq: 4, yours: 2 })).toBe("echo: hi (#4, total 4, yours 2)");
    expect(encodeCtlReply({ reply: "callback", secs: 3 })).toBe("ok: callback in 3s");
    expect(encodeCtlReply({ reply: "log", records: [{ seq: 1, ts_ms: 2, peer: "p", kind: "ctl", text: "x" }] })).toBe(
      '[{"seq":1,"ts_ms":2,"peer":"p","kind":"ctl","text":"x"}]',
    );
  });
});

describe("truncateUtf8", () => {
  it("never splits a character", () => {
    expect(truncateUtf8("abc", 5)).toBe("abc");
    expect(truncateUtf8("héllo", 2)).toBe("h");
    expect(truncateUtf8("héllo", 3)).toBe("hé");
    expect(truncateUtf8("a😀", 4)).toBe("a");
    expect(truncateUtf8("a😀", 5)).toBe("a😀");
  });
});
