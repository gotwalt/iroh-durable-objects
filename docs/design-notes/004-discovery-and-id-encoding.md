# 004: EndpointId text form and discovery publishing

**Status:** accepted (2026-10-21). Amends §4, §5.1, §5.9, §2.2.
**Evidence:** `docs/verification/discovery-and-prior-art.md` (V16).

1. **`<E>` is always z-base-32 (52 chars)**, produced by `PublicKey::to_z32()`
   and parsed **only** by `PublicKey::from_z32()`. `EndpointId`'s `Display` is
   64-char hex and its `FromStr` misparses z32 (and silently yields a
   different key for ~0.6% of inputs), so `.parse()` / `to_string()` are banned
   on the routing path; a router unit test pins this. z32 matches iroh's own
   DNS naming (`_iroh.<z32>.dns.iroh.link`) and keeps the hostname form
   available to operators with wildcard certs.
2. **Publish at mint time, before E is handed out**, then confirm by GET
   (a stale-timestamp PUT returns 204 but is not stored). This avoids
   resolvers negatively caching a not-yet-published name (~30 min seen).
3. **Republish from a DO alarm every 24 h** (n0's server expires records after
   7 days by default). Record TTL 30 s, relay-only, matching iroh's publisher.
4. **Rate limit risk:** n0's pkarr relay limits PUTs per IP (~1 per 4 s,
   burst 2), and Workers egress from shared IPs. On 429, the alarm retries
   with jittered backoff; measure in Phase 6. A self-hosted pkarr relay Worker
   is the documented escape hatch, not built for the example.
5. **Spec correction:** custom transport ids are `u64` in 1.4.0, not `u16`.
6. The demo CLI accepts `--relay-url` (`EndpointAddr::new(id).with_relay_url`)
   to bypass discovery for debugging; the default demo path uses discovery
   only (P3).
