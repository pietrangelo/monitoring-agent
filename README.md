<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<!-- Copyright (C) 2026 Pietrangelo Masala -->

# System Agent & Hub — Distributed Linux Monitoring

A two-component monitoring stack for Linux servers. **System Agent** runs on every machine and exposes system metrics. **System Hub** aggregates data from many agents into a central dashboard with persistent storage and alerting.

---

## Architecture

```
 ┌──────────────────────────────────────────────────────────────────┐
 │                        SYSTEM HUB  :9091                         │
 │                                                                  │
 │  ┌──────────┐  ┌──────────────┐  ┌───────────┐  ┌────────────┐  │
 │  │ REST API │  │ Push Receiver│  │ Collector │  │ SQLite DB  │  │
 │  │ /api/*   │  │ WS /api/push │  │ (poller)  │  │            │  │
 │  └────┬─────┘  └──────┬───────┘  └─────┬─────┘  └─────┬──────┘  │
 │       │               │               │              │          │
 │       │         ┌─────┴─────┐   ┌─────┴─────┐  ┌─────┴─────┐    │
 │       │         │ Live      │   │ Metrics   │  │ Alerts    │    │
 │       │         │ Metrics   │   │ History   │  │ History   │    │
 │       │         │ Cache     │   │ (TS)      │  │           │    │
 │       │         └───────────┘   └───────────┘  └───────────┘    │
 │       │                                                          │
 │  ┌────┴──────────────────────────────────────────────────────┐   │
 │  │  SSE Stream  (/api/stream/summary)                        │   │
 │  │  Web Dashboard (static/index.html)                        │   │
 │  └───────────────────────────────────────────────────────────┘   │
 └──────────────────────┬──────────────────────┬────────────────────┘
                        │                      │
        ┌───────────────┘                      └───────────────┐
        ▼ (HTTP poll)                           ▼ (WS push)    ▼
 ┌──────────────┐                        ┌──────────────┐
 │ SYSTEM AGENT │                        │ SYSTEM AGENT │
 │    :9090     │                        │    :9090     │
 │              │                        │              │
 │ ┌──────────┐ │                        │ ┌──────────┐ │
 │ │ REST API │ │◄── Dashboard (local)   │ │ REST API │ │
 │ │ /api/*   │ │                        │ │ /api/*   │ │
 │ ├──────────┤ │                        │ ├──────────┤ │
 │ │ Alerts   │ │                        │ │ Alerts   │ │
 │ │ Engine   │ │                        │ │ Engine   │ │
 │ ├──────────┤ │                        │ ├──────────┤ │
 │ │ SSE/WS   │ │                        │ │ SSE/WS   │ │
 │ │ Streams  │ │                        │ │ Streams  │ │
 │ └──────────┘ │                        │ ├──────────┤ │
 │              │                        │ │Push Client│─┼──▶ Hub
 └──────────────┘                        │ └──────────┘ │
                                         └──────────────┘
```

**Two connection modes** between agent and hub:

| Mode | Direction | Protocol | Use case |
|---|---|---|---|
| **HTTP Poll** | Hub → Agent | REST + JSON | Agent behind firewall, hub can reach it |
| **Push** | Agent → Hub | WebSocket + MessagePack | Agent can connect out, lower latency, binary efficient |

Agents auto-register on the hub on first connection (push mode) or when manually added via the dashboard (poll mode).

---

## Project structure

```
monitoring-agent/
├── Cargo.toml                  # System Agent
├── static/index.html           # Single-machine dashboard
└── src/
    ├── main.rs                 # Server entry, push client spawn
    ├── models.rs               # Data types
    ├── state.rs                # Shared state (history ring buffers)
    ├── auth.rs                 # Token auth middleware
    ├── alerts.rs               # Alert engine (thresholds, cooldowns)
    ├── push.rs                 # Push client → connects to Hub
    ├── collectors/
    │   ├── mod.rs              # Background metric collector
    │   ├── system.rs           # CPU, memory, disk, network, processes
    │   ├── packages.rs         # dpkg/rpm/pacman/apk
    │   ├── services.rs         # systemd units
    │   ├── containers.rs       # Docker containers
    │   └── ports.rs            # TCP listening ports
    └── routes/
        ├── mod.rs
        ├── api.rs              # REST endpoints
        ├── sse.rs              # Server-Sent Events streams
        └── ws.rs               # WebSocket endpoint

monitoring-agent/system-hub/
├── Cargo.toml                  # System Hub
├── static/index.html           # Multi-system hub dashboard
├── system-hub.db               # SQLite (auto-created)
└── src/
    ├── main.rs                 # Server entry
    ├── models.rs               # Data types
    ├── state.rs                # Shared state + live metrics cache
    ├── db.rs                   # SQLite: migrations, CRUD, queries
    ├── collector.rs            # HTTP poller (pulls agent /api/system)
    ├── push.rs                 # Push receiver (accepts agent WS connections)
    └── routes/
        ├── mod.rs
        ├── api.rs              # REST endpoints
        └── sse.rs              # SSE stream with live metrics
```

