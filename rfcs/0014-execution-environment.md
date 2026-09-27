# RFC 0014: Execution Environment — the Agent Monitors What It Runs In

- Status: Accepted
- Author: Claude (pairing with pietrangelomasalaMD)
- Date: 2026-09-27
- Affects: `system-agent` (collection, API, push loop); `system-hub` only in how its registry
  refreshes a system's memory capacity and how its dashboard labels it (§8). The push frame's shape is unchanged.
- **Owner's decision (2026-09-27):** an agent running in a container monitors *the container*
  (its workload and its cgroup limits), not the host it shares a kernel with. There is no
  host-scope mode and no operator switch.

## Motivation

The agent takes every value from `sysinfo` as the kernel presents it
(`src/collectors/system.rs::collect`), and nothing in the crate knows where it runs. That is
right on bare metal. In the two other places the agent ships to, the data source and the
meaning of a resource limit change.

**In a container** (the repo ships a `Dockerfile` and a `docker-compose.yml`), most of `/proc`
isn't confined by namespaces. Measured in `docker run --cpus=1.5 --memory=512m
debian:bookworm-slim` on the owner's machine: `nproc` says 16, `cpu.max` says
`150000 100000`, `memory.max` says `536870912`.

| Value today | What it reports | What the container's workload actually has |
|---|---|---|
| `cpu.usage_percent` | host-wide busy time over all 16 host CPUs (`/proc/stat`) | the cgroup's CPU time over its 1.5-CPU quota |
| `memory.total_bytes`, `usage_percent` | the host's RAM | the cgroup's 512 MiB limit and its working set |
| `swap.*` | the host's swap | the cgroup's swap limit |
| `uptime_seconds` | the host's boot time (`/proc/uptime`) | the container's age |
| `top_processes[].memory_percent` | relative to host RAM | relative to the memory limit |
| `load_average` | host-wide (not namespaced) | no per-container equivalent |

So a container at 100 % of its quota can report 9 % CPU, and one about to be OOM-killed can
report 3 % memory. Alert rules evaluated on those values never fire. The compose file and the
README (§ Docker, the "Caveat" paragraph) admit this, but the numbers aren't container-level
either: they're a mix of the two.

**On every environment, CPU % is a since-boot average today.** `collect()` builds a fresh
`System::new_all()` and calls `refresh_all()` right after it (`system.rs:23-24`). sysinfo
0.31 skips a CPU re-read less than 200 ms after the last one, and its first reading has no
previous counters, so `cpu.usage_percent` is busy time over *everything since boot*: a nearly
flat line that a real spike barely moves. Per-process `cpu_usage` is 0 on a first sample, so
`top_processes` is ordered by memory alone in practice. The fix this RFC needs for deltas (§6)
fixes this too, and changes CPU readings on *every* agent (see Rollout).

**In a virtual machine**, `/proc` describes the guest faithfully, and limits mean what they
say. What's missing is the one signal that only exists there: **steal time**, the CPU time the
hypervisor gave to someone else while the guest wanted to run. A VM whose CPU reads 40 % busy
but whose steal time is 30 % is starved, and today the agent can't say so.

Doing nothing leaves containerised agents reporting numbers that are wrong in the direction
that hides incidents, and every agent reporting a CPU figure that can't show one.

## Proposed design

### 1. Terms

- **Execution environment**: what the agent runs in, found once at startup: *bare metal*, a
  *virtual machine* (with its hypervisor when known), a *container* (with its runtime when
  known), or *undetermined*. It decides the source of CPU and memory readings.
- **Monitored cgroup**: in a container, the cgroup holding the whole workload: the root of the
  agent's cgroup namespace when that namespace is the container's own, else the agent's own
  cgroup (§4).
- **Resource capacity**: how much CPU (in CPUs, possibly fractional) and memory (in bytes) the
  monitored environment may use. Outside a container it's the kernel's view. In a container it
  is the smallest of the host's amount and every **resource limit** on the monitored cgroup
  and its visible ancestors.
- **Resource limit**: a cgroup's bound on one resource: *bounded* by an amount, or *unbounded*
  (`max`, or the controller's file absent because the controller isn't enabled there).
- **Reading source**: where a snapshot's readings came from: the *cgroup*, the *kernel*
  (sysinfo's host-wide view), or *unavailable* (a carry past its bound). Chosen per reading
  group, per snapshot.
- **Reading group**: readings sourced and carried together so their invariants hold: CPU,
  memory, swap.
- **Stale snapshot**: the latest snapshot, when it was read longer ago than the staleness
  bound (30 s) on a monotonic clock.
- **Steal time**: the share of CPU time the hypervisor withheld from a virtual machine between
  two readings.
- **Workload**: the processes inside the container that the agent monitors. The agent's own
  PID namespace already confines what it lists.

### 2. Module layout

The domain core is pure; the file reads live beside the other collectors.

```
src/environment/               domain (Host Telemetry), no I/O
  mod.rs        ExecutionEnvironment, Hypervisor, ContainerRuntime, LoadScope, classify
  evidence.rs   EnvironmentEvidence, ContainerMarker, CpuArchitecture, the evidence parsers and DMI table
  cgroup.rs     parsers (cpu.max, memory.max, cpuset, /proc/self/cgroup, mountinfo, memory.stat, cpu.stat)
                CgroupPath, MonitoredCgroup, ResourceLimit, CpuCapacity, MemoryCapacity, capacity rules
  usage.rs      CpuCounters, StealCounters, Percent, and the delta rules
  sourcing.rs   choose_readings: environment + raw readings + previous snapshot → snapshot values
src/collectors/
  environment.rs  the adapter: reads the files below, under a root path, into evidence and raw readings
  sampler.rs      Sampler: owns sysinfo::System and the previous counters; calls the adapter and the domain
```

```rust
pub enum ExecutionEnvironment {
    BareMetal,
    VirtualMachine { hypervisor: Hypervisor },
    Container { runtime: Option<ContainerRuntime>, cgroup: CgroupAccess },
    Undetermined,
}

pub enum Hypervisor { Kvm, Qemu, Vmware, HyperV, Wsl, Xen, VirtualBox, AmazonEc2, GoogleCompute, Other }
pub enum ContainerRuntime { Docker, Podman, Kubernetes, Lxc, SystemdNspawn }

/// Whether the agent found a cgroup v2 hierarchy it can read, at startup.
pub enum CgroupAccess {
    V2 { monitored: MonitoredCgroup },
    Unreadable(CgroupUnreadable), // V1Only, NoCgroupMount, SelfCgroupMissing, OutsideMountRoot
}

