# Chat protocols

The wire definition of the four ALPNs that the chat object serves and uses:
`iroh-do/room/1`, `iroh-do/ctl/1`, `iroh-do/echo/1` and `iroh-do/notify/1`. It
describes what the Rust chat app (`examples/chat/app/src/{lib,room,ctl,store}.rs`)
does, and where design note 008 and the code differ, the code is authoritative
(see [Differences](#differences-from-design-note-008-and-between-implementations)).
Every server implementation must match it, and every client must be able to talk
to it. The golden fixtures in [`fixtures/`](fixtures/README.md) express the same
behavior as scripted exchanges; the Rust app runs them in
`examples/chat/app/tests/fixtures.rs` and the client checks its types against
them in `examples/chat/client/src/fixtures_check.rs`.
[`constants.json`](constants.json) holds the ALPNs and limits that both sides
duplicate; the app's test and the client's test each assert it.

Terms are as in `docs/design.md`: the *object* is the Durable Object that hosts
the endpoint `E`, a *peer* is any other iroh endpoint, a *member* is a peer that
has joined the room, and a member is *present* while its relay socket to the
object is open (the chat client makes the object its home relay, so this holds
while its process runs). Endpoint ids in these protocols are the 52-character
z-base-32 form (`PublicKey::to_z32`), never the hex `Display` form.

## Conventions

- A *line* is UTF-8 text ended by `\n`. Lines the object writes always end in
  exactly `\n`. JSON lines are one JSON value on one line; the object writes keys
  in the order shown below, but readers must not depend on it.
- The object escapes `"`, `\` and characters below U+0020 (as `\u00XX`) in JSON
  strings and writes everything else as raw UTF-8. Readers must accept any valid
  JSON string encoding.
- A request that arrives on a stream, or a read that fails, is answered on the
  same stream. Which streams exist is defined per protocol below.
- Any connection may be closed by the object when idle (`t_idle`, 10 s; design
  note 008). Nothing in the protocols depends on a connection staying open.

## `iroh-do/room/1`

ALPN bytes `iroh-do/room/1`. A small chat room: one room per object, anyone who
knows `E` can join.

### Connections and streams

Either side may open the connection, and each side uses an open connection to
the other before dialing a new one. A member dials to send a request. The object
dials a member (over the member's relay socket) to deliver events. Whichever side
dialed, the same rules apply on that connection:

- **Requests** travel on bidirectional streams that the member opens, one request
  per stream. The member writes the request and finishes its send side. The
  object reads until the end of the stream, then writes zero or more reply lines
  and finishes its send side. The object does not reply before it has seen the
  end of the request, so a member that does not finish its send side gets no
  reply.
- **Deliveries** travel on unidirectional streams that the object opens. A member
  that dialed the object must accept them on that connection; a member that did
  not must accept the connection the object dials. Each stream carries one or
  more event lines and is then finished.

The object opens no bidirectional streams and accepts no unidirectional ones.

### Request framing

The request is one line of at most `MAX_REQUEST` = 1024 bytes, not counting its
terminator. The terminator is optional: the request may end with `\n`, `\r\n` or
nothing. The object reads at most 1026 bytes (1024 plus `\r\n`). Checks run in
this order and the first that fails ends the request with an `err` reply:

| Condition | `err` text |
|---|---|
| more than 1026 bytes on the stream, or the stream failed | `request too long or unreadable` |
| line (after removing one trailing `\n`, then one trailing `\r`) longer than 1024 bytes | `request too long` |
| line contains `\n` | `one request per stream` |
| line is not valid UTF-8 | `request is not utf-8` |

These four replies are sent before the rate limit is consulted, so they do not
count against it. An empty stream (zero bytes) is the empty line.

### Request grammar

The line is split at its first space (U+0020) into a command and an argument. A
line without a space is a command with an empty argument. The argument is then
trimmed of leading and trailing white space (Unicode `White_Space`, as Rust's
`str::trim`). A command is matched exactly and is case-sensitive; only the space
character separates it from its argument (`/say\thi` is the unknown command
`/say\thi`).

After the framing checks, every request is handled in this order:

1. **Rate limit.** At most `RATE_LIMIT` = 10 requests per sliding window of 10
   seconds per peer id. The 11th and later requests inside the window get
   `{"t":"err","text":"rate limit: at most 10 requests per 10s"}` and are not
   counted in the window. The window is kept in memory.
2. **`/join`** is handled as below, whether or not the peer is joined.
3. A peer that is not joined gets `{"t":"err","text":"not joined: send /join [nick] first"}`
   for every other request, including unknown ones.
4. The remaining commands, for joined members.

| Request | Reply lines |
|---|---|
| `/join [nick]` | history events (`hist:true`), then `ready` |
| `/say <text>` | `ok` |
| `/me <text>` | `ok` |
| `/nick <name>` | `ok` |
| `/who [ignored]` | `who` |
| `/history [n]` | zero or more events (`hist:true`) |
| `/since <seq>` | zero or more events (`hist:true`) |
| `/quit [reason]` | `ok` |
| anything else | `{"t":"err","text":"unknown request"}` |

Any request can instead get a single `err` line.

**`/say`, `/me`.** An empty argument gets `usage: /say <text>` (or `/me`). The text
is the trimmed argument, at most 1019 bytes in practice; it is stored as is. It
stores a `msg` (`/say`) or `act` (`/me`) event with the member's current nick.

**`/nick <name>`.** First `name` is checked as a nick (below): invalid gets
`invalid nick: 1-24 characters of A-Z a-z 0-9 _ . -` (this includes an empty
argument), taken gets `nick <name> is taken` with `name` as given. Then, if
`name` is byte-for-byte the member's current nick, `nick unchanged`. Otherwise
the member's nick becomes `name` and a `nick` event is stored. A change of case
only (`bob` to `BOB`) is a rename.

**Nick rule.** 1 to `NICK_MAX` = 24 characters, each one of `A-Z a-z 0-9 _ . -`
(ASCII only, so characters and bytes coincide). A nick is taken when another
*joined* member has it, comparing ASCII case-insensitively; a member's own nick
never conflicts with itself, and a member that has quit holds no nick. The
nicks `.` and `..` are valid.

**`/join [nick]`.**

- Joined, argument empty or equal to the current nick (byte-for-byte): no change.
- Joined, any other argument: renames exactly as `/nick` does (same errors, same
  `nick` event), without the `ok`.
- Not joined, argument given: the nick must be valid and not taken, else the
  `err` as for `/nick`. The member joins with it.
- Not joined, no argument: the member's previous nick if it has one and it is
  not taken; otherwise the first 8 characters of its id; if that is taken, the
  same with `-2`, `-3`, ... appended.

Joining stores a `join` event (not for the two no-change cases). Then the reply is
the newest `HISTORY_DEFAULT` = 50 room events, oldest first, each with
`hist:true`, which includes the member's own `join` and any `nick` event this
request made, followed by one `ready` line.

**`/who`.** One `who` line listing joined members ordered by nick, comparing
ASCII case-insensitively. Arguments are ignored.

**`/history [n]`.** Empty argument: `n` = 50. Otherwise the argument must parse as
a non-negative decimal integer (digits, with an optional leading `+`), else
`usage: /history [n], n <= 200`. `n` above `HISTORY_MAX` = 200 is reduced to 200
without an error. Replies with the newest `n` room events, oldest first; `n` = 0
replies with no lines.

**`/since <seq>`.** The argument must parse as an unsigned 64-bit decimal integer
(optional leading `+`), else `usage: /since <seq>`. Replies with the room events
whose `seq` is greater than `seq`, oldest first, at most `SINCE_MAX` = 500. When
more exist, the reply holds the oldest 500, so a caller pages by repeating the
request with the last `seq` it received.

**`/quit [reason]`.** The member stops being joined and its queued deliveries are
dropped. A `part` event is stored with the trimmed argument as `reason` (empty
if none) and delivered to the other members. The member keeps its previous nick
for a later `/join` but no longer holds it.

### Reply and event lines

Every line is a JSON object with a `t` string. `seq`, `prev`, `ts`, `last_seq`
are non-negative integers. `peer` and `room` are z-base-32 ids.

```
{"t":"msg", "seq":12,"prev":9, "ts":1792573500000,"peer":"<z32>","nick":"ana","text":"hi"}
{"t":"act", "seq":13,"prev":12,"ts":…,"peer":"<z32>","nick":"ana","text":"waves"}
{"t":"join","seq":14,"prev":13,"ts":…,"peer":"<z32>","nick":"bo"}
{"t":"part","seq":15,"prev":14,"ts":…,"peer":"<z32>","nick":"bo","reason":"bye"}
{"t":"nick","seq":16,"prev":15,"ts":…,"peer":"<z32>","old":"bo","new":"bob"}
{"t":"who","members":[{"peer":"<z32>","nick":"ana","present":true}]}
{"t":"ready","room":"<E z32>","you":{"peer":"<z32>","nick":"ana"},"last_seq":16}
{"t":"ok","seq":18,"prev":16}
{"t":"err","text":"…"}
```

Room events (`msg`, `act`, `join`, `part`, `nick`) have exactly the fields shown,
plus `"hist":true` as the last key when the event is a line of a `/join`,
`/history` or `/since` reply. Deliveries and `ok` never carry `hist`.

| Field | Type | Meaning |
|---|---|---|
| `seq` | integer | position in the object's message log; unique, increasing, never reused |
| `prev` | integer | `seq` of the room event before this one, 0 if none |
| `ts` | integer | milliseconds since the UNIX epoch on the object's clock when the event was stored |
| `peer` | string | the author's id; for `nick` and `part`, the member concerned |
| `nick` | string | the author's nick at that time (`join`, `part`, `msg`, `act`) |
| `text` | string | message or action text, trimmed, 1 to 1019 bytes |
| `reason` | string | `part` only; always present, `""` when none |
| `old`, `new` | string | `nick` only; the nick before and after |
| `members[]` | object | `who` only: `peer`, `nick`, `present` (boolean: the member's relay socket to the object is open now) |
| `you` | object | `ready` only: the requester's `peer` and `nick` |
| `room` | string | `ready` only: the object's endpoint id |
| `last_seq` | integer | `ready` only: `seq` of the newest room event, 0 if none (after a successful join it is at least 1) |

`ok.seq` and `ok.prev` are those of the event the request stored.

`err` is a single line `{"t":"err","text":"…"}`. The texts the object writes are
exactly: the four framing errors and the rate limit above, `not joined: send
/join [nick] first`, `unknown request`, `usage: /say <text>`, `usage: /me
<text>`, `usage: /history [n], n <= 200`, `usage: /since <seq>`, `nick unchanged`,
`invalid nick: 1-24 characters of A-Z a-z 0-9 _ . -`, `nick <name> is taken` and
`storage error` (the log could not be written). Clients should show the text and
not branch on it.

### Sequence numbers and gaps

`seq` numbers every record in the log, which also holds the diagnostic records of
`iroh-do/ctl/1`, `echo` and `notify`, so consecutive room events can skip
numbers. `prev` skips them: it is the `seq` of the previous room event. A client
that last saw room event `L` has missed room events exactly when an event or
`ok` arrives with `prev > L`, and `/since L` returns them. `seq` and `prev` are
never lower after pruning (see Retention).

### Deliveries

When an event is stored, the object queues it for every member that is joined and
present at that moment, except the member the event concerns (for `part`, the
member that quit; for `nick`, the one that renamed). The queue of each recipient
is flushed by a task that first waits `DELIVERY_BATCH` = 50 ms, then writes all
queued lines on one unidirectional stream, repeating while more arrive. A
delivery that fails is not retried; the member notices the gap through `prev`.
The object writes the lines of one recipient in `seq` order, but two streams are
independent, so a receiver may read the later stream first. The object does not
deliver to a member that is joined but not present.

### Retention

The log keeps at most `LOG_RETENTION` = 10 000 rows and no row older than 14 days
(1 209 600 s). Both are applied on every insert, and a quiet log is pruned by an
alarm scheduled `LOG_MAX_AGE` + 60 s after the newest insert. Rows of all kinds
count. History and `/since` return only rows that remain.

## `iroh-do/ctl/1`

ALPN bytes `iroh-do/ctl/1`. A diagnostic line protocol on the same object: it
echoes lines and reports state, and the `lifecycle` command of the chat client
uses it.

### Streams

The peer opens a bidirectional stream and writes request lines on it. The object
replies with exactly one line per request line, in order, on the same stream, and
keeps reading. Lines end in `\n` or `\r\n`; a final line without terminator at
the end of the stream counts as a line. When the peer finishes its send side, the
object finishes its own. Several streams may be open on a connection.

Lines are at most `MAX_LINE` = 4096 bytes. A line that is longer, not UTF-8, or
cannot be read gets one reply line `error: line too long`, `error: line is not
utf-8` or `error: <read error>`; the object then stops reading that stream and
finishes it, so the peer must open a new stream.

**Unprompted line.** After it has written the first reply on a connection, the
object opens one unidirectional stream on that connection and writes
`hello from <E z32>: you are <peer z32>\n`, once per connection.

### Requests

A line that equals one of the commands below, or is the command followed by a
space and an argument, is that command. Any other line, including `/status x`,
`/count x` and `/logx`, is logged and echoed.

| Line | Reply (one line) |
|---|---|
| `/status` | one JSON object, see below; not logged |
| `/count` | `{"total":T,"yours":Y,"peers":P}`; not logged |
| `/log [n]` | JSON array of log records; not logged |
| `/callback <secs>` | `ok: callback in <secs>s`; not logged |
| any other line | `echo: <line> (#<seq>, total <seq>, yours <Y>)`; logged |

**Echo.** The line (without terminator) is stored as a record of kind `ctl`, its
text cut to `MAX_LOG_TEXT` = 1024 bytes at a character boundary, and `seq` is the
record's sequence number. The reply repeats the whole line, uncut. `total` is the
same number as `seq`. `Y` is the number of retained records of this peer, the one
just stored included.

**`/count`.** `T` is the highest `seq` ever used (monotonic through pruning), `Y`
the retained records of the asking peer, `P` the distinct peers among retained
records.

**`/log [n]`.** `n` defaults to `ctl_log_default` = 5; the argument must parse as
a non-negative decimal integer, else `error: usage /log [n], n <= 50`; values
above 50 are reduced to 50. The reply is the newest `n` retained records of every
kind (room events included), oldest first, as
`{"seq":int,"ts_ms":int,"peer":"<z32>","kind":"…","text":"…"}`. `kind` is `ctl`,
`echo`, `callback`, `msg`, `act`, `join`, `part` or `nick`. `text` is the line
for `ctl`, `"<n> bytes"` for `echo`, `delivered` or `failed: <reason>` for
`callback`, and for room kinds the message, the action, empty for `join`, the
reason for `part`, and the old nick for `nick`.

**`/status`.** A JSON object with these keys: `id` (the object's id), `app`
(`"chat"`), `state` (`"Active"` while answering), `wakes`, `endpoint_builds`,
`connections` (integers), `bundle` (string, a build identifier), `since_wake_ms`
(integer), `records` (integer, rows in the log), `members` (joined members) and
`present` (those with an open relay socket). Which host runs the object decides
the values of `wakes`, `endpoint_builds`, `connections`, `bundle` and
`since_wake_ms`; the others are defined by the chat app.

**`/callback <secs>`.** `secs` must parse as an unsigned decimal integer of at most
`MAX_CALLBACK_SECS` = 50 (the peer's relay client closes a relay socket that is not
its home relay 60 s after its last send), else `error: usage /callback <secs>,
secs <= 50`. The object schedules one dial-back for that peer after `secs`
seconds. See `iroh-do/notify/1`.

## `iroh-do/echo/1`

ALPN bytes `iroh-do/echo/1`. The peer opens a bidirectional stream; the object
writes back every byte it reads, in order, and finishes its send side when the
peer finishes. Any bytes are allowed. When the stream ends, the object stores a
record of kind `echo` with text `<n> bytes` (n bytes echoed; an empty stream
stores `0 bytes`). The record is stored after the stream ends, so it can be
missing from a `/log` that follows at once.

## `iroh-do/notify/1`

ALPN bytes `iroh-do/notify/1`. The *peer* accepts this protocol. When a
`/callback` delay ends, the object dials the peer over the peer's relay socket
with this ALPN, opens one unidirectional stream, writes

```
callback from <E z32> wakes=<N> records=<T>\n
```

and finishes it (`N`: the object's wake count; `T`: records in the log when the
line is written). It then waits up to 5 s for the peer to close the connection and
closes it. If the dial or the write fails, nothing is delivered. Either way a
record of kind `callback` is stored with text `delivered` or `failed: <reason>`.
The object writes it once the attempt has ended, so it appears in `/log` after
the dial-back.

## Not guaranteed

- Delivery of room events. A delivery is best effort, once. Members rely on
  `prev` and `/since`.
- The grouping of delivered lines into streams, and the order of a delivery
  relative to the reply of the request that caused it (they travel on different
  streams and connections).
- That `present` is current: it is read when the line is built.
- That connections stay open: the object closes idle ones.
- That the rate limit survives an eviction of the object (it is in memory).
- That `ts` is monotonic or that events with higher `seq` have a later `ts`.
- The values of `/status` fields other than their types and the app's own fields.
- Anything about nicks outside the rule above; the object does not normalize
  Unicode because it accepts only ASCII nicks.
- That history reaches back further than the retention above.

## Differences from design note 008 and between implementations

Where note 008 and the code differ, this document follows the code.

1. **`/who` lists joined members only.** Note 008 does not say so. A member that
   quit is not listed.
2. **`/since` returns the oldest 500 events after `seq`**, not the newest. Note
   008 says "max 500". A caller that gets 500 lines must ask again.
3. **Request framing.** Note 008 says "one UTF-8 line (≤ 1024 bytes)". The object
   accepts a missing, `\n` or `\r\n` terminator, reads up to 1026 bytes, and has
   four distinct framing errors (listed above). Framing errors are not rate
   limited.
4. **Unknown requests from a peer that has not joined** get `not joined`, not
   `unknown request`; note 008 says requests other than `/join` get `err` and is
   silent on the text.
5. **`part.reason` is always present** (`""` when none); note 008's example shows it
   with a value only.
6. **`/join [nick]` when already joined.** Note 008 says a different nick renames.
   A nick that differs only in case counts as different. The same nick, or none,
   changes nothing and stores no event.
7. **A returning member without a nick** keeps its previous nick only if it is
   free; otherwise it gets the first 8 characters of its id, with `-2`, `-3`, ...
   if that is taken. Note 008 says it keeps its previous nick.
8. **`/nick` with the member's current nick** gets `nick unchanged`; note 008 does
   not mention it.
9. **`/history 0`** replies with no lines; values above 200 are silently reduced.
10. **Retention alarm.** Note 008 and `docs/design.md` speak of a daily alarm. The
    app schedules one alarm per insert for 14 days plus 60 seconds later under the
    key `prune`, so there is a single pending alarm, not a daily one.
11. **Integer arguments.** `/history` and `/log` parse a platform `usize`: on the
    wasm32 build that serves the object, values above 4 294 967 295 get the usage
    error, where the native build reduces them to the maximum. `/since` and
    `/callback` parse `u64`. All of them accept a leading `+` (`/history +2`), and leading zeros.
    The fixtures do not cover these corners.
12. **Argument trimming** uses Rust's `trim` (Unicode `White_Space`). An
    implementation in another language must use the same set for arguments that
    contain such characters (for example U+00A0 or U+2003), or document the
    difference. JavaScript's `trim` also removes U+FEFF, which Rust does not.
13. **The chat client** (`examples/chat/client/src/room.rs`) differs from the
    object in two ways, both on purpose and both recorded in
    `fixtures_check.rs`: its `Event` has no `hist` field, so it drops it; and its
    `check_nick` also refuses `.` and `..`, which name its state directory, while
    the object accepts them.
14. **Clock.** Note 008 does not say whose clock `ts` uses: it is the object's,
    taken when the event is stored.