---

## Building

### Prerequisites

- **Rust** 1.85+ (edition 2024)
- **Linux** (agent uses `/proc`, `/sys`, `dpkg`, `rpm`, `systemctl`, `ss`, `docker`)

### Build both components

```sh
# Agent
cd monitoring-agent
cargo build --release
# → target/release/system-agent

# Hub
cd monitoring-agent/system-hub
cargo build --release
# → target/release/system-hub
```

---

## Running

### 1. Start the System Hub (central server)

```sh
cd monitoring-agent/system-hub

# Basic — no auth, HTTP only
./target/release/system-hub
# → Dashboard: http://localhost:9091/
# → API:       http://localhost:9091/api/health
# → Push:      ws://localhost:9091/api/push

# With push authentication token
HUB_PUSH_TOKEN=my-secret-key ./target/release/system-hub
```

### 2. Start System Agents (on each monitored machine)

```sh
cd monitoring-agent

# Standalone mode — serves its own dashboard at :9090
./target/release/system-agent
# → Dashboard: http://localhost:9090/

# With API token (protects the agent's REST API)
SYSTEM_AGENT_TOKEN=agent-key ./target/release/system-agent

# Push mode — connects directly to the Hub
PUSH_TO=ws://hub-host:9091 \
PUSH_TOKEN=my-secret-key \
PUSH_INTERVAL=2 \
./target/release/system-agent
```

### 3. Register agents in the Hub dashboard

Open **http://localhost:9091/** and click **"+ Add System"**:

| Field | Value |
|---|---|
| Name | `Production DB` |
| URL | `http://192.168.1.50:9090` |
| API Token | `agent-key` (if set on the agent) |
| Poll Interval | `10` seconds |

For push-mode agents, they appear automatically — no manual registration needed.

---

## Configuration reference

### System Agent — environment variables

| Variable | Default | Description |
|---|---|---|
| `SYSTEM_AGENT_TOKEN` | *(none)* | API token required for REST access |
| `PUSH_TO` | *(none)* | Hub WebSocket URL, e.g. `ws://hub:9091` |
| `PUSH_TOKEN` | *(none)* | Shared secret for hub authentication |
| `PUSH_INTERVAL` | `2` | Seconds between push snapshots (min 2) |

### System Hub — environment variables

| Variable | Default | Description |
|---|---|---|
| `HUB_PUSH_TOKEN` | *(none)* | Shared secret agents must provide on push connect |

---

## API Reference

### System Agent (port 9090)

| Endpoint | Description |
|---|---|
| `GET /api/health` | Health check + version |
| `GET /api/system` | Full snapshot — CPU, memory, disk, network, load, processes |
| `GET /api/system/cpu` | CPU model, cores, usage % |
| `GET /api/system/memory` | RAM + swap |
| `GET /api/system/disk` | All mounted disks |
| `GET /api/system/network` | Interfaces, IPs, traffic |
| `GET /api/system/processes?limit=N` | Top N processes by CPU |
| `GET /api/history/{cpu,memory,swap,load,disk}?limit=N` | Ring-buffer history (3600 points) |
| `GET /api/alerts` | Active alerts + rule count |
| `GET /api/alerts/config` | Current alert rules |
| `POST /api/alerts/config` | Replace alert rules (JSON body) |
| `GET /api/{packages,services,containers,ports}` | Installed packages, services, Docker, ports |
| `GET /api/stream/system` | **SSE** — CPU/mem/load every 2s |
| `GET /api/stream/processes` | **SSE** — Top processes every 3s |
| `GET /api/stream/alerts` | **SSE** — Active alerts every 3s |
| `GET /api/ws/system` | **WebSocket** — Full state every 2s |

### System Hub (port 9091)

