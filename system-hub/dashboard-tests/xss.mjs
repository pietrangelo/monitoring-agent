#!/usr/bin/env node
// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Pietrangelo Masala
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

// XSS smoke test for the hub dashboard (static/index.html).
//
// The dashboard's only XSS control is its rendering rule (docs/ARCHITECTURE.md § Trust
// boundaries): server strings reach the DOM through textContent, ids reach URLs through
// encodeURIComponent. This script stubs fetch and EventSource with a hostile hub: every
// free-text field of an unknown, an offline and an online system and of every alert record
// breaks out of text, quoted attributes and raw-text elements (statuses and severities are
// hostile only where they test the allowlists' fallback), ids also break out of inline
// JS, system urls are hostile both as markup and as javascript: URLs, and numbers arrive as
// strings or out of range. Each system's applications (RFC 0009) are hostile the same way:
// names break out of markup and of the metric query string, healths test the allowlist, and
// gauges and chart series arrive as strings. It drives the dashboard in headless Chromium (open
// a system, open one of its applications, refresh the details, open another application, take
// a live refresh with it open, refresh again once the round has dropped it, take a live refresh, acknowledge an alert record, decline then accept each delete, open an
// offline system and take a changed summary in which it has no live metrics, a system
// changes status, and a system and an alert record arrive), then fires pointer, mouse, focus,
// key and form events at every element, shadow roots included, and window and document
// events, lets two minutes of virtual time pass for timers, and checks that no script ran,
// nothing was injected, and every request went to a known route with its id percent-encoded.
//
// Usage:  node system-hub/dashboard-tests/xss.mjs [path/to/index.html]
// Exit:   0 all checks pass · 1 a check failed, or Chromium failed or produced no report
//         2 no Chromium found (set CHROME_BIN)
//
// Running it against the pre-fix dashboard shows it goes red, but only coarsely:
//   old=$(mktemp) && git show 40fa16a:system-hub/static/index.html > "$old"
//   node system-hub/dashboard-tests/xss.mjs "$old"
// That page throws before rendering, so most checks fail for the same reason. Each check's
// own power is shown by mutating one rendering path at a time (red-test-adversary, mutation
// mode).

