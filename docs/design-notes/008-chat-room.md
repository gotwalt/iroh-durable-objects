# 008: The demo object is a chat room for a few people

**Status:** accepted (2026-10-21). Supersedes the chat part of note 006 and
extends note 007.

## Decision

Each endpoint is a small chat room: "me and my friends", not a public channel.
Members run `iroh-do-chat <E>`, see scrollback, and exchange messages
live. Messages are stored in the object's SQLite log, so scrollback survives
hibernation, eviction and redeploys.

The diagnostic protocol that `lifecycle` uses (echo with totals, `/status`,
`/count`, `/log`, `/callback`) keeps its behaviour and is renamed from
`iroh-do/chat/1` to `iroh-do/ctl/1`, so "chat" means the room only.

## Why members do not hold connections

A Durable Object chat built on plain WebSockets hibernates between messages
even with every member connected; its cost follows messages. A QUIC connection
cannot do that: it has timers (loss recovery, keep-alive, idle), and an object
with pending timers cannot hibernate. If members held QUIC connections, an
occupied room would stay Active and its cost would follow occupancy.

So members do not hold QUIC connections. A connection exists only while there
is something to say:

- A member that sends something dials the room (or reuses an open
  connection), sends, and lets the connection close when idle.
- To deliver new messages, the room dials each present member (or reuses an
  open connection) over the member's relay socket (dial-back).
- After `t_idle` (10 s) without streams, connections close, the object drops
  its `Endpoint` after `t_linger` (2 s), and hibernates.

### Presence through the home relay

The room can only dial a member whose relay socket to the room is open. A
stock iroh client closes a relay socket that is not its home relay 60 s after
its last send, which would make quiet members unreachable. The chat client
therefore configures the room's relay URL as its home relay
(`RelayMode::Custom`, stock iroh API). A client keeps its home relay
connection open and pings it every 15 s; the object answers each ping without
building an `Endpoint` (about 40 ms per ping, measured in
`docs/measurements/cost-profile.md`).

A member is **present** while its relay socket to the room is open, i.e. while
its client runs. The cost of the choice: the client can reach other endpoints
only through the room (which forwards nothing), so it is a dedicated chat
client, not a general-purpose iroh endpoint.

### Cost and its limits

Cost follows conversation time plus a small per-present-member term:

- While messages flow, the object is Active; it hibernates 12 s after the last
  one (`t_idle` + `t_linger`).
