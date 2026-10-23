# Golden fixtures for the chat protocols

Scripted exchanges that define the behavior of `iroh-do/room/1`, `iroh-do/ctl/1`
and `iroh-do/echo/1` (and the lines the object sends unprompted, including
`iroh-do/notify/1`) as described in [`../PROTOCOL.md`](../PROTOCOL.md). Every
server implementation runs them; the client checks its types against them.

| Who | What | Where |
|---|---|---|
| Rust chat app | runs every fixture against the app in the native harness, with stock iroh peers | `examples/chat/app/tests/fixtures.rs` |
| TypeScript chat object | the same runner, against `wrangler dev` of `examples/ts-deployment`: `npm --prefix examples/chat/server run test:fixtures` builds it, starts it and runs the fixtures (`FIXTURE_TARGET=<port>`, see the runner's header) | `examples/chat/app/tests/fixtures.rs` |
| Rust chat client | parses every reply and event line with its own types and fails on a line it cannot parse or a field it drops; checks its request limits and nick rule against the requests | `examples/chat/client/src/fixtures_check.rs` |
| Both | assert the ALPNs and limits in [`../constants.json`](../constants.json) | the same two files |

The fixtures were recorded from the Rust app. To re-record, remove the `expect`
(or `expect_raw`) and `deliveries` of the steps to redo, or write a new step with
only `member` and `send`, and run

    RECORD_FIXTURES=1 cargo test -p iroh-do-chat-app --test fixtures

The runner fills in what is missing from what the app did and leaves steps that
already have an expectation alone (it still checks them). Look at the diff:
recording writes down whatever the server does, right or wrong.

## File format

One JSON file per scenario, `NN-name.json`. A scenario runs on a new, empty
object. Files are independent and may run concurrently.

```json
{
  "name": "join-and-talk",
  "description": "What the scenario shows.",
  "members": ["ana", "bo"],
  "steps": [ ... ]
}
```

`members` names the peers. Each is a separate iroh endpoint with its own key
whose home relay is the object, so it is present from the start. Names are only
labels for the fixture; nicks are chosen by the steps.

### Steps

| Key | Meaning |
|---|---|
| `member` | who acts: one of `members` |
| `proto` | `"room"` (default), `"ctl"` or `"echo"` |
| `note` | free text for the reader |
| `send` | the exact request bytes, as *parts* (below) |
| `times` | send the same request this many times, expecting the same reply each time (default 1) |
| `finish` | `ctl` only: finish the send side after writing, and read the reply to the end of the stream |
| `expect` | the reply lines, in order: a list of *line patterns* (below) |
| `expect_raw` | `echo` only: the reply bytes, as parts or a pattern string |
| `deliveries` | lines the object sends unprompted because of this step: `{ "<member>": [line patterns] }` |

A step `{"sleep_ms": N, "note": "..."}` waits N milliseconds; the rate limit
fixture uses it to let the window pass.

A `room` step opens a new connection, opens a bidirectional stream, writes `send`,
finishes the send side, and reads the reply to the end. The reply lines are the
lines in it; an empty reply is an empty `expect` list. A `ctl` member keeps one
connection and one stream across its steps (the stream is the protocol's
session); a step reads one reply line, or, with `finish`, everything until the
stream ends. A new connection is opened after a `finish`. An `echo` step opens a
stream, writes `send`, finishes, and reads to the end.

After each step the runner waits for every line in `deliveries` (each member's
list, in order, within 5 s) and then for 250 ms, and fails if any member
received a line that is not listed. Lines that members receive unprompted are:
events on `room`, the `hello` line on `ctl`, and the `notify` line after
`/callback`. Which stream a delivered line arrived on is not recorded.

Steps are written so that no member exceeds the room's rate limit (10 requests
per 10 s) except in `09-rate-limit.json`.

### Parts: request bytes and text

`send`, `expect_raw` and text patterns are *parts*: a string, a part object, or
an array of these, concatenated into bytes. Part objects:

| Part | Bytes |
|---|---|
| `{"$repeat": ["a", 1019]}` | the string repeated that many times |
| `{"$hex": "ff0a"}` | those bytes |
| `{"$ref": "name"}` | the bound value (below) as text |
| `{"$any": "int"}` | in an expectation only: one or more ASCII digits |

### Line patterns

An entry in `expect` or `deliveries` describes one line the object writes:

- A **string** is a text line that must equal it exactly (without `\n`).
- `{"$text": parts}` is a text line built from parts.
- Any **other JSON value** (an object, an array) is a JSON line, and the line's
  parsed value must match the pattern.

A JSON pattern matches a value like this:

- Scalars (strings, numbers, booleans, `null`) must be equal. Integers are
  literals: a scenario starts with an empty object, so `seq` and `prev` are
  deterministic and are written out.
- An array must have the same length and each element must match.
- An object must have exactly the same keys, in any order, and each value must
  match. This is what makes "fields the implementation adds or drops" a failure.
- A placeholder object, recognized by its single `$` key, matches as follows:

| Placeholder | Matches |
|---|---|
| `{"$any": "int"}` | any integer |
| `{"$any": "ts"}` | an integer above 1 600 000 000 000 (milliseconds since the epoch) |
| `{"$any": "string"}`, `"bool"`, `"any"` | any string, boolean, or value |
| `{"$capture": "name", "as": "int"}` | a value of type `as` (as for `$any`), and binds it to `name`; a name binds once |
| `{"$ref": "name"}` | a value equal to the bound value |
| `{"$text": parts}` | a string equal to the parts |

### Bound names

A runner binds these before the first step: `peer:<member>` (the member's
endpoint id, z-base-32), `peer8:<member>` (its first 8 characters, the default
nick) and `room` (the object's id). `$capture` binds more as steps run. A name
used before it is bound is an error.

## Writing a runner in another language

1. Start a new object per scenario, create one stock iroh endpoint per member
   with the object's relay as its only (home) relay, and wait until it is online.
2. For each step, run it as above, bind and match with the rules here, collect
   everything the member endpoints receive on unidirectional streams (on
   connections they dialed and on connections the object dialed), and attribute
   it to the step in which it arrives.
3. The runner must pass every fixture. The fixtures do not cover: `/history` or
   `/since` at their caps (200 and 500 events), the 10 000 row and 14 day
   retention, members that are joined but not present, and a delivery that
   fails. PROTOCOL.md defines those.
