# RFC 0017: Mail Transport: Sealed Reports over SMTP for Agents Without Internet Access

- Status: Draft
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-10-03 (revised the same day for one `rfc-adversary` pass; see Review)
- Affects: both. `system-agent` gains a third way to reach a hub (`mail/`, an SMTP client and
  the `MAIL_*` variables). `system-hub` gains a mail intake (`mail_intake/`, a Maildir reader
  and the `HUB_MAIL_*` variables), one table (`mail_receipts`), an overdue sweep, and a
  `mail-key` subcommand. The push handshake gains one refusal (`transport mismatch`, §6).
  No endpoint, push frame or poll response changes.
- Depends on:
  - RFC 0005 (Implemented): the `SystemId` rule, which the sealed report's header carries.
  - RFC 0007 (Implemented): the snapshot rule and `snapshot_intake::store_snapshot`, which
    every mailed snapshot goes through, as a pushed or polled one does.
  - RFC 0016 (Draft): the agent's id computed once (`push/identity.rs::AgentId`), reused here
    as the mail system id, and `registry.rs::SystemSource`, which this RFC extends with a
    `Mail` variant (§6). This RFC is implemented after 0016, or carries those two pieces
    itself if 0016 is rejected.
- Interacts with three Drafts, none of which it waits for: RFC 0008 (if accepted, its
  registry limit should count mail registrations too), RFC 0010 (if accepted, `mail_receipts`
  becomes a catalog table) and RFC 0011 (if accepted, its sealed credentials could replace the
  derived mail key, §3).

## Motivation

Today a hub learns about an agent in one of two ways (`docs/ARCHITECTURE.md` § Data flow):

| Mode | Needs |
|---|---|
| HTTP poll | the hub can open a TCP connection to the agent |
| WebSocket push | the agent can open a TCP connection to the hub |

Many of the environments where monitoring matters most allow neither. A plant network, a
bank's segregated zone, a ship, a hospital's medical-device VLAN or a classified enclave has
hosts that can't open a connection to the internet, or to the network where the hub runs, and
that the hub can't reach either. What these networks almost always do have is **mail**: an
internal SMTP relay (a smarthost) that every host may hand messages to, and that forwards them
outward through a mail gateway the security team already audits, filters and logs. Mail is
store-and-forward, so it also crosses links that are up only part of the day.

Without a third mode, such a host is either not monitored at all, or monitored by opening a
firewall hole for a WebSocket that the network's owners won't approve. This RFC adds a mode
that needs only what those networks already allow: the agent hands a message to its local
relay, and the hub reads the messages that reach its mailbox.

The price is latency and resolution. A mailed system is seen in minutes, not seconds, and at
the resolution of its report's samples, not of every snapshot. The mode is for systems that
otherwise wouldn't be seen at all; it doesn't replace push or poll where those work.

## Proposed design

### 1. The shape of the mode

```
 isolated network                     │ mail gateway │            hub's network
                                      │              │
 agent ──SMTP(STARTTLS)──▶ relay ─────┼──── ... ─────┼──▶ MTA ──▶ Maildir ──▶ hub mail intake
   one sealed report                  │              │   (postfix,       (HUB_MAIL_DIR)
   every MAIL_INTERVAL                                    fetchmail, ...)
```

- The **agent** speaks SMTP to one relay (`MAIL_RELAY`) and nothing else. It never needs DNS
  for the hub, a route to the hub, or the internet.
- The **hub** opens no new socket. It reads a **Maildir** (`HUB_MAIL_DIR`) that an MTA the
  operator already runs delivers into: postfix or another MTA's local delivery, or fetchmail /
  getmail pulling from an IMAP or POP3 mailbox. Mail delivery, spam filtering, TLS to the
  outside world and mailbox credentials stay in software built for them.
- Between them, every message passes through relays this project doesn't control and must
  treat as **untrusted and observing**: they can read, delay, reorder, duplicate, drop and
  rewrite messages (add footers, re-encode the body, rewrite headers). So the report is
  **sealed** end to end (§3): confidential and authenticated from the agent's memory to the
  hub's, whatever the path between.

A system is a *mail system* when it was registered by a mail report. Its registry row has
`url = "mail://"`, as a push system's has `push://`.

### 2. The mail report

One message carries one **mail report**: what the agent saw over one mail interval.

```rust
/// What one mail report carries (agent: `mail/report.rs`; hub: `mail_intake/report.rs`).
pub struct MailReport {
    kind: ReportKind,                  // "mail-report.v1"
    id: ReportId,                      // agent run + report sequence (from 1, never rewinds)
    created_at: u64,                   // unix seconds, agent clock
    interval_secs: MailInterval,       // the agent's MAIL_INTERVAL, 60..=86400
    reason: ReportReason,              // Scheduled | Incident | Other (a later agent's reason)
    snapshots: Vec<MailedSnapshot>,    // 1..=MAX_MAILED_SNAPSHOTS (60), oldest first
    alerts: Vec<MailedAlert>,          // every incident active since the last report, 0..=64
    round: Option<MailedRound>,        // the newest scrape round, when applications are on
}
```

