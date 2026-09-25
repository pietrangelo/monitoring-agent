# RFC NNNN: Title

- Status: Draft
- Author:
- Date:
- Affects: `system-agent` / `system-hub` / both

## Motivation

What problem does this solve? Why now? What happens if we don't do it?

## Proposed design

The actual design: new/changed endpoints, message shapes, schema changes, module structure.
Be concrete — include request/response shapes, function signatures, or schema DDL where
relevant, not just prose.

## Domain impact

Which bounded contexts from `../docs/ARCHITECTURE.md` § Domain model this touches, which
glossary terms it adds or changes, and whether it changes a published contract between
contexts (the push frame, or the poll responses the hub consumes). Say how a mixed-version
fleet (new agent + old hub, old agent + new hub) behaves.

## Alternatives considered

What else was considered and why it was rejected (including "do nothing").

## Security implications

Explicitly run through the OWASP Top 10 (Web) and OWASP API Security Top 10 categories from
`../CLAUDE.md`. For each category the change plausibly touches, state the impact and
mitigation; for categories that clearly don't apply, say so briefly rather than omitting them.

## Testing plan

What will be tested, at what level (unit vs integration), and what new test infrastructure (if
any) this requires.

## Impact on `docs/ARCHITECTURE.md`

Which sections of the architecture doc will need to change if this is implemented as designed.

## Rollout / migration notes

Any backward-compatibility, data-migration, or deployment-order considerations (e.g. schema
migrations in `system-hub/src/db.rs`, changes to the push protocol requiring matched
agent/hub versions).
