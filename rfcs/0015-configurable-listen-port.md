# RFC 0015: Configurable Listen Address for the Agent and the Hub

- Status: Implemented
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-27
- Affects: both. `system-agent` gains `SYSTEM_AGENT_LISTEN` and `system-hub` gains
  `HUB_LISTEN`. There are no endpoint, frame or schema changes.
- **Owner's decision (2026-09-27):** the owner asked for a different port on both binaries.
  RFC 0012 (Draft) already specifies `HUB_LISTEN` as a full bind address, so both binaries
  take a full address rather than a port-only variable. This keeps one variable per binary
  for the listen socket, lets a service bind to loopback, and lets the tests use port 0.
  This RFC ships 0012's `HUB_LISTEN` bullet early, and 0012 is amended to cite it.

## Motivation

The agent always binds `0.0.0.0:9090` (`src/main.rs::LISTEN`), and the hub always binds
`0.0.0.0:9091` (`system-hub/src/main.rs::run`). When another process already holds the port,
the binary can't start, and the only way out is to rebuild it. On the owner's machine 9090 is
already taken, so a local agent can't run there at all. A container can remap the port, but a
bare-metal install, a systemd unit or a second agent on the same host can't. The hub's
real-binary test (`system-hub/tests/fail_closed.rs`) also has to bind the real 9091, and fails
whenever something else holds it. RFC 0012 recorded that as a CONFIRMED finding.

## Proposed design

### Variables

| Variable | Crate | Default | Meaning |
|---|---|---|---|
| `SYSTEM_AGENT_LISTEN` | agent | `0.0.0.0:9090` | The address and port that the agent's API, streams and dashboard are served on |
| `HUB_LISTEN` | hub | `0.0.0.0:9091` | The address and port that the hub's API, push endpoint and dashboard are served on |

To change only the port, set e.g. `SYSTEM_AGENT_LISTEN=0.0.0.0:9190`. To keep a service off
external interfaces, set e.g. `127.0.0.1:9090`. IPv6 addresses take brackets: `[::]:9090`.

### Parsing: one newtype per crate, at the boundary

The two crates aren't a workspace and share no code, so each crate declares its own small type
in its own module: `src/listen.rs` for the agent, `system-hub/src/listen.rs` for the hub. The
hub's `main.rs` is already 376 lines. Each crate parses the variable once, before anything
binds or starts:

```rust
/// Where the process serves: an IP address and a port. Port 0 asks the OS for a free port,
/// and the startup line logs the one bound.
pub struct ListenAddress(SocketAddr);

pub enum ListenAddressError {
    /// Set, but not `<IPv4>:<port>` or `[<IPv6>]:<port>`.
    Invalid,
    /// Set, but not valid UTF-8.
    NotUnicode,
}

impl ListenAddress {
    pub const VARIABLE: &str;            // "SYSTEM_AGENT_LISTEN" / "HUB_LISTEN"
    pub const DEFAULT: ListenAddress;    // 0.0.0.0:9090 / 0.0.0.0:9091
    pub fn from_env(value: Result<String, std::env::VarError>) -> Result<Self, ListenAddressError>;
    pub fn get(self) -> SocketAddr;
}
```

`from_env` takes what `std::env::var` returns, so tests pass plain values and never touch the
process environment:

- `Err(NotPresent)` or `Ok("")` → the default. An empty value means the default, as it already
  does for `HUB_STATIC_DIR`.
- `Ok(s)` where `SocketAddr::from_str(s)` succeeds → that address. That parser takes only
  literal IP addresses with a port, so a host name is `Invalid` and no DNS lookup ever
  happens at startup. It also rejects surrounding whitespace, a missing port, a port over
  65535 and an IPv6 address without brackets.
- Anything else → `Invalid`. `Err(NotUnicode)` → `NotUnicode`.
- Port 0 is accepted: the OS picks a free port, and the startup line logs the address actually
  bound (`TcpListener::local_addr`), so the port can still be found. The real-binary tests
  use it (see Testing plan).

A refusal stops the process before it binds, opens the database, starts a task or makes a
connection. The message names the variable and the expected form, never the raw value. The
raw string is untrusted and could carry control characters into the log. Once parsed, the
address isn't secret, so the startup line and `StartupError::Bind` keep printing it. The
agent's `StartupError` doc comment ("never with a configuration value") changes to say
exactly this: never a raw or secret configuration value.