- **Samples, not every snapshot.** The agent keeps one published snapshot every
  `MAIL_SAMPLE_INTERVAL` (default 60 s) and mails them together every `MAIL_INTERVAL`
  (default 300 s). `MAIL_INTERVAL / MAIL_SAMPLE_INTERVAL` may not exceed 60: a startup check
  refuses the pair otherwise. Each snapshot keeps its own `collected_at`, so the hub's charts
  show the samples at their real times.
- **`MailedSnapshot`** carries what a push snapshot frame carries (system info, CPU, memory,
  swap, load, uptime, disks, and its `timestamp` as `collected_at`) **without the top
  processes**: they are the largest and most sensitive part of a snapshot, and the hub stores
  none of them. Only the newest snapshot carries the system info strings.
- **Alerts travel too.** Unlike a push connection, a mail report carries **every alert
  incident that was active at any tick since the previous report**, once each by incident id
  (incident id, rule, metric, severity, message ≤ 512 bytes, `fired_at`), so an incident that
  fires and ends inside one interval still leaves an alert record. The agent keeps that set
  in the batch (`MailBatch::note_alerts`), and closing the batch empties it. The hub stores them through the same
  alert-record rule as the poll (`INSERT OR IGNORE` keyed `<system id>_<incident id>`), but
  from a typed DTO, not untyped JSON.
- **Incident reports.** When an incident becomes active, the agent doesn't wait for the
  interval: it mails a report at once with `reason = Incident`, holding the samples so far.
  At most one incident report per minute; incidents in between ride the next report. The next
  scheduled report starts a fresh batch.
- **Encoding: MessagePack with field names** (`rmp_serde::to_vec_named`). The push frame is
  positional because it is sent every 2 s; a report is sent every few minutes, and a named
  encoding lets a later version add a field that a v1 hub ignores, with no frame-order
  contract to break across a mixed-version fleet that mail makes hard to upgrade in step
  (an isolated network can't be upgraded at the hub's pace). `kind` names the version: an
  incompatible shape gets a new kind, which an older hub refuses and counts. Field names are
  only half of evolvability: **closed sets stay open at the DTO.** `reason`, an alert's
  `metric` and its `severity` are strings on the wire (at most 32 bytes each); the hub maps an
  unknown reason to `ReportReason::Other` and stores an unknown metric or severity as the
  string it is, as it already stores severity. So a later agent's new variant never makes a
  v1 hub refuse the whole report.
- **Snapshot times are bounded by the report.** Parsing refuses a report (`BadReport`) any
  of whose snapshots has `collected_at` outside `created_at − interval − 120 s ..=
  created_at + 5 s` (the slack absorbs an NTP step back between a sample and the close), or whose snapshots aren't in ascending order. An authentic report from an
  agent whose clock jumped can't reach the store with a far-future point that would prune a
  series' history, because `created_at` itself is bounded on the hub (§6).

The domain core that decides what a report holds is pure (`mail/batch.rs`): `MailBatch::
offer(snapshot, now_mono) -> Offer` keeps a sample when `MAIL_SAMPLE_INTERVAL` has passed
since the last kept one, and `MailBatch::close(alerts, round, reason, created_at) ->
MailReport` empties it. The clock, the alerts and the round are arguments.

### 3. Sealing