| Endpoint | Method | Description |
|---|---|---|
| `GET /api/health` | GET | Health check |
| `GET /api/systems` | GET | List registered systems |
| `POST /api/systems` | POST | Register a system `{name, url, token, poll_interval_secs}` |
| `GET /api/systems/{id}` | GET | System details |
| `PUT /api/systems/{id}` | PUT | Update config |
| `DELETE /api/systems/{id}` | DELETE | Remove system + all data |
| `GET /api/summary` | GET | Aggregated stats (online/offline/alerts) |
| `GET /api/systems/{id}/metrics?metric=cpu&limit=300` | GET | Time-series for a specific metric |
| `GET /api/systems/{id}/history?limit=300` | GET | Combined CPU + memory history |
| `GET /api/alerts?acknowledged=false&limit=50` | GET | Alert history |
| `POST /api/alerts/{id}/acknowledge` | POST | Acknowledge an alert |
| `GET /api/stream/summary` | **SSE** | Live summary + system list + live metrics every 5s |
| `GET /api/push` | **WebSocket** | Agent push endpoint (MessagePack) |

### Push protocol (WebSocket + MessagePack)

**Handshake:**

```
Agent → Hub:  {"type":"auth","system_id":"<uuid>","token":"<secret>"}
Hub → Agent:  {"type":"auth_ok"}   or   {"type":"auth_error","message":"..."}
```

**Data frames (every 2s):**

Agent sends binary MessagePack-encoded frames:

```
MessagePack({
  system_id: String,
  hostname: String,
  os_name: String,
  kernel: String,
  cpu_percent: f32,
  cpu_cores: usize,
  cpu_model: String,
  memory_percent: f32,
  memory_used_display: String,
  memory_total_display: String,
  memory_used_bytes: u64,
  memory_total_bytes: u64,
  swap_percent: f32,
  load_one: f64,
  load_five: f64,
  load_fifteen: f64,
  uptime_seconds: u64,
  uptime_display: String,
  disks: [{mount_point, usage_percent, total_display, used_display}],
  top_processes: [{pid, name, cpu_usage, memory_usage_display, memory_percent}],
  timestamp: u64
})
```

MessagePack is ~50-70% smaller than equivalent JSON. A typical 2 KB JSON snapshot compresses to ~600-800 bytes.

---

## Default alert rules (per agent)

| Condition | Severity | Duration | Cooldown |
|---|---|---|---|
| CPU > 90% | Warning | 60s | 5 min |
| CPU > 95% | Critical | 30s | 5 min |
| Memory > 90% | Warning | 60s | 5 min |
| Memory > 95% | Critical | 30s | 5 min |
| Disk > 85% | Warning | 5 min | 10 min |
| Disk > 95% | Critical | 60s | 10 min |

Configure via `POST /api/alerts/config` or programmatically through the API.

---

## Deployment patterns

### Single machine (just the agent)

```sh
./system-agent
# Dashboard at http://localhost:9090/
```

### Fleet with push (agents → hub)

```sh
# Hub server
HUB_PUSH_TOKEN=secret ./system-hub

# Each agent
PUSH_TO=ws://hub.internal:9091 PUSH_TOKEN=secret ./system-agent
```

### Fleet with polling (hub → agents)

Agents run with `SYSTEM_AGENT_TOKEN`, hub polls them. Best for agents behind NAT/firewall.

```sh
# Agent
SYSTEM_AGENT_TOKEN=agent-secret ./system-agent

# Hub (add via dashboard or API)
curl -X POST http://hub:9091/api/systems \
  -H 'Content-Type: application/json' \
  -d '{"name":"web-01","url":"http://10.0.1.5:9090","token":"agent-secret","poll_interval_secs":10}'
```

### TLS termination

Neither component has built-in TLS. Place them behind a reverse proxy:

```nginx
# Nginx example for the hub
server {
    listen 443 ssl;
    server_name hub.example.com;

    ssl_certificate /etc/ssl/certs/hub.crt;
    ssl_certificate_key /etc/ssl/private/hub.key;

    location / {
        proxy_pass http://127.0.0.1:9091;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

Agents would then use `wss://hub.example.com` for push or `https://agent-host:9090` for polling.

---

## Database

The hub stores everything in **SQLite** (`system-hub.db`, created automatically).

| Table | Contents |
|---|---|
| `systems` | Registered agents, URLs, tokens, status, OS info |
| `metrics` | Time-series: cpu, memory, swap, load1, load5, disk:{mount} |
| `alerts` | Deduplicated alert history with acknowledge support |
| `metric_retention` | Per-system per-metric retention (default 24h) |

Metrics are automatically pruned after their retention period. Run `sqlite3 system-hub.db` for direct queries.

---

## Dashboards

| URL | Description |
|---|---|
| `http://agent:9090/` | Single-machine dashboard with CPU gauge, charts, alerts |
| `http://hub:9091/` | Multi-system hub with system cards, live metrics, alert feed |

Both are self-contained single HTML files with zero external dependencies. The hub dashboard updates live via SSE every 5 seconds.

---

## License

GNU Affero General Public License v3.0 or later
