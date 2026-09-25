# RFCs

Design records for changes with lasting consequences: new features, protocol changes, schema
changes, breaking API changes, or other decisions future contributors (human or agent) will
need the rationale for.

Trivial fixes, dependency bumps, formatting, and pure test-additions don't need one — see the
"RFCs for new changes" section in `../CLAUDE.md` for the full policy.

## Process

1. Copy `0000-template.md` to `NNNN-short-kebab-title.md`, where `NNNN` is the next unused
   4-digit number (check existing files in this directory for the current highest).
2. Fill it in: motivation, design, domain impact, alternatives considered, security
   implications (run through the OWASP checklist in `../CLAUDE.md`), testing plan, and impact
   on `../docs/ARCHITECTURE.md`.
3. Set status to `Draft` while under discussion.
4. Run the `rfc-adversary` subagent on the draft and act on its findings (see "Adversarial
   review" in `../CLAUDE.md`). Once the approach is agreed, set status to `Accepted` and
   implement test-first.
5. Once merged and working, set status to `Implemented`. If an accepted RFC is abandoned or
   replaced, mark it `Rejected` or `Superseded by NNNN` instead of deleting it — the history is
   the point.

## Index

| # | Title | Status |
|---|---|---|
| [0001](0001-constant-time-token-comparison.md) | Constant-Time API Token Comparison | Implemented |
| [0002](0002-tdd-ddd-adversarial-development.md) | TDD, DDD, Adversarial Review and the Rosette | Implemented |
| [0003](0003-hub-push-handshake-hardening.md) | Hub Push Handshake Hardening | Implemented |
| [0004](0004-alert-incident-identity.md) | Alert Incident Identity | Implemented |
| [0005](0005-system-id-dot-segments.md) | System Ids That Are Always One Path Segment | Implemented |
| [0006](0006-push-connection-limits.md) | Push Connection Limits and Fail-Closed Token | Implemented |
| [0007](0007-push-ingestion-cost.md) | One Snapshot Rule, One Transaction, One Serialisation | Draft |
| [0008](0008-push-registry-limit.md) | Push Registry Limit and Push System Lifecycle | Draft |
