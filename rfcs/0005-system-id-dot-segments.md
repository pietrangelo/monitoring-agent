# RFC 0005: System Ids That Are Always One Path Segment

- Status: Implemented
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-25
- Affects: `system-hub`

## Motivation

The hub dashboard builds every per-system URL as `/api/systems/<id>` or
`/api/systems/<id>/history`, with the id passed through `encodeURIComponent`. The page then
depends on that id being exactly one path segment. Two kinds of id aren't, and the hub accepts
both from any push client, because `SystemId` (`system-hub/src/models.rs`) rejects only the
empty id:

- **Dot segments.** `encodeURIComponent` leaves `.` alone, and the URL parser removes dot
  segments. With id `.`, the record fetch goes to `/api/systems/` and the history fetch to
  `/api/systems/history`. With id `..`, they go to `/api/` and `/api/history`. None of these
  returns the system: `/api/systems/history` returns the `SystemInfo` of a system whose id is
  `history`, which has no metrics for the charts. The WHATWG URL parser treats `%2e`, in either
  case, as a dot too, so encoding can't fix this on the page.
- **Ids longer than a request line.** hyper answers `414 URI Too Long` for a URI over
  65,534 bytes. `encodeURIComponent` turns each non-ASCII byte into three, so an id of about
  22 KB of emoji is enough. Behind a reverse proxy such as the README's nginx example, the
  proxy's request-line buffer (8 KB by default in nginx) is the limit.

Either way, the system can't be opened and can't be deleted from the dashboard. For an
over-long id, no HTTP request can delete it at all: only editing SQLite by hand can. Its card
looks normal, because the default name is the first 8 bytes. The ids reach the hub
self-asserted in the push handshake, so any push-token holder can create such a system. When
`HUB_PUSH_TOKEN` is unset, anyone can.

## Proposed design

A system id is one URL path segment: non-empty, at most 255 bytes, and not `.` or `..`.
`SystemId`'s constructor enforces all three, and its error becomes an enum that says which
rule the value broke:

```rust
pub const MAX_SYSTEM_ID_BYTES: usize = 255;

/// Why a value isn't a system id.
#[derive(Debug, PartialEq, Eq)]
pub enum SystemIdError {
    /// The empty string.
    Empty,
    /// Longer than `MAX_SYSTEM_ID_BYTES`, so its URLs could outgrow a request line.
    TooLong,
    /// `.` or `..`, which the dashboard's URLs would resolve away.
    DotSegment,
}

impl TryFrom<String> for SystemId {
    type Error = SystemIdError;
    // "" → Empty; > 255 bytes → TooLong; "." | ".." → DotSegment; anything else → Ok
}
```

- **Why 255 bytes.** Machine ids are 32 bytes, UUIDs 36, and Linux hostnames at most 64
  (DNS names at most 253). Encoded, 255 bytes is at most 765 characters in a URL, far below any
  proxy's request-line buffer.
- **Only the literal `.` and `..`.** `%2e`-style ids are safe because `encodeURIComponent`
  turns the `%` into `%25`. Ids that only contain dots (`...`, `a.b`, `.hidden`) are single
  segments and stay valid.
- **`SystemIdError` replaces the unit struct `EmptySystemId`.** Its only other caller,
  `push::authenticate`, maps every variant to the existing
  `HandshakeRejection::InvalidSystemId` through an exhaustive `match`, so a new variant forces
  that site to decide. The wire answer stays `invalid system_id`. The check stays last, after
  the shape and token checks, so an unauthenticated client still can't probe id validation.

Nothing else changes: no schema, no route, no push frame field. Hub-generated ids (UUIDs from
`POST /api/systems`) always pass. Alert-record ids are `<system id>_<incident id>` or a UUID,
so they always contain `_` or `-` and are never a dot segment. Their length is a separate gap
(see Open questions).

### What the invariant covers

`SystemId` is built in one place, the push handshake. Every other path (DB rows, REST path
parameters, the SSE summary and the poller) still carries ids as a bare `String`. So after
this RFC the hub **accepts no new** id that breaks the rule. It doesn't guarantee that no
stored id breaks it: rows created before the upgrade keep their ids (see Rollout). Anyone who
later parses DB rows into `SystemId` must skip a row that fails and log that a row was
skipped, never its id, rather than fail the whole list. `Database::list_systems` collects into one `Result`, and its callers fall back to an
empty list, so one bad row would empty the dashboard.

## Domain impact

