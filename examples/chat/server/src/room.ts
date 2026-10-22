// `iroh-do/room/1`: one chat room per object (design note 008, PROTOCOL.md).
//
// Members do not hold connections, so that the object can hibernate between messages. A
// member opens a connection to send a request; the room opens one to deliver events. Each side
// reuses a connection to the other that is already open, so the room keeps the connections it
// serves in `#connections`, and keeps serving a connection it dialed, until it closes.
//
// A member is present while it holds a relay socket to the object, the only path the room can
// dial it on. Each new event is queued for every joined, present member except the one it
// concerns, and the queue is written on one unidirectional stream after
// LIMITS.deliveryBatchMs. A failed delivery is not retried; members catch up with `/since`.
//
// Requests are handled synchronously, from the nick checks to the writes that depend on them,
// so no other request can interleave: the object runs one event at a time and yields only at
// an `await`.
import { type BiStream, type Connection, type Endpoint, EndpointId } from "iroh-do";
import { rethrowTrap, unlessTrap } from "./errors";
import { roomEvent } from "./events";
import { background, errorText, type Logger } from "./log";
import {
  defaultNicks,
  encodeLines,
  encodeUtf8,
  errorLine,
  frameRequest,
  historyLine,
  isValidNick,
  LIMITS,
  MAX_REQUEST_STREAM,
  nickKey,
  nickTakenError,
  parseRoomRequest,
  ROOM_ALPN,
  ROOM_ERRORS,
  type RoomKind,
  type RoomLine,
  type RoomRequest,
  requestKind,
} from "./protocol";
import { RateLimiter } from "./rate-limit";
import { type ChatStorage, type Member, type NewRecord, type RoomRecord, StorageError } from "./storage";

/** Counters since the object was constructed, for tests. */
export interface RoomStats {
  deliveriesOk: number;
  deliveriesFailed: number;
  /** Connections dialed to deliver events. */
  deliveryDials: number;
  /** Delivery streams opened on a connection that was already open. */
  deliveryReuses: number;
  rateLimited: number;
}

/** Event lines waiting for one recipient. */
interface Outbox {
  readonly lines: string[];
  /** The batch timer, until it fires. */
  timer: ReturnType<typeof setTimeout> | undefined;
}

type Via = "open" | "dial" | "none";

export class Room {
  readonly stats: RoomStats = {
    deliveriesOk: 0,
    deliveriesFailed: 0,
    deliveryDials: 0,
    deliveryReuses: 0,
    rateLimited: 0,
  };
  readonly #storage: ChatStorage;
  readonly #endpoint: () => Endpoint;
  readonly #log: Logger;
  readonly #limiter = new RateLimiter(LIMITS.rateLimitRequests, LIMITS.rateLimitWindowMs);
  /** Open room connections by peer, whichever side dialed; the newest wins. */
  readonly #connections = new Map<string, Connection>();
  /** Pending deliveries by recipient. An entry exists while its delivery runs. */
  readonly #outboxes = new Map<string, Outbox>();

  /** `endpoint` is called only once the node is built, from handlers and deliveries. */
  constructor(storage: ChatStorage, endpoint: () => Endpoint, log: Logger) {
    this.#storage = storage;
    this.#endpoint = endpoint;
    this.#log = log;
  }

  /**
   * Drops the connections of a node that no longer exists. The object builds a new node after
   * a wake or a wasm reset, and the connections of the old one are gone with it.
   */
  forgetConnections(): void {
    this.#connections.clear();
  }

