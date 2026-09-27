# RFC 0012: Hub Access Control — Admin Token over Registry and Retention Writes, Reserved Push Ids

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-26 (updated the same day for the redb store; see Review)
- Affects: `system-hub` (routes, push handshake, `main`, dashboard, `xss.mjs`),
  `docker-compose.yml`, `.env.example`
- Depends on: RFC 0010 (the store on redb: store-owned retention and pending shortenings, the
  retention clock, `LiveStatus`, `/api/storage`), RFC 0011 (`Source`, `transact`, tombstones),
  RFC 0008 (the handshake order). Ships in the same release as 0008, 0010 and 0011 (0010's
  header), never before them. RFC 0013 is Rejected (no migration).
- The new dashboard reaches Compose users through the static-files prerequisite (0010's header),
  shipped first as its own change.
- This RFC adds the hub's first client authentication. `docs/ARCHITECTURE.md` says that is "an
  architectural change requiring an RFC", and this is that RFC.

## Motivation

The hub's REST API has no authentication. Under RFCs 0010 and 0011, writes to it become far
more destructive than today:

1. **Retention** decides how long history lives. A per-system override anyone can set can wipe
   a system's history within one retention pass.
2. **`DELETE /api/systems/:id`** erases a system's history. Every id is listed by the open
   `GET /api/systems`.
3. **`PUT /api/systems/:id`** can repoint a polled system at a dead URL, so it goes offline, and
   the operator's next "delete offline" then erases its history *with the operator's own
   token*. `POST` registers any URL for the hub to fetch (the standing SSRF surface).

And one hole comes from 0010's hub-time rule:

4. **Push ids aren't reserved.** Anyone holding the shared push token (anyone at all when
   `HUB_PUSH_TOKEN` is unset) can push under a *polled* system's UUID. Under 0010's
   `NotAfterLast`, the first point of each second wins, so two alternating connections displace
   the honest poll entirely.

**Owner decisions carried by this RFC:**

| Question | Decision |
|---|---|
| Who sets retention | the global policy from configuration (`HUB_RETENTION`, 0010); per-system overrides through the API, gated by a new `HUB_ADMIN_TOKEN` |
| Which writes need the admin token | **every registry write**: `POST`, `PUT` and `DELETE` on `/api/systems`, plus every retention write. Reads, SSE and alert acknowledgement stay open. With the token unset, these writes are refused. *Behaviour change for scripts.* |
| "Delete offline" | **a server-side decision**: the hub deletes only systems offline for more than N minutes, and the dashboard lists their names before it sends the token. *Behaviour change.* |
| Push ids of polled systems | **a push handshake whose id belongs to a polled system is refused** with `auth_error`. *Behaviour change.* |

## Proposed design

### 1. The admin token, and the listen address

```rust
/// `HUB_ADMIN_TOKEN`: 32 to 1,024 visible ASCII characters (0x21..=0x7E). No `Debug`.
pub struct AdminToken(String);
pub enum AdminAuth { Disabled, Required(AdminToken) }
pub enum AdminAuthError { NotUnicode, NotVisibleAscii, TooShort, TooLong, SameAsPushToken }
```

`HUB_ADMIN_TOKEN` is parsed in `main`, with `HUB_PUSH_TOKEN`, before the store opens (so an
invalid token refuses startup before any directory or file is created):

| Value | Outcome |
|---|---|
| unset or empty | `Disabled`: every admin route answers 403 |
| 32–1,024 visible ASCII characters | `Required`; startup logs that admin routes are enabled |
| not UTF-8 | refuse startup (`NotUnicode`) |
| any other character: whitespace, non-ASCII | refuse startup (`NotVisibleAscii`): HTTP header values carry only visible ASCII reliably (`HeaderValue::to_str`, httparse trims whitespace, browsers send U+0080–U+00FF as Latin-1), so such a token could never be presented |
| fewer than 32 characters | refuse startup (`TooShort`): there is no lockout, so the token's entropy is the control, and `openssl rand -hex 32` gives 64 characters |
| more than 1,024 characters | refuse startup (`TooLong`) |
| equal to `HUB_PUSH_TOKEN` | refuse startup (`SameAsPushToken`): every agent host holds the push token |

Every refusal names the variable, never the value.

- `push::config::PushToken` gains one crate-visible method, `pub(crate) fn is_same_secret(&self,
  other: &[u8]) -> bool`, constant-time like `accepts`; `main` uses it once.
- A request presents `Authorization: Bearer <token>`, compared in constant time (`subtle`).
  `subtle`'s slice comparison returns early on a length mismatch, so length leaks; with a
  32-character minimum that reveals nothing useful.
- A missing or wrong token answers 401; `Disabled` answers 403. The answers never say which of
  "missing" and "wrong" it was.
- **Refusal logging is rate-limited per reason** (missing, wrong, disabled): the first per
  minute of each is logged at `warn` with the route; the rest are counted in `/api/storage` as
  `admin_refusals`. The limiter takes the clock as an argument, so its boundaries are unit-tested.
