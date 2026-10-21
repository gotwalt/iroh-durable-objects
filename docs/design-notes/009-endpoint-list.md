# 009: A public list of endpoints at admin.<zone>

**Status:** accepted (2026-10-21).

## Decision

`GET https://admin.<zone>/` serves a public HTML page listing every minted
endpoint. The deployment is a proof of concept, and its endpoint ids are
treated as public: anyone who reads the page can join any room. Minting stays
behind the admin token (`POST /mint`), because each minted endpoint is a
stored object.

## Why a registry

Cloudflare lists Durable Objects only by internal id, not by the name they
were created from, so the set of EndpointIds cannot be recovered from the
namespace. It is recorded when an endpoint is minted.

A second Durable Object class, `EndpointRegistry`, has one instance (named
`registry`, which is not valid z-base-32 and cannot collide with an
EndpointId). Its SQLite holds `endpoints(id TEXT PRIMARY KEY, label TEXT,
minted_at INTEGER)`. Adding the class is a new migration step (`v2`,
`new_sqlite_classes = ["EndpointRegistry"]`).

- **Mint** writes the row after the endpoint object has stored its identity.
  `POST /mint` accepts an optional `label` (≤ 64 chars).
- **Backfill:** an endpoint that has no `registered` flag in its own storage
  registers itself once, on its next wake, and sets the flag. Endpoints
  minted before the registry existed appear after their next use.
- Unminted names cannot register: an object without a stored identity refuses
  all traffic without writing storage (design.md, Minting).

## The page

Server-rendered HTML from the router, no client-side framework, no external
assets. One row per endpoint, newest first: label, `E` with a copy button,
mint time, and the command `iroh-do-chat <E>`. Rendering wakes only the
registry.

Live status (state, members present, records) is fetched per row on demand
(`GET /status/<E>` → the endpoint object's `/status` data as JSON), because
each status request wakes that endpoint's object. The page never polls.

`GET /endpoints.json` returns the same list as JSON.

## Destroying an endpoint

```
DELETE https://admin.<zone>/endpoints/<E>
Authorization: Bearer <ADMIN_TOKEN>
```

Added 2026-10-21, after a room id was published by mistake. The router
checks the token as for minting and calls the object's internal destroy path.
The object closes every relay socket (code 1000, reason `destroyed`), drops
its node, deletes its alarm, and calls `deleteAll()`, which removes the key,
the log and every other table. Node tasks still winding down find no storage
handles and write nothing. The router then removes `E` from the registry and
answers `{id, destroyed: true}`.

Afterwards the object is indistinguishable from one that was never minted:
relay upgrades and `/status` get 404 and no request writes storage, because
the constructor writes only for an object with an identity. Destroying an id
that was never minted, or destroying twice, is 404. If the registry update
fails after the object was destroyed, the router answers 502; a retry finds
the object unminted and removes the stale entry. `just destroy-room <E>` wraps
the call and asks for confirmation. `harness/workerd/destroy-test.mjs` checks
all of the above.

## Not in scope

Authentication for the page, hiding endpoints without destroying them,
pagination beyond the newest 500.