  /**
   * Serves requests on `conn`, accepted or dialed, until it closes. While this runs, the room
   * may reuse the connection for deliveries.
   */
  async serve(conn: Connection): Promise<void> {
    const peer = conn.remoteId().toZ32();
    this.#connections.set(peer, conn);
    try {
      for (;;) {
        // acceptBi rejects once the connection is closed, by either side or when idle.
        const stream = await conn.acceptBi().catch(unlessTrap(null));
        if (stream === null) return;
        background(this.#serveRequest(peer, stream), this.#log, "room request");
      }
    } finally {
      if (this.#connections.get(peer) === conn) this.#connections.delete(peer);
    }
  }

  /** The room's status fields. */
  status(): { members: number; present: number } {
    const present = this.#presentPeers();
    const members = this.#storage.joinedMembers();
    return { members: members.length, present: members.filter((m) => present.has(m.peer)).length };
  }

  async #serveRequest(peer: string, stream: BiStream): Promise<void> {
    // Rejects for a request longer than the limit and for a stream the peer reset.
    const bytes = await stream.recv.readToEnd(MAX_REQUEST_STREAM).catch(unlessTrap(null));
    const replies = bytes === null ? [errorLine(ROOM_ERRORS.unreadable)] : this.#handleBytes(peer, bytes);
    try {
      await stream.send.writeAll(encodeUtf8(encodeLines(replies)));
      await stream.send.finish();
    } catch (error) {
      rethrowTrap(error);
      // The member closed the connection or reset the stream: the reply cannot reach it.
    }
  }

  /** Frames, parses and handles one request; logs it as `room_request`, without its text. */
  #handleBytes(peer: string, bytes: Uint8Array): RoomLine[] {
    const frame = frameRequest(bytes);
    // Framing errors are answered before the rate limit, and are not counted against it.
    if (!frame.ok) return [errorLine(frame.error)];
    const request = parseRoomRequest(frame.line);
    const replies = this.#respond(peer, request);
    this.#log.info("room_request", {
      peer,
      kind: requestKind(request),
      nick: this.#storage.member(peer)?.nick ?? "",
      ok: replies[0]?.t !== "err",
    });
    return replies;
  }

  #respond(peer: string, request: RoomRequest): RoomLine[] {
    if (!this.#limiter.allow(peer, Date.now())) {
      this.stats.rateLimited++;
      return [errorLine(ROOM_ERRORS.rateLimited)];
    }
    try {
      return this.#handle(peer, request);
    } catch (error) {
      // Only a write that was rolled back: the request stored nothing, so the member may send
      // it again. Anything else is not answered as if nothing happened.
      if (!(error instanceof StorageError)) throw error;
      this.#log.error("room_request_failed", { peer, kind: requestKind(request), error: errorText(error) });
      return [errorLine(ROOM_ERRORS.storage)];
    }
  }

  #handle(peer: string, request: RoomRequest): RoomLine[] {
    const member = this.#storage.member(peer);
    if (request.command === "join") return this.#join(peer, member, request.nick);
    if (member === null || !member.joined) return [errorLine(ROOM_ERRORS.notJoined)];
    switch (request.command) {
      case "say":
        return [this.#publish(null, { peer, kind: "msg", nick: member.nick, text: request.text })];
      case "me":
        return [this.#publish(null, { peer, kind: "act", nick: member.nick, text: request.text })];
      case "nick": {
        const problem = this.#nickProblem(peer, request.nick);
        if (problem !== null) return [errorLine(problem)];
        if (request.nick === member.nick) return [errorLine(ROOM_ERRORS.nickUnchanged)];
        return [this.#rename(member, request.nick)];
      }
      case "who":
        return [this.#who()];
      case "history":
        return this.#storage.recentEvents(request.count).map(historyOf);
      case "since":
        return this.#storage.eventsAfter(request.after, LIMITS.sinceMax).map(historyOf);
      case "quit":
        this.#cancelDelivery(peer);
        return [
          this.#publish({ ...member, joined: false }, { peer, kind: "part", nick: member.nick, text: request.reason }),
        ];
      case "invalid":
        return [errorLine(request.error)];
      case "unknown":
        return [errorLine(ROOM_ERRORS.unknown)];
    }
  }

  /** `/join [nick]` (PROTOCOL.md): the history and `ready`, or an error. */
  #join(peer: string, member: Member | null, nick: string): RoomLine[] {
    if (member?.joined === true) {
      if (nick !== "" && nick !== member.nick) {
        const problem = this.#nickProblem(peer, nick);
        if (problem !== null) return [errorLine(problem)];
        this.#rename(member, nick);
        return this.#joinReply(peer, nick);
      }
      return this.#joinReply(peer, member.nick);
    }
    let chosen: string;
    if (nick === "") {
      chosen = this.#defaultNick(peer, member);
    } else {
      const problem = this.#nickProblem(peer, nick);
      if (problem !== null) return [errorLine(problem)];
      chosen = nick;
    }
    this.#publish({ peer, nick: chosen, joined: true }, { peer, kind: "join", nick: chosen, text: "" });
    return this.#joinReply(peer, chosen);
  }

  #joinReply(peer: string, nick: string): RoomLine[] {
    const history: RoomLine[] = this.#storage.recentEvents(LIMITS.historyDefault).map(historyOf);
    const room = this.#endpoint().id().toZ32();
    return [...history, { t: "ready", room, you: { peer, nick }, last_seq: this.#storage.roomSeq() }];
  }

  #rename(member: Member, nick: string): RoomLine {
    return this.#publish({ ...member, nick }, { peer: member.peer, kind: "nick", nick, text: member.nick });
  }

  /** A previous nick if it is still free, else the first free one of `defaultNicks`. */
  #defaultNick(peer: string, previous: Member | null): string {
    const taken = this.#takenNicks(peer);
    if (previous !== null && !taken.has(nickKey(previous.nick))) return previous.nick;
    const candidates = defaultNicks(peer);
    // Ends: finitely many nicks are taken.
    for (;;) {
      const nick = candidates.next().value;
      if (!taken.has(nickKey(nick))) return nick;
    }
  }

  /** Null if `nick` is valid and free for `peer`, else the error text. */
  #nickProblem(peer: string, nick: string): string | null {
    if (!isValidNick(nick)) return ROOM_ERRORS.invalidNick;
    return this.#takenNicks(peer).has(nickKey(nick)) ? nickTakenError(nick) : null;
  }

  /** The nick keys held by joined members other than `peer`. */
  #takenNicks(peer: string): Set<string> {
    return new Set(this.#storage.joinedMembers().flatMap((m) => (m.peer === peer ? [] : [nickKey(m.nick)])));
  }

  /**
   * Stores an event, with the member's new state if `update` is given, queues it for the
   * other joined, present members, and returns the `ok` reply. Throws StorageError if nothing
   * was stored. Once the event is stored the reply is `ok`, even if queueing it fails: a member
   * told otherwise would send it again and store it twice.
   */
  #publish(update: Member | null, record: NewRecord<RoomKind>): RoomLine {
    const now = Date.now();
    const stored =
      update === null ? this.#storage.append(record, now) : this.#storage.putMemberAndAppend(update, record, now);
    try {
      this.#queue(stored);
    } catch (error) {
      rethrowTrap(error);
      // The members that miss the event see the gap through `prev`.
      this.#log.error("room_queue_failed", { peer: stored.peer, seq: stored.seq, error: errorText(error) });
    }
    return { t: "ok", seq: stored.seq, prev: stored.prev };
  }

  /** Queues a stored event for the joined, present members other than the one it concerns. */
  #queue(stored: RoomRecord): void {
    const line = `${JSON.stringify(roomEvent(stored))}\n`;
    const present = this.#presentPeers();
    let recipients = 0;
    for (const m of this.#storage.joinedMembers()) {
      if (m.peer === stored.peer || !present.has(m.peer)) continue;
      recipients++;
      this.#enqueue(m.peer, line);
    }
    this.#log.info("room_event", { peer: stored.peer, kind: stored.kind, seq: stored.seq, recipients });
  }

  #who(): RoomLine {
    const present = this.#presentPeers();
    const members = this.#storage
      .joinedMembers()
      .map((m) => ({ peer: m.peer, nick: m.nick, present: present.has(m.peer) }));
    return { t: "who", members };
  }

  /** The peers holding a relay socket to the object. */
  #presentPeers(): Set<string> {
    return new Set(
      this.#endpoint()
        .peers()
        .map((id) => id.toZ32()),
    );
  }

  // ---- Deliveries ------------------------------------------------------------------------

  #enqueue(recipient: string, line: string): void {
    const pending = this.#outboxes.get(recipient);
    if (pending !== undefined) {
      pending.lines.push(line);
      return;
    }
    const outbox: Outbox = { lines: [line], timer: undefined };
    outbox.timer = setTimeout(() => {
      outbox.timer = undefined;
      background(this.#flush(recipient, outbox), this.#log, "room delivery");
    }, LIMITS.deliveryBatchMs);
    this.#outboxes.set(recipient, outbox);
  }

  /** Drops the pending deliveries of a member that quit. */
  #cancelDelivery(recipient: string): void {
    const outbox = this.#outboxes.get(recipient);
    if (outbox === undefined) return;
    if (outbox.timer !== undefined) clearTimeout(outbox.timer);
    this.#outboxes.delete(recipient);
  }

  /**
   * Writes `outbox` until it is empty or cancelled; lines queued meanwhile go in the next batch.
   * The outbox is removed however this ends: while it is registered, `#enqueue` only appends
   * to it, so an outbox left behind by a delivery that threw would hold that member's
   * deliveries forever.
   */
  async #flush(recipient: string, outbox: Outbox): Promise<void> {
    try {
      while (this.#outboxes.get(recipient) === outbox && outbox.lines.length > 0) {
        const lines = outbox.lines.splice(0);
        const started = Date.now();
        const { ok, via } = await this.#deliver(recipient, lines.join(""));
        const fields = { peer: recipient, events: lines.length, via, ok, ms: Date.now() - started };
        if (ok) {
          this.stats.deliveriesOk++;
          this.#log.info("room_delivery", fields);
        } else {
          this.stats.deliveriesFailed++;
          this.#log.warn("room_delivery", fields);
        }
      }
    } finally {
      if (this.#outboxes.get(recipient) === outbox) this.#outboxes.delete(recipient);
    }
  }

  /** Writes `payload` to `recipient` on one unidirectional stream, reusing a connection if one is open. */
  async #deliver(recipient: string, payload: string): Promise<{ ok: boolean; via: Via }> {
    const open = this.#openConnection(recipient);
    if (open !== undefined && (await writeUni(open, payload))) {
      this.stats.deliveryReuses++;
      return { ok: true, via: "open" };
    }
    if (!this.#presentPeers().has(recipient)) return { ok: false, via: "none" };
    let conn: Connection;
    try {
      conn = await this.#endpoint().connect(EndpointId.fromZ32(recipient), ROOM_ALPN);
    } catch (error) {
      rethrowTrap(error);
      // The relay socket closed, or the member did not answer the handshake.
      this.#log.warn("room_dial_failed", { peer: recipient, error: errorText(error) });
      return { ok: false, via: "dial" };
    }
    this.stats.deliveryDials++;
    // The member may send its next requests on this connection.
    background(this.serve(conn), this.#log, "room connection");
    return { ok: await writeUni(conn, payload), via: "dial" };
  }

  /** The open connection to `peer` that the room keeps, if any; forgets a closed one. */
  #openConnection(peer: string): Connection | undefined {
    const conn = this.#connections.get(peer);
    if (conn === undefined) return undefined;
    let closed: boolean;
    try {
      closed = conn.closeReason() !== undefined;
    } catch (error) {
      // A connection of a wasm instance that was replaced throws on every call.
      rethrowTrap(error);
      closed = true;
    }
    if (!closed) return conn;
    if (this.#connections.get(peer) === conn) this.#connections.delete(peer);
    return undefined;
  }
}

function historyOf(record: RoomRecord): RoomLine {
  return historyLine(roomEvent(record));
}

/** Writes `payload` on a new unidirectional stream; false if the connection failed. */
async function writeUni(conn: Connection, payload: string): Promise<boolean> {
  try {
    const stream = await conn.openUni();
    await stream.writeAll(encodeUtf8(payload));
    await stream.finish();
    return true;
  } catch (error) {
    rethrowTrap(error);
    // The connection closed or the member stopped the stream; the caller counts the failure.
    return false;
  }
}