import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { delimiter, dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const dashboardPath = resolve(process.argv[2] ?? join(here, "..", "static", "index.html"));
const enc = encodeURIComponent;

// ── The hostile hub ────────────────────────────────────

// Closes a quoted attribute, a text node or a raw-text element, then plants a handler.
const P = `"'></script></style></textarea><img src=x onerror="window.__pwned=true">`;
const SYSTEM_A = `');window.__pwned=true;//${P}`;
// A's id is a prefix of B's and C's, so card selection must compare whole ids.
const SYSTEM_B = `${SYSTEM_A}-b`;
const SYSTEM_C = `${SYSTEM_A}-c`;
const ALERT_ID = `${SYSTEM_A}_${P}`;
const HOSTILE_SEVERITY = `x"><img src=x onerror="window.__pwned=true">`;

const hostileStrings = {
    name: P,
    last_seen: P,
    last_error: P,
    os: P,
    hostname: P,
    kernel: P,
    // The details show the model up to its "@ <clock>" suffix.
    cpu_model: `${P} @ 3.00GHz`,
    total_memory_display: P,
};
// A has no hostname yet, as a system has until its first snapshot, so its details fall back
// to its (javascript:) url.
const systemA = {
    ...hostileStrings,
    hostname: null,
    id: SYSTEM_A,
    url: `javascript:window.__pwned=true//${P}`,
    status: `online" onmouseover="window.__pwned=true" x="`,
    cpu_cores: "42",
};
const systemB = { ...hostileStrings, id: SYSTEM_B, url: `http://b/${P}`, status: "offline", cpu_cores: 8 };
const systemC = { ...hostileStrings, id: SYSTEM_C, url: `https://c/${P}`, status: "online", cpu_cores: 8 };
const systems = [systemA, systemB, systemC];
// Joins in the changed summary.
const SYSTEM_D = `${SYSTEM_A}-d`;
const systemD = { ...hostileStrings, id: SYSTEM_D, url: `http://d/${P}`, status: "offline", cpu_cores: 8 };

const hostileAlert = (id, severity) => ({ id, severity, message: P, system_name: P, fired_at: P });
const alerts = [
    hostileAlert(ALERT_ID, HOSTILE_SEVERITY),
    hostileAlert(`${SYSTEM_B}_1`, "critical"),
    hostileAlert(`${SYSTEM_C}_1`, "warning"),
    hostileAlert(`${SYSTEM_C}_2`, "critical x"),
    hostileAlert(`${SYSTEM_C}_3`, "info"),
    hostileAlert(`${SYSTEM_C}_4`, "crit"),
    hostileAlert(`${SYSTEM_C}_5`, "x critical"),
];
// The alerts once the changed summary has arrived: one more record.
const laterAlerts = [...alerts, hostileAlert(`${SYSTEM_D}_1`, "critical")];

const numericMetrics = { cpu_percent: 12.34, memory_percent: 50, load_one: 1, disks: [] };
const summary = {
    total_systems: P,
    online_count: P,
    offline_count: P,
    active_alerts: P,
    systems,
    live_metrics: {
        [SYSTEM_A]: {
            cpu_percent: "42",
            memory_percent: P,
            load_one: 0.5,
            disks: [[P, "42"], ["/", 95.5], ["/over", 150], ["/under", -5], ["/missing", null]],
        },
        [SYSTEM_B]: numericMetrics,
        [SYSTEM_C]: numericMetrics,
    },
};

// The later summary: B turns online and loses its live metrics while it is open, C reports a
// status that is only part of a known name, and D joins with one that only ends in one.
const changedSummary = {
    ...summary,
    systems: [systemA, { ...systemB, status: "online" }, { ...systemC, status: "line" }, { ...systemD, status: "x offline" }],
    live_metrics: { [SYSTEM_A]: summary.live_metrics[SYSTEM_A], [SYSTEM_C]: numericMetrics, [SYSTEM_D]: numericMetrics },
};

// Applications (RFC 0009 §10). A's names break out of markup and of a query string (so the
// metric parameter must be encoded), and the first is a prefix of the second, so row
// selection must compare whole names. Its health, version and gauges are hostile, its round is
// stale, and its age is a string.
const MIB = 1048576;
const APP_1 = `${P}&metric=cpu#`;
const APP_2 = `${APP_1}-b`;
const hostileGauges = {
    heap_used_bytes: "42",
    heap_max_bytes: P,
    cpu_percent: "12",
    live_threads: null,
    http_requests_per_second: P,
    http_server_errors_per_second: "1",
    http_mean_latency_ms: {},
    db_connections_active: [],
    uptime_seconds: "90061",
};
const applicationsA = {
    system_id: P,
    received_at: P,
    age_secs: "40",
    freshness: "stale",
    applications: [
        { name: APP_1, health: `up" onmouseover="window.__pwned=true" x="`, version: P, gauges: hostileGauges },
        { name: APP_2, health: "out_of_service x", version: null, gauges: P },
        null,
        "x",
        { name: P, health: "unreachable", version: P, gauges: null },
    ],
};
// What a detail refresh brings: the same round, now with a negative age.
const refreshedApplicationsA = { ...applicationsA, age_secs: -3 };
// Then a round in which the open application no longer appears.
const applicationsAWithoutApp2 = {
    ...applicationsA,
    applications: applicationsA.applications.filter((app) => app?.name !== APP_2),
};
// C's round is fresh and numeric, so formatting and every known health can be read.
const applicationsC = {
    system_id: SYSTEM_C,
    received_at: 1790000000,
    age_secs: 7.4,
    freshness: "fresh",
    applications: [
        {
            name: "orders",
            health: "up",
            version: "2.4.1",
            gauges: {
                heap_used_bytes: 3 * MIB,
                heap_max_bytes: 8 * MIB,
                cpu_percent: 12.34,
                live_threads: 42,
                http_requests_per_second: 1.5,
                http_server_errors_per_second: 0,
                http_mean_latency_ms: 12.34,
                db_connections_active: 3,
                uptime_seconds: 90061,
            },
        },
        { name: "billing", health: "down", version: null, gauges: { heap_used_bytes: 3 * MIB, uptime_seconds: 3720 } },
        { name: "ledger", health: "out_of_service", version: "1", gauges: { uptime_seconds: 59 } },
        { name: "search", health: "unknown", version: "1", gauges: { uptime_seconds: -5 } },
        // Named like the application open on A when C opens, so switching systems must drop
        // the selection itself rather than lose it by accident.
        { name: APP_1, health: "up", version: "1", gauges: {} },
    ],
};
// B's `applications` isn't a list, and D holds no round.
const applicationsB = { system_id: SYSTEM_B, received_at: 1, age_secs: 5, freshness: "stale", applications: P };
const noRound = { system_id: SYSTEM_D, received_at: null, age_secs: null, freshness: null, applications: [] };
const applicationsOf = { [SYSTEM_A]: applicationsA, [SYSTEM_B]: applicationsB, [SYSTEM_C]: applicationsC, [SYSTEM_D]: noRound };

const seriesRoute = (systemId, app, gauge) =>
    `GET /api/systems/${enc(systemId)}/metrics?metric=${enc(`app:${app}:${gauge}`)}`;
// Points that aren't numbers are left out; if the string counted, the heap scale would be 100
// MiB, not 11. APP_1's heap has no numeric point at all, and its requests stay so low that
// without the floor of 1 the top label would round to 0. Every other application's requests
// peak at 20, so their scale is 22.
const heapSeries = { points: [{ value: String(100 * MIB) }, { value: 10 * MIB }, { value: null }, null, { value: 2 * MIB }] };
const noNumericSeries = { points: [{ value: "5" }, null, { value: null }] };
const lowRequestsSeries = { points: [{ value: 0.1 }, { value: 0.3 }] };
const requestsSeries = { points: [{ value: 5 }, { value: 20 }] };

const ALERTS_ROUTE = "GET /api/alerts?acknowledged=false&limit=50";
// Every request the dashboard may make; anything else is recorded as unexpected.
const routes = {
    [ALERTS_ROUTE]: alerts,
    // Two offline systems (B, D), so deleting offline systems must delete every one of them.
    "GET /api/systems": [...systems, systemD],
    ...Object.fromEntries(
        [...systems, systemD].flatMap((s) => [
            [`GET /api/systems/${enc(s.id)}`, s],
            [`GET /api/systems/${enc(s.id)}/history?limit=300`, { cpu: [], memory: [] }],
            [`DELETE /api/systems/${enc(s.id)}`, {}],
            [`GET /api/systems/${enc(s.id)}/applications`, applicationsOf[s.id]],
        ]),
    ),
    ...Object.fromEntries(
        [[SYSTEM_A, applicationsA], [SYSTEM_C, applicationsC]].flatMap(([id, round]) =>
            round.applications.filter((app) => app && typeof app === "object").flatMap((app) => [
                [seriesRoute(id, app.name, "heap_used_bytes"), app.name === APP_1 ? noNumericSeries : heapSeries],
                [seriesRoute(id, app.name, "http_requests_per_second"), app.name === APP_1 ? lowRequestsSeries : requestsSeries],
            ]),
        ),
    ),
    ...Object.fromEntries(laterAlerts.map((a) => [`POST /api/alerts/${enc(a.id)}/acknowledge`, {}])),
};

// ── Page side ──────────────────────────────────────────
// These functions run inside the page, before the dashboard's own script, and are
// serialised with toString(): they must not close over anything from this module.

function allElements(root) {
    var out = [];
    (function walk(node) {
        Array.prototype.forEach.call(node.children || [], function (child) {
            out.push(child);
            if (child.shadowRoot) walk(child.shadowRoot);
            walk(child);
        });
    })(root);
    return out;
}

function handlerSignature(node) {
    return Array.prototype.filter
        .call(node.attributes, function (a) { return /^on/i.test(a.name); })
        .map(function (a) { return a.name + "=" + a.value; })
        .join(";");
}

function installStubs(fx, log) {
    window.addEventListener("error", function (e) {
        if (e.target === window) log.errors.push(String(e.message));
    });
    window.addEventListener("unhandledrejection", function (e) {
        log.errors.push(String(e.reason));
    });
    // Closed shadow roots would hide rendered markup from the scans.
    var attachShadow = Element.prototype.attachShadow;
    Element.prototype.attachShadow = function (init) {
        return attachShadow.call(this, Object.assign({}, init, { mode: "open" }));
    };
    // Hover-capable, fine-pointer device, so JS gated on those media queries renders too.
    var matchMedia = window.matchMedia.bind(window);
    window.matchMedia = function (query) {
        if (!/\((any-)?(hover:\s*hover|pointer:\s*fine)\)/.test(query)) return matchMedia(query);
        return { matches: true, media: query, onchange: null, addEventListener: function () {}, removeEventListener: function () {}, addListener: function () {}, removeListener: function () {} };
    };
    // A javascript: URL opened in a popup would run where __pwned can't be seen.
    window.open = function (url) {
        log.opened.push(String(url));
        return null;
    };
    // What each canvas last drew as text (its axis labels), so a chart's scale can be read.
    var ctx = CanvasRenderingContext2D.prototype;
    var clearRect = ctx.clearRect;
    var fillText = ctx.fillText;
    ctx.clearRect = function () {
        log.canvasText[this.canvas.id] = [];
        return clearRect.apply(this, arguments);
    };
    ctx.fillText = function (text) {
        (log.canvasText[this.canvas.id] = log.canvasText[this.canvas.id] || []).push(String(text));
        return fillText.apply(this, arguments);
    };
    window.alert = function () {};
    window.confirm = function () {
        log.confirms++;
        return log.confirmAnswer;
    };
    window.fetch = function (url, opts) {
        var call = ((opts && opts.method) || "GET") + " " + url;
        log.calls.push(call);
        var known = Object.prototype.hasOwnProperty.call(fx.routes, call);
        if (!known) log.unexpected.push(call);
        return Promise.resolve({
            ok: known,
            status: known ? 200 : 404,
            json: function () {
                return Promise.resolve(known ? fx.routes[call] : {});
            },
        });
    };
    window.EventSource = function () {
        this.close = function () {};
        this.addEventListener = function (name, listener) {
            log.listeners[name] = listener;
        };
    };
}

// What got in: elements, handlers or script URLs the page's own markup doesn't have.
function scanInjections(baseline) {
    var ownTags = ["DIV", "SPAN", "STRONG", "BUTTON"];
    var urlAttrs = ["href", "src", "action", "formaction"];
    var nodes = allElements(document);
    return {
        foreignTags: nodes
            .filter(function (n) { return !baseline.nodes.has(n) && ownTags.indexOf(n.tagName) < 0; })
            .map(function (n) { return n.tagName; }),
        changedHandlers: nodes.filter(function (n) {
            return handlerSignature(n) !== (baseline.handlers.get(n) || "");
        }).length,
        scriptUrls: nodes.filter(function (n) {
            return urlAttrs.some(function (a) { return /^\s*javascript:/i.test(n.getAttribute(a) || ""); });
        }).length,
    };
}

function texts(selector, root) {
    return Array.prototype.map.call((root || document).querySelectorAll(selector), function (n) {
        return n.textContent;
    });
}

function readCard(card) {
    var dot = card.querySelector(".sys-status-dot");
    var last = card.lastElementChild;
    return {
        selected: card.classList.contains("selected"),
        dotClass: dot && dot.className,
        dotTitle: dot && dot.title,
        text: texts(".sys-card-name, .sys-card-url", card),
        metrics: texts(".sys-metric .val", card),
        meta: texts(".sys-meta span", card),
        // The error line is the card's only unclassed child.
        error: last && !last.className ? last.textContent : null,
    };
}

// Missing elements read as absent rather than throwing, so a page without them fails each
// check on its own instead of stopping the journey.
function isShown(id) {
    var node = document.getElementById(id);
    return node !== null && getComputedStyle(node).display !== "none";
}

function readApplications() {
    var section = document.getElementById("appsSection");
    var updated = document.getElementById("appsUpdated");
    return {
        shown: isShown("appsSection"),
        stale: section !== null && section.classList.contains("stale"),
        updated: updated && updated.textContent,
        charts: isShown("appCharts"),
        rows: Array.prototype.map.call(document.querySelectorAll("#appsList .app-row"), function (row) {
            var health = row.querySelector(".app-health");
            return {
                name: texts(".app-name", row)[0],
                healthClass: health && health.className,
                health: health && health.textContent,
                version: texts(".app-version", row)[0],
                gauges: texts(".app-gauge .val", row),
                selected: row.classList.contains("selected"),
            };
        }),
    };
}

// What got rendered as text.
function readRendered() {
    function byId(id) {
        return document.getElementById(id).textContent;
    }
    return {
        cards: Array.prototype.map.call(document.querySelectorAll(".sys-card"), readCard),
        counters: ["statTotal", "statOnline", "statOffline", "statAlerts"].map(byId),
        detail: ["detailName", "dOs", "dKernel", "dCpu", "dMem"].map(byId),
        alerts: Array.prototype.map.call(document.querySelectorAll(".alert-card"), function (a) {
            return { cls: a.className, body: a.querySelector(".alert-body").textContent };
        }),
        disks: {
            names: texts("#detailDisks .disk-name"),
            pcts: texts("#detailDisks .disk-pct"),
            widths: Array.prototype.map.call(document.querySelectorAll("#detailDisks .disk-bar-fill"), function (n) {
                return n.style.width;
            }),
            stale: document.querySelectorAll("#detailDisks [data-stale]").length,
        },
    };
}

// Fires every handler the page may carry, with real event types, buttons, modifiers and
// coordinates, so one planted anywhere, even behind an `e.button`, `e.ctrlKey` or `e.key`
// guard, gets its chance to run.
function fireEverything() {
    var primary = { button: 0, buttons: 1 };
    var middle = { button: 1, buttons: 4 };
    var events = [
        ["pointerover", PointerEvent], ["pointerenter", PointerEvent], ["pointermove", PointerEvent],
        ["mouseover", MouseEvent], ["mouseenter", MouseEvent], ["mousemove", MouseEvent],
        ["pointerdown", PointerEvent], ["mousedown", MouseEvent], ["pointerup", PointerEvent],
        ["mouseup", MouseEvent], ["click", MouseEvent], ["click", MouseEvent, { ctrlKey: true, metaKey: true }],
        ["dblclick", MouseEvent], ["contextmenu", MouseEvent, { button: 2, buttons: 2 }],
        ["mousedown", MouseEvent, middle], ["mouseup", MouseEvent, middle], ["auxclick", MouseEvent, middle],
        ["focus", FocusEvent], ["focusin", FocusEvent], ["keydown", KeyboardEvent], ["keyup", KeyboardEvent],
        ["keydown", KeyboardEvent, { key: " " }], ["keydown", KeyboardEvent, { key: "Escape" }],
        ["keydown", KeyboardEvent, { key: "ArrowDown" }], ["input", Event], ["change", Event],
        ["pointerout", PointerEvent], ["pointerleave", PointerEvent], ["mouseout", MouseEvent],
        ["mouseleave", MouseEvent], ["blur", FocusEvent], ["focusout", FocusEvent],
    ];
    allElements(document).forEach(function (node) {
        events.forEach(function (e) {
            var bubbles = !/(enter|leave)$/.test(e[0]) && e[0] !== "focus" && e[0] !== "blur";
            var init = Object.assign(
                { bubbles: bubbles, cancelable: true, composed: true, view: window, clientX: 1, clientY: 1, key: "Enter" },
                primary,
                e[2],
            );
            node.dispatchEvent(new e[1](e[0], init));
        });
    });
    ["resize", "scroll"].forEach(function (type) { window.dispatchEvent(new Event(type)); });
    document.dispatchEvent(new Event("visibilitychange"));
}

// The user journey, as named steps; each step's requests are attributed to it by name.
function buildSteps(fx, log, baseline, result, harness) {
    function deliver(summary) {
        return function () {
            if (!log.listeners.summary) return log.missing.push("summary listener");
            log.listeners.summary({ data: JSON.stringify(summary) });
        };
    }
    function click(selector) {
        return function () {
            var node = document.querySelector(selector);
            if (!node) return log.missing.push(selector);
            node.click();
        };
    }
    function answerConfirm(answer) {
        return function () { log.confirmAnswer = answer; };
    }
    // Each open and each acknowledgement targets a different position, so a handler that
    // always picks the first card or record can't pass.
    function readOpened(name) {
        return function () {
            result.opened[name] = {
                selected: readRendered().cards.map(function (c) { return c.selected; }),
                disks: document.getElementById("detailDisks").textContent,
                apps: readApplications(),
            };
        };
    }
    function readCharts(name) {
        return function () {
            result.charts[name] = {
                apps: readApplications(),
                heap: log.canvasText.appHeapChart,
                requests: log.canvasText.appRequestsChart,
                heapShown: isShown("appHeapChart"),
                requestsShown: isShown("appRequestsChart"),
                heapEmpty: isShown("appHeapChartMsg"),
                requestsEmpty: isShown("appRequestsChartMsg"),
            };
        };
    }
    function serveApplicationsA(round) {
        return function () { fx.routes[fx.applicationsARoute] = round; };
    }
    var refreshDetails = click('button[onclick="refreshDetailCharts()"]');
    var deleteSystem = click('button[onclick="deleteSystem()"]');
    var deleteOffline = click('button[onclick="deleteOfflineSystems()"]');
    return [
        // The dashboard's own <script> follows this one; it was not parsed yet at baseline.
        ["baseline", function () { baseline.nodes.add(harness.nextElementSibling); }],
        ["first summary", deliver(fx.summary)],
        ["open system", click(".sys-card")],
        ["read selection", readOpened("open system")],
        // APP_1 is a prefix of APP_2, so selecting it must not select APP_2's row too.
        ["open application", click("#appsList .app-row:nth-child(1)")],
        ["read application", readCharts("open application")],
        ["serve refreshed round", serveApplicationsA(fx.refreshedApplicationsA)],
        ["refresh details", refreshDetails],
        ["read refreshed application", readCharts("refresh details")],
        ["open another application", click("#appsList .app-row:nth-child(2)")],
        ["read another application", readCharts("open another application")],
        ["live refresh with an application open", deliver(fx.summary)],
        ["serve round without it", serveApplicationsA(fx.applicationsAWithoutApp2)],
        ["refresh without the application", refreshDetails],
        ["read round without the application", readCharts("refresh without the application")],
        ["mark disks", function () { document.querySelectorAll("#detailDisks > *").forEach(function (n) { n.dataset.stale = ""; }); }],
        ["live refresh", deliver(fx.summary)],
        ["read page", function () { result.rendered = readRendered(); result.injected = scanInjections(baseline); }],
        ["acknowledge", click(".alert-card button")],
        ["acknowledge another", click(".alert-card:nth-child(3) button")],
        ["reopen application", click("#appsList .app-row:nth-child(1)")],
        ["open third system", click(".sys-card:nth-child(3)")],
        ["read third selection", readOpened("open third system")],
        ["decline", answerConfirm(false)],
        ["declined delete system", deleteSystem],
        ["declined delete offline", deleteOffline],
        ["accept", answerConfirm(true)],
        ["delete system", deleteSystem],
        ["delete offline", deleteOffline],
        ["open offline system", click(".sys-card:nth-child(2)")],
        ["read offline selection", readOpened("open offline system")],
        ["changed summary", function () {
            fx.routes[fx.alertsRoute] = fx.laterAlerts;
            deliver(fx.changedSummary)();
        }],
        ["read changed page", function () {
            result.changed = readRendered();
            result.changed.disksText = document.getElementById("detailDisks").textContent;
            result.changed.injected = scanInjections(baseline);
        }],
        ["fire everything", fireEverything],
    ];
}

function runPage(fx) {
    var harness = document.currentScript;
    var nodes = allElements(document);
    var baseline = {
        nodes: new Set(nodes),
        handlers: new Map(nodes.map(function (n) { return [n, handlerSignature(n)]; })),
    };
    var log = { calls: [], steps: [], unexpected: [], errors: [], listeners: {}, missing: [], opened: [], canvasText: {}, confirms: 0, confirmAnswer: true };
    var result = { log: log, opened: {}, charts: {} };
    installStubs(fx, log);
    var steps = buildSteps(fx, log, baseline, result, harness);

    function report() {
        result.injectedAtEnd = scanInjections(baseline);
        result.pwned = window.__pwned === true;
        var out = document.createElement("pre");
        out.id = "xss-report";
        // URI-encoded so the dumped DOM carries it without HTML entities to undo.
        out.textContent = encodeURIComponent(JSON.stringify(result));
        document.body.appendChild(out);
    }
    (function next(i) {
        // Virtual time: two minutes, past a once-a-minute timer, for a delayed or periodic
        // sink to fire, at no wall-clock cost.
        if (i === steps.length) return setTimeout(report, 130000);
        setTimeout(function () {
            log.steps.push({ name: steps[i][0], at: log.calls.length, confirms: log.confirms });
            steps[i][1]();
            next(i + 1);
        }, 100);
    })(0);
}

const PAGE_SIDE = [
    allElements, handlerSignature, installStubs, scanInjections, texts,
    readCard, isShown, readApplications, readRendered, fireEverything, buildSteps, runPage,
];

// ── Node side ──────────────────────────────────────────

function harnessScript() {
    // `<` escaped so no payload can end the <script> element it is embedded in.
    const fixtures = JSON.stringify({
        summary, changedSummary, routes, laterAlerts, alertsRoute: ALERTS_ROUTE,
        applicationsARoute: `GET /api/systems/${enc(SYSTEM_A)}/applications`, refreshedApplicationsA,
        applicationsAWithoutApp2,
    })
        .replace(/</g, "\\u003c");
    return `<script>\n${PAGE_SIDE.join("\n")}\nrunPage(${fixtures});\n</script>\n`;
}

// Playwright's browser folders, in search order: PLAYWRIGHT_BROWSERS_PATH, then the default
// cache. "0" is Playwright's value for "inside node_modules", not a folder.
function playwrightRoots() {
    const custom = process.env.PLAYWRIGHT_BROWSERS_PATH;
    const cache = join(homedir(), ".cache", "ms-playwright");
    return custom && custom !== "0" ? [custom, cache] : [cache];
}

// A folder's builds, newest first. Something that isn't a readable folder has none, so a
// mistyped PLAYWRIGHT_BROWSERS_PATH ends in "no Chromium found", not a crash.
function buildsIn(root) {
    const build = (name) => Number(name.match(/-(\d+)$/)?.[1] ?? -1);
    try {
        return readdirSync(root)
            .sort((a, b) => build(b) - build(a))
            .map((entry) => join(root, entry));
    } catch {
        return [];
    }
}

function playwrightBuilds() {
    return playwrightRoots().flatMap(buildsIn);
}

function findChrome() {
    if (process.env.CHROME_BIN) return existsSync(process.env.CHROME_BIN) ? process.env.CHROME_BIN : null;
    const names = ["chrome-headless-shell", "chromium", "chromium-browser", "google-chrome", "google-chrome-stable"];
    const onPath = (process.env.PATH ?? "")
        .split(delimiter)
        .filter(Boolean)
        .flatMap((dir) => names.map((n) => join(dir, n)));
    // Linux Playwright layouts only, current and older. Every headless shell comes before any
    // full Chromium, as it starts fastest; within each, folders keep their search order.
    const builds = playwrightBuilds();
    const inPlaywright = [
        ...builds.flatMap((dir) => [
            join(dir, "chrome-headless-shell-linux64", "chrome-headless-shell"),
            join(dir, "chrome-linux", "headless_shell"),
        ]),
        ...builds.flatMap((dir) => [join(dir, "chrome-linux64", "chrome"), join(dir, "chrome-linux", "chrome")]),
    ];
    return [...onPath, ...inPlaywright].find(existsSync) ?? null;
}

function dumpDom(chrome, page) {
    // --no-sandbox: Chromium's sandbox often can't start in WSL or containers. The page is a
    // local file with a stubbed backend, and its only "attack" sets a window flag.
    const args = [
        "--headless",
        "--no-sandbox",
        "--disable-gpu",
        // No --user-data-dir: Chromium's headless modes already use a throwaway profile, so it
        // buys nothing, and with it a full Chromium never finishes --dump-dom (seen on WSL,
        // Chromium 1228).
        "--virtual-time-budget=150000",
        // A desktop viewport, so layout-gated rendering (e.g. wide-screen only) runs too.
        "--window-size=1920,1080",
        "--dump-dom",
        pathToFileURL(page).href,
    ];
    try {
        return execFileSync(chrome, args, { encoding: "utf8", timeout: 30_000, stdio: ["ignore", "pipe", "pipe"] });
    } catch (err) {
        const stderr = String(err.stderr ?? "").trim().split("\n").slice(-5).join("\n");
        throw new Error(`Chromium failed: ${err.message}${stderr ? `\n${stderr}` : ""}`);
    }
}

function renderUnderAttack(chrome, html) {
    const marker = "<script>";
    const at = html.indexOf(marker);
    if (at < 0) throw new Error(`no ${marker} in ${dashboardPath}`);
    const dir = mkdtempSync(join(tmpdir(), "hub-dashboard-xss-"));
    try {
        const page = join(dir, "index.html");
        writeFileSync(page, html.slice(0, at) + harnessScript() + html.slice(at));
        const match = dumpDom(chrome, page).match(/<pre id="xss-report">([^<]*)<\/pre>/);
        if (!match) throw new Error("the dashboard never produced a report (a handler may have navigated away)");
        return JSON.parse(decodeURIComponent(match[1]));
    } finally {
        rmSync(dir, { recursive: true, force: true });
    }
}

// ── Checks ─────────────────────────────────────────────

const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const show = (value) => JSON.stringify(value);

// The requests a named step made, up to the next step.
function callsOf(log, name) {
    const i = log.steps.findIndex((s) => s.name === name);
    if (i < 0) return null;
    return log.calls.slice(log.steps[i].at, log.steps[i + 1]?.at ?? log.calls.length);
}

function confirmsOf(log, name) {
    const i = log.steps.findIndex((s) => s.name === name);
    return i < 0 || !log.steps[i + 1] ? null : log.steps[i + 1].confirms - log.steps[i].confirms;
}

const selections = (r) => Object.fromEntries(Object.entries(r.opened).map(([step, o]) => [step, o.selected]));
const alertBody = (a) => `${String(a.severity).toUpperCase()} — ${a.message}${a.system_name} · ${a.fired_at}`;
const withoutCores = (meta) => (meta ?? []).filter((m) => !m.endsWith(" cores"));
const scansAgree = (r, test) =>
    [r.injected, r.changed?.injected, r.injectedAtEnd].every((scan) => scan !== undefined && test(scan));

// One row per behaviour: [name, pass, what was seen].
function checks(r) {
    const { log } = r;
    const seen = r.rendered ?? {};
    const cards = seen.cards ?? [];
    const [a, b, c] = cards;
    const disks = seen.disks ?? {};
    const alertCards = seen.alerts ?? [];
    const calls = (name) => callsOf(log, name);
    const appsA = r.opened["open system"]?.apps;
    const appsC = r.opened["open third system"]?.apps;
    const charts = (step) => r.charts?.[step];
    return [
        // The dashboard's own SSE listener swallows render errors (`catch (_) {}`), so those
        // surface only through the rendering rows below.
        ["the dashboard runs every step without an uncaught script error",
            log.errors.length === 0 && log.missing.length === 0 && r.rendered !== undefined,
            `errors ${show(log.errors)}, missing ${show(log.missing)}`],
        ["no hostile script runs, even with every element clicked, hovered, focused and keyed",
            !r.pwned, `window.__pwned = ${r.pwned}`],
        ["no element outside the dashboard's own tags is added",
            scansAgree(r, (s) => same(s.foreignTags, [])), show([r.injected, r.changed?.injected, r.injectedAtEnd])],
        ["no event-handler attribute is added or changed",
            scansAgree(r, (s) => s.changedHandlers === 0), show([r.injected, r.changed?.injected, r.injectedAtEnd])],
        ["no javascript: URL appears in the page",
            scansAgree(r, (s) => s.scriptUrls === 0), show([r.injected, r.changed?.injected, r.injectedAtEnd])],
        ["no javascript: URL is opened in a window",
            !log.opened.some((u) => /^\s*javascript:/i.test(u)), show(log.opened)],
        ["every system card's name and url render as text",
            same(cards.map((k) => k.text), systems.map((s) => [s.name, s.url])), show(cards.map((k) => k.text))],
        ["every system card's error renders as text",
            same(cards.map((k) => k.error), [P, P, P]), show(cards.map((k) => k.error))],
        ["every system card's OS, memory and last-seen render as text",
            cards.length === 3 && cards.every((k) => same(withoutCores(k.meta), [P, `${P} RAM`, `Seen: ${P}`])),
            show(cards.map((k) => k.meta))],
        ["a core count that isn't a number is left out",
            a !== undefined && !a.meta.some((m) => m.endsWith(" cores")), show(a?.meta)],
        ["a core count that is a number renders",
            [b, c].every((k) => k?.meta.includes("8 cores")), show([b?.meta, c?.meta])],
        ["the fleet counters render as text", same(seen.counters, [P, P, P, P]), show(seen.counters)],
        ["the open system's details render as text, falling back to its url without a hostname",
            same(seen.detail, [`${P} (${systemA.url})`, P, P, P, P]), show(seen.detail)],
        ["the open system's details show its hostname when it has one",
            r.changed?.detail[0] === `${P} (${P})`, show(r.changed?.detail)],
        ["every alert record's strings render as text",
            same(alertCards.map((k) => k.body), alerts.map(alertBody)), show(alertCards.map((k) => k.body))],
        ["an unknown system status renders as unknown", a?.dotClass === "sys-status-dot unknown", show(a?.dotClass)],
        ["an unknown system status is titled unknown", a?.dotTitle === "unknown", show(a?.dotTitle)],
        ["a known system status keeps its class",
            b?.dotClass === "sys-status-dot offline" && c?.dotClass === "sys-status-dot online",
            show([b?.dotClass, c?.dotClass])],
        ["an unknown severity gets no severity class", alertCards[0]?.cls === "alert-card", show(alertCards[0]?.cls)],
        ["a known severity keeps its class",
            same([1, 2, 4].map((i) => alertCards[i]?.cls), ["alert-card critical", "alert-card warning", "alert-card info"]),
            show(alertCards.map((k) => k.cls))],
        ["a severity only matches a whole known name",
            same([3, 5, 6].map((i) => alertCards[i]?.cls), ["alert-card", "alert-card", "alert-card"]),
            show(alertCards.map((k) => k.cls))],
        ["opening a system selects only its card",
            same(selections(r), {
                "open system": [true, false, false],
                "open third system": [false, false, true],
                "open offline system": [false, true, false],
            }),
            show(selections(r))],
        ["a live refresh keeps only the open system's card selected",
            same(cards.map((k) => k.selected), [true, false, false]) &&
                same(r.changed?.cards.map((k) => k.selected), [false, true, false, false]),
            show([cards.map((k) => k.selected), r.changed?.cards.map((k) => k.selected)])],
        ["opening a system without disks says there is no disk data yet",
            r.opened["open third system"]?.disks === "No disk data yet", show(r.opened["open third system"])],
        ["card metrics that aren't numbers render as —", same(a?.metrics, ["—", "—", "0.5"]), show(a?.metrics)],
        ["card metrics that are numbers render with one decimal",
            same(b?.metrics, ["12.3%", "50.0%", "1.0"]), show(b?.metrics)],
        ["a live refresh re-renders the open system's disks", disks.stale === 0, show(disks)],
        ["disk mount points render as text", same(disks.names, [P, "/", "/over", "/under", "/missing"]), show(disks)],
        ["a disk percentage that isn't a number renders as — with no bar",
            disks.pcts?.[0] === "—" && disks.pcts?.[4] === "—" && disks.widths?.length === 3, show(disks)],
        ["disk bars stay within 0–100%",
            same(disks.pcts?.slice(1, 4), ["95.5%", "150.0%", "-5.0%"]) && same(disks.widths, ["95.5%", "100%", "0%"]),
            show(disks)],
        ["a live refresh without the open system's live metrics shows the waiting message",
            r.changed?.disksText === "Waiting for disk data...", show(r.changed?.disksText)],
        ["a system that joins later renders as text",
            same(r.changed?.cards.map((k) => k.text), changedSummary.systems.map((s) => [s.name, s.url])),
            show(r.changed?.cards.map((k) => k.text))],
        ["a system whose status changes takes its new status class",
            r.changed?.cards[1]?.dotClass === "sys-status-dot online", show(r.changed?.cards[1]?.dotClass)],
        ["a system status only matches a whole known name",
            same(r.changed?.cards.slice(2).map((k) => k.dotClass), ["sys-status-dot unknown", "sys-status-dot unknown"]),
            show(r.changed?.cards.map((k) => k.dotClass))],
        ["an alert record that arrives later renders as text",
            same(r.changed?.alerts.map((k) => k.body), laterAlerts.map(alertBody)),
            show(r.changed?.alerts.map((k) => k.body))],
        ["every application's name, health and version render as text, skipping entries that aren't objects",
            same(appsA?.rows.map((k) => [k.name, k.health, k.version]), [
                [APP_1, "unknown", P],
                [APP_2, "unknown", "—"],
                [P, "unreachable", P],
            ]),
            show(appsA?.rows)],
        ["an application health outside the allowlist renders as unknown",
            same(appsA?.rows.map((k) => k.healthClass), ["app-health unknown", "app-health unknown", "app-health unreachable"]),
            show(appsA?.rows.map((k) => k.healthClass))],
        ["a known application health keeps its class and reads in words",
            same(appsC?.rows.map((k) => [k.healthClass, k.health]), [
                ["app-health up", "up"],
                ["app-health down", "down"],
                ["app-health out_of_service", "out of service"],
                ["app-health unknown", "unknown"],
                ["app-health up", "up"],
            ]),
            show(appsC?.rows)],
        ["application gauges that aren't numbers render as —",
            appsA?.rows.length === 3 && appsA.rows.every((k) => same(k.gauges, Array(8).fill("—"))),
            show(appsA?.rows.map((k) => k.gauges))],
        ["application gauges that are numbers render formatted, and a missing one as —",
            same(appsC?.rows.map((k) => k.gauges), [
                ["3.0 / 8.0 MiB", "12.3%", "42", "1.5", "0.0", "12.3 ms", "3", "1d 1h"],
                ["3.0 MiB", "—", "—", "—", "—", "—", "—", "1h 2m"],
                ["—", "—", "—", "—", "—", "—", "—", "0m"],
                Array(8).fill("—"),
                Array(8).fill("—"),
            ]),
            show(appsC?.rows.map((k) => k.gauges))],
        ["a stale round is dimmed and labelled stale, leaving out an age that isn't a number",
            appsA?.stale === true && appsA?.updated === "stale", show(appsA)],
        ["a fresh round shows its age and isn't dimmed",
            appsC?.stale === false && appsC?.updated === "updated 7 s ago", show(appsC)],
        ["a system with applications shows the section, and one without hides it",
            appsA?.shown === true && appsC?.shown === true && r.opened["open offline system"]?.apps.shown === false,
            show([appsA?.shown, appsC?.shown, r.opened["open offline system"]?.apps])],
        ["opening an application selects only its row, even when its name is a prefix of another's",
            same(charts("open application")?.apps.rows.map((k) => k.selected), [true, false, false]) &&
                same(charts("open another application")?.apps.rows.map((k) => k.selected), [false, true, false]),
            show([charts("open application")?.apps.rows, charts("open another application")?.apps.rows])],
        ["a chart without a numeric point hides its canvas and says there is no data yet",
            charts("open application")?.apps.charts === true &&
                charts("open application")?.heapShown === false && charts("open application")?.heapEmpty === true &&
                charts("open application")?.requestsShown === true && charts("open application")?.requestsEmpty === false,
            show(charts("open application"))],
        ["a chart with numeric points shows its canvas again",
            charts("open another application")?.heapShown === true && charts("open another application")?.heapEmpty === false,
            show(charts("open another application"))],
        ["a chart's scale floors at 1 for a series that stays below it",
            same(charts("open application")?.requests, ["1", "1", "1", "0", "0", "Req/s"]),
            show(charts("open application")?.requests)],
        ["a chart scales to its largest numeric point with 10% headroom",
            same(charts("open another application")?.heap, ["11", "8", "6", "3", "0", "Heap MiB"]) &&
                same(charts("open another application")?.requests, ["22", "17", "11", "6", "0", "Req/s"]),
            show([charts("open another application")?.heap, charts("open another application")?.requests])],
        ["a detail refresh re-reads the applications and redraws the open application's charts",
            same(calls("refresh details"), [
                `GET /api/systems/${enc(SYSTEM_A)}/history?limit=300`,
                `GET /api/systems/${enc(SYSTEM_A)}/applications`,
                seriesRoute(SYSTEM_A, APP_1, "heap_used_bytes"),
                seriesRoute(SYSTEM_A, APP_1, "http_requests_per_second"),
            ]),
            show(calls("refresh details"))],
        ["a detail refresh keeps the open application selected and its charts shown",
            same(charts("refresh details")?.apps.rows.map((k) => k.selected), [true, false, false]) &&
                charts("refresh details")?.apps.charts === true,
            show(charts("refresh details"))],
        ["a detail refresh whose round no longer has the open application drops it and hides its charts",
            same(calls("refresh without the application"), [
                `GET /api/systems/${enc(SYSTEM_A)}/history?limit=300`,
                `GET /api/systems/${enc(SYSTEM_A)}/applications`,
            ]) &&
                charts("refresh without the application")?.apps.rows.length === 2 &&
                charts("refresh without the application")?.apps.rows.every((k) => !k.selected) &&
                charts("refresh without the application")?.apps.charts === false,
            show([calls("refresh without the application"), charts("refresh without the application")])],
        ["a live refresh re-reads the open system's applications, without redrawing an open application's charts",
            same(calls("live refresh with an application open")?.filter((k) => k !== ALERTS_ROUTE), [
                `GET /api/systems/${enc(SYSTEM_A)}/applications`,
            ]),
            show(calls("live refresh with an application open"))],
        ["a negative age is left out of the header",
            charts("refresh details")?.apps.updated === "stale", show(charts("refresh details")?.apps.updated)],
        ["opening another system clears the application selection and hides its charts",
            appsC?.rows.length === 5 && appsC.charts === false && appsC.rows.every((k) => !k.selected), show(appsC)],
        ["every request goes to a known route", log.unexpected.length === 0, show(log.unexpected)],
        ["opening a system fetches exactly it and its history, by percent-encoded id",
            [["open system", SYSTEM_A], ["open third system", SYSTEM_C], ["open offline system", SYSTEM_B]].every(([step, id]) =>
                same(calls(step), [
                    `GET /api/systems/${enc(id)}`,
                    `GET /api/systems/${enc(id)}/history?limit=300`,
                    `GET /api/systems/${enc(id)}/applications`,
                ])),
            show(["open system", "open third system", "open offline system"].map(calls))],
        ["opening an application fetches exactly its heap and request series, by percent-encoded name",
            same(calls("open application"), [
                seriesRoute(SYSTEM_A, APP_1, "heap_used_bytes"),
                seriesRoute(SYSTEM_A, APP_1, "http_requests_per_second"),
            ]) &&
                same(calls("open another application"), [
                    seriesRoute(SYSTEM_A, APP_2, "heap_used_bytes"),
                    seriesRoute(SYSTEM_A, APP_2, "http_requests_per_second"),
                ]),
            show([calls("open application"), calls("open another application")])],
        ["acknowledging an alert record acknowledges only it, by percent-encoded id",
            [["acknowledge", alerts[0].id], ["acknowledge another", alerts[2].id]].every(([step, id]) =>
                same(calls(step)?.filter((k) => k.endsWith("/acknowledge")), [`POST /api/alerts/${enc(id)}/acknowledge`])),
            show([calls("acknowledge"), calls("acknowledge another")])],
        ["declining a delete sends nothing",
            same(calls("declined delete system"), []) && same(calls("declined delete offline"), []),
            show([calls("declined delete system"), calls("declined delete offline")])],
        ["each delete asks for confirmation",
            ["declined delete system", "declined delete offline", "delete system", "delete offline"].every(
                (step) => confirmsOf(log, step) === 1),
            show(log.steps)],
        ["deleting the open system deletes exactly it, by percent-encoded id",
            // The open system is C, the third card, by now.
            same(calls("delete system"), [`DELETE /api/systems/${enc(SYSTEM_C)}`]), show(calls("delete system"))],
        ["deleting offline systems deletes exactly those, by percent-encoded id",
            same(calls("delete offline"), [
                "GET /api/systems",
                `DELETE /api/systems/${enc(SYSTEM_B)}`,
                `DELETE /api/systems/${enc(SYSTEM_D)}`,
            ]),
            show(calls("delete offline"))],
    ];
}

const chrome = findChrome();
if (!chrome) {
    console.error(
        "No Chromium found on PATH or in Playwright's browser folders. " +
            "Set CHROME_BIN to an existing Chromium or headless-shell binary.",
    );
    process.exit(2);
}

let failed = 0;
try {
    const report = renderUnderAttack(chrome, readFileSync(dashboardPath, "utf8"));
    for (const [name, pass, detail] of checks(report)) {
        console.log(`${pass ? "ok  " : "FAIL"} ${name}${pass ? "" : `\n     ${detail}`}`);
        if (!pass) failed++;
    }
} catch (err) {
    console.error(`FAIL ${err.message}`);
    process.exit(1);
}
console.log(failed ? `\n${failed} check(s) failed` : "\nall checks passed");
process.exit(failed ? 1 : 0);