- Each present member costs one short wake per 15 s ping. The room does not
  need these wakes; they are the client's liveness check of its home relay,
  hard-coded in the stock relay client. The object must answer each Ping with
  a Pong echoing its random payload, which Cloudflare's WebSocket
  auto-response (fixed text pairs only) cannot do, so each ping wakes the
  object. A configurable ping interval in iroh, or a payload-echoing
  auto-response in workerd (workerd#6062), would remove this cost; this
  project uses neither.
- Every delivery to a member without an open connection is a new QUIC
  handshake from the object (0-RTT is not available across object restarts on
  stock iroh 1.4.0; see `upstream/`).

This suits a quiet room with a handful of members. It does not suit a busy
room or many members: fanout costs one connection per member, and pings scale
with presence. The structural fix is upstream: if noq could park an idle
connection with nothing in flight (no timers while parked), members could
hold connections and the object could still hibernate between messages, which
is the cost model of a plain WebSocket chat. Peers' 5 s keep-alives would then
need to be configurable or longer, since each one wakes the object.

## Protocol: `iroh-do/room/1`

Either side may open the connection. Each side reuses an open connection to
the other before dialing a new one.

### Requests (member → room)

The member opens a bidirectional stream per request, writes one UTF-8 line
(≤ 1024 bytes), and finishes its send side. The room replies with zero or more
JSON lines and finishes.

| Request | Reply |
|---|---|
| `/join [nick]` | History (newest 50 events, `hist: true`), then `ready`. Stores a `join` event unless the member is already joined. Without `nick`, a returning member keeps its previous nick and a new one gets the first 8 characters of its z-base-32 id. From a joined member, a different `nick` renames it as `/nick` does. |
| `/say <text>` | `{"t":"ok","seq":N,"prev":P}` |
| `/me <text>` | `{"t":"ok","seq":N,"prev":P}` |
| `/nick <name>` | `{"t":"ok","seq":N,"prev":P}` or `err` (1–24 chars of `[A-Za-z0-9_.-]`, unique among joined members ignoring ASCII case) |
| `/who` | `who` |
| `/history [n]` | Newest `n` events (default 50, max 200), `hist: true` |
| `/since <seq>` | Events after `seq` (max 500), `hist: true` |
| `/quit [reason]` | `{"t":"ok","seq":N,"prev":P}`; stores a `part` event; the member stops receiving deliveries |

Requests other than `/join` from a peer that has not joined get `err`.

### Deliveries (room → member)

For each new event the room delivers to every joined, present member except
the sender: it opens a unidirectional stream and writes the event's JSON line
(events produced within 50 ms are batched on one stream). A delivery that
fails is not retried; the member notices the gap through the next event's
`prev` and catches up with `/since` (the client tracks the last `seq` it has
seen).

### Events

```
{"t":"msg", "seq":12,"prev":9, "ts":…,"peer":"<z32>","nick":"ana","text":"hi"}
{"t":"act", "seq":13,"prev":12,"ts":…,"peer":"<z32>","nick":"ana","text":"waves"}
{"t":"join","seq":14,"prev":13,"ts":…,"peer":"<z32>","nick":"bo"}
{"t":"part","seq":15,"prev":14,"ts":…,"peer":"<z32>","nick":"bo","reason":"bye"}
{"t":"nick","seq":16,"prev":15,"ts":…,"peer":"<z32>","old":"bo","new":"bob"}
{"t":"who","members":[{"peer":"<z32>","nick":"ana","present":true}]}
{"t":"ready","room":"<E z32>","you":{"peer":"<z32>","nick":"ana"},"last_seq":16}
{"t":"ok","seq":18,"prev":16}
{"t":"err","text":"…"}
```

`ts` is milliseconds since the UNIX epoch. Replies to `/join`, `/history` and
`/since` set `"hist":true` on events. `ready.last_seq` is the newest room
event's `seq`.

`seq` numbers every log record, including the diagnostic ones, so consecutive
room events can skip numbers (10 and 11 above are `ctl` records, 17 an `echo`).
`prev` is the `seq` of the room event before this one (0 if none), on every
room event and on `ok`. A client that last saw room event `L` has missed events
exactly when an event or `ok` arrives with `prev > L`; `/since L` returns them.

### Limits

10 requests per 10 s per member; excess requests get `err`.

Retention: the log keeps at most the newest 10 000 rows (note 007) and no row
older than 14 days. Both limits are applied on every insert and by the daily
alarm, so a quiet room also sheds old rows. With chat lines capped at 1024
bytes this bounds a room's log to about 10 MB. `seq` and `total` stay
monotonic: pruning never reuses or lowers them.

## Storage

The `log` table has `nick` and `prev` columns. Kinds: `msg`, `act`, `join`,
`part`, `nick`, plus `echo`, `callback`, and `ctl` (lines on
`iroh-do/ctl/1`, formerly `chat`). The `counters` table holds `room_seq`, the
newest room event's `seq`, which becomes the next room event's `prev`; it
survives pruning. A `members` table holds peer id, nick, joined flag and
timestamps.

The schema is created as is on every wake (`CREATE TABLE IF NOT EXISTS`);
there is no upgrade path. This is prototype code: changing the schema means
resetting the object's state.

## Client

`iroh-do-chat <E> [--nick NAME]`: a terminal UI (ratatui) on stock
iroh, default features, home relay set to the room. A persistent key (so nick
and membership survive client restarts) and the last seen `seq` are stored per
room and nick under the user's data directory, so clients with different nicks
on one machine are different members. A lock refuses a second client with the
same room and nick. On start the client sends `/join`, and
`/since <last_seq>` if the oldest event in the join reply has `prev` beyond its
cursor. The client contacts the room only on user actions and to
catch up; it never polls, since every request wakes the object.

## Not in scope

Multiple rooms per object, private messages, operators, kicks, bans,
authentication beyond the member's iroh identity. Anyone who knows `E` can
join.