- **Agent:** `start()` parses the address next to `ApplicationsConfig`, before the runtime
  exists. A refusal is a new `StartupError::Listen`, whose exit code is 78 (`EX_CONFIG`) like
  every other refused configuration. It gets its own row in `main.rs`'s
  `only_a_refused_configuration_exits_with_ex_config` table.
- **Hub:** `run()` parses it first, next to `HUB_PUSH_TOKEN` and before `HUB_STATIC_DIR` is
  checked or `system-hub.db` is opened. A refusal is a new `StartupError::Listen`, and the hub
  exits non-zero, as it does for every startup error.

### Binding and logs

Both binaries bind the parsed address. The startup lines print the bound address from
`local_addr()`, not the configured one, so port 0 shows the real port. Both log
`listening on http://<bound>` at INFO. The URLs they suggest (the agent's dashboard line, and
the hub's dashboard and push lines, which hard-code `9091` today) follow one rule, a pure
function of the bound address that each crate has: an unspecified IP (`0.0.0.0`, `[::]`) shows
as `localhost`, and any other IP shows as bound. So `192.168.1.5:9190` prints
`http://192.168.1.5:9190/`, never a `localhost` that doesn't answer.

Port 0 is meant for tests and one-off runs. Its port can only be found in the INFO line, which
a `RUST_LOG` above `info` hides. The README says so.

**IPv4 and IPv6:** `0.0.0.0` and `127.0.0.1` serve IPv4 only. `tokio` doesn't set
`IPV6_V6ONLY`, so `[::]` is dual-stack on Linux by default (`net.ipv6.bindv6only=0`), and
IPv6-only on the BSDs and Windows. `[::1]` is IPv6 loopback only: an agent whose `PUSH_TO`
says `127.0.0.1` can't reach it. The README states all three.

### Container images and compose

`EXPOSE` in both Dockerfiles stays at the default, since it only documents a port. The
compose file keeps the defaults. The README says that a changed port must also change the port
mapping, `PUSH_TO` for agents pushing to that hub, and the URL a polled agent is registered
with. It also warns that an agent bound to loopback can't be polled by a hub on another host.

### RFC 0012

§1's `HUB_LISTEN` bullet is amended in the same change to say it shipped with this RFC, with
the same default, the same fail-closed parse and the same port-0 test mechanism. 0012 keeps
its own additions, which this RFC doesn't implement: the peer address in admin-route logs
(`ConnectInfo`).

## Domain impact

No bounded context changes. The listen address is process wiring, like `HUB_STATIC_DIR`, and
no glossary term is added. Neither published contract changes: the push frame and the poll
responses keep their shapes. In a mixed-version fleet, a new agent or hub on its default
address can't be told apart from an old one. A hub on a new port needs its agents' `PUSH_TO`
updated, and an agent on a new port needs its hub registration updated. Both are operator
configuration, not something the two sides negotiate.

## Alternatives considered

- **Do nothing:** a taken port then means rebuilding the binary. Rejected.
- **A port-only variable (`SYSTEM_AGENT_PORT`, `HUB_PORT`):** this was the first draft, and it
  is simpler to type. It collides with 0012's `HUB_LISTEN`: the hub would end up with two
  variables setting the same port, and an undefined precedence between them. It also can't
  bind to loopback. Without port 0, the tests would have to probe for a free port, drop it and
  hand it to the child, which leaves a window in which another socket can take it. The owner
  chose the full address.
- **Accepting host names (`localhost:9090`):** this would need a DNS lookup at startup, and a
  name can resolve to several addresses. A literal IP is unambiguous. Rejected.
- **A `--listen` command-line flag:** neither binary parses arguments today. Every other
  setting is an environment variable, and containers and systemd units already pass those.
  Rejected for consistency.
- **The conventional `PORT` variable:** too generic. The agent and the hub often share a host
  or a compose environment, and some platforms set `PORT` for their own reasons. Rejected.

## Security implications

- **A01 / API1 / API5 (access control):** the auth middleware and the routes don't depend on
  the address, so nothing changes. Binding to loopback is now possible, which narrows who can
  reach an unauthenticated agent. It supplements a token and doesn't replace one.
- **A02 (cryptography):** N/A.
- **A03 (injection):** the value becomes a `SocketAddr` before use. The raw string never
  reaches a log line or a response, and the parsed address is printed through `Display`.
- **A04 (insecure design):** a refused value stops startup instead of falling back to the
  default. Otherwise a typo would silently serve on `0.0.0.0`, possibly where a firewall rule
  expects nothing.
- **A05 / API8 (misconfiguration):** ports below 1024 are allowed and the OS decides. Without
  `CAP_NET_BIND_SERVICE` the bind fails, and the existing `Bind` error reports it (exit 1).
  CORS isn't touched: it stays wide open, as recorded in `CLAUDE.md`, and isn't widened.
- **A06 (components):** no dependency change.
- **A07 / API2 (authentication):** N/A. Tokens are unchanged, and the address is not a secret.
- **A08 (integrity):** N/A.
- **A09 (logging):** the bound address is logged at startup. A refusal is logged once, and it
  names the variable, not the value.
- **A10 / API7 (SSRF):** N/A. The hub's poller fetches registered URLs as before, whatever
  address the hub listens on.
- **API3, API4, API6:** N/A. There is no body, no new resource and no business flow.
- **API9 (inventory):** the README's configuration tables gain both variables, and the API
  headings say "default port".
- **API10 (unsafe consumption):** N/A.

## Testing plan

Test-first and table-driven, in each crate:

- **`ListenAddress::from_env`** (unit, pure):
  - Unset or empty → the default.
  - Each of these → that address: `0.0.0.0:9190`, `127.0.0.1:9090`, `[::1]:9091`,
    `[::]:9090`, `127.0.0.1:0`, `0.0.0.0:65535`.
  - Each of these → `Invalid`: `9090` (no address), `localhost:9090`, `127.0.0.1`,
    `127.0.0.1:65536`, `::1:9090`, `" 127.0.0.1:9090"`, `"127.0.0.1:9090 "`, `127.0.0.1:+80`,
    `abc`.
  - `127.0.0.1:09090` → port 9090 (pinned: leading zeros can't name another port).
  - A `NotUnicode` value → `NotUnicode`.
- **The suggested URL** (unit, pure): `0.0.0.0:9090` and `[::]:9090` → `localhost:9090`;
  `127.0.0.1:9090`, `192.168.1.5:9190` and `[::1]:9091` → as bound.
  - The refusal's `Display` names the variable and never the value: a marker value is asserted
    absent.
- **Agent exit code:** a `StartupError::Listen` row, exiting 78, in
  `only_a_refused_configuration_exits_with_ex_config`.
- **Agent end to end** (`tests/startup.rs`):
  - With `SYSTEM_AGENT_LISTEN=127.0.0.1:0`, the agent logs `listening on http://127.0.0.1:<p>`
    with a non-zero `<p>`, and answers `GET /api/health` on it.
  - With an invalid value (a marker string) and `PUSH_TO` pointing at the counting listener, it
    exits 78 and logs "refusing to start" naming `SYSTEM_AGENT_LISTEN`. It prints neither the
    marker, `STARTED` nor "listening", and the counting listener sees 0 connections. This is
    the same bar as the `SPRING_BOOT_*` refusal test, so a parse moved after the collector or
    the push client fails.
- **Hub end to end** (`system-hub/tests/fail_closed.rs`):
  - `run_hub_with` also removes `HUB_LISTEN`, so a developer's shell can't decide a case.
  - The full-router test runs the hub with `HUB_LISTEN=127.0.0.1:0`, reads the bound port from
    the startup line on its stdout, and no longer needs 9091 free. Once the line is read, a task
    keeps draining stdout, so a full pipe can never block the hub's logging.
  - New: an invalid `HUB_LISTEN` (a marker string) makes the hub exit non-zero, naming the
    variable and not the marker, before `system-hub.db` exists in its working directory.

## Impact on `docs/ARCHITECTURE.md` and `README.md`

- `docs/ARCHITECTURE.md`:
  - The agent startup paragraph (the `SPRING_BOOT_APPS` bullet): exit 78 now also covers
    `SYSTEM_AGENT_LISTEN`, parsed before the runtime.
  - The hub's `HUB_PUSH_TOKEN` startup text in the trust-boundary section: `HUB_LISTEN` is
    parsed with it, before any file exists.
  - The Testing section: what `tests/` and `system-hub/tests/` now assert.
  - The SVG diagrams (`docs/images/architecture.svg`, `data-flow.svg`) label 9090 and 9091.
    Those are the defaults, and they stay.
- `README.md`:
  - The configuration tables gain `SYSTEM_AGENT_LISTEN` and `HUB_LISTEN`.
  - The paragraph after the agent table ("no other failure uses that code") covers the listen
    variable too.
  - The `PUSH_TO` row notes that its port is the hub's `HUB_LISTEN` port.
  - The API headings become "System Agent (default port 9090)" and "System Hub (default port
    9091)".
  - The TLS-termination example sets `HUB_LISTEN=127.0.0.1:9091`, so the proxy can't be
    bypassed on the plain port, and says the same for a polled agent behind a proxy.
  - The variable rows note that `[::]` is dual-stack on Linux, and that port 0 needs INFO
    logs.

## Rollout / migration notes

There is nothing to migrate. Unset variables keep today's addresses, so existing deployments
don't change. An old binary ignores the new variables and keeps its fixed port.

## Review

`rfc-adversary`, first pass (port-only draft):

| Finding | Verdict | Resolution |
|---|---|---|
| collides with 0012's `HUB_LISTEN`, and rejects the port 0 that 0012's tests rely on | CONFIRMED | owner's decision: full address in both binaries, port 0 accepted, 0012 amended |
| no test fails if the agent parses late (after the collector and push client start) | CONFIRMED | the invalid-value case runs with `PUSH_TO` against the counting listener: 0 connections, no `STARTED`, no "listening"; named 78 row |
| the ARCHITECTURE impact list named text that doesn't exist | CONFIRMED | replaced with the four real places; the SVGs keep the defaults |
| README inventory incomplete (exit-78 prose, headings, `PUSH_TO`) | CONFIRMED | listed above |
| "never with a configuration value" is already false for `Bind` | CONFIRMED | rule restated: never a raw or secret value; the parsed address is logged |
| free-port probing races with other sockets | PLAUSIBLE | moot: tests bind port 0 and read the bound port from the log |
| `run_hub_with` inherits `HUB_LISTEN` | CONFIRMED | removed there |
| where the type lives | note | `src/listen.rs`, `system-hub/src/listen.rs` |

`rfc-adversary`, second pass (full-address design, amendments only). No blocker: every
`SocketAddr` row in the test table was checked against a compiled program and held, and the
agent's existing tests don't depend on a free 9090.

| Finding | Verdict | Resolution |
|---|---|---|
| 0012 amended in one bullet only: its A05, API9, test plan and ARCHITECTURE impact still claimed `HUB_LISTEN` | CONFIRMED | all five places now point at 0015 |
| the README's TLS example leaves the plain port open on every interface | CONFIRMED | the example binds `127.0.0.1:9091` |
| the agent prints `localhost` for any bound IP, the hub the bound address | CONFIRMED | one rule for both: unspecified → `localhost`, else as bound; unit-tested |
| `[::]` dual-stack behaviour undocumented | PLAUSIBLE | adopted: stated in the RFC and the README |
| the hub test could block on an undrained stdout; `RUST_LOG` can hide port 0's port | PLAUSIBLE | adopted: stdout drained after reading; port 0 documented as for tests, needing INFO |

`red-test-adversary` ran three passes on the red tests. The first found DECORATION in the
wiring tests: a single `127.0.0.1:0` row, a late parse, no agent non-UTF-8 case, one constant
refusal message, and a string-replace `reachable_at`. The second found DECORATION in the
suggested URLs, which no row with an unspecified IP checked. The third found every test
EVIDENCE once `0.0.0.0:0` and `[::]:0` rows were added. One mutant is out of a test's reach
and is a review rule instead: the agent parsing `SYSTEM_AGENT_LISTEN` at the top of `run`,
inside the runtime but before its first log line.