- **Audit.** Every successful admin action is logged at `warn` with the route, the system id in
  `Debug` form, and the peer address. The hub is served with
  `into_make_service_with_connect_info::<SocketAddr>()`, and the admin middleware takes
  `Option<ConnectInfo<SocketAddr>>`: when the extension is missing it logs `peer=unknown` and
  still answers, so a lost `ConnectInfo` can never turn admin routes into 500s. A real-binary
  test pins that production `main` serves connect info. Behind a reverse proxy the peer is
  always the proxy, so the field attributes nothing there; no forwarded header is trusted. The
  README and A09 say so.
- The token never appears in a log line, an error or a response; a test scans the run's output.
- **Poll tokens can't equal the admin token** (they are sent to whatever URL a system has):
  `POST` and `PUT` refuse one (400), in constant time; and **at startup**, once 0011 has opened
  the stored tokens, the hub compares the admin token with each in constant time and refuses to
  start on a match, logging the system ids in `Debug` form, never a token.
- **`HUB_LISTEN`** (new): the address the hub binds, default `0.0.0.0:9091`, parsed with the rest
  of the configuration (an invalid value refuses startup, naming it). The startup line logs the
  address actually bound. The real-binary tests start the hub with `HUB_LISTEN=127.0.0.1:0`
  and read the bound port from that line, so they never collide with a running hub, each other,
  or the Compose stack.

### 2. Admin routes

| Route | Meaning |
|---|---|
| `POST /api/systems` | register a polled system (0011 §2), **now admin-only** |
| `PUT /api/systems/:id` | edit a system (0011 §2), **now admin-only** |
| `DELETE /api/systems/:id` | delete a system (0011 §3), **now admin-only** |
| `POST /api/offline-systems/delete` | delete the systems in `{"ids":[…],"min_minutes":N}` (at most 50 ids) that are **still** offline candidates when the request runs; one transaction per request; answers `deleted`, `skipped` (no longer candidates) and nothing else |
| `PUT /api/systems/:id/retention` | set a system's override: `{"raw":"24h","minute":"14d","hour":"400d"}`, each tier bounded as in 0010 §5 (omitted tiers follow the global policy) |
| `DELETE /api/systems/:id/retention` | remove the override |

Open read routes: `GET /api/offline-systems?min_minutes=` (below), `GET /api/retention` (the
global policy, every override and each pending shortening), and `GET /api/storage` (0010).

- **Offline candidates.** `min_minutes` is a bounded newtype, 10 to 525,600 (a year), default 60;
  out of range is 400. A system is a candidate when its `LiveStatus` is
  `Offline { since }` and **0010's retention clock** is at least `min_minutes` past `since`. The
  retention clock can't outrun real time by more than 2× nor pass the system clock, so a
  forward clock fault can't make every briefly offline system look offline for a year.
  **Disabled poll systems are never candidates**: an operator who disabled a system to keep its
  history doesn't lose it to a bulk delete. The list answers each candidate's id, name and
  `since`, which the dashboard shows.
- **Routing.** The offline routes live under `/api/offline-systems`, so no static segment can
  shadow a system whose id is `offline` or `delete-offline` (which `SystemId` allows and a push
  client can choose).
- **Ids.** Every `:id` route parses the path into `SystemId` and answers 400 otherwise. With no
  migration, every stored id is a valid `SystemId`, and the standing rule that `DELETE` takes the
  stored id as it is ends with this release.
