# RFC 0001: Constant-Time API Token Comparison

- Status: Implemented
- Author: Claude (pairing with pietrangelo.masala@gmail.com)
- Date: 2026-07-26
- Affects: `system-agent`

## Motivation

`src/auth.rs::require_auth` gates every non-exempt `system-agent` API route behind a shared
secret (`SYSTEM_AGENT_TOKEN`). Until this change, the comparison between the presented token
and the expected one was:

```rust
let token = extract_token(&request);
if token.as_deref() == Some(&expected) {
    return Ok(next.run(request).await);
}
```

`&str`/`&[u8]` equality via `==` (`PartialEq`) short-circuits on the first differing byte. For
a network-facing comparison against a secret, that turns per-byte comparison time into an
oracle: an attacker who can measure response latency with enough precision (repeated requests,
statistical averaging to cancel network jitter) can in principle recover the token one byte at a
time, trying each candidate byte and keeping whichever shifts the timing distribution latest.
This is a textbook timing side-channel and was called out explicitly as standing tech debt in
`CLAUDE.md`'s OWASP section (A02 Cryptographic Failures / API2 Broken Authentication), with the
instruction to fix it "if you touch this function."

This RFC documents that fix, written up after the fact per CLAUDE.md's allowance for "alongside,
for small-enough same-session work" — the change is small (single function), self-contained
(one module, no protocol/schema impact), and was implemented and merged before this RFC was
requested; it's recorded here for the design rationale, not as a gate before implementation.

## Proposed design

Add `subtle` (a small, widely-used, `no_std`-friendly constant-time-comparison crate already
the de facto standard for this in the Rust ecosystem — used by `ring`, `rustls`-adjacent crates,
etc.) as a dependency, and replace the direct `==` with a dedicated helper:

```rust
use subtle::ConstantTimeEq;

fn tokens_match(presented: Option<&str>, expected: &str) -> bool {
    match presented {
        Some(token) => token.as_bytes().ct_eq(expected.as_bytes()).into(),
        None => false,
    }
}
```

`require_auth` now calls `tokens_match(token.as_deref(), &expected)` instead of comparing
directly. `ConstantTimeEq::ct_eq` for `[u8]` returns a `subtle::Choice` computed by ORing the
XOR of every byte pair without early return, then only branches once at the very end to produce
the `bool` — the content of the token no longer affects comparison time.

One caveat, intentionally accepted: `subtle`'s slice impl checks `self.len() != other.len()`
before the constant-time byte loop and returns `Choice::from(0)` immediately on mismatch. That
means *token length* is still observable via timing, but the token's *content* is not. This
matches standard practice elsewhere (e.g. `ring::constant_time::verify_slices_are_equal` has the
same property) — length isn't the secret; a fixed-length token (as this deployment model
effectively requires, since it's a single shared env-var secret) leaks nothing useful from a
length oracle.

## Alternatives considered

- **Do nothing.** Rejected — CLAUDE.md explicitly flags this as a fix-on-touch item, and the
  attack is realistic for a token gating a network API, even a self-hosted one.
- **Hand-rolled constant-time compare** (XOR-accumulate loop written inline). Rejected in favor
  of `subtle`: hand-rolled constant-time code is exactly the kind of thing that's easy to get
  subtly wrong (compiler autovectorization or short-circuiting optimizations can silently
  reintroduce a timing leak that manual review won't catch), and CLAUDE.md's own guidance names
  `subtle::ConstantTimeEq` as the preferred approach.
- **Hash the token before comparing** (e.g. compare `sha256(token)` instead of the raw token).
  Rejected as unnecessary complexity for this case — hashing doesn't eliminate the need for a
  constant-time comparison of the *hash output* anyway, so it just adds a hash computation
  without removing the actual fix needed.
- **Rate-limiting instead of / in addition to constant-time comparison.** Complementary, not a
  substitute — rate limiting raises the number of measurements an attacker needs, but doesn't
  remove the channel. Out of scope here (tracked separately as the existing "No rate limiting"
  standing risk in CLAUDE.md); not bundled into this change to keep it focused.

## Security implications

Run through per CLAUDE.md's OWASP checklist:

- **A02 Cryptographic Failures / API2 Broken Authentication**: this is the category the change
  directly addresses — closes the timing side-channel on `SYSTEM_AGENT_TOKEN` comparison.
- **A01 Broken Access Control / API1 BOLA**: N/A — no change to *what* is authorized, only to
  *how* the token match is computed. The set of exempt paths (`/api/health`, `/static/*`, `/`,
  `/dashboard`, `/index.html`) is unchanged.
- **A03 Injection**: N/A — no parsing/rendering changes.
- **A05 Security Misconfiguration**: N/A — no config surface changed; still opt-in via
  `SYSTEM_AGENT_TOKEN` env var, same as before.
- **A06 Vulnerable Components**: added one new dependency, `subtle` (v2). It's a small,
  dependency-light, widely-audited crate (no known advisories at time of writing); `cargo audit`
  is not installed in this environment, so it wasn't run — flagging per policy rather than
  silently skipping.
- **A09 Logging & Monitoring**: N/A — no logging changes; the token itself was never logged
  before or after this change.
- **API4 Unrestricted Resource Consumption**: N/A — comparison cost is O(max(len(token),
  len(expected))) either way; no measurable resource change.
- All other Top-10 / API-Top-10 categories: not touched by this change.

## Testing plan

Unit tests for the new `tokens_match` helper (`src/auth.rs::tests`), covering:

- equal tokens match,
- different tokens of equal length don't match,
- different lengths don't match in either direction (presented longer than expected, and vice
  versa),
- `None` (no token presented) never matches,
- the empty-string edge case (`expected == ""` only matches a presented empty string — though in
  practice `configured_token()` already filters out an empty `SYSTEM_AGENT_TOKEN`, so this path
  isn't reachable through `require_auth` itself; it's tested at the helper level for completeness).

The existing `require_auth` integration tests (bearer/API-key/query-token accept and reject
paths, exempt-path bypass) were already in place from the broader test-coverage pass and
continue to pass unchanged, since external behavior (accept/reject decisions) is identical —
only the comparison mechanism changed, not its truth table.

All four crate-level gates pass: `cargo fmt --all`, `cargo clippy --all-targets --all-features
-- -D warnings`, `cargo test` (122 tests in `system-agent`), `cargo build --release`.

## Impact on `docs/ARCHITECTURE.md`

None. This is an internal implementation change to an already-documented trust boundary (the
`SYSTEM_AGENT_TOKEN`-gated API surface) — no new component, route, protocol, or schema, and the
auth *decision* (which requests are allowed) is unchanged, only *how equality is computed*
internally. `docs/ARCHITECTURE.md` already describes token-based auth at the level of "a shared
secret gates the API"; it doesn't (and shouldn't) document comparison-algorithm internals.

## Rollout / migration notes

None required. This is a same-binary, same-version change with no wire-format, schema, or
config-shape impact — `SYSTEM_AGENT_TOKEN` is read and compared exactly as before from the
operator's point of view. No coordinated agent/hub version bump needed (this only affects
`system-agent`'s own inbound API auth, not the push protocol to `system-hub`). Safe to deploy
independently.