A report is sealed with **XChaCha20-Poly1305** (an AEAD: confidentiality and integrity in one
step, with a random 24-byte nonce that can't repeat in practice) under the system's **mail
key**:

```
sealed report = "SAMR" | version: u8 = 1 | id_len: u8 | system id (id_len bytes)
                | nonce (24 bytes) | ciphertext (MessagePack report + 16-byte tag)
associated data = everything before the nonce
```

The header is in the clear because the hub needs the system id to find the key; it is
authenticated as associated data, so no relay can move a report to another id. Everything
else, hostnames included, is encrypted.

**One key per system, derived from one hub secret.** The hub holds a 32-byte **mail master
key** (`HUB_MAIL_KEY`, base64). Each system's mail key is

```
mail key(system id) = HKDF-SHA256(ikm = master key, salt = "system-agent mail key v1",
                                  info = system id bytes), 32 bytes
```

The operator gets it with `system-hub mail-key <system id>` (reads `HUB_MAIL_KEY` from the
environment, prints the base64 key on stdout, touches no database) and sets it as the agent's
`MAIL_KEY`. The agent logs its system id at startup (it isn't secret) so the operator knows
what to derive for. So, unlike the push token:

- A host that is compromised leaks only its own key, which forges only its own id. The push
  mode's self-asserted id (API1, `docs/ARCHITECTURE.md` § Open architectural questions) does
  not carry over to mail.
- Rotating `HUB_MAIL_KEY` re-keys every mailed system at once, and revoking one system means
  changing the master key (there is no per-system revocation list in v1; see Alternatives).
- The hub keeps no per-system secret, and no secret in SQLite.

Opening a report on the hub, as a pure function over bytes and keys:

```rust
pub fn open(sealed: &[u8], master: &MailMasterKey) -> Result<(SystemId, MailReport), OpenRefusal>;

pub enum OpenRefusal {
    BadArmor,          // no armour block, or base64 that doesn't decode
    TooLarge,          // armour over MAX_SEALED_BYTES (512 KiB decoded)
    BadHeader,         // wrong magic, an id_len that overruns
    UnknownVersion(u8),
    InvalidSystemId(SystemIdError),
    NotAuthentic,      // the tag doesn't verify under the id's key
    BadReport,         // authentic, but not a mail-report.v1 MessagePack map, or out of bounds
}
```

The id is parsed into `SystemId` before any key is derived. Tag verification is the AEAD's own
constant-time check. Every refusal is counted by variant; none logs the bytes, the id of an
unauthenticated report, or any key.

### 4. Armour and the message

The sealed bytes travel as base64, wrapped at 76 columns, between armour lines, in a
`text/plain; charset=us-ascii` body:

```
From: <MAIL_FROM>
To: <MAIL_TO>
Subject: system-agent report <system id> <run>-<seq>
Date: ...
Message-ID: <uuid@system-agent.invalid>
Content-Type: text/plain; charset=us-ascii
Content-Transfer-Encoding: 7bit

-----BEGIN SYSTEM-AGENT REPORT-----
WFNBTVIBDHdlYi0w...
-----END SYSTEM-AGENT REPORT-----
```

The hub **trusts no header**: not `From`, `Subject` nor `Date`. It decodes the message's text
parts (undoing any quoted-printable or base64 transfer encoding a relay applied), finds the
first armour block, ignores anything around it (a gateway's footer or disclaimer), and opens
it. The subject is only for people reading the mailbox. A message that is too large or holds
no block is refused (`BadArmor` / `TooLarge`). Messages are text, not an attachment, because
gateways that strip or quarantine unknown attachments are common in exactly these networks.

### 5. Agent: the mail client

`src/mail/` (Telemetry Publishing context):

- `config.rs`: `MailConfig::parse` over a lookup function, at startup next to the other
  configuration, before the runtime exists. A refusal exits with 78 (`EX_CONFIG`), naming the
  variable, never the value.
- `batch.rs`: the pure `MailBatch` (§2) and the incident-report pacing,
  `IncidentPace::allows(now_mono) -> bool`.
- `seal.rs`: the pure sealing (§3), the nonce passed in by the caller so tests are
  deterministic; the adapter draws it from the OS RNG.
- `client.rs`: the adapter. It reads the published snapshot channel, offers each snapshot to
  the batch, and on each interval tick (or allowed incident) closes, seals, armours and hands
  the message to an outbox task.
- **Outbox.** Sending uses `lettre`'s Tokio SMTP transport, so nothing blocks the runtime. A
  report that the relay refuses temporarily (4xx, a connect or TLS failure) stays queued and
  is retried with exponential backoff from 30 s to `MAIL_INTERVAL`; a permanent refusal (5xx)
  is logged at `error` and dropped. The outbox holds at most 288 reports (a day at the
  default interval): the oldest is dropped first, and drops are counted and logged at most
  hourly. The outbox lives in memory, so reports queued when the agent stops are lost: a
  restart costs at most the queued backlog, which the hub shows as an overdue system (§7).
- **TLS to the relay.** `MAIL_TLS` is `starttls` (default, required: the session fails if the
  relay doesn't offer it), `tls` (implicit TLS, port 465) or `none`. `none` is accepted only
  when `MAIL_RELAY` is a loopback address, so a host-local MTA (`127.0.0.1:25`) works while
  metadata never crosses a network in clear SMTP; the report itself is sealed either way.
  Certificate verification can't be turned off. `MAIL_RELAY_CA` may name a PEM bundle for an
  internal CA.
- **Relay authentication** is optional: `MAIL_RELAY_USERNAME` and `MAIL_RELAY_PASSWORD`, used
  only over TLS (`none` with credentials is refused at startup).
- **Exclusive with push.** An agent configured with both `PUSH_TO` and `MAIL_TO` refuses to
  start: two transports would deliver the same snapshots under the same id, and the hub
  would store the samples twice. Falling back from push to mail is an Alternative below.

### 6. Hub: the mail intake

`system-hub/src/mail_intake/` (Ingestion context):

- `config.rs`: `MailIntakeConfig::from_env`. Mail intake is **off** unless both
  `HUB_MAIL_DIR` and `HUB_MAIL_KEY` are set. One without the other, a key that isn't 32 bytes
  of base64, or a directory without `new/` and `cur/` refuses startup, naming the variable.
  There is no "open" mode: unlike `HUB_PUSH_TOKEN`, an empty key can't mean "accept
  everything", because every report must open under a key.
- `report.rs` / `seal.rs`: the hub's own declarations of the report and `open` (§3). The two
  crates share no code; a golden sealed report (`testdata/mail-report-v1.sealed`, made with a
  fixed test key and nonce by `testdata/generate_mail_report_v1.py`) ties the declarations,
  as the application frame's golden bytes do.
- `scan.rs`: the adapter. Every 10 s, on the blocking pool, it lists `new/` and takes at most
  `MAX_MESSAGES_PER_SCAN` (256) files, oldest first. For each: refuse a file over 1 MiB
  without reading it whole, read it, `open` it, then ingest it (below). **Every message is
  deleted after it is handled, accepted or refused**: the hub keeps no copy, so a mailbox
  that anyone on the internet can write to can't fill the hub's disk. Refusals are counted
  by variant and logged at `warn` at most hourly (`hourly_warning`), naming the variant and
  the Maildir file name, never content. A file the hub can't delete is logged and skipped
  until the next scan.
- **Ingesting an opened report.** The decisions are pure (`mail_intake/receipt.rs`); the
  writes are one new store method, `Database::store_mail_report`, which runs **one SQLite
  transaction** under the database mutex, on the blocking pool. Today's
  `insert_system_if_absent`, `store_snapshot` and `insert_alert` each commit on their own, so
  calling them in sequence would not be atomic; the new method reuses their cached
  statements inside its own transaction instead.
  1. **Freshness** (pure, `receipt::fresh(created_at, now) -> Result<(), Stale>`): refuse a
     report whose `created_at` is more than 5 minutes ahead of the hub's clock or older than
     the **receipt window** (7 days). The window is the replay bound, not the retention: a
     weekend gateway outage or a day's outbox backlog still arrives inside it, and its alert
     records are kept even when its points have expired (step 5).
  2. **Transport** (in the transaction): if the id's row exists and its URL isn't `mail://`,
     refuse the report (`TransportMismatch`, counted): a mail report never writes into a
     pushed or polled system. The other direction is closed too: the **push handshake
     refuses an id whose row is `mail://`** (a new `HandshakeRejection::TransportMismatch`,
     answered `auth_error` with `transport mismatch`, checked after the token like the id
     rule), so a push connection can never feed a mail row that the overdue sweep (§7) then
     marks offline under it. Shipped agents retry any `auth_error` after 5 s, as today. Otherwise
     insert the row if absent, with `url = "mail://"`.
  3. **Recency** (pure, `receipt::recency(created_at, previous_newest) -> Recency`), from
     the system's newest receipt **read before this report's receipt is inserted**: the
     report is `Newest` when the system has no receipt yet, or when its `created_at` is at
     least the previous newest's (an incident report and a scheduled one in the same second
     are both `Newest`, the later arrival winning); otherwise it is `Backfill` (delivered
     late or out of order).
  4. **Receipt**: `INSERT INTO mail_receipts ... ON CONFLICT DO NOTHING`. No row
     inserted means `(run, seq)` was already accepted: the report is a duplicate (mail is
     delivered at least once, and a relay may replay it), the transaction is rolled back and
     the message deleted. Because the receipt and the points commit together, a failure
     anywhere leaves neither, and a later duplicate of the same message is ingested whole,
     never twice.
  5. **Snapshots**: each through the snapshot rule, oldest first, at its own `collected_at`.
     Expiry is per point, not per snapshot: a point older than its own series' retention
     (`snapshot_retention`, read in the transaction for that series) is left out and counted
     as `Expired`, a new mail-intake count, while the snapshot's other points are kept (the
     snapshot rule itself takes no time). Only a `Newest` report updates the system's status (online),
     last seen and live metrics, from its newest snapshot. A `Backfill` report adds points and
     alert records only, so a late report never rewinds the dashboard or flips an overdue
     system back online.
  6. **Alerts**: each through the alert-record rule (`INSERT OR IGNORE`).
  7. **Prune**: delete the system's receipts older than the receipt window, **except its
     newest**, which `mail_status` (§7) reads.

  After the commit, the round of a `Newest` report (if any) goes through
  `round_intake::store_round` with the system's mail source pace; its own duplicate rule
  (recent rounds) makes a replayed round a duplicate. A `Backfill` report's round is dropped
  and counted: `store_round` replaces the shown round and stamps `app:*` points at hub time
  (RFC 0009), so storing a late round would show stale data as fresh. Stamping late rounds at
  `created_at` would be a change to RFC 0009 and is out of scope. A database failure rolls everything back: the message is refused, deleted and
  counted, and its samples are lost (see Alternatives for keeping a failed message). There
  is **no per-system report pace**: a link that was down for hours delivers its backlog in
  one scan, and every report in it is stored. Cost is bounded per report (API4) and per
  system by the receipts and the key holder's own rate.
- **Mail systems are never polled.** `SystemSource` (RFC 0016) gains a `Mail` variant for
  exactly `MAIL_URL` (`"mail://"`), matched exhaustively; the poller polls only `Poll`.
- **Deleting a system** deletes its receipts: `delete_system` gains an explicit
  `DELETE FROM mail_receipts WHERE system_id = ?`, as it already deletes `metrics`, `alerts`
  and `metric_retention` by hand (the hub sets no `PRAGMA foreign_keys`, so no cascade can
  be relied on).

### 7. Status of a mail system

A mail system has no connection to end, so its status comes from its reports:

```rust
/// Pure: whether a mail system's reports are on time.
pub fn mail_status(newest_report_at: SnapshotTime, interval: MailInterval, now: SnapshotTime)
    -> MailPresence; // OnTime | Overdue
```

A system is **overdue** once more than `3 × interval + 15 min` has passed since the
`created_at` of its newest accepted report, on the hub's clock: three missed reports plus a
margin for mail latency. The newest receipt is never pruned (§6), so the rule always has an
input, whatever the interval. A mail row with no receipt at all (a receipt lost to a manual
SQL edit) is overdue. An accepted report marks the system online through the snapshot's
transaction, as any stored snapshot does. A sweep every 60 s (on the blocking pool) marks each
overdue mail system offline and evicts its live metrics. A mail system's *last seen* is its
newest report's `created_at`: a new `LastSeen::ReportedAt(SnapshotTime)` variant, written
only by a `Newest` report.

### 8. Configuration

Agent:

| Variable | Default | Meaning |
|---|---|---|
| `MAIL_TO` | — | The hub's mailbox address. Unset: mail is off |
| `MAIL_FROM` | `system-agent@<hostname>` | Envelope and header sender |
| `MAIL_RELAY` | — (required with `MAIL_TO`) | `host:port` of the SMTP relay |
| `MAIL_TLS` | `starttls` | `starttls`, `tls`, or `none` (loopback relay only) |
| `MAIL_RELAY_CA` | system roots | PEM bundle to verify the relay |
| `MAIL_RELAY_USERNAME` / `MAIL_RELAY_PASSWORD` | — | SMTP AUTH, over TLS only |
| `MAIL_KEY` | — (required with `MAIL_TO`) | This system's mail key, from `system-hub mail-key` |
| `MAIL_INTERVAL` | `300` | Seconds between scheduled reports, 60 to 86400 |
| `MAIL_SAMPLE_INTERVAL` | `60` | Seconds between kept samples, 10 to `MAIL_INTERVAL`; at most 60 samples a report |

Hub:

| Variable | Default | Meaning |
|---|---|---|
| `HUB_MAIL_DIR` | — | Maildir the intake reads. Unset: mail intake is off |
| `HUB_MAIL_KEY` | — (required with `HUB_MAIL_DIR`) | 32-byte mail master key, base64 |

`MAIL_KEY`, `MAIL_RELAY_PASSWORD` and `HUB_MAIL_KEY` are secrets: never logged, never in an
error message, and their types implement no `Debug`.

## Domain impact

- **Telemetry Publishing** (agent) gains a second transport, `mail/`. **Ingestion** (hub)
  gains a third source, `mail_intake/`. **Fleet Registry** gains a third registration path
  and a status rule (`mail_status`). **Fleet History** gains no new store: mailed snapshots,
  alerts and rounds go through the existing rules. No new bounded context.
- **Glossary terms added:** *mail report*, *report id* (agent run + report sequence),
  *mail interval*, *sample interval*, *incident report*, *sealed report*, *armour*, *mail key*,
  *mail master key*, *mail receipt*, *receipt window*, *mail system*, *overdue*, *backfill
  report*. *System status* (a mail system goes offline when overdue, and only a newest
  report marks it online) and *last seen* (`LastSeen::ReportedAt`) are amended for mail
  systems, and `SystemSource` gains `Mail`. Not *push frame*: a mail report is
  never sent on a push connection.
- **New published contract:** the sealed report (header, AEAD, `mail-report.v1` map), tied by
  golden bytes. Mixed versions:
  - Old agent + new hub: the agent never mails; nothing changes.
  - New agent + old hub: the old hub reads no Maildir; reports pile up in the mailbox, never
    in the hub. The operator sees the system missing and upgrades the hub.
  - A later agent adding a field to the report: a v1 hub ignores unknown map keys. A new
    reason, alert metric or severity is a string a v1 hub keeps or maps to `Other` (§2).
  - A later incompatible report (`mail-report.v2`): a v1 hub refuses it as `BadReport` and
    counts it; the agent must keep sending v1 until the hub is upgraded (a `MAIL_REPORT_KIND`
    switch would be that RFC's business).
- The push frame and the poll responses are unchanged. The push handshake gains one
  refusal, `transport mismatch`, for an id whose row is `mail://`. Every shipped agent treats
  it as any `auth_error` (logs it, retries after 5 s), so no agent changes for it; it can
  only occur once a hub with this RFC has mail systems.

## Alternatives considered

- **Do nothing.** Isolated hosts stay unmonitored, or get firewall exceptions their owners
  won't grant. Rejected: this is the request.
- **An SMTP server inside the hub.** The hub would listen on port 25 and be the MX. Rejected
  for v1: it puts an internet-facing protocol parser into the hub, duplicates what every MTA
  does better (greylisting, spam filtering, TLS policy, queueing), and needs the hub to be
  reachable from the internet. A Maildir works behind any MTA and any mailbox, and an
  embedded receiver could still be added later over the same `open`.
- **IMAP in the hub.** Simpler for operators with only a hosted mailbox, but adds an IMAP
  client, mailbox credentials and a long-lived connection to the hub. fetchmail or getmail
  into a Maildir gives the same result with no code here. Can be revisited.
- **Signing only (HMAC or Ed25519), no encryption.** Leaves hostnames, OS versions, disk
  layout and alert messages readable by every relay and gateway on the way: reconnaissance
  for an attacker, and often a policy violation in exactly these networks. AEAD costs the
  same.
- **S/MIME or OpenPGP.** Standard, but needs certificate or keyring management on every
  agent and a heavy dependency (or shelling out to `gpg`) on both sides. A derived symmetric
  key is one variable per agent.
- **One shared key for all agents (like `HUB_PUSH_TOKEN`).** Simpler, but one compromised
  host could forge every system. The HKDF derivation costs one subcommand.
- **Per-system random keys stored in the hub.** Allows revoking one system without
  re-keying all, but puts secrets in SQLite and needs RFC 0011's sealed credentials. The
  derivation can later be replaced by stored keys without changing the wire format (the
  header already names the id).
- **Positional encoding, like the push frame.** Smaller, but mail-isolated fleets upgrade
  slowly and out of step; a named map lets fields be added safely. Size isn't a concern at
  one report per interval.
- **Push with mail fallback in the same agent.** Useful for flaky links, but the hub would
  need to deduplicate samples across transports by snapshot time. Deferred; v1 makes the two
  exclusive.
- **Keeping refused or failed messages** (a `rejected/` folder). Helps debugging but lets
  anyone who can mail the address fill the hub's disk. v1 deletes them and counts.
- **Persisting the agent's outbox on disk.** Would survive restarts, but adds a state
  directory and its permissions to an agent that has none today. Deferred.

## Security implications

**OWASP Top 10 (2021):**

- **A01 Broken Access Control:** a mail report can write only to the system id it was sealed
  for, because each id has its own key (§3). Mail registration is unbounded by count, like
  push (API4 below, RFC 0008). The `mail-key` subcommand prints a key to its own stdout only;
  anyone who can run it already holds `HUB_MAIL_KEY`.
- **A02 Cryptographic Failures:** XChaCha20-Poly1305 with random nonces (no nonce reuse
  concern at this rate), HKDF-SHA256 with a fixed salt and the id as info, a 32-byte master
  key refused at startup if shorter. RustCrypto crates (`chacha20poly1305`, `hkdf`, `sha2`),
  no hand-written crypto, tag checked by the AEAD in constant time. TLS to the relay by
  default with no way to disable verification. Keys never logged; key types have no `Debug`.
  The hub's clock bounds replay to the receipt window.
- **A03 Injection:** the hub never uses header values. System ids, hostnames, alert messages
  and gauge names from an opened report pass the same rules as push and poll (`SystemId`,
  the snapshot rule, the registry's display bounds, the round rules) before SQL (bound
  parameters) or the dashboard (its `textContent` rendering rule). The agent builds the
  message with `lettre`'s builder, so its own hostname can't inject headers.
- **A04 Insecure Design:** relays are modelled as hostile (§1); duplicates, replays,
  reordering and delays are handled by receipts and the freshness bounds, not assumed away.
- **A05 Security Misconfiguration:** mail intake is off by default and has no open mode;
  half a configuration refuses startup on both sides; `MAIL_TLS=none` only to loopback.
- **A06 Vulnerable Components:** new dependencies: `lettre` (agent), `mail-parser` (hub),
  `chacha20poly1305`, `hkdf`, `sha2`, `base64` (both). Run `cargo audit` when they land.
  `mail-parser` parses attacker-controlled input and is bounded by the 1 MiB file cap.
- **A07 Identification & Authentication Failures:** authentication is the AEAD tag under a
  per-system key. Relay SMTP AUTH credentials are sent only over TLS.
- **A08 Software & Data Integrity Failures:** the header is associated data, so an id can't
  be swapped; the report version is inside the authenticated header.
- **A09 Logging & Monitoring Failures:** refusals counted by variant and logged hourly;
  outbox drops counted on the agent; overdue systems go offline on the dashboard, which is
  the signal that a path is broken. No content, key or unauthenticated id is logged.
- **A10 SSRF:** none: the hub makes no outbound request for mail, and the agent connects only
  to the operator's configured relay.

**OWASP API Security Top 10 (2023):**

- **API1 BOLA:** improved over push for mailed systems (per-system keys). A push token holder
  can still push as a mail system's id: the registry doesn't bind an id to a transport. That
  stays one of push's standing risks; it isn't widened.
- **API2 Broken Authentication:** as A07.
- **API3 Property-level authorization:** the report is a closed, typed map; unknown keys are
  ignored, nothing in it selects a column or a system other than the sealed id.
- **API4 Unrestricted Resource Consumption:** per scan at most 256 messages, each at most
  1 MiB read and 512 KiB decoded, each report at most 60 snapshots (each through the snapshot
  rule's 1024-disk bound), 64 alerts and one round. Unauthenticated mail costs one bounded
  read, one MIME parse and one AEAD attempt, then deletion. Authentic reports are stored
  without a pace, so a backlog isn't lost (§6): a compromised agent can store as many
  reports as it can mail, bounded by its relay, the 256-a-scan cap and the 7-day window's
  points, and only as its own id. `mail_receipts` is pruned to the 7-day window plus each
  system's newest receipt. Registrations are unbounded, as push's are (RFC 0008, Draft).
- **API5 Function-level authorization:** no new HTTP endpoint.
- **API6 Sensitive business flows:** N/A.
- **API7 SSRF:** as A10.
- **API8 Security Misconfiguration:** as A05. CORS is untouched.
- **API9 Improper Inventory Management:** no endpoint changes; the README gains the new
  variables, the subcommand and the Maildir deployment pattern.
- **API10 Unsafe Consumption of APIs:** the opened report is agent data like a push frame,
  converted into domain types once, at the edge, and every bound above applies before
  storing.

## Testing plan

Domain cores, table-driven:

- Agent `MailBatch`: sample spacing at the boundary (exactly `MAIL_SAMPLE_INTERVAL`), the
  60-sample cap, close empties, incident pacing (two incidents within a minute, one report).
- Agent `MailConfig::parse`: every variable's bounds, `PUSH_TO` + `MAIL_TO` refused,
  `none` with a non-loopback relay refused, `none` with credentials refused, a key that isn't
  32 bytes refused; refusals never contain the value.
- Both crates' sealing: round trip agent `seal` → hub `open`; each `OpenRefusal` variant
  (truncated header, `id_len` overrun, version 2, `..` as id, a flipped bit in the header,
  nonce or ciphertext, wrong system's key, an authentic but malformed map); the golden sealed
  report opens on the hub and re-seals byte-identically on the agent with the fixed nonce.
- Hub armour: a footer after the block, quoted-printable re-encoding, CRLF line ends, two
  blocks (the first wins), no block, an oversize block.
- Hub `receipt::fresh` and `receipt::recency`: 5 min + 1 s in the future, window + 1 s old,
  a system's first report (`Newest`), a new run, an older report after a newer one
  (`Backfill`), equal `created_at` (`Newest`).
- Hub report parsing: a `collected_at` one second outside its bounds, snapshots out of
  order, an unknown reason (→ `Other`), an unknown metric and severity (kept), an extra map
  key (ignored), each pinned by a golden variant.
- Agent `MailBatch::note_alerts`: an incident that starts and ends between two closes is in
  the next report once; the set is empty after a close.
- Hub `mail_status`: exactly `3 × interval + 15 min` (on time) and one second past (overdue).
- HKDF: a fixed test vector, and two ids giving different keys.

Adapters:

- Hub scan (temp Maildir via `tempfile`): accepted message stored and deleted; refused
  message deleted and counted; 257 files take two scans; a 1 MiB + 1 byte file refused
  unread; 96 reports of a backlog in one scan are all stored.
- Hub `store_mail_report` (temp SQLite): a failure injected at the k-th snapshot leaves no
  points and no receipt, and the same message ingested again stores each point once; a
  duplicate stores nothing; a backfill report doesn't change status, last seen or live
  metrics; a report for a `push://` id is refused; pruning keeps the newest receipt with a
  86400 s interval; a point older than its series' retention is counted `Expired` while the
  same snapshot's other points are kept; a backfill report's round is dropped and counted.
- Hub push handshake: an id whose row is `mail://` is answered `transport mismatch`, after a
  wrong token is still answered as a wrong token.
- Hub: the poller skips `mail://` rows (`SystemSource::Mail`); the sweep marks an overdue
  mail system offline and evicts its live metrics; `delete_system` removes the receipts.
- Hub `mail-key` subcommand: prints the derived key for an id, refuses an invalid id, never
  opens the database.
- Agent outbox against a scripted SMTP server on an ephemeral port (a small Tokio test
  double): 4xx retried, 5xx dropped, the 289th report drops the oldest, STARTTLS required
  when not offered fails the session.
- Agent `main`: both transports configured exits 78.

No dashboard change in v1 (a mail system renders as any system, its URL as text), so
`xss.mjs` is unchanged; any later "via mail" badge extends it.

## Impact on `docs/ARCHITECTURE.md`

When implemented: § Components (both binaries: the mail client and the mail intake), § Data
flow (a third row: "Mail | Agent → relay → Maildir → Hub | SMTP + sealed MessagePack |
agent can't open connections beyond its network"), § Domain model (the contexts' modules, the
new published contract, the glossary terms above, amended *system status* and *last seen*),
§ Trust boundaries (Agent → Hub (mail): relays untrusted, per-system keys, receipts), § Storage
(`mail_receipts`), § Testing architecture (the golden sealed report), and § Open architectural
questions (outbox lost on restart, no per-system revocation, mail registrations unbounded).
While this RFC is a Draft, the document is unchanged: it describes the system as it is.

## Rollout / migration notes

- **Schema:** one new table, created with `CREATE TABLE IF NOT EXISTS` at startup like the
  others; no existing table changes, so an older hub opening the database ignores it.

  ```sql
  CREATE TABLE IF NOT EXISTS mail_receipts (
      system_id     TEXT    NOT NULL,
      run           TEXT    NOT NULL,
      seq           INTEGER NOT NULL,
      created_at    INTEGER NOT NULL,
      interval_secs INTEGER NOT NULL,
      received_at   INTEGER NOT NULL,
      PRIMARY KEY (system_id, run, seq)
  );
  CREATE INDEX IF NOT EXISTS mail_receipts_newest ON mail_receipts (system_id, created_at);
  ```

  If RFC 0010 (Draft) is accepted, the table becomes a catalog table of the hub store.
- **Order:** upgrade the hub and set `HUB_MAIL_DIR` / `HUB_MAIL_KEY` first, derive each
  system's key, then configure agents. Reports mailed before the hub is ready wait in the
  mailbox and are ingested when it starts, as long as they are inside the 7-day window.
- **Moving an agent between push and mail** (either way): delete the system on the hub
  first, then reconfigure the agent. Its history goes with it; until it is deleted, the new
  transport is refused (`TransportMismatch`) on both sides.
- **Rolling back to a hub without this RFC:** first disable every mail system
  (`PUT /api/systems/:id` with `enabled: false`), since an older hub classifies `mail://`
  as a polled URL and would mark it offline every 30 s. An older `delete_system` leaves
  `mail_receipts` rows behind; they are harmless and ignored by that hub, and an upgraded hub
  prunes them past the window.
- **Deleting a mail system** from the dashboard deletes its receipts (`delete_system`); its agent's
  next report registers it again with no history, as a push system's reconnect does.
- **Key rotation:** set a new `HUB_MAIL_KEY`, re-derive and redeploy every agent's
  `MAIL_KEY`. Reports sealed with the old key and still in transit are refused as
  `NotAuthentic`.

## Review

One `rfc-adversary` pass on the first draft (2026-10-03). Every finding was CONFIRMED except
one PLAUSIBLE, and all were addressed in place:

- A per-system report pace would have refused and deleted a delayed backlog: removed (§6).
- The ingestion steps weren't one transaction against today's store, so a failure followed by
  a relay duplicate stored points twice: one `store_mail_report` transaction, receipt first.
- A late report would have rewound live metrics, status and last seen: `Recency` and
  backfill reports, and `LastSeen::ReportedAt`.
- `mail://` rows would have been polled under RFC 0016's `SystemSource`: a `Mail` variant,
  and the dependency declared.
- The snapshot rule takes no time, so the "left out for age" claim was false, and an
  authentic far-future `collected_at` could prune history: snapshot times bounded by the
  report, an explicit `Expired` count, and a 24 h receipt window.
- `ON DELETE CASCADE` never fires (no `PRAGMA foreign_keys`): explicit delete.
- With an 86400 s interval the newest receipt was pruned before the system could be overdue:
  the newest receipt is never pruned.
- (PLAUSIBLE) New enum variants would have made a v1 hub refuse whole reports: reason,
  metric and severity are strings at the DTO. Decided: adopted.
- Incidents ending between reports left no record: the report carries every incident active
  since the previous one.
- Minor: dependencies on Draft RFCs reworded as conditional; a mail report for an id held by
  a push or poll row is refused (`TransportMismatch`).

Closest attack to a design flaw that held: the crypto (AEAD with the header as associated
data, per-id HKDF keys, the id parsed before any key is derived) and the absence of blocking
work on the runtime.

A second pass on the amendments (2026-10-03) confirmed the first pass's fixes against the code
and found five new problems (three CONFIRMED, two PLAUSIBLE), all addressed:

- Recency was computed after the report's own receipt was inserted, so no report could be
  `Newest`: it now reads the previous newest receipt first, and ties count as `Newest`.
- A backfill report's round would have replaced the shown round and been charted at hub
  time: only a `Newest` report's round is stored.
- `TransportMismatch` was one-way, so a push connection could feed a mail row that the sweep
  then marked offline every minute: the push handshake refuses `mail://` ids, and Rollout
  documents moving an agent between transports.
- (PLAUSIBLE) A 24 h receipt window lost reports from a weekend outage: the replay window is
  now 7 days, separate from retention. Decided: adopted.
- (PLAUSIBLE) Rolling back to an older hub polls mail rows: Rollout now says to disable them
  first. Decided: adopted, as a documented step rather than a change to RFC 0016.
- Minor: 5 s of slack after `created_at` for a clock step; expiry is per point.

The push handshake refusal touches the published push handshake contract, so a third pass is
due before the RFC becomes `Accepted`.
