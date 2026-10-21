# 001: Endpoint URL form for the reference deployment

**Status:** accepted (2026-10-21). Supersedes the earlier path-form decision.
**Touches:** §5.1, §13.2. No §1 principle changes.

## Decision

Endpoints are addressed by hostname, the spec default:

    https://<E>.iroh-do.example.com/

`<E>` is the 52-char z-base-32 form (note 004). The zone carries an Advanced
Certificate Manager certificate pack for `*.iroh-do.example.com`.

## Rejected

- **Path form** (`https://iroh-do.example.com/e/<E>/`): impossible. The
  client does `dial_url.set_path("/relay")`, which replaces the path
  (`iroh-relay-1.4.0/src/client.rs:267-268`, wasm `:391-392`). V1 is negative.
- **Query form** (`https://iroh-do.example.com/?<E>`): works in 1.4.0 only
  because `set_path` keeps the query and the TXT parser
  (`iroh-dns-1.4.0/src/attrs.rs:122`, `s.split('=')`, second piece only)
  tolerates a query with no `=`. Neither is a contract; a value containing `=`
  is already silently truncated. Fragile against upstream.
- **Labels under the apex** (`<E>.example.com`): needs a zone-wide
  wildcard DNS record and route, colliding with existing hostnames.

## Consequences

- Infra: proxied wildcard DNS `*.iroh-do`, `cloudflare_workers_route`
  `*.iroh-do.example.com/*`, and an ACM certificate pack. Custom domains do
  not take wildcards, so note 002 §4's custom-domain choice is replaced by the
  route. Token needs Zone: DNS Edit, Workers Routes Edit, SSL and
  Certificates Edit; Account: Workers Scripts Edit.
- Admin surface: reserved label `admin.iroh-do.example.com` (not valid z32
  of length 52, so unambiguous), per spec §13.4.
- Local mode: `http://<E>.localhost:8787/`. hickory resolves `*.localhost`
  without DNS and wrangler preserves Host (V1, V18).