- **Bodies.**
  - `POST` and `PUT` on `/api/systems` are capped at **8 KiB** (0011's field limits, escaped, fit:
    info fields have control characters stripped) and **ignore unknown fields, as today**; a
    push system accepts the values its `GET` shows as no-ops (0011 §2), so a script that `GET`s any
    system and `PUT`s it back keeps working.
  - The retention bodies are capped at 4 KiB. The offline-delete body is capped at **96 KiB**: 50
    ids of up to 255 bytes, each worst-case escaped (6 bytes per control character) is about
    77 KiB, plus the JSON around them. These **refuse unknown fields** (`deny_unknown_fields`), so
    a typo can't silently mean "the default".
  - A body that doesn't parse, including an unknown field or a wrong type, answers **422**
    (axum 0.7's `Json` rejection for data errors); a body over its cap answers 413; an id the
    registry doesn't hold answers 404.
- **Pending shortenings** (store-owned, 0010 §5), each change compared per tier against the
  policy **the retention pass enforces now** (for a tier with a pending shortening, its
  `current`):
  - a tier that gets longer applies at once, since it deletes nothing;
  - a tier that gets shorter becomes a `PendingShortening` effective **10 minutes of `Instant`
    time** later;
  - **a tier set back to exactly its enforced value removes that tier's pending entry**: the
    natural revert;
  - `DELETE .../retention` and a `PUT` that omits a tier are compared the same way (dropping an
    override longer than the global policy is a shortening);
  - a mixed change does both, tier by tier;
  - the override and its pending entries are written in **one** store transaction
    (`set_retention`, 0010 §9), and 0010 re-arms each pending entry with the full delay at every
    open, so a crash or restart can only lengthen the window;
  - every change is logged at `warn` with the system id (`Debug`) and the old, new and pending
    policies;
  - the window guards against **mistakes**, not theft: someone holding the admin token can
    `DELETE` a system, which makes its data unreadable at once. A04 says so.
- **No retention resume.** 0010's retention clock can't be suspended, so there is nothing to
  resume, and the previous drafts' `POST /api/storage/retention/resume` is gone (Review).
- **Alert acknowledgement** stays open. Repeating it writes nothing (0011 §5).

### 3. Reserved push ids

In the push handshake, after the token and `SystemId` checks pass (so it never runs for a client
without a valid token, and isn't an oracle of which ids are polled), the hub looks the id up in
the Registry:

| Registry holds | Outcome |
|---|---|
| nothing | register as a push system (RFC 0008's transaction) |
| a `Push` system | accept, as today |
| a `Poll` system (0011's `Source::Poll`) | **refuse**: `auth_error` / `system id belongs to a polled system`, a new `HandshakeRejection` variant. The polled system's record and `LiveStatus` are not touched. |

- The refusal is logged at `warn` at most **once per id per hour**, then counted, reusing §1's
  limiter. A refused agent retries every 5 s, and an unauthenticated loop (with the push token
  unset) can't flood the log.
- `PUT` can't change a system's source (0011) and needs the admin token, so nobody can flip a
  system between push and poll to get around the rule.
- The check runs where RFC 0008 §4 puts it: after the token and the `SystemId` rule, before
  registration.
- **A behaviour change.** A system registered by `POST` for polling can no longer also push under
  its UUID. Agents push under their machine id (or RFC 0008's other sources), never under a
  hub-minted UUID, so an honest fleet is unaffected. An operator moving a host from poll to push
  deletes the polled entry (admin); the agent registers itself on its first push.

### 4. The dashboard

- **Every admin action asks for the token**: adding a system, deleting it, and "delete offline".
  (The dashboard has no edit UI; `PUT` is for scripts.) The modal is built with `createElement`:
  an `<input type="password" autocomplete="off">` inside a node with the fixed id
  `admin-token-modal`. **One modal at a time**: while it is open, further admin actions are
  ignored. Browsers' password managers may still offer to save the input; the README says so.
  - The token lives in a local variable **for one action only**: sent as the `Authorization`
    header of that action's requests, then dropped, and the input cleared and removed with the
    modal. Never in `localStorage`, `sessionStorage`, a cookie, a URL or a body.
  - A 401 says the token was wrong and asks again. A 403 says admin routes are disabled.
- **"Delete offline"** fetches `GET /api/offline-systems`, shows each candidate's name and how long
  it has been offline, and only then asks for the token and sends `POST
  /api/offline-systems/delete` in **batches of at most 50 ids**, each re-validated by the server.
  It **stops at the first batch that fails** and reports what was deleted, what was skipped, and
  which batches weren't sent. One prompt covers the batches of one confirmation.
- **Deleting a push system** warns that an online agent will register it again unless it is
  stopped (0011 §3).
- No retention UI in this RFC; retention is set with `curl`, and the README shows how.
- **`xss.mjs` is extended**, because this opens new paths from the page to requests:
  - **an injected CSP catches every real network attempt**, whatever the API: the harness inserts
    a `<meta http-equiv="Content-Security-Policy">` (`default-src 'none'; script-src
    'unsafe-inline'; style-src 'unsafe-inline'; form-action 'none'`) **as the first child of
    `<head>`** (a meta CSP in `<body>` is ignored by Chromium), and records every
    `securitypolicyviolation` event's `blockedURI`. A **canary** proves the CSP is enforced: the
    harness itself makes one known blocked request at baseline and requires exactly that
    violation to be recorded (then excludes it), so a CSP that isn't in force fails the run.
    Any other violation (an image, a CSS `url()`, `srcset`, `<a ping>`, a form, a beacon, a
    WebSocket) fails the test;
  - **stubs**: `fetch` normalises headers through `new Headers(init)` and records method, URL,
    headers and body; it can answer 401; a body must be **absent or a string** (the dashboard
    sends only JSON strings or nothing), and anything else (`FormData`, `Blob`,
    `URLSearchParams`, a stream) fails the test; `EventSource` records its URL; `XMLHttpRequest`,
    `navigator.sendBeacon` and `WebSocket` are recorded too;
  - the hostile hub serves systems whose ids, names and URLs try to reach the modal (including an
    injected `<div id="admin-token-modal">`), and a hostile offline list whose names must render
    **as text** in the confirmation;
  - the test enters a marker token for each admin action and checks it was sent **only** as
    `Authorization` on that action's expected routes: never in a URL (`location.href` included),
    a body, `EventSource`, XHR, beacon, WebSocket, CSP report, `localStorage`, `sessionStorage`,
    IndexedDB, `document.cookie`, `window.name`, `history.state`, or any own property of `window`;
  - a 401 answer makes the page ask again and never reuse the marker;
  - "dropped after one action": a second admin action started and its prompt declined sends no
    `Authorization` header;
  - after the modal closes, the marker is absent from the serialised DOM and every input's value,
    and the modal node is gone;
  - the tag scan exempts **only the input and buttons the page itself created in the modal**,
    captured by identity when the page opens it, and requires exactly one `admin-token-modal`
    while it is open. An injected same-id node is neither exempt nor allowed;
  - **the existing checks this changes**, listed so `red-test-adversary` attacks each rewrite:
    "declining a delete sends nothing" (now: the offline list is fetched, and nothing else is
    sent), "deleting offline systems deletes exactly those" (now: exactly the listed candidates, in
    batches of at most 50), and the route table (gains the two `/api/offline-systems` routes).
    Every other existing check is unchanged;
  - `fireEverything`'s repeated clicks on "Delete offline" open at most one modal;
  - `red-test-adversary` attacks the extension in mutation mode.

### 5. Hub → dashboard trust boundary and CORS

The rendering rule is unchanged. What is new is a secret typed into the page, held for one action
and dropped, which raises the stakes of the rendering rule and of the missing
Content-Security-Policy (already an open question). This RFC doesn't serve a CSP (the inline
script would have to move first); the test harness injects one.

**CORS stays as it is** (`Any`), and that is safe for the admin routes: tower-http's
`allow_headers(Any)` answers `*`, which the Fetch standard doesn't extend to `Authorization`, so
a cross-origin preflight for an admin request fails; and the token isn't ambient (no cookie).

## Domain impact

- **New context: Hub Access** (hub): `AdminToken`, `AdminAuth`, the admin middleware, refusal
  counting and the audit log. Mirrors the agent's Agent Access.
- **Ingestion**: the handshake gains the reserved-id rule and a `HandshakeRejection` variant.
- **Fleet Registry**: registry writes are admin-only; "delete offline" is a server-side rule over
  `LiveStatus` and the retention clock.
- **Fleet History**: retention changes with per-tier pending shortenings against the enforced
  policy, stored with the override in the store.
- **Glossary**: added admin token, admin route, offline candidate, reserved push id, listen
  address; changed **push handshake** (a new rejection).
- **Published contracts**: the handshake gains one `auth_error` message; `POST`, `PUT` and
  `DELETE` on `/api/systems` need a token (breaking for scripts); two routes under
  `/api/offline-systems`; bodies capped; 422 for bodies that don't parse.
- **Mixed-version fleet**: an old agent pushing under a polled id is refused, and retries every
  5 s; other agents are unaffected.

## Alternatives considered

- **A retention resume route** (the previous drafts). Needed only while 0010 could suspend
  retention; its retention clock can't, so the route and its fault ids are gone.
- **Offline routes under `/api/systems/`.** They shadowed systems with those ids.
- **Offline age on hub time.** A forward clock fault made every offline system a candidate.
- **Retention from a config file only.** Declined by the owner.
- **Gate only `DELETE` and retention.** An open `PUT` repointed systems so the operator's own
  "delete offline" erased them. The owner chose to gate every registry write.
- **Gate alert acknowledgement too.** Declined by the owner.
- **Keep "delete offline" client-side.** The page chose ids from state anyone could influence.
- **Allow push under polled ids and document displacement.** The owner chose to refuse.
- **One token for push and admin.** Every agent host holds the push token.
- **Keep the token for the page's lifetime, or in `sessionStorage`.** Fleet dashboards stay open
  for days, so one action per prompt.
- **A lockout after failed attempts.** It would let anyone lock the operator out. A 32-character
  minimum makes online guessing hopeless instead.

## Security implications

**OWASP Top 10 (2021)**

- **A01 Broken Access Control:** every registry write and every retention write is gated.
  "Delete offline" deletes only what the server still sees offline, never a disabled system.
  Reads and acknowledgement stay open (owner's decision; standing for reads).
- **A02 Cryptographic Failures:** the admin token travels in a header. The hub still has no TLS
  (standing: a reverse proxy terminates it), and the README says to use the token only over TLS.
- **A03 Injection / XSS:** the modal is built with `createElement`; candidate names render as
  text; `xss.mjs` checks every way the token could leave the page, under an enforced CSP.
- **A04 Insecure Design:** a 10-minute monotonic window for every shortening, compared against
  the enforced policy, stored with the override and re-armed at every open (it guards against
  mistakes, not a stolen token); offline age on the guarded retention clock;
  `deny_unknown_fields` on the retention and offline-delete bodies; no push token or poll token
  reuse as the admin token.
- **A05 Security Misconfiguration:** unset means disabled (fail closed). A token that can't be
  presented, or is too short, refuses startup. `docker-compose.yml` passes `HUB_ADMIN_TOKEN:
  ${HUB_ADMIN_TOKEN:-}` with no development default, and `.env.example` shows `openssl rand -hex
  32`. CORS `Any` doesn't cover `Authorization` (§5). `HUB_LISTEN` fails closed on a bad value.
- **A06 Vulnerable Components:** none added.
- **A07 Identification & Authentication Failures:** constant-time comparison, a minimum length,
  no logging of the token, per-reason rate-limited refusal logs, an audit line per action. No
  lockout, by design.
- **A08 Software & Data Integrity Failures:** retention changes logged with old, new and pending
  policies; admin actions audited.
- **A09 Logging & Monitoring Failures:** per-reason refusal counters; the audit log with the peer
  address (behind a reverse proxy always the proxy, which attributes nothing); reserved-id
  refusals limited per id. Push handshake refusals are still logged without a peer address
  (open question).
- **A10 SSRF:** registering a URL now needs the admin token, which narrows who can use the
  standing SSRF surface. The host isn't validated (standing).

**OWASP API Security Top 10 (2023)**

- **API1:** reserved push ids close the displacement of polled systems. Push systems sharing a
  self-asserted id remain the standing risk.
- **API2:** a new credential, handled as above.
- **API3:** the retention and offline-delete bodies `deny_unknown_fields`; `POST`/`PUT` keep
  ignoring unknown fields, and their DTOs list only settable fields.
- **API4:** 4 KiB, 8 KiB and 96 KiB bodies; rate-limited refusal logging; "delete offline" bounded
  to 50 ids per request.
- **API5:** the first function-level authorization on the hub: six admin routes.
- **API6:** "delete offline" is a sensitive bulk flow; it needs the token and deletes only
  re-validated candidates.
- **API7:** see A10.
- **API8:** see A05.
- **API9:** the six admin routes, the three open read routes (`GET /api/offline-systems`,
  `GET /api/retention`, `GET /api/storage`), `HUB_ADMIN_TOKEN`, `HUB_LISTEN`, the new `auth_error`
  row in the README's push-protocol table, and the breaking changes go into the README.
- **API10:** N/A.

## Testing plan

- **`AdminAuth` parsing**, as a table: unset; empty; 31, 32, 1,024 and 1,025 characters; a space
  inside; surrounding whitespace; a non-ASCII letter; not UTF-8; equal to the push token; equal
  to an unset push token (allowed).
- **`HUB_LISTEN` parsing**: the default; `127.0.0.1:0`; `[::1]:9091`; garbage and a port over
  65,535 (refused).
- **Admin middleware** over each admin route: disabled (403), missing (401), wrong (401), wrong
  length (401), right (2xx), `Bearer` with an empty value (401).
- **Refusal limiter** with an injected clock: per reason, one `warn` in 60 s and the next at
  exactly 60 s, not at 59 s; missing-token noise doesn't suppress the first wrong-token line;
  counters in `/api/storage`.
- **Audit**: each successful admin action logs route, id and peer address; a router without
  `ConnectInfo` logs `peer=unknown` and still answers 2xx.
- **Real binary** (`system-hub/tests/`, `HUB_LISTEN=127.0.0.1:0`, the bound port read from the
  startup line): an admin request answers 2xx and logs the peer address; an invalid
  `HUB_ADMIN_TOKEN` exits before `HUB_DATA_DIR` is created; a stored poll token equal to the admin
  token refuses startup (the token stored by a first run with another admin token), naming the
  system id and never the token.
- **The token never appears** in any log line, error or response body of the run.
- **Poll token equal to the admin token**: refused on `POST` and `PUT`.
- **Registry writes**: `POST`, `PUT`, `DELETE` without the token (401, nothing changed), with it
  (done), disabled (403).
- **Delete offline**: candidates exactly at `min_minutes` and one second under, on an injected
  retention clock; a +1 year system clock step makes no new candidate; a disabled poll system is
  never a candidate; `min_minutes` 9, 10, 525,600 and 525,601; an id that came back online
  between the list and the request is skipped; 50 ids accepted and 51 refused (400); 50 ids of 255
  control characters each fit the 96 KiB cap; a dashboard list of 120 sent as three batches, and
  a failure in the second stops the third and is reported.
- **Routing**: a system whose id is `offline`, and one whose id is `delete-offline`, can be read,
  edited and deleted through `/api/systems/:id`, and the offline routes still work.
- **Retention routes**: bounds per tier (exactly at the minimum and maximum, one unit out); an
  unknown field and a wrong type (422); a 4 KiB + 1 body (413); an unknown or deleted id (404); a
  shortening pending until 10 minutes of injected `Instant` time; `DELETE .../retention` of a
  longer override is a pending shortening, and so is a `PUT` omitting a longer tier; a mixed
  `PUT`; **a revert to exactly the enforced value removes the pending entry**; a second `PUT`
  during the window (24 h enforced, `raw=1h` pending, then `raw=2h`) stays pending from 24 h; the
  override and pending entry committed together: a child killed right after the `PUT` answered,
  and at reopen the entry is there, re-armed with the full 10 minutes; `POST`/`PUT` on
  `/api/systems` with an unknown field (accepted, ignored); an 8 KiB + 1 body (413).
- **Reserved push ids**, on a real server: push under a `Poll` id with a wrong token (`invalid
  token`: the token is checked first); with the right token (refused, not registered, the polled
  system untouched, logged once per hour); under a `Push` id (accepted); under a new id
  (registered).
- **`xss.mjs`**: the extension of §4, including the CSP canary, `Headers` normalisation, the
  absent-or-string body rule, names rendered as text, `location.href` and IndexedDB scans, the
  identity-based exemption against an injected same-id node, one modal at a time, and the three
  rewritten checks; every other existing check unchanged.

## Impact on `docs/ARCHITECTURE.md`

- **Trust boundaries**: *Client → Hub* gains admin routes, the audit log and the CORS note;
  *Agent → Hub (push)* gains reserved ids; *Hub API → hub dashboard* gains the per-action token.
- **Domain model**: the Hub Access context and the glossary.
- **Components**: `HUB_LISTEN` in `main`'s configuration.
- **Testing architecture**: `xss.mjs`'s injected CSP with its canary, wider stubs and token checks;
  the hub's real-binary tests on `127.0.0.1:0`.
- **Open questions**: closes "No hub-side client authentication" for writes, API1 for polled
  systems, and the legacy-id rule for `DELETE`; keeps open reads, no CSP served by the hub (now
  with a token typed into the page), no TLS, and push handshake refusals logged without a peer
  address.

## Rollout / migration notes

- **Breaking for scripts:** `POST`, `PUT` and `DELETE` on `/api/systems` need `HUB_ADMIN_TOKEN`.
  With it unset they answer 403, including the dashboard's add and delete buttons. The README
  flags it and shows how to generate a token. With no migration (0010), re-registering polled
  systems after the upgrade needs this token.
- **Behaviour change for agents** pushing under a polled system's id: refused.
- `docker-compose.yml` and `.env.example` gain `HUB_ADMIN_TOKEN` (no default value), and the
  compose file's header comment, which says tokens default to development placeholders, is
  corrected: the admin token has none.
- Ships in the same release as 0008, 0010 and 0011, after the static-files prerequisite.

## Review

The earlier passes were written against the custom engine and the SQLite import. Their tables
are kept as they were; the **redb rewrite** block after the third pass says what each CONFIRMED
finding is now.


This RFC holds these findings from the first `rfc-adversary` pass on the undivided draft (the
full table is in RFC 0010's Review):

| Finding | Verdict | Resolution |
|---|---|---|
| DELETE unauthenticated while it destroys 400 days | CONFIRMED | admin token required (owner) (§2) |
| `NotAfterLast` lets a token holder displace a polled system | CONFIRMED | push ids reserved for push systems (owner) (§3) |
| retention delay really 0–10 min | CONFIRMED | a pending window (§2) |
| empty admin token would match `Bearer ` | PLAUSIBLE | empty = unset = disabled (§1) |
| admin token equal to the push token | PLAUSIBLE | refused at startup (§1) |
| refusal log flood with the token unset | PLAUSIBLE | rate-limited, with counters (§1) |
| a clock fault suspends retention with no way to resume | CONFIRMED (via 0010's guard) | `POST /api/storage/retention/resume` (§2) |

`rfc-adversary`, first pass on this RFC after the split. Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| the gate on `DELETE` is sidestepped: open `PUT` repoints systems, and the operator's own "delete offline" erases them | CONFIRMED | every registry write needs the token (owner); "delete offline" decided by the server over `live_status`, with the names shown first (owner) (§2, §4) |
| shipped before 0011, the reserved-id rule is bypassed or turned into a DoS by the open `PUT` | CONFIRMED | ships only with 0010–0013; `PUT` gated and unable to change source (§3, header) |
| `AdminAuth` accepts tokens no client can present, and no minimum entropy; the `SameAsPushToken` seam | CONFIRMED | visible ASCII, 32–1,024 characters, refused at startup otherwise; `PushToken::is_same_secret` as the one widening (§1) |
| `xss.mjs` can't observe headers, bodies or other request APIs; the modal's `INPUT` breaks the tag scan, and a global allowance would admit a fake prompt | CONFIRMED | stubs for fetch (headers, bodies, 401), EventSource, XHR, beacon, WebSocket, forms and resource URLs; exemption of the modal node only; DOM and input checks after close (§4) |
| the pending window shrinks with hub-time steps and misses `DELETE` and omitted tiers; orphaned overrides | CONFIRMED | per-tier effective comparison for every change; the window on `Instant` time, checkpointed; overrides removed with the system (0011); 404 for unknown ids (§2) |
| the resume has no guard of its own, and resuming isn't tested to resume | PLAUSIBLE / CONFIRMED | the echoed hub time within 120 s; `/api/storage` shows what a pass would delete; the baseline reset and its test (§2) |
| no audit of successful destructive actions; the limiter hides brute force; no peer address | CONFIRMED | an audit line per admin action with the peer address (`ConnectInfo`); per-reason limiting; counters in 0010's `/api/storage` (§1) |
| untested behaviour: token in logs, check order, limiter boundaries, token shapes, unknown ids, `DELETE` as a shortening, the refused push leaving the polled system untouched | CONFIRMED | rows added (Testing plan) |
| inventory: compose and `.env.example`, the `auth_error` table, ARCHITECTURE's testing and CORS sections; wrong migration advice | CONFIRMED | listed and fixed; the advice is "disable or delete the polled entry" (§3, Rollout, Impact) |
| the new push refusal logged without a limit | CONFIRMED | once per id per hour, counted (§3) |
| poll tokens equal to the admin token get sent to arbitrary URLs | PLAUSIBLE | refused at `POST`/`PUT` (§1) |
| can `POST` re-register an existing id? | PLAUSIBLE | `POST` never takes an id (0011 §2) |
| the token stays in page memory for the page's lifetime | PLAUSIBLE | kept for one action only (§4) |

Came closest and survived: the handshake oracle (the reserved-id lookup runs after the
constant-time token check), and CORS `Any` with `Authorization` (not covered by `*`, and the
token isn't ambient).

`rfc-adversary`, second pass on this RFC. Most first-pass resolutions held; five didn't hold as
written (the pending window's durability, the resume guard, the audit's peer address, what
`xss.mjs` can observe, the poll-token check). Every finding was acted on:

| Finding | Verdict | Resolution |
|---|---|---|
| a pending shortening is persisted nowhere: a crash inside the window applies it at once or loses it | CONFIRMED | written with the override in one 0011 transaction; re-armed with the full delay at every open; a kill-before-checkpoint test (§2) |
| the resume guard compares hub time with itself, so it can't refuse a clock still wrong | CONFIRMED | the resume names its `FaultId`, discards the fault and pending steps, and retention acts on `min(hub, system)`; a +1 year fault test (§2; 0010 §5) |
| `ConnectInfo` makes admin routes 500 wherever connect info is missing, and no test reaches `main` | CONFIRMED | `Option<ConnectInfo>` with `peer=unknown`; a real-binary test of an admin request (§1, Testing plan) |
| static routes shadow systems whose id is `offline` or `delete-offline` | CONFIRMED | the offline routes moved to `/api/offline-systems`; a routing test (§2) |
| "delete offline" hits the 4 KiB cap at about 100 ids | CONFIRMED | batches of at most 50 ids, a 16 KiB body for that route, a test with 120 candidates (§2, §4) |
| legacy push systems given a URL become polled on import, and their agents are refused | PLAUSIBLE | 0013 warns about imported polled systems with non-UUID ids; the README and Rollout say what to do (§3) |
| `xss.mjs` enumerates APIs and can't catch everything; the modal exemption is keyed on an id | PLAUSIBLE | an injected CSP with every violation recorded; non-string bodies fail; the exemption by node identity with exactly one such id; window, `window.name`, `history.state`, `location.hash` scans; `autocomplete="off"`; a declined-second-prompt test (§4) |
| which bodies refuse unknown fields is contradictory, and 4 KiB is below 0011's field limits | CONFIRMED | `POST`/`PUT` ignore unknown fields as today, capped at 8 KiB; only retention, resume and offline-delete bodies refuse them (§2) |
| the pending window can be bypassed in two requests | PLAUSIBLE | "before" is the enforced value; a second-`PUT` test; A04 states the window guards against mistakes, not theft (§2, Security) |
| the poll-token/admin-token check runs only on `POST`/`PUT` | PLAUSIBLE | also at startup, against every stored poll token, refusing with the system ids (§1) |
| inventory: "editing" with no edit UI; the ARCHITECTURE peer-address item; the compose header comment; A09 behind a proxy | CONFIRMED | "editing" dropped; the item closed in Impact; the comment corrected in Rollout; A09 states that attribution behind a proxy is nil (§1, §4, Security, Impact, Rollout) |

Came closest and survived: CORS `Any` with `Authorization` (a preflight doesn't extend `*` to
`Authorization`, and the token isn't ambient), the handshake oracle (the reserved-id lookup
runs after the token check), and the token comparisons themselves.

`rfc-adversary`, third pass on this RFC (against the custom-engine draft):

| Finding | Verdict | Now |
|---|---|---|
| one resume permanently turned off the forward-step guard, so a second fault erased history | CONFIRMED | **moot**: no resume, no suspension; 0010's retention clock bounds every pass (§2; 0010 §2) |
| the injected CSP is ignored where the harness injects today (in `<body>`) | CONFIRMED | **resolved**: the meta CSP is the first child of `<head>`, with a canary that must be recorded (§4) |
| the 16 KiB offline-delete cap is about 4.7× too small for valid ids; partial failure unspecified | CONFIRMED | **resolved**: a 96 KiB cap from the worst-case escaping; one transaction per request; the dashboard stops at the first failed batch and reports (§2, §4) |
| the real-binary tests had to bind the fixed port 9091 | CONFIRMED | **resolved**: `HUB_LISTEN`, tests on `127.0.0.1:0` (§1) |
| "delete offline" on hub time with no clock guard; disabled systems swept; `min_minutes` unbounded | PLAUSIBLE | **resolved**: offline age on the retention clock; disabled poll systems excluded; `min_minutes` 10..=525,600; each candidate's `since` shown (§2) |
| GET-then-PUT is false for every push system | CONFIRMED | **resolved**: `GET`'s values accepted as no-ops (0011 §2) |
| "every existing `xss.mjs` check unchanged" is false; gaps in the new checks | CONFIRMED / PLAUSIBLE | **resolved**: the three rewritten checks listed; names as text; `location.href` and IndexedDB; `Headers` normalised; absent-or-string bodies; one modal at a time; the exemption by identity for the page's own input and buttons (§4) |
| an unknown field answers 422, not 400, with axum's `Json`; the `fault` type | CONFIRMED | **resolved**: 422 stated; the `fault` body is gone with the resume (§2) |
| inventory: the peer-address open question; route counts | CONFIRMED | **resolved**: the question stays open (push refusals have no peer address; the second pass's "closes" was wrong); six admin routes and three open reads counted (Security, Impact) |
| reverting to exactly the enforced value unspecified | PLAUSIBLE | **resolved**: it removes the pending entry; a test row (§2) |

Came closest and survived in the third pass: CORS `Any` with `Authorization`, and the handshake
oracle (the reserved-id lookup runs after the token check).

**redb rewrite.** The owner rebuilt the store on redb (RFC 0010) and chose no migration (RFC 0013
Rejected). Every CONFIRMED finding of the earlier passes, and what it is now:

| Finding (pass) | Now |
|---|---|
| `DELETE` unauthenticated (undivided) | **resolved**, unchanged: admin token (§2) |
| `NotAfterLast` displacement of a polled system (undivided) | **resolved**, unchanged: reserved push ids (§3) |
| retention delay really 0–10 min (undivided) | **resolved**: the pending window, now held in the store-owned `retention` table (0010 §5) |
| a clock fault suspends retention with no resume (undivided) | **moot**: 0010's retention clock never suspends; the resume route is gone |
| open `PUT` + "delete offline" sidesteps the `DELETE` gate (first) | **resolved**, unchanged (§2, §4) |
| shipped before 0011 (first) | **resolved**: one release of 0008, 0010, 0011, 0012 (header) |
| `AdminAuth` token shapes and the `SameAsPushToken` seam (first) | **resolved**, unchanged (§1) |
| `xss.mjs` can't observe the token's paths (first) | **resolved**, extended by the third pass (§4) |
| pending window shrinks with hub time, misses `DELETE` and omitted tiers (first) | **resolved**: `Instant` delay, per-tier comparison, written by `set_retention` in one store transaction (§2) |
| the resume isn't tested to resume (first) | **moot**: no resume |
| no audit, limiter hides brute force, no peer address (first) | **resolved**, unchanged (§1) |
| untested behaviour rows (first) | **resolved**; the crash row now kills the child right after the `PUT` answered (no checkpoint exists on redb) |
| inventory and wrong migration advice (first) | **resolved**; the advice is "delete the polled entry"; nothing is imported |
| push refusal logged without a limit (first) | **resolved**, unchanged (§3) |
| pending shortening persisted nowhere (second) | **resolved**: one redb transaction with the override; re-armed with the full delay at every open (0010 §5) |
| the resume guard compares hub time with itself (second) | **moot**: no resume |
| `ConnectInfo` 500s (second) | **resolved**, unchanged (§1) |
| static routes shadow `offline` ids (second) | **resolved**, unchanged (§2) |
| offline-delete body cap (second) | **resolved**, now 96 KiB (third pass) |
| which bodies refuse unknown fields (second) | **resolved**; 422 stated (third pass) |
| inventory: edit UI, peer-address item, compose comment, A09 (second) | **resolved**; the peer-address item stays open (third pass) |
| legacy push systems imported as polled (second, PLAUSIBLE) | **moot**: no import, so no legacy polled system with a non-UUID id |

**Still open**: nothing CONFIRMED. The next `rfc-adversary` pass reviews this rewrite before the
RFC is accepted.
