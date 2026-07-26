# RFCs

Design records for changes with lasting consequences: new features, protocol changes, schema
changes, breaking API changes, or other decisions future contributors (human or agent) will
need the rationale for.

Trivial fixes, dependency bumps, formatting, and pure test-additions don't need one — see the
"RFCs for new changes" section in `../CLAUDE.md` for the full policy.

## Process

1. Copy `0000-template.md` to `NNNN-short-kebab-title.md`, where `NNNN` is the next unused
   4-digit number (check existing files in this directory for the current highest).
2. Fill it in: motivation, design, alternatives considered, security implications (run through
   the OWASP checklist in `../CLAUDE.md`), testing plan, and impact on `../docs/ARCHITECTURE.md`.
3. Set status to `Draft` while under discussion.
4. Once the approach is agreed, set status to `Accepted` and implement.
5. Once merged and working, set status to `Implemented`. If an accepted RFC is abandoned or
   replaced, mark it `Rejected` or `Superseded by NNNN` instead of deleting it — the history is
   the point.

## Index

| # | Title | Status |
|---|---|---|
| _(none yet)_ | | |