pub fn classify(evidence: &EnvironmentEvidence) -> ExecutionEnvironment;
```

`Hypervisor::Other` is a variant meaning "a hypervisor we don't name", not a sentinel: its
presence is evidence (the CPU flag), only its name is missing. Every rule that finds a virtual
machine finds a hypervisor, so it isn't optional: `Other` is the one spelling of "unnamed". All three enums are closed and
carry no free text, so no string read from the environment reaches the API.

`Container`'s `cgroup` field, and the `self_cgroup` / `cgroup2_mount` evidence it needs,
landed with step 5 (cgroup location): rules 1–4 below don't read them, so step 4 classified
without them and `/api/system`'s `environment` gained `cgroup` then. Whose processes the
agent lists is a `ProcessView` derived from the environment: `Host` outside a container,
`UnmeasuredContainer` with no readable v2 hierarchy, `Workload` when PID 1 is in the monitored
cgroup, `SharedNamespace` when it isn't.

### 3. Classification (pure, run once at startup)

`collectors/environment.rs` gathers `EnvironmentEvidence`, a plain value:

| Evidence | Source |
|---|---|
| `dockerenv` | `/.dockerenv` exists |
| `containerenv` | `/run/.containerenv` exists (Podman, Buildah) |
| `container_env_var` | the agent's own `container` env var, parsed into a closed enum (`podman`, `lxc`, `systemd-nspawn`, `docker`, `wsl`, other) |
| `systemd_container` | `/run/systemd/container`, parsed into the same enum. systemd publishes the manager's `$container` there because services start with a clean environment, so an agent running as a service under an init in LXC or nspawn has only this |
| `kubernetes` | `KUBERNETES_SERVICE_HOST` is set |
| `self_cgroup` | `/proc/self/cgroup` |
| `cgroup2_mount` | the cgroup2 line in `/proc/self/mountinfo`: mount point and mount root |
| `cpu_hypervisor_flag` | `hypervisor` in `/proc/cpuinfo`'s `flags` (x86 only) |
| `architecture` | compile-time `cfg!(target_arch)`: `X86` or `NonX86`, passed in as a value |
| `dmi_hypervisor` | `/sys/class/dmi/id/sys_vendor` and `product_name`, parsed at the edge into the hypervisor they name, if any. A vendor that also sells physical machines names one only with its virtual product: Microsoft with `Virtual Machine`, Amazon EC2 unless the product ends in `.metal`, Google with `Google Compute Engine` |
| `xen` | `/sys/hypervisor/type` reads `xen` |
| `wsl` | `/proc/sys/kernel/osrelease` contains `microsoft` (case-insensitive) |

The rules, in precedence order (first match wins):

1. Any **explicit container marker** (`dockerenv`, `containerenv`, `kubernetes`, or a
   `container_env_var` / `systemd_container` value other than `wsl`) → `Container`. The
   runtime is `Kubernetes` if that marker is set, else `Podman` for `containerenv` or
   `podman` in either value, else the first of `container_env_var` then `systemd_container`
   reading `lxc` / `systemd-nspawn`, else `Docker` for `dockerenv` or `docker`, else `None`.
2. `wsl` (osrelease, or `wsl` in either container value: systemd writes `wsl` to
   `/run/systemd/container` on WSL, measured on the owner's machine) → `VirtualMachine { Wsl }`. `xen`, the CPU flag, or a DMI vendor/product in the known
   table → `VirtualMachine`, naming the hypervisor from DMI when it matches, else `Xen` when
   `xen` is set, else `Other`.
3. `architecture` `X86` with no CPU flag and no virtual DMI → `BareMetal`.
4. Otherwise → `Undetermined` (ARM without DMI, for example).

**Only a marker makes a container.** Neither a cgroup limit nor an `overlay` root filesystem
is enough on its own. A systemd service with `MemoryMax=` runs in
`/system.slice/system-agent.service` with a real `memory.max`, and it is monitoring the host.
Raspberry Pi OS's overlay-root option and live-USB sessions mount `/` as overlay on bare
metal. Both classify from rules 2–4, so their limits are never applied. A containerd or CRI-O
container with no marker and no Kubernetes env classifies as whatever the host is and reports
as today, which is the safe direction. All three are table rows in the tests.

A container inside a VM classifies as `Container`: the container is what's monitored. Steal
time is still reported there (§5), because the workload is starved too.

The environment is logged once at `info`, e.g. `execution environment: container (podman),
cgroup v2, cpu capacity 1.5, memory capacity 512 MiB`.

### 4. The monitored cgroup and resource capacity

**Locating the agent's cgroup** (pure, at startup, `cgroup.rs`):

- `/proc/self/cgroup`'s `0::<path>` line gives the agent's path relative to its cgroup
  namespace root. mountinfo's cgroup2 line gives the mount point and the mount **root** (field
  4), the path of the mounted subtree in that same namespace.
- The agent's directory is the mount point joined with `<path>` *with the mount root stripped
  as a prefix*. If the root isn't a prefix of the path, or the path contains `..`, the result
  is `Unreadable(OutsideMountRoot)`. The usual case is root `/` and nothing to strip.

**Choosing the monitored cgroup** (pure):

- The cgroup namespace root (the mount point, when the mount root is `/`) holds the whole
  workload when the agent is in a **private cgroup namespace**, Docker's, Podman's,
  containerd's and CRI-O's default on v2. A private namespace's root is a non-root cgroup,
  and every non-root cgroup has a `cgroup.type` file whatever controllers are enabled; the
  host's root cgroup has none. So: when the namespace root has `cgroup.type`, it is the
  monitored cgroup. (`memory.current` would work too in the common cases, measured with
  `--cgroupns=private` and `=host`, but it vanishes when the memory controller isn't
  delegated.) That covers a container whose agent sits in a service cgroup under an init
  (LXC, nspawn, Podman with systemd) as well as one where the agent *is* PID 1.
- Otherwise (`--cgroupns=host`), the monitored cgroup is the agent's own. That is the
  container's cgroup when the agent is the container's only process tree (the shipped
  `Dockerfile`). An init plus `--cgroupns=host` would measure the agent's service only: a
  recorded known gap, not detected.

**Capacity** (pure over values read every tick, because limits change at runtime:
`docker update`, Kubernetes in-place resize, VM memory hotplug):

- The adapter reads each limit file from the monitored cgroup **and each visible ancestor up
  to the mount point**, at most 32 levels, because a parent's limit binds the child. Under a
  private namespace the monitored cgroup *is* the mount point, so there are no visible
  ancestors: the walk only matters under `--cgroupns=host`. Limits set on an ancestor the
  namespace hides (Kubernetes pod-level limits on the pod's cgroup, with no container-level
  limit) aren't seen and read as the host's amount: a recorded gap (§9).
- `cpu.max` → `ResourceLimit<CpuCount>`: `"max 100000"` is unbounded, `"150000 100000"` is
  1.5 CPUs. `cpuset.cpus.effective` (`"0-3,6"`) → a CPU count.
- CPU capacity = min(host online CPUs, every bounded quota, the cpuset count).
- `memory.max` and `memory.swap.max` → `ResourceLimit<Bytes>`. Memory capacity = min(host RAM,
  every bounded limit); swap likewise over host swap.
- **An absent limit file is `Unbounded`**: the cpu and cpuset controllers are often not
  delegated (rootless Podman under a systemd user session gets memory and pids only), and
  `memory.swap.max` is absent without swap accounting. Absence is the kernel saying "no limit
  here", not an error.
- **A present but malformed file is an error** (`CgroupReadError`), never a guess. So is a
  `memory.stat` missing beside a present `memory.current`, and an empty
  `cpuset.cpus.effective`: neither is how the kernel says "no limit".
- mountinfo is read for its first line whose filesystem type (after the ` - ` separator) is
  `cgroup2`, with the kernel's octal escapes decoded; a relative or climbing mount point, or
  a root that climbs, doesn't count. The namespace root is only ever monitored when the
  mount root is `/`: a subtree mount can't show it.

### 5. Usage from cumulative counters (pure deltas)

Container CPU usage and steal time are rates, so they need the previous reading.

```rust
pub struct CpuCounters { usage: Duration, read_at: Instant }          // cgroup cpu.stat usage_usec
pub struct StealCounters { steal: u64, total: u64 }                    // /proc/stat "cpu" line, jiffies

