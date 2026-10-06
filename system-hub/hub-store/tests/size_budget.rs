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

//! The codec's size budget (RFC 0010 §4): seeded generators that model timestamps as well as
//! values, encoded the way the store seals chunks, and asserted against each kind's ceiling.
//! The ceilings are the measured sizes plus 10%, recorded in RFC 0010 §4 and
//! `docs/ARCHITECTURE.md`; run with `--nocapture` to see the measurements.

use hub_store::codec::{Bucket, MAX_RAW_CHUNK_BYTES, RawChunk, RawPoint, RollupChunk};
use hub_store::rollup::Accumulator;
use hub_store::tier::{RollupTier, Tier};
use hub_store::value::ValueKind;

/// A day of points per series.
const DAY: u64 = 86_400;
const START: f64 = 1_800_000_000.0;

/// SplitMix64: a small seeded generator, so every run measures the same series.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in [lo, hi).
    fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Standard normal, by Box–Muller.
    fn normal(&mut self) -> f64 {
        let u1 = self.uniform(f64::MIN_POSITIVE, 1.0);
        let u2 = self.uniform(0.0, 1.0);
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

/// How a series' values move.
#[derive(Clone, Copy, Debug)]
enum Model {
    /// CPU: a random walk of σ = 3 points per step, inside 0..=100.
    NoisyPercent,
    /// Memory, swap, disk: σ = 0.05 points per step, with an occasional step of a few points.
    SlowPercent,
    /// A load average: a smooth walk around 1.
    LoadWalk,
    /// Threads, connections: a count that changes by one now and then.
    CountWalk,
    /// `up`: always 1.
    Constant(f64),
    /// Uptime in seconds: the time since the application started.
    Uptime,
    /// Heap used: a sawtooth that grows, then drops by 10–60% at a GC.
    Heap,
    /// Requests per second, latency: log-normal around a level.
    LogNormal(f64),
}

/// When a series' points arrive.
#[derive(Clone, Copy, Debug)]
enum Cadence {
    /// A host snapshot every 2 s plus U(0, 0.3) s of jitter.
    Host,
    /// An application round every 15 s plus U(0.05, 1.0) s.
    Application,
}

struct Series {
    kind: ValueKind,
    model: Model,
    cadence: Cadence,
}

fn generate(series: &Series, seed: u64) -> Vec<RawPoint> {
    let mut rng = Rng(seed);
    let (interval, jitter) = match series.cadence {
        Cadence::Host => (2.0, (0.0, 0.3)),
        Cadence::Application => (15.0, (0.05, 1.0)),
    };
    let mut points: Vec<RawPoint> = Vec::new();
    let mut state = match series.model {
        Model::NoisyPercent => 30.0,
        Model::SlowPercent => 55.0,
        Model::LoadWalk => 1.0,
        Model::CountWalk => 40.0,
        Model::Constant(v) => v,
        Model::Uptime => 0.0,
        Model::Heap => 200.0 * 1024.0 * 1024.0,
        Model::LogNormal(level) => level,
    };
    let mut nominal = START;
    while nominal < START + DAY as f64 {
        let at = nominal + rng.uniform(jitter.0, jitter.1);
        nominal += interval;
        state = step(series.model, state, &mut rng, at);
        let ts = at as u64;
        if points.last().is_some_and(|last| last.ts >= ts) {
            continue; // the store refuses a second point in one second (`NotAfterLast`)
        }
        let value = series
            .kind
            .scale_value(state)
            .expect("the generators stay in the domain")
            .get();
        points.push(RawPoint { ts, value });
    }
    points
}

fn step(model: Model, state: f64, rng: &mut Rng, at: f64) -> f64 {
    match model {
        Model::NoisyPercent => (state + 3.0 * rng.normal()).clamp(0.0, 100.0),
        Model::SlowPercent => {
            let jump = if rng.uniform(0.0, 1.0) < 0.001 {
                rng.uniform(-5.0, 5.0)
            } else {
                0.0
            };
            (state + 0.05 * rng.normal() + jump).clamp(0.0, 100.0)
        }
        Model::LoadWalk => (state + 0.02 * rng.normal()).clamp(0.0, 64.0),
        Model::CountWalk => {
            let r = rng.uniform(0.0, 1.0);
            let delta = if r < 0.05 {
                -1.0
            } else if r > 0.95 {
                1.0
            } else {
                0.0
            };
            (state + delta).max(0.0)
        }
        Model::Constant(v) => v,
        Model::Uptime => (at - START + 3_600.0).floor(),
        Model::Heap => {
            let max = 512.0 * 1024.0 * 1024.0;
            let grown = state + rng.uniform(1.0, 5.0) * 1024.0 * 1024.0;
            if grown > 0.8 * max {
                grown * (1.0 - rng.uniform(0.1, 0.6))
            } else {
                grown
            }
        }
        Model::LogNormal(level) => level * (0.3 * rng.normal()).exp(),
    }
}

/// Bytes of the raw chunks a day of points takes, sealed as the store seals them: when full,
/// and at each span boundary.
fn raw_bytes(points: &[RawPoint], kind: ValueKind) -> usize {
    let mut total = 0;
    let mut chunk = RawChunk::new(kind.encoding());
    let mut span = Tier::Raw.span_of(points[0].ts);
    for &point in points {
        if Tier::Raw.span_of(point.ts) != span || chunk.push(point).is_err() {
            total += chunk.encoded_len();
            span = Tier::Raw.span_of(point.ts);
            chunk = RawChunk::new(kind.encoding());
            chunk.push(point).expect("an empty chunk takes a point");
        }
    }
    assert!(chunk.encoded_len() <= MAX_RAW_CHUNK_BYTES);
    total + chunk.encoded_len()
}

fn buckets(points: &[RawPoint], tier: RollupTier) -> Vec<Bucket> {
    let mut closed = Vec::new();
    let mut acc: Option<Accumulator> = None;
    for &RawPoint { ts, value } in points {
        match acc.as_mut() {
            Some(open) if open.holds(tier, ts) => open.add(value),
            _ => {
                closed.extend(acc.map(|a| a.close()));
                acc = Some(Accumulator::open(tier, ts, value));
            }
        }
    }
    closed.extend(acc.map(|a| a.close()));
    closed
}

/// Bytes of the rollup chunks of a day's buckets: one chunk per series per span.
fn rollup_bytes(buckets: &[Bucket], tier: RollupTier) -> usize {
    let per_span = tier.tier().span_secs() / tier.bucket_secs();
    let mut total = 0;
    let mut chunk = RollupChunk::new(tier);
    let mut span = buckets[0].index / per_span;
    for &bucket in buckets {
        if bucket.index / per_span != span {
            total += chunk.encoded_len();
            span = bucket.index / per_span;
            chunk = RollupChunk::new(tier);
        }
        chunk.push(bucket).expect("a span's buckets fit one chunk");
    }
    total + chunk.encoded_len()
}

/// Encoded bytes per raw point, per minute bucket and per hour bucket, and the counts.
#[derive(Default, Clone, Copy)]
struct Cost {
    raw_bytes: usize,
    points: usize,
    rollup_bytes: usize,
    buckets: usize,
}

impl Cost {
    fn of(series: &Series, seed: u64) -> Cost {
        let points = generate(series, seed);
        let minute = buckets(&points, RollupTier::Minute);
        let hour = buckets(&points, RollupTier::Hour);
        Cost {
            raw_bytes: raw_bytes(&points, series.kind),
            points: points.len(),
            rollup_bytes: rollup_bytes(&minute, RollupTier::Minute)
                + rollup_bytes(&hour, RollupTier::Hour),
            buckets: minute.len() + hour.len(),
        }
    }

    fn add(self, other: Cost, times: usize) -> Cost {
        Cost {
            raw_bytes: self.raw_bytes + times * other.raw_bytes,
            points: self.points + times * other.points,
            rollup_bytes: self.rollup_bytes + times * other.rollup_bytes,
            buckets: self.buckets + times * other.buckets,
        }
    }

    fn per_point(&self) -> f64 {
        self.raw_bytes as f64 / self.points as f64
    }

    fn per_bucket(&self) -> f64 {
        self.rollup_bytes as f64 / self.buckets as f64
    }
}

fn host(kind: ValueKind, model: Model) -> Series {
    Series {
        kind,
        model,
        cadence: Cadence::Host,
    }
}

fn app(kind: ValueKind, model: Model) -> Series {
    Series {
        kind,
        model,
        cadence: Cadence::Application,
    }
}

/// One budget row: a generator and its ceilings in bytes per raw point and per bucket.
struct Budget {
    name: &'static str,
    series: Series,
    raw: f64,
    rollup: f64,
}

fn budgets() -> Vec<Budget> {
    use Model::*;
    use ValueKind::{Bytes, Millis, Monotonic, Percent, Rate};
    vec![
        // Measured on 2026-10-06 (raw, rollup): (2.044, 7.233), (1.101, 4.507), (0.995, 4.245),
        // (2.426, 8.401), (3.002, 9.638), (2.136, 7.182), (0.378, 2.192), (0.304, 5.303).
        Budget {
            name: "Percent, noisy",
            series: host(Percent, NoisyPercent),
            raw: 2.25,
            rollup: 7.96,
        },
        Budget {
            name: "Percent, slow",
            series: host(Percent, SlowPercent),
            raw: 1.22,
            rollup: 4.96,
        },
        Budget {
            name: "Load",
            series: host(ValueKind::Load, LoadWalk),
            raw: 1.10,
            rollup: 4.67,
        },
        Budget {
            name: "Bytes (heap)",
            series: app(Bytes, Heap),
            raw: 2.67,
            rollup: 9.25,
        },
        Budget {
            name: "Rate",
            series: app(Rate, LogNormal(50.0)),
            raw: 3.31,
            rollup: 10.61,
        },
        Budget {
            name: "Millis",
            series: app(Millis, LogNormal(20.0)),
            raw: 2.35,
            rollup: 7.91,
        },
        Budget {
            name: "Count",
            series: app(ValueKind::Count, CountWalk),
            raw: 0.42,
            rollup: 2.42,
        },
        Budget {
            name: "Monotonic (uptime)",
            series: app(Monotonic, Uptime),
            raw: 0.34,
            rollup: 5.84,
        },
    ]
}

#[test]
fn each_kind_stays_within_its_ceiling() {
    let over: Vec<String> = budgets()
        .into_iter()
        .enumerate()
        .filter_map(|(seed, budget)| {
            let cost = Cost::of(&budget.series, seed as u64 + 1);
            let (raw, rollup) = (cost.per_point(), cost.per_bucket());
            println!(
                "{:<20} raw {raw:.3} B/point ({} points), rollup {rollup:.3} B/bucket",
                budget.name, cost.points
            );
            (raw > budget.raw || rollup > budget.rollup)
                .then(|| format!("{}: {raw:.3} B/point, {rollup:.3} B/bucket", budget.name))
        })
        .collect();
    assert!(over.is_empty(), "over their ceilings: {over:?}");
}

/// A system of the scale target: 8 host series at 2 s, and five applications of 11 series at
/// 15 s (10,000 systems and 50,000 applications).
fn scale_target_mix() -> Vec<(Series, usize)> {
    use Model::*;
    use ValueKind::{Bytes, Millis, Monotonic, Percent, Rate};
    vec![
        (host(Percent, NoisyPercent), 1),          // cpu
        (host(Percent, SlowPercent), 2),           // memory, swap
        (host(ValueKind::Load, LoadWalk), 2),      // load1, load5
        (host(Percent, SlowPercent), 3),           // three disks
        (app(Percent, NoisyPercent), 5),           // cpu_percent
        (app(Percent, LogNormal(0.5)), 5),         // gc_pause_percent
        (app(ValueKind::Count, CountWalk), 5),     // live_threads
        (app(ValueKind::Count, CountWalk), 5),     // db_connections_active
        (app(ValueKind::Count, Constant(1.0)), 5), // up
        (app(Monotonic, Uptime), 5),               // uptime_seconds
        (app(Bytes, Heap), 5),                     // heap_used_bytes
        (app(Bytes, Constant(536_870_912.0)), 5),  // heap_max_bytes
        (app(Rate, LogNormal(50.0)), 5),           // http_requests_per_second
        (app(Rate, LogNormal(0.2)), 5),            // http_server_errors_per_second
        (app(Millis, LogNormal(20.0)), 5),         // http_mean_latency_ms
    ]
}

#[test]
fn the_scale_target_mix_stays_within_its_weighted_ceiling() {
    let total = scale_target_mix()
        .iter()
        .enumerate()
        .fold(Cost::default(), |acc, (seed, (series, times))| {
            acc.add(Cost::of(series, 100 + seed as u64), *times)
        });
    println!(
        "weighted mix: raw {:.3} B/point, rollup {:.3} B/bucket",
        total.per_point(),
        total.per_bucket()
    );
    // Measured on 2026-10-06: 1.248 B per raw point, 4.985 B per bucket.
    assert!(
        total.per_point() <= 1.38,
        "{:.3} B per raw point",
        total.per_point()
    );
    assert!(
        total.per_bucket() <= 5.49,
        "{:.3} B per bucket",
        total.per_bucket()
    );
}