- **Fleet Registry** (hub): the `SystemId` invariant grows from "never empty" to "one path
  segment: non-empty, at most 255 bytes, not `.` or `..`", for ids the hub accepts from now on.
  The glossary's **system id** entry changes to match. No new terms.
- **Ingestion** (hub, `push.rs`): the handshake rejects more ids, with the answer it already
  gives for an empty one.
- **Published contracts:** the push *frame* is untouched. The push *handshake* now refuses the
  new invalid ids. Mixed-version fleets:
  - *New hub, any agent:* agents send their machine id (`/etc/machine-id`, then the D-Bus
    machine id, then the hostname, then a random UUID). None of these is over 255 bytes, and a
    host named `.` or `..` would need an administrator to set it deliberately, so in practice
    no real agent is refused. An agent that is refused gets `invalid system_id` and retries
    every 5 s, as it does today for an empty id.
  - *Old hub:* keeps accepting these ids until upgraded. Nothing on the agent side changes.
  - *Rollback:* safe. There's no schema change.

## Alternatives considered

- **Do nothing.** It leaves a self-asserted id able to create a system that can't be opened or
  deleted, and in the long-id case that no HTTP request can delete.
- **Encode dots in the dashboard.** It doesn't work: the URL parser treats `%2e` as a dot.
- **Rewrite the id on the hub** (for example, map `.` to a UUID). The agent would then push
  under an id the hub doesn't store, and every frame would create another system.
- **Restrict ids to a character set** (for example `[A-Za-z0-9._-]`). It's broader than this
  bug needs. It would refuse non-ASCII hostnames already stored by running hubs, and it would
  need its own mixed-version analysis. It's left to the future RFC on push identity, which the
  per-system credentials gap needs anyway.
- **Delete stored violating systems in a migration.** It would delete an operator's data
  without asking, to fix a state that only a misbehaving or hostile client can create. See
  Rollout for the manual cleanup.
- **Warn at startup about stored ids that break the rule.** It would tell operators the cleanup
  is needed. It's deferred: such rows can only come from a hostile client, and the Open
  questions entry records how to find them.

## Security implications

- **A01 Broken Access Control / API1 BOLA:** the id stays self-asserted (see Open architectural
  questions). This RFC removes one thing a forged id can do, which is to create a system the
  operator can't open or remove. It doesn't bind ids to credentials. When a per-system fetch
  fails, the dashboard keeps showing the previously opened system. That is a separate
  dashboard bug, reachable without these ids (for example, when a system is deleted between
  two refreshes), and is recorded in Open questions.
- **A03 Injection:** this is path confusion, not XSS. The dashboard doesn't change, so its
  rendering rule and `xss.mjs` are unaffected.
- **A04 Insecure Design:** the rule sits in the `SystemId` constructor at the push boundary, so
  no *newly accepted* id can break it. Stored rows are covered by Rollout and Open questions.
- **A07 / API2 Authentication:** the token check keeps its order and its constant-time
  comparison. The id check still comes after it.
- **A09 Logging:** a refused id is logged at `warn` as `InvalidSystemId(<rule>)`, for example
  `InvalidSystemId(DotSegment)`. The rejection carries the `SystemIdError`, so the log names
  the broken rule without echoing the hostile id itself. The wire answer stays
  `invalid system_id` for every rule. (Settled during implementation, after
  `rosette-auditor` noted that the first design discarded the rule for no reason.)
- **API4 Resource Consumption:** partly touched. The id is now bounded. Message size, the
  handshake deadline and registration count are RFC 0006.
- **API8 Misconfiguration:** unchanged. With `HUB_PUSH_TOKEN` unset, anyone could create these
  ids before this change, and after it no one can.
- **API10 Unsafe Consumption:** the hub consumes fewer unsafe values from push clients.
- A02, A05, A06 (no dependency change), A08, A10 / API7, API3, API5, API6, API9: not touched.
  No route, field, dependency or outbound call changes.

## Testing plan

TDD, per `CLAUDE.md`:

1. **Pure refactor under green.** `EmptySystemId` becomes `SystemIdError::Empty`, and
   `authenticate` maps it through an exhaustive `match`. The existing tests are the contract.
