# RFC 0005: System Ids Are Never Dot Segments

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-25
- Affects: `system-hub`

## Motivation

The hub dashboard builds every per-system URL as `/api/systems/<id>` or
`/api/systems/<id>/history`, with the id passed through `encodeURIComponent`. That is safe for
every id except two: `encodeURIComponent` leaves `.` alone, and the URL parser removes dot
segments. So:

- With id `.`, the history fetch `/api/systems/./history` becomes `GET /api/systems/history`.
  That route returns the record of whichever system has the id `history`. The system's own
  record and its delete go to `/api/systems/`, which no route serves.
- With id `..`, the requests go to `/api/` and `/api/history`, which no route serves.

Such a system can't be opened or deleted from the dashboard, and a `.` system shows another
system's record in place of its history. The ids reach the hub self-asserted, in the push
handshake: `SystemId` (`system-hub/src/models.rs`) rejects only the empty id, so any push-token
holder can create either system, and so can anyone at all when `HUB_PUSH_TOKEN` is unset.

Encoding can't fix this on the page. The WHATWG URL parser treats `%2e`, in either case, as a
dot too, so `.`, `%2e`, `..`, `.%2e`, `%2e.` and `%2e%2e` are all dot segments. The fault is
that the hub accepts an id that can't be a path segment.

## Proposed design

A system id is never `.` or `..`. `SystemId`'s constructor rejects both, next to the empty id,
and its error becomes an enum that says which rule the value broke:

```rust
/// Why a value isn't a system id.
#[derive(Debug, PartialEq, Eq)]
pub enum SystemIdError {
    /// The empty string.
    Empty,
    /// `.` or `..`, which the dashboard's URLs would resolve away.
    DotSegment,
}

impl TryFrom<String> for SystemId {
    type Error = SystemIdError;
    // "" → Empty; "." | ".." → DotSegment; anything else → Ok
}
```

`SystemIdError` replaces the unit struct `EmptySystemId`. Its only other caller,
`push::authenticate`, maps both variants to the existing `HandshakeRejection::InvalidSystemId`,
through an exhaustive `match`, so a new variant forces that site to decide. The wire answer
stays `invalid system_id`, and the check stays last, after the shape and token checks, so an
unauthenticated client still can't probe id validation.

Only the literal `.` and `..` are rejected. `%2e`-style ids are safe because
`encodeURIComponent` turns the `%` into `%25`. Ids that merely contain dots (`...`, `a.b`,
`.hidden`) are single segments and stay valid.

Nothing else changes: no schema, no route, no push frame field. Hub-generated ids (UUIDs from
`POST /api/systems`) never match. Alert-record ids are `<system id>_<incident id>`, so they
always contain `_` and are never a dot segment either.

## Domain impact

- **Fleet Registry** (hub): the `SystemId` invariant grows from "never empty" to "never empty,
  and not `.` or `..`". The glossary's **system id** entry changes to match. No new terms.
- **Ingestion** (hub, `push.rs`): the handshake rejects two more ids, with the answer it
  already gives for an empty one.
- **Published contracts:** the push *frame* is untouched. The push *handshake* now refuses
  `.` and `..`. Mixed-version fleets:
  - *New hub, any agent:* agents send their machine id (`/etc/machine-id`, then the D-Bus
    machine id, then the hostname, then a random UUID). A machine id is 32 hex digits, and a
    host named `.` or `..` would need an administrator to set it deliberately, so in
    practice no real agent is refused.
    An agent whose id were `.` or `..` would get `invalid system_id` and keep retrying, which
    is what it does today for an empty id.
  - *Old hub:* keeps accepting both ids until upgraded. Nothing on the agent side changes.

## Alternatives considered

- **Do nothing.** It leaves a self-asserted id able to make a system undeletable from the
  dashboard and to show another system's data in its place.
- **Encode dots in the dashboard.** It doesn't work: the URL parser treats `%2e` as a dot.
- **Rewrite the id on the hub** (for example, map `.` to a UUID). The agent would then push
  under an id the hub doesn't store, and every frame would create another system.
- **Restrict ids to a character set**, such as `[A-Za-z0-9._-]`. It's broader than this bug
  needs. It would refuse hostnames and machine ids already stored by running hubs, and it
  would need its own mixed-version analysis. It's left to a future RFC on push identity,
  which the per-system credentials gap needs anyway.
- **Delete stored `.` and `..` systems in a migration.** It deletes an operator's data
  without asking, to fix a state that can only come from a misbehaving or hostile client.
  See Rollout for the manual cleanup instead.

## Security implications

- **A01 Broken Access Control / API1 BOLA:** the id stays self-asserted (see Open
  architectural questions). This RFC removes one thing a forged id could do, which is to make
  a system shadow another system's history in the dashboard. It doesn't bind ids to
  credentials.
- **A03 Injection:** the dot segments are a path-confusion problem, not XSS. The dashboard's
  rendering rule is unchanged, and `xss.mjs` isn't affected because the dashboard doesn't change.
- **A04 Insecure Design:** the fix sits at the boundary, in the `SystemId` constructor, so no
  code path can hold a dot-segment id.
- **A07 / API2 Authentication:** the token check keeps its order and its constant-time
  comparison, and the id check still comes after it.
- **A09 Logging:** the rejection is logged at `warn` with its variant, as now, and never with
  the token.
- **API4 Resource Consumption:** unchanged. A refused handshake closes the socket, as today.
- **API8 Misconfiguration:** unchanged. With `HUB_PUSH_TOKEN` unset, anyone could create these
  ids before this change, and after it no one can.
- **API10 Unsafe Consumption:** the hub consumes one less unsafe value from agents.
- A02, A05, A06 (no dependency change), A08, A10 / API7, API3, API5, API6, API9: not touched.
  No route, field, dependency or outbound call changes.

## Testing plan

TDD, per `CLAUDE.md`:

1. Pure refactor under green: `EmptySystemId` becomes `SystemIdError::Empty`, and the
   existing tests are the contract.
2. Red: `SystemIdError::DotSegment` exists but is never returned. New rows in the
   table-driven `SystemId` test (`.` and `..` rejected; `...`, `a.b` and `.hidden` accepted)
   fail on their assertions. `red-test-adversary` attacks them.
3. Red: a new row in `push::authenticate`'s table (a `.` id with a valid token →
   `InvalidSystemId`, and a `.` id with a wrong token → `InvalidToken`, so the order holds). The
   existing real-server test for an empty id becomes a table over `""`, `.` and `..`: the
   handshake answers `invalid system_id` and no system is registered.
4. Minimal green, refactor, then the gate in `system-hub` and `rosette-auditor`.

## Impact on `docs/ARCHITECTURE.md`

- § Domain model, glossary: **system id** reads "non-empty, and not `.` or `..`".
- § Trust boundaries, Agent → Hub (push): the id check rejects empty and dot-segment ids.
- § Open architectural questions: the dot-segment entry is replaced by a note about systems
  already stored under such ids (below).
- `README.md`: the `invalid system_id` row of the handshake table.

## Rollout / migration notes

Hub-only. Deploy in any order relative to agents.

A hub that already stores a system under `.` or `..` keeps the row, and the fix doesn't
remove it. That system can't push again, so it goes offline and stays offline, and it still
can't be opened or deleted from the dashboard. An operator removes it with a request that
keeps the encoded dots, which the hub decodes into the path's id:

```sh
curl --path-as-is -X DELETE "http://<hub>/api/systems/%2E"     # the system "."
curl --path-as-is -X DELETE "http://<hub>/api/systems/%2E%2E"  # the system ".."
```

A test pins that this request reaches the delete handler with the decoded id.
