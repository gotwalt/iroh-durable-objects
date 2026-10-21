# 003: Bidirectional messaging and DO-initiated dials are in scope

**Status:** accepted (2026-10-21). Amends §13.5 and §13.8.

## Decision

The replicable example must prove the DO is a *full* iroh endpoint inside a
hibernating environment, not only a server. §13.8's exclusion of
DO-initiated dials is lifted. The example adds:

1. **`iroh-do/chat/1` ALPN.** One connection, messages in both directions:
   the peer sends, the DO replies, and the DO also opens a stream to the peer
   unprompted (e.g. a server-originated notice on a timer while Active).
2. **DO-initiated dial-back.** Peer P connects (its relay client now holds a
   hibernatable socket to the DO), its QUIC connection closes, the DO goes
   Active → Warm and hibernates. A later trigger (admin request or alarm)
   wakes the DO, which **dials P** with `Endpoint::connect(P, ALPN)` and
   delivers a message on a stream it opened. P's application accepts it.
3. `iroh-do-demo` gains `chat <E>` and `listen <E>` (connects, registers,
   goes idle and waits to be dialed back), and `lifecycle` includes a
   dial-back step with timings.

## Mechanism

Preferred (no outbound connection, hibernation unaffected): datagrams from E
to P flow over P's **existing inbound** relay socket. The ingress transport's
`send(CustomAddr(P))` already does exactly this; dialing only requires the DO
Endpoint to know P is reachable at `CustomAddr(P)`, which the glue supplies
from the live-socket map (an address-lookup over attached peers).

Depends on **V20**: a stock client must accept an incoming connection that
arrives over a relay connection that is not its home relay.

Fallback if V20 fails: the DO opens a short-lived outbound relay connection to
P's published home relay for the duration of the dial, and drops it before
returning to Warm. Its cost (resident duration while open) is measured and
reported. This is a design note's worth of change, not a principle change.

## Acceptance

`just demo` shows, against both workerd and production: a chat session with a
DO-originated stream; then, after the DO is verified hibernated, a dial-back
from the DO that the idle peer accepts, with cold-wake → connect timing.