2. **Stub, then all the red tests at once.** Add the `TooLong` and `DotSegment` variants and
   map them in `authenticate`, but leave the constructor accepting everything but `""`. Then
   write every new test before the constructor changes, so each one fails on an assertion:
   - the `SystemId` table: `.` and `..` (→ `DotSegment`); 255 bytes (accepted); 256 bytes and
     a multi-byte character straddling byte 255, such as 254 × `a` + `é`, which is 255
     characters but 256 bytes (→ `TooLong`); and `...`, `a.b`, `.hidden` (accepted);
   - the `authenticate` table: a `.` id with a valid token gives `InvalidSystemId`, and a
     `.` id with a wrong token gives `InvalidToken`, so the check order holds; also a 256-byte
     id with a valid token;
   - the real-server handshake test for an empty id becomes a table over `""`, `.`, `..` and a
     256-byte id: each is answered `invalid system_id`, and no system is registered.

   `red-test-adversary` attacks these tests.
3. **Characterisation test for the cleanup path.** A table over `.` (`/api/systems/%2E`) and
   `..` (`/api/systems/%2E%2E`): seed the system through `insert_system`, send the delete
   through the `Router`, and check that `get_system` finds nothing afterwards. The handler
   answers 200 whatever it deletes, so that last check is what proves the path's id was
   decoded. It passes on first run by design, so
   `red-test-adversary` attacks it in mutation mode.
4. **Minimal green, refactor,** then the gate in `system-hub`, then `rosette-auditor`.

## Impact on `docs/ARCHITECTURE.md`

- § Domain model, glossary: **system id** becomes "one URL path segment: non-empty, at most
  255 bytes, and not `.` or `..`. The hub refuses any other id at the push handshake; rows
  stored before the rule may still hold one (see Open architectural questions)".
- § Trust boundaries:
  - Agent → Hub (push): the id check rejects empty, over-long and dot-segment ids.
  - Hub API → hub dashboard: `encodeURIComponent` keeps ids in one path segment only because
    of the `SystemId` rule, which stored rows from before it may break.
- § Open architectural questions:
  - Replace the dot-segment entry with one on stored ids that may break the rule. It covers
    finding and deleting them, why they don't go stale on their own (the `push://` polling
    bug), and why a row parser must skip them.
  - Add the dashboard's stale panel on a failed per-system fetch.
  - Add the unbounded length of alert-record ids (`collector.rs` appends the agent's alert id
    unchecked, so an acknowledge URL can hit 414).
- `README.md`: the handshake prose ("It must not be empty") and the `invalid system_id` row.
- `rfcs/README.md`: the index row.

## Rollout / migration notes

Hub-only. Deploy in any order relative to agents.

A hub that already stores a system whose id breaks the rule keeps the row, and the upgrade
doesn't remove it. That system can't push again. It turns offline only because the poller
also polls `push://` rows and fails, which is a known bug. The hub has no graceful shutdown, so
the disconnect handler doesn't run on the upgrade restart. Once that polling bug is fixed, such
a row would stay `online` until deleted, so the operator should delete it:

```sh
# Find them. The ids are hostile by definition, so print a hex prefix and the byte length,
# never the raw id, which could carry terminal escape sequences or megabytes of text.
sqlite3 system-hub.db "SELECT hex(substr(id, 1, 16)), length(CAST(id AS BLOB)) FROM systems
  WHERE id IN ('.', '..') OR length(CAST(id AS BLOB)) > 255;"
# A dot-segment id: the hub decodes %2E into the path's id
# --path-as-is keeps curl from treating %2E as a dot segment itself.
curl --path-as-is -X DELETE "http://<hub>/api/systems/%2E"     # the system "."
curl --path-as-is -X DELETE "http://<hub>/api/systems/%2E%2E"  # the system ".."
# An id over 255 bytes whose encoded URL fits a request line deletes from the dashboard as
# usual. One that doesn't fit (from about 2.7 KB behind nginx, 21 KB against hyper) can't
# be addressed by URL. With the hub stopped, delete it in SQLite from
# the same four tables `Database::delete_system` clears, children first. The hub's bundled
# SQLite enforces foreign keys, but the sqlite3 CLI usually runs with them off, so don't rely
# on ON DELETE CASCADE here.
sqlite3 system-hub.db <<'SQL'
BEGIN;
CREATE TEMP TABLE doomed AS SELECT id FROM systems WHERE length(CAST(id AS BLOB)) > 255;
DELETE FROM metrics          WHERE system_id IN (SELECT id FROM doomed);
DELETE FROM alerts           WHERE system_id IN (SELECT id FROM doomed);
DELETE FROM metric_retention WHERE system_id IN (SELECT id FROM doomed);
DELETE FROM systems          WHERE id        IN (SELECT id FROM doomed);
COMMIT;
SQL
```

The characterisation test in the testing plan pins the `%2E` delete.