pub fn cpu_usage(prev: &CpuCounters, cur: &CpuCounters, capacity: CpuCount) -> Option<Percent>;
pub fn steal_share(prev: &StealCounters, cur: &StealCounters) -> Option<Percent>;
```

- `cpu_usage` = Δusage / (Δwall × capacity), clamped to `Percent`'s 0–100 (a `cpu.max.burst`
  can briefly exceed the quota; the clamp is recorded behaviour, not an accident). `None` when
  Δwall is under 500 ms, or a counter went backwards (a cgroup recreated).
- `steal_share` = Δsteal / Δtotal. `None` when Δtotal is zero, a counter went backwards, or the
  kernel's `cpu` line has no steal column. On bare metal it's a real `0.0`, not a sentinel.
- The clock is passed in: the sampler reads the monotonic clock and stamps the counters with
  that `Instant`, a plain value the domain compares and never reads.

### 6. One collector, one snapshot: the prerequisite this design forces

Deltas need one owner of "the previous reading". Today `system::collect()` runs from ten call
sites besides the background collector: `routes/api.rs` ×6 (`:72,76,80,88,92,106`),
`routes/sse.rs` ×2 (`:41,73`), `routes/ws.rs:51` and `push/mod.rs:167`. Each builds a fresh
`System::new_all()` (the since-boot CPU average of the Motivation) and each **blocks the Tokio
runtime** (a standing `CLAUDE.md` defect: sysinfo, `/etc/os-release`, the `lsb_release`
shell-out).

So the background collector becomes the only collector:

- `collectors/sampler.rs::Sampler` owns the environment, one long-lived `sysinfo::System`
  refreshed in place, and the previous `CpuCounters` and `StealCounters`. Its tick reads raw
  values through `collectors/environment.rs` and sysinfo, then calls the pure
  `environment::sourcing::choose_readings` (environment + raw readings + the previous
  snapshot's values → the snapshot's values and their reading sources). No sourcing rule lives
  in the sampler.
- Each tick runs the sampler in `spawn_blocking`, awaited, and publishes an
  `Arc<SystemSnapshot>` on a `tokio::sync::watch` channel held in `AppState`, the pattern
  RFC 0009's scrape round already uses (`AppState.rounds`). The push loop, which takes its
  receivers as parameters rather than `AppState` (`main.rs:163-189`), gets the snapshot
  receiver threaded in the same way as `rounds`.
- **Startup:** `main` takes a priming reading, waits 1 s, and collects the first snapshot from
  that interval (both off the runtime) before the listener is bound, so the first snapshot's
  CPU is measured, not a since-boot average, and no route ever sees "no snapshot yet". The
  collector's ticks then start at `interval_at(now + 2 s)`, never an immediate tick after the
  first snapshot.
- **Priming:** a `Sampler` never publishes from its own first reading. Building one (at
  startup, or after a panic) takes a priming reading of sysinfo, the cgroup counters and the
  steal counters, inside `spawn_blocking` like every reading, and its first published
  snapshot comes from a reading at least 1 s after the priming one. That's the sampler's own
  guarantee, not the interval's: the sampler keeps its priming `Instant` and refuses to
  publish from a reading taken less than 1 s after it (the tick then only refreshes its
  counters), and the collector calls `interval.reset()` after every (re)build so the next tick
  is a full period away. The interval also uses `MissedTickBehavior::Delay`, which still fires
  one tick at once after a slow tick but never a burst. So a rebuilt sampler never publishes
  sysinfo's since-boot CPU average, on any environment, whatever the timing of the panic.
- **Panic:** a panic in the tick surfaces as a `JoinError`. The collector logs it at `error`,
  builds a fresh (priming) `Sampler`, and keeps ticking; the published snapshot stays the
  previous one for that tick. A build that fails too is retried on the next tick, never in a
  loop, and logged on the transition only; a panic that repeats every tick ends in the
  staleness rule below.
- **Staleness:** a snapshot is **stale** when it was read more than **30 s** ago: a tick that
  hangs (the `lsb_release` shell-out has no timeout; `statvfs` on a dead NFS mount) or that
  keeps panicking. 30 s, not a few ticks, because the agent shares its container's CPU quota:
  under `--cpus=0.1` with a saturated workload, one tick can take seconds of wall time, and
  that container must show "CPU 100 %", not "offline". 30 s stays well inside the hub's 90 s
  push idle deadline. The age is measured on a monotonic clock: the published snapshot carries
  the `tokio::time::Instant` it was read at beside `collected_at` (tokio's, so tests move it
  with `advance()` under paused time), and staleness is a pure function of that age and the
  bound, so an NTP step or a VM resume can't fake or hide it. The collector doesn't
  abandon a hung tick, so blocking threads can't pile up; it logs at `error` once when the bound
  passes. While the snapshot is stale:
  - `/api/system` and its sub-routes answer `503 Service Unavailable` with the body
    `{"error": "stale snapshot"}` (no snapshot fields), so the hub's poller
    marks the system offline as it does on any non-success answer
    (`system-hub/src/collector/mod.rs:134`);
  - the push loop closes its connection and marks the reason "stale" for its caller; the
    reconnect loop in `main` then awaits, **before** `connect_async`, a snapshot that isn't
    stale by the monotonic rule (`watch::Receiver::wait_for(|s| !is_stale(s.read_at, now))`), so a wedged agent
    makes no handshake at all rather than a connect–auth–close storm. The hub marks the system
    offline on the disconnect;
  - the SSE streams (`/api/stream/system`, `/api/stream/processes`) and WS stay open, send one
    `stale` event, and send nothing more until a fresh snapshot, so a browser's `EventSource`
    never meets a 503 (which it treats as fatal); the agent dashboard shows the stale state
    from that event, and its `fetchJSON` (`static/index.html:862`) learns to treat a non-OK
    answer as "no data" instead of dereferencing it, so its disk, network and process
    refreshes survive a stale period;
  - `/api/alerts`, `/api/stream/alerts` and `/api/history/*` keep answering with the last
    evaluated state, frozen with the collector, as they are today when the background
    collector hangs.

  **What this costs, accepted:** on both hub paths the system's RFC 0009 application data
  stops for the stale period. On push, rounds travel on the same connection, so rounds
  published during the gap are lost and only the latest is resent on reconnect
  (`current_round`); on poll, the early return on 503 skips `store_agent_alerts` and
  `poll_applications`. That is what happens today when `collect()` hangs: `send_snapshot`
  blocks the push loop's `select!` (rounds and pings with it) and the poll times out before
  the same early return. Keeping applications flowing through a wedged system collector
  needs a hub-side staleness rule for snapshots, which is a hub design change for another
  RFC.
- The push frame's `timestamp` is the snapshot's `collected_at`.
- Every former call site reads `watch::Receiver::borrow().clone()`. A snapshot is at most one
  tick (2 s) old, which is what SSE and the push loop already deliver. The request routes
  (`/api/system*`) lose per-request freshness: that's accepted, it's what makes collection
  cost independent of request rate (API4).
- **Snapshot sequence:** each published snapshot carries a `SnapshotSeq`, a counter the
  collector (not the sampler) increments on every publish, so it's monotonic across rebuilds
  and blind to the wall clock. It stays in the domain snapshot and isn't on the wire.
- **Push cadence:** the push loop keeps its `PUSH_INTERVAL` tick, and skips a tick whose
  snapshot has the same `SnapshotSeq` as the last frame it sent, so the hub never stores the
  same snapshot twice. With both intervals at 2 s, drift occasionally skips a snapshot; that is
  accepted (the hub's resolution is the push interval, not the collector's).

**Unavailable readings.** Some values have no reading on some ticks: `cpu_usage` /
`steal_share` return `None`, or a cgroup file failed to read or parse. `choose_readings`
decides per **reading group**: CPU (usage, capacity), memory (total, used, free, available,
from `memory.current`, `memory.stat` and the limits) and swap. A group is sourced and carried
as one unit, so a fresh `memory.current` is never combined with a carried `used` and the
invariants below hold on every tick.

The decision is over (this tick's cgroup files × the group's **lineage**). A group's lineage
is `Kernel` until its first successful cgroup reading and `Cgroup` from then on, for the
**agent process's** life: a `Cgroup` group never goes back to the kernel's scale. Lineage, the
carry's start and "has this usage file ever been present" are held by the collector, not the
`Sampler`, and passed into `choose_readings` each tick, so a sampler rebuilt after a panic
inherits them. A usage file is **absent** only if it has never been present since the agent
started; one that disappears after being read (a recreated cgroup, a controller un-delegated
at runtime) counts as unreadable.

| This tick's files | Lineage `Kernel` | Lineage `Cgroup` |
|---|---|---|
| readable and well-formed, delta available | cgroup group, source `cgroup` → **Measured**; lineage becomes `Cgroup` | cgroup group, source `cgroup` → **Measured** |
| absent since start (controller not delegated) | kernel group, source `kernel` → **Measured** | — (can't happen: a `Cgroup` group had its files) |
| malformed, unreadable (EACCES, EIO, ENODEV, ENOENT after present), or a `None` delta | kernel group, source `kernel` → **Measured**; `warn` on the transition only | previous group, source `cgroup` → **Carried**; `warn` on the transition only |
| as the row above, the carry having lasted more than 30 s | — | carried group, source `unavailable` → **Unavailable**; `warn` once |

A group that recovers returns to **Measured**, source `cgroup`.

A tick with no cgroup reading at all counts as every usage file absent: quiet for a group
whose lineage is `Kernel` (outside a container, the only case), a failure for one whose
lineage is `Cgroup`. The CPU group also fails on a tick with no previous counters (`None`
delta); the sampler's priming reading takes counters, so a published reading always has them
unless the file was unreadable at priming.

The carry is bounded because a file that stays unreadable (a permission change) would
otherwise publish a frozen value as `cgroup` and hold an incident "active, quiet" forever. Past
the bound the agent says it doesn't know: `source: "unavailable"` on its API, and the incident
ends. The push frame has no absent form for a number, so the hub keeps receiving the carried
value and charts it flat until frame versioning can say more: a recorded gap.

A group that has been `cgroup`-sourced never switches to the host's scale on a failure,
which would clear a live incident with a false value and spike the hub's series.

**Alerts see a three-state reading.** `AlertManager::evaluate` takes a public `Readings`
value object whose CPU, memory and swap fields are `Reading<Percent>` and whose load fields
are `LoadAverage` (a newtype, not `Percent`: a load of 150 on a 128-core host is valid):

```rust
pub enum Reading<T> { Measured(T), Carried, Unavailable }
```

`choose_readings` returns this state per group on the domain snapshot; the §7 DTO drops it
(the wire has only `source`), and the collector passes it to `evaluate`. Load stays a plain
`LoadAverage`, not a `Reading`: it is always sysinfo's host-wide value, so `Carried` and
`Unavailable` arms would be states never produced. `RuleState::tick` matches it exhaustively: `Measured` advances the rule as today, `Carried`
**skips** it, `Unavailable` **clears** it (`Breach::Clear`, from `Pending` as from
`Active`: an active incident resolves, and a pending breach's `since` is dropped, so the
duration restarts from the next `Measured` breach) and keeps it idle until a `Measured`
returns. This retires the `#[allow(clippy::too_many_arguments)]` on `evaluate` (`src/alerts.rs:429`),
whose only reason to stay was the blocking collector this section removes. For a rule whose
metric is `Carried` on a tick:

- its `Breach` state is unchanged, including `since`: duration is wall-clock, so the skipped
  seconds count toward it, as they would have if the reading had been taken;
- nothing activates and nothing notifies on the `Carried` tick itself;
- an active incident stays in `active_alerts` with its last `current_value` (stored inside
  `Breach::Active`, reachable only through a `Measured` tick, since `ActiveAlert` is rebuilt
  from the tick's value today) and its message rebuilt from that value and the current
  duration, keeps its incident id
  (RFC 0004), and is reported as quiet: no notification on that tick;
- a cooldown that elapses during a skip notifies on the next evaluated tick, if the breach
  still holds.

How each value is sourced when its cgroup reading is available:

| Value | Bare metal / VM / undetermined | Container, cgroup v2 |
|---|---|---|
| CPU usage | sysinfo, now over a real 2 s interval | `cpu_usage` over CPU capacity |
| memory `total_bytes` | host RAM | memory capacity |
| memory `used_bytes` | sysinfo (total − available) | `memory.current` − `inactive_file` (the working set, saturating), capped at capacity |
| memory `free_bytes` | sysinfo | capacity − `memory.current`, saturating |
| memory `available_bytes` | sysinfo | capacity − used |
| swap total / used / free | sysinfo | swap capacity / `memory.swap.current` capped at capacity / the difference |
| `*_display` strings | from the bytes above | from the bytes above |
| uptime | `System::uptime()` | now − start time of PID 1 in the agent's PID namespace; `System::uptime()` if PID 1 isn't visible (`hidepid`) |
| process memory % | over host RAM | over memory capacity |
| steal time | `steal_share` | `steal_share` (the VM's, if any) |
| load average, `logical_cores` | unchanged | unchanged, host-wide (see below) |

Invariants: `used + available = total` in both columns (sysinfo defines used as total −
available). In the cgroup column also `free ≤ available ≤ total`, since the working set is at
most `memory.current`. Not in the kernel column: the kernel's `MemAvailable` subtracts reserved
pages and can sit below `MemFree` on a host with little cache. `used + free` isn't `total`
anywhere.

A container with `cgroup: Unreadable` uses the left column for everything but uptime.

**A container that shares the host's PID namespace** (`--pid=host`, or a pod with
`shareProcessNamespace`, where PID 1 isn't the container's) lists processes that aren't the
workload. The sampler reads `/proc/1/cgroup` at startup, with its own rule rather than the
`CgroupPath` parser (which refuses `..`): PID 1 is **inside** only when its path is the
monitored cgroup or under it, compared **segment by segment** in namespace-relative paths
(`/proc/1/cgroup` against the monitored cgroup's namespace-relative path: `/` when it's the
namespace root, the agent's own path from `/proc/self/cgroup` otherwise), so `/foobar` is not under `/foo`. A path starting with `..` (the kernel's rendering of a cgroup
outside the reader's namespace: `0::/../../..` for the host's init under `--pid=host`,
`/../<id>` for a pod's pause container), any other path, or an unreadable file means
**outside**. Outside, process memory % is over the kernel's memory and uptime is
`System::uptime()`, while CPU and memory stay the container's. That rule is part of
`choose_readings`, and a classification-time value, not a guess per tick.

**Load average stays host-wide, deliberately.** Linux has no per-cgroup load average; the
closest per-container signal is PSI (`cpu.pressure`), which is a different metric and a future
RFC's business. `logical_cores` keeps meaning the host kernel's CPU count, so the load alert's
"0 means one per core" baseline keeps comparing host load with host cores, which is the only
consistent pairing. The API says the load is host-wide (§7).

### 7. Agent API

`GET /api/system` gains fields, all additive:

```jsonc
{
  "collected_at": 1790000000,       // unix seconds, the agent's clock, when this snapshot was read
  "environment": {
    "kind": "container",            // bare_metal | virtual_machine | container | undetermined
    "runtime": "podman",            // container only; null when unknown
    "hypervisor": null,             // virtual_machine only, "other" when unnamed; else null
    "cgroup": "v2",                 // container only: "v2" | "unreadable"
    "load_scope": "host"            // "host" in a container, "environment" elsewhere
  },
  "cpu":    { "...": "...", "capacity_cpus": 1.5, "steal_percent": 0.0, "source": "cgroup" },  // steal: null when unmeasurable
  "memory": { "...": "...", "limit": { "bounded": 536870912 }, "source": "cgroup" },           // limit: container-v2 only, else absent
  "swap":   { "...": "...", "limit": "unbounded", "source": "kernel" }   // source: cgroup | kernel | unavailable
}
```

No existing field changes shape; some change **value** (§6, Rollout). `GET /api/system/cpu`
returns the `cpu` object and `GET /api/system/memory` returns `{ memory, swap }`
(`routes/api.rs:74-84`), so they gain the same fields. `/api/system/disk`, `/network` and
`/processes` gain nothing, but read the published snapshot like every other route.

SSE (`/api/stream/system`) and WS (`/api/ws/system`) build hand-picked `json!` objects
(`sse.rs:42-57`, `ws.rs:58-76`). They gain `cpu_capacity_cpus`, `cpu_steal_percent` and
`collected_at`: the live values a chart would plot. Both also send the `stale` event (§6). The static `environment` stays on
`/api/system` only. SSE's `timestamp` field, today `uptime_seconds`, is left as it is.

The `runtime` values are `docker`, `podman`, `kubernetes`, `lxc` and `systemd_nspawn`; the
`hypervisor` values are `kvm`, `qemu`, `vmware`, `hyperv`, `wsl`, `xen`, `virtualbox`,
`amazon_ec2`, `google_compute` and `other`. They are chosen on the wire DTO, not derived from
the domain's names, and a test pins each one.

The wire shape gets its own DTO (`models.rs`), converted from the domain values at the edge,
rather than serde derives on `ExecutionEnvironment`: the first step, for the types touched, of
the anti-corruption layer `docs/ARCHITECTURE.md` lists as missing.

The agent dashboard shows the environment in its header, via `textContent`, and labels its
cores figure "host cores" in a container, since it no longer matches the CPU % next to it.

### 8. Push frame and hub

**The frame's shape doesn't change.** Measured with `rmp-serde` 1.3.1 (the version both lock
files pin): a hub whose `PushPayload` has N fields **rejects a frame with N + k elements**
(`LengthMismatch`), and the hub drops frames that fail to decode. Appending the environment
would make a new agent invisible to every old hub. So no field is added, removed or moved.
Carrying the environment to the hub needs frame versioning (the handshake advertising a frame
version), which is its own RFC. Polled systems expose it in `/api/system`, which the hub's
poller already reads with unknown fields ignored.

**Some fields' values change.** In a container `cpu_percent`, `memory_*` and `uptime_*` carry
the workload's values; everywhere `cpu_percent` becomes a real interval reading; `timestamp`
becomes the snapshot's `collected_at` rather than the send time (seconds apart at most, same
type). `cpu_cores` stays the host's `logical_cores`: it's an integer, capacity is fractional,
and it pairs with the host-wide load average.

**The hub's registry would keep the host's memory forever.** The push path
(`system-hub/src/push/mod.rs::update_registry`) and the poll path
(`system-hub/src/collector/mod.rs`, `record_system_info`) write `cpu_cores` and
`total_memory_*` only while `hostname` or `os` is missing, so an existing row keeps the
host's RAM after the agent upgrade, and the hub dashboard shows it beside a memory % that is
now over the container's limit. The hub changes so that both paths **refresh
`total_memory_display` and `total_memory_bytes` on every frame and every poll** when they
differ from the stored ones; identity fields (hostname, OS, kernel, CPU model) keep their
fill-once rule. This also fixes the same staleness for VM memory hotplug. The refresh goes
through a new narrow `db` method with fixed column names (`update_memory_capacity`), not
`update_system_info`, which overwrites every column; it writes the display and the bytes together
or not at all, only when both are present, so a poll response without memory never nulls the
stored total and the two never disagree.

The hub dashboard labels that figure "Total RAM" (`system-hub/static/index.html:556`) and
"… RAM" (`:888-890`), which would read "512 MB RAM" for a container. Both become "Memory",
true on every environment. That's a text-only change to fixed labels, but `xss.mjs` asserts
the card's meta text (`xss.mjs:744`, `` `${P} RAM` ``), so that check changes with it, and the
changed check gets a `red-test-adversary` mutation-mode pass. It's a behaviour
change inside the hub only: no schema change, no frame change, and an old hub with a new
agent keeps the stale value it has today (the release notes say so).

**Stored history changes meaning at the upgrade, unmarked.** A containerised system's `cpu`
and `memory` series in the hub's `metrics` table switch from host values to workload values,
and every system's `cpu` series from a since-boot average to a real reading. Marking the
switch would need the environment on the hub (frame versioning). Accepted, and stated in the
release notes.

### 9. Out of scope

- A host-scope mode (agent in a privileged container reading the host's `/proc`): rejected by
  the owner.
- cgroup v1: detected and reported as `cgroup: "unreadable"` with the kernel values, not
  measured. cgroup v1 is legacy on every distribution the Dockerfile targets.
- A container with an init under `--cgroupns=host` (§4).
- Limits the cgroup namespace hides: Kubernetes pod-level limits with no container-level limit
  read as the host's amount (§4).
- `packages` / `services` / `containers` in a container: they already report the workload's
  own inventory (the image's packages) or nothing (no systemd, no socket). Making "not
  available here" explicit instead of an empty array changes three endpoints' shape: a
  follow-up.
- Disks in a container list bind mounts whose sizes are the host filesystem's. Unchanged,
  recorded in `docs/ARCHITECTURE.md`'s known gaps.
- The other collectors' blocking shell-outs (`packages`, `services`, `containers`, `ports`).
- PSI pressure metrics, per-process cgroup attribution, marking history discontinuities on
  the hub.

## Domain impact

- **Host Telemetry** gains `environment/` as its domain core, and `collectors/environment.rs`
  and `collectors/sampler.rs` as adapters. Its *Owns* column becomes "the snapshot of the
  agent's execution environment and its recent history".
- **Glossary**: adds *execution environment*, *monitored cgroup*, *resource capacity*,
  *resource limit*, *reading source*, *reading group*, *stale snapshot*, *steal time*,
  *workload*. Changes *snapshot* ("one
  point-in-time reading of a host" → "…of the agent's execution environment") and *metric
  readings* (percentages are over resource capacity, and a reading is *measured*, *carried*
  or *unavailable*). *System* is unchanged: to the hub, a
  containerised agent is a system.
- **Alerting**: `Readings` becomes public with a three-state `Reading<T>` per metric
  (`Measured`, `Carried` → the rule skips: state and incident kept, no notification;
  `Unavailable` → the rule clears and stays idle). Its inputs change meaning in a container: CPU/memory/swap rules fire
  against limits, which is the point.
- **Fleet Registry (hub)**: memory capacity becomes a live property of a system, refreshed on
  every frame and poll; identity stays fill-once.
- **Published contracts**: the push frame is unchanged in shape (§8). The poll response gains
  fields only. Mixed fleet:
  - *new agent + old hub*: identical frame shape; the hub stores the new values; its registry
    keeps the host's memory total (stale card, as today on hotplug).
  - *old agent + new hub*: the registry refresh stores the same host value; nothing visible.

## Alternatives considered

- **Do nothing.** Containerised agents keep under-reporting in the direction that hides
  incidents, and every agent's CPU stays a since-boot average.
- **sysinfo's `cgroup_limits()`.** It covers memory only, reads `/sys/fs/cgroup/memory.max`
  without resolving the agent's cgroup path or walking ancestors, and has no CPU quota or
  usage. Too little of the problem, and not testable against fixture trees.
- **Treat any cgroup limit, or an overlay root, as a container.** Wrong for systemd services
  with `MemoryMax=` and for overlay-root hosts (§3): they'd report the agent's own unit.
- **Read usage from the agent's own cgroup always.** Wrong whenever an init runs in the
  container: it measures the agent, not the workload (§4).
- **An operator switch (`SYSTEM_AGENT_ENVIRONMENT=…`).** Detection covers Docker, Podman,
  Kubernetes, LXC, nspawn and the common hypervisors. A switch would be a second source of
  truth, and an override that claims `container` on bare metal would apply a systemd unit's
  limits. Can be added later; `Undetermined` behaves as today, which is safe.
- **Keep ten collectors and share the previous counters behind a mutex.** Fixes the deltas
  but keeps ten blocking collections on the runtime, and deltas over whatever interval two
  unrelated requests happen to leave. The single collector removes both.
- **Fall back per snapshot when any cgroup file fails.** A missing swap-accounting file would
  cost the container its memory values too. Per value is no harder.
- **Publish `0` for an unavailable CPU reading.** A sentinel the alerting would read as
  "idle". Keeping the previous value and skipping the rule is honest about the gap.
- **Append the environment to the push frame.** Breaks every old hub (§8, measured).
- **Leave the hub's registry fill-once and tell operators to re-register.** A manual step per
  upgraded system for a value the hub receives every frame anyway.

## Security implications

**OWASP Top 10 (2021)**

- **A01 Broken Access Control**: `/api/system` and its sub-routes, SSE and WS keep their
  existing auth (`SYSTEM_AGENT_TOKEN` when set). The new fields go to the same audience that
  already sees the kernel version. The hub's registry refresh writes only values from an
  authenticated push or a registered system's poll, as today; a forged push (the standing
  self-asserted-id risk) could already set these values on a new row, and now can on an
  existing one: same trust, same audience, no new capability.
- **A02 Cryptographic Failures**: N/A, no secrets or crypto touched.
- **A03 Injection / XSS**: the new API fields are closed enums, numbers and a timestamp. No text
  read from DMI, env or cgroup files reaches the response (`Hypervisor::Other` carries no name;
  `container=` is parsed into a closed enum). The agent dashboard renders them with
  `textContent`. The hub dashboard changes two fixed labels only ("RAM" → "Memory"), no
  new data path; `total_memory_display` was already rendered from frames and polls. `xss.mjs`
  runs and must pass.
- **A04 Insecure Design**: every parse failure is a typed error with a documented per-value
  fallback, never a panic, never a guessed value. The container rule needs an explicit marker
  (§3). A `..` in `/proc/self/cgroup`, or a path outside the mount root, is refused, so the
  ancestor walk stays under the cgroup mount.
- **A05 Security Misconfiguration**: needs no new privilege, capability or bind mount; the
  `Dockerfile` and compose file stay unprivileged. The design explicitly doesn't recommend
  mounting host `/proc` or running privileged.
- **A06 Vulnerable Components**: no new dependency.
- **A07 Identification & Authentication Failures**: N/A.
- **A08 Software & Data Integrity Failures**: the push frame's shape is unchanged (§8), so no
  decode risk on the hub. `timestamp`'s meaning moves from send time to collection time, same
  type and clock.
- **A09 Logging & Monitoring Failures**: the environment is logged once at startup; cgroup read
  failures log on transition only (no log flooding); a collector panic logs at `error` and the
  collector recovers; a snapshot older than 30 s makes the agent answer 503 and drop its
  push connection, so a wedged collector shows as offline on the hub instead of as a healthy
  system with flat metrics. Nothing secret is logged.
- **A10 SSRF**: N/A, no outbound request added.

**OWASP API Security Top 10 (2023)**

- **API1 BOLA / API5 BFLA**: N/A, no new object or function; existing auth applies.
- **API2 Broken Authentication**: N/A, auth untouched; `tokens_match` stays constant-time.
- **API3 Broken Object Property Level Authorization**: the response gains properties (runtime,
  hypervisor, limits). Reconnaissance value is on par with the kernel version already served;
  accepted, stated here.
- **API4 Unrestricted Resource Consumption**: every file read is capped (64 KiB, 1 MiB for
  `mountinfo`, whose overlay lines are long), the ancestor walk is capped at 32 levels, and
  requests no longer trigger a collection at all (§6), so an API client can't make the agent
  run `sysinfo` or `lsb_release` per request any more. That's a net improvement.
- **API6**: N/A.
- **API7 SSRF**: N/A.
- **API8 Security Misconfiguration**: CORS untouched, not widened.
- **API9 Improper Inventory Management**: no new endpoint; the README rows for
  `/api/system`, `/api/system/cpu`, `/api/system/memory`, the SSE/WS stream fields and the
  Docker caveat are updated.
- **API10 Unsafe Consumption of APIs**: the agent consumes kernel pseudo-files, parsed strictly
  into typed values; the hub's consumption of the agent is unchanged in shape, and the
  registry refresh takes `total_memory_*` from the same decoded frame and poll it already
  trusts for metrics.

## Testing plan

Test-first per behaviour, table-driven, per `CLAUDE.md`.

- **`classify`** (pure): rows for Docker (`dockerenv`, `0::/`), rootless Podman
  (`containerenv`, `container=podman`), Kubernetes, LXC (`container=lxc`), nspawn, an
  **overlay root with a systemd service and no marker** (→ not `Container`), a **systemd
  service with `MemoryMax`** (→ `BareMetal`, never `Container`), WSL2 (the owner's machine: CPU
  flag, no DMI, `microsoft` osrelease), KVM, VMware, Hyper-V, Xen, an unknown hypervisor
  (→ `Other`), x86 bare metal, ARM with no evidence (→ `Undetermined`), a container inside a
  VM (→ `Container`), an agent service under an init with only `/run/systemd/container`
  reading `lxc` / `systemd-nspawn` (→ `Container`), and `/run/systemd/container` reading `wsl`
  (→ `VirtualMachine { Wsl }`, never `Container`).
- **Parsers** (pure): `cpu.max` (`max 100000`, `150000 100000`, zero period, garbage, empty),
  `memory.max` (`max`, a number, overflow, garbage), cpuset lists (`0-3,6`, `0`, empty,
  reversed range), `/proc/self/cgroup` (`0::/`, a nested path, v1-only lines, a `..` segment),
  mountinfo (cgroup2 mount point with root `/`, with a non-`/` root, missing), `memory.stat`
  `inactive_file`, `cpu.stat` `usage_usec`, the `/proc/stat` `cpu` line with and without steal.
- **Cgroup location** (pure): mount root `/`; a non-`/` mount root that prefixes the path
  (stripped); one that doesn't (→ `OutsideMountRoot`); the namespace root with `cgroup.type`
  (→ monitored) and without (→ the agent's own); **init in the container** (agent at
  `/system.slice/x.service`, namespace root monitored); a private namespace root with **no
  memory files** and an init (→ still the namespace root); PID 1 **inside** (`0::/` with the
  namespace root monitored — the shipped Dockerfile; a nested path under it; `/init.scope`
  under an init) and **outside** (`0::/../../..`, `0::/../abc`, `/foobar` against a monitored
  `/foo`, or unreadable → process % over the kernel's memory, kernel uptime).
- **Capacity rules** (pure): min across ancestors (child unbounded, parent bounded), quota vs
  cpuset vs host, memory limit above host RAM (→ host RAM), **absent `cpu.max` / cpuset /
  `memory.swap.max`** (→ unbounded).
- **Deltas** (pure): a normal interval, exactly 100 %, a burst over 100 % (→ clamped), Δwall
  under 500 ms, counters going backwards, bare-metal steal of 0.
- **`choose_readings`** (pure): each cell of §6's lineage table, including a `None` delta
  keeping the previous group; a malformed or unreadable file, and ENOENT after a present file,
  with lineage `Cgroup` (→ `Carried`, never the kernel's scale); a carry exactly at and one
  tick past 30 s (→ `Unavailable`), then another failing tick (still `Unavailable`, never
  `kernel`), then recovery (→ `Measured`, `cgroup`); a **rebuilt sampler** given lineage `Cgroup` and a
  running carry, with a failing file (→ `Carried`, carry bound not restarted, never `kernel`); memory `used + available = total` and, in the
  cgroup column, `free ≤ available ≤ total`, with a fixture whose `inactive_file` isn't 0 and
  a carried group after a lowered capacity; swap without accounting (→ kernel, per value, memory still cgroup); warn on the
  transition only in **both** lineages (the decision to warn is a returned value, so a test
  fails if every tick warns, including a group failing from the start with lineage `Kernel`).
- **Alerting** (table-driven over each metric): a rule given `Carried` keeps its
  `Breach` and `since`, and on that tick nothing activates and nothing notifies, even when the
  duration or cooldown has passed; the `Carried` tick's message states the grown duration;
  `Pending` → `Unavailable` → a `Measured` breach restarts the duration from that tick; an active incident stays in `active_alerts` with its last
  `current_value` and id, quiet; a cooldown that elapsed during the skip notifies on the next
  evaluated tick; a pending breach whose duration passed during the skip goes active on the
  next evaluated tick that still breaches; a load reading of 150 (→ not clamped); `Unavailable`
  resolves an active incident and stays idle over further `Unavailable` ticks; the next
  `Measured` breach mints a new incident id.
- **`collectors/environment.rs`** (adapter): reads from a `root: &Path`, so tests build fake
  `/proc` and `/sys/fs/cgroup` trees in a `tempfile` dir: the Docker tree measured above, a
  a `--cgroupns=host` tree with limits on an ancestor, an init-in-container tree, an unreadable `memory.max`, absent
  controller files, a missing cgroup mount.
- **`collect()` seam**: today `system.rs::collect` reads the real `/` and `/etc/os-release`
  with no seam; its helpers (`format_bytes`, `parse_os_release_content`, …) already have 13
  tests (`system.rs:256-346`). Commit 1 gives the sampler its root path and characterises what
  `collect` publishes from fixtures, attacked by `red-test-adversary` in mutation mode.
- **Single collector**: `/api/system` via `oneshot` serves the published snapshot and never
  collects (`AppState` gets a constructor taking a snapshot receiver; the 15 test call sites of
  `AppState::new()` use a fixture snapshot); `main`'s `serve` is split into
  `run(listener, …)` so a test can bind an ephemeral listener and assert the first snapshot is
  published before the first request is answered; a `JoinError` in a tick (an injected
  panicking sampler) logs, rebuilds, and the rebuilt sampler's first publish comes from a
  reading at least 1 s after its priming (a fake sysinfo source whose first reading is a
  since-boot value that must never be published), including a panic right after a slow tick,
  where `Delay` fires the next tick at once; an injected **hanging** sampler, blocked on a
  channel the test releases at the end so the runtime can shut down, makes the snapshot
  stale once the test `advance()`s paused time past 30 s (auto-advance is inhibited while a
  blocking task runs), after which `/api/system` answers 503 with the stale body, the push
  loop closes, SSE sends one `stale` event and stays open; a fake hub sees **no handshake**
  while the snapshot is stale and one after a fresh snapshot is published, also when that
  fresh snapshot's `collected_at` is *older* than the stale one's (a backward clock step); staleness as a pure function of a monotonic age (exactly at the bound,
  just past it); the collector's first tick is 2 s after startup, not immediate.
- **Push**: the agent gets a characterisation test in commit 1: `src/push/mod.rs`'s frame,
  built from a snapshot with distinct same-typed values, encodes to exactly the golden bytes,
  and stays green untouched through every later commit except for `timestamp`'s source. A test
  pins that a tick with an unchanged `SnapshotSeq` sends no frame, including after the wall
  clock steps backwards.
- **Hub registry**: push and poll each refresh `total_memory_*` when they differ and leave
  hostname, OS, kernel and CPU model untouched once set; a poll response with no memory, or
  with only one of display and bytes, leaves the stored total as it was (a `tempfile` SQLite,
  both paths).
- **Hub dashboard**: `xss.mjs:744`'s expected meta becomes `` `${P} Memory` ``; the gate runs
  `node system-hub/dashboard-tests/xss.mjs` and `red-test-adversary` attacks the changed
  check in mutation mode.
- **Manual check** (recorded in the change summary): `docker run --cpus=1.5 --memory=512m`
  with a CPU burner and an allocator; the agent's CPU and memory track `docker stats`, and a
  CPU alert rule at 80 % fires. The same with `podman run --cpus=1.5 --memory=512m`, rootful
  and rootless (which also checks where crun's systemd driver writes the limits). And
  `--cpus=0.1` with four burner threads: the agent reports CPU near 100 % and never goes
  stale. The agent on
  the host reports as `virtual_machine (wsl)`.

## Impact on `docs/ARCHITECTURE.md` and `README.md`

- `ARCHITECTURE.md` § Components (agent): the `environment/` domain module, the
  `collectors/environment.rs` and `collectors/sampler.rs` adapters; the single background
  collector and the snapshot `watch` channel; routes no longer collect.
- § Components (hub): the registry refreshes memory capacity on every frame and poll.
- § Data flow: collection happens once per tick off the runtime; routes, SSE, WS and push read
  the latest snapshot; the frame's `timestamp` is the collection time.
- § Domain model: the Host Telemetry and Fleet Registry rows; the nine new glossary terms and
  the two changed ones.
- § Open architectural questions: remove "collectors block the runtime" for `system.rs` (the
  other collectors remain) and the `too_many_arguments` exception on `evaluate`; add
  disks-in-containers, init-under-`--cgroupns=host`, "the hub doesn't know the environment
  until frame versioning", and the unmarked history discontinuity.
- `README.md`: the `/api/system`, `/api/system/cpu`, `/api/system/memory` rows (the endpoint
  table has no field lists today, so each row gains a one-line note on the new fields) and
  the SSE/WS descriptions; the Docker "Caveat" paragraph is rewritten: the agent reports the container,
  not the host, with no bind mount needed.
- `docker-compose.yml`: its fidelity comment is rewritten to match.

## Rollout / migration notes

- No schema change, no frame shape change, no deployment order.
- Release notes, three lines:
  - **Every agent:** CPU % becomes a real 2 s reading instead of a since-boot average. CPU
    alert rules that never breached may start firing, and hub CPU history steps at the upgrade.
  - **Containerised agents:** CPU, memory, swap and uptime report the container against its
    limits; alert rules fire against limits; hub history changes meaning at the upgrade.
  - **Hub:** a system's memory total follows what the agent reports. With an old hub, a
    containerised system's card keeps the host's memory total until the hub is upgraded.
- Implementation order, one commit each: (1) the `collect` seam, its characterisation tests
  and the golden push frame; (2) the single collector, the `watch` snapshot, `collected_at`
  and the push skip, pinning the CPU change on purpose; (3) `Readings` public with
  `Reading<Percent>` / `LoadAverage` and the skip and clear rules; (4) `environment/` classification and the `environment` field;
  (5) cgroup location, capacity and container sourcing; (6) steal time; (7) the hub registry
  refresh and the "Memory" labels; (8) docs, README and the agent dashboard.

## Review

`rfc-adversary` ran five passes. The first was on the whole draft; each later one covered
only the previous round's amendments, because every round changed the design. Every
CONFIRMED finding was acted on. PLAUSIBLE ones were decided as recorded.

**Pass 1** (whole draft):

| Finding | Verdict | Resolution |
|---|---|---|
| the single collector changes CPU % on every agent (since-boot average today) | CONFIRMED | stated in Motivation, §6, §8 and Rollout; own release-note line |
| usage read from the agent's leaf cgroup; overlay root counted as a container | CONFIRMED | *monitored cgroup* (§4); only explicit markers make a container (§3) |
| the hub registry keeps the host's memory total forever | CONFIRMED | hub refreshes memory capacity on every frame and poll (§8) |
| SSE/WS don't gain fields; wrong paths and README rows | CONFIRMED | §7 rewritten against the real routes |
| the single collector is a failure point with no staleness signal | CONFIRMED | panic handling, staleness rule (§6) |
| unavailable readings undefined; startup double tick | CONFIRMED | unavailable-readings rules; priming; `interval_at` (§6) |
| all-or-nothing fallback; absent controller files | PLAUSIBLE | adopted: absent limit = unbounded; fallback per reading group |
| the mountinfo root ignored | CONFIRMED | stripped as a prefix (§4) |
| free/available bytes undefined | CONFIRMED | defined per environment (§6) |
| sourcing rule in an adapter; `#[allow]` kept | CONFIRMED | pure `choose_readings`; adapters in `collectors/`; `#[allow]` retired |
| call sites, paths, testing claims | CONFIRMED | corrected |
| push cadence drift; `CgroupAccess` fixed at startup | PLAUSIBLE | push skips an unchanged snapshot; per-group `source` field |

**Pass 2:**

| Finding | Verdict | Resolution |
|---|---|---|
| a hung collector stays "online" on the hub | CONFIRMED | staleness bound → 503 and push close (§6) |
| a rebuilt sampler republishes the since-boot CPU average | CONFIRMED | every sampler primes before publishing |
| `used + free = total` untrue; skip semantics unspecified | CONFIRMED | kernel invariants; skip rules spelt out |
| LXC/nspawn under an init undetected | PLAUSIBLE | adopted: `/run/systemd/container` marker, `wsl` → VM |
| `memory.current` as the namespace-root discriminant | PLAUSIBLE | adopted: `cgroup.type` |
| transient malformed file flips to host scale | PLAUSIBLE | adopted: keep previous and skip |
| Kubernetes pod limits hidden by the namespace; Podman unverified | PLAUSIBLE | recorded gap (§9); Podman in the manual check |
| `--pid=host` mixes scopes | PLAUSIBLE | adopted: `/proc/1/cgroup` rule |
| hub "RAM" label; poll refresh could write NULL | PLAUSIBLE | adopted: "Memory" labels; narrow `update_memory_capacity` |

**Pass 3:**

| Finding | Verdict | Resolution |
|---|---|---|
| staleness stops application frames and poll sync | CONFIRMED | accepted cost, equivalent to today's hung `collect()` (verified by pass 4) |
| SSE 503 is fatal to `EventSource` | CONFIRMED | SSE/WS stay open and send a `stale` event |
| staleness on the wall clock | CONFIRMED | monotonic `tokio::time::Instant` |
| missed ticks burst under default tokio behaviour | CONFIRMED | `MissedTickBehavior::Delay` (completed in pass 4) |
| load average forced into `Percent` | CONFIRMED | `LoadAverage` newtype |
| carry unbounded; no read-error row | CONFIRMED | 30 s carry bound → `unavailable`; read errors = malformed |
| per-value carry breaks memory invariants | CONFIRMED | reading groups carried as one unit |
| `free ≤ available` claimed for the kernel | CONFIRMED | claimed for the cgroup column only |
| `/proc/1/cgroup` paths with `..` refused by the parser | CONFIRMED | its own rule: `..`, other or unreadable → outside |
| relabel breaks `xss.mjs:744` | CONFIRMED | check updated, mutation-mode pass planned |
| 6 s bound fires on a throttled container | PLAUSIBLE | adopted: 30 s; manual check at `--cpus=0.1` |
| alerts/history routes during staleness | PLAUSIBLE | one line per route |

**Pass 4:**

| Finding | Verdict | Resolution |
|---|---|---|
| `Option` can't express "clear" | CONFIRMED | `Reading<T> { Measured, Carried, Unavailable }` |
| a `cgroup` group can drop to the host's scale | CONFIRMED | lineage table; ENOENT after present = unreadable |
| `Delay` still fires one immediate tick | CONFIRMED | sampler's own 1 s guard plus `interval.reset()` |
| paused-time staleness test can't pass | CONFIRMED | tokio `Instant`, explicit `advance()`, releasable hang |
| `/proc/1/cgroup` has no "inside" rows | CONFIRMED | inside rows; segment-wise comparison |
| reconnect storm while stale | CONFIRMED | wait for a fresh snapshot before `connect_async`; fake-hub test |
| 503 body and the agent dashboard | PLAUSIBLE | adopted: fixed body; `fetchJSON` checks `ok` |
| glossary for readings | PLAUSIBLE | adopted |

**Pass 5:**

| Finding | Verdict | Resolution |
|---|---|---|
| lineage lost on a sampler rebuild | CONFIRMED | lineage, carry start and file history held by the collector |
| push wait and skip keyed on wall-clock `collected_at` | CONFIRMED | monotonic `SnapshotSeq` and the monotonic stale rule |
| Kernel-lineage warnings every tick | CONFIRMED | transition only in both lineages; test row |
| alerting rows missing (Pending → Unavailable, message duration, nothing on a Carried tick) | CONFIRMED | rows added; `Unavailable` clears `Pending` too |
| implementation order still said `Option` | CONFIRMED | corrected |
| where `Carried` comes from; `current_value` placement | PLAUSIBLE | adopted: per-group state on the domain snapshot; value in `Breach::Active` |
| a rebuild that panics too | PLAUSIBLE | adopted: retried next tick, logged on transition |
| `Reading<LoadAverage>` never Carried | PLAUSIBLE | adopted: plain `LoadAverage` |
| PID 1 comparison wording | PLAUSIBLE | adopted: the monitored cgroup's namespace-relative path |

Came closest and survived all five passes: the unchanged push frame shape against a
mixed-version fleet (`LengthMismatch` measured), and the equivalence between the staleness
rule's costs and today's behaviour when `collect()` hangs.

Pass 5's fixes change no design decision: they move state the design already has to the
collector, change a comparison key and add test rows. So, per `CLAUDE.md`, there is no sixth
pass, and the RFC is Accepted on the owner's go-ahead (2026-09-27).
