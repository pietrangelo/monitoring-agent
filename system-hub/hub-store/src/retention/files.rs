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

//! What a retention pass does to the block files (RFC 0010 §5): retire a file whose every
//! chunk is dead, rewrite at most one file without its dead chunks, keep the rest.

use super::liveness::{Death, Liveness, Owner, Tombstones, chunk_liveness, expired};
use super::policy::Policies;
use crate::tier::{SpanStart, Tier};

/// A **holder**: the chunks one generation holds in a file (a `block_generations` entry), and
/// whose they are (`None`: unmapped, its generation maps to no system).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub owner: Option<Owner>,
    pub bytes: u64,
}

/// Whether the pass may touch a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileCondition {
    Sound,
    /// Opened unreadable, or a rewrite of it met a chunk failing its CRC: retired like any
    /// other, never rewritten.
    Damaged,
    /// Its handoff or rewrite runs: left alone until it commits or fails.
    InFlight,
}

/// One block file as the pass sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFacts {
    pub tier: Tier,
    pub span: SpanStart,
    pub length: u64,
    pub condition: FileCondition,
    pub holders: Vec<Holder>,
}

/// Why a file is rewritten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RewriteCause {
    /// Chunks of tombstoned (or unmapped) generations reach 25% of the file.
    Erased,
    /// The span is past the tier's global period: some chunks expired, and only longer
    /// overrides keep the others.
    Expired,
}

/// What becomes of one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileFate {
    Keep,
    Retire,
    Rewrite(RewriteCause),
}

/// A file named by its tier and span (one `blocks` row per span).
pub type FileKey = (Tier, SpanStart);

/// One pass's plan over the files: every file to retire, and at most one rewrite.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilePlan {
    pub retire: Vec<FileKey>,
    pub rewrite: Option<(FileKey, RewriteCause)>,
}

/// The fate of one file at `retention_now`.
pub fn file_fate(
    file: &FileFacts,
    policies: &Policies,
    tombstones: &Tombstones,
    retention_now: u64,
) -> FileFate {
    let deaths: Vec<(Liveness, u64)> = file
        .holders
        .iter()
        .map(|h| {
            let liveness = chunk_liveness(
                file.tier,
                file.span,
                h.owner.as_ref(),
                policies,
                tombstones,
                retention_now,
            );
            (liveness, h.bytes)
        })
        .collect();
    let all_dead = deaths.iter().all(|(l, _)| matches!(l, Liveness::Dead(_)));
    match (file.condition, all_dead) {
        (FileCondition::InFlight, _) => FileFate::Keep,
        (FileCondition::Sound | FileCondition::Damaged, true) => FileFate::Retire,
        (FileCondition::Damaged, false) => FileFate::Keep,
        (FileCondition::Sound, false) => rewrite_fate(file, policies, &deaths, retention_now),
    }
}

/// Whether a sound file holding live chunks is rewritten: erased chunks first, then chunks
/// expired past the global period.
fn rewrite_fate(
    file: &FileFacts,
    policies: &Policies,
    deaths: &[(Liveness, u64)],
    retention_now: u64,
) -> FileFate {
    let erased: u64 = deaths
        .iter()
        .filter(|(l, _)| matches!(l, Liveness::Dead(Death::Tombstoned | Death::Unmapped)))
        .map(|(_, bytes)| bytes)
        .sum();
    let past_global = expired(
        file.tier,
        file.span,
        policies.global().period(file.tier),
        retention_now,
    );
    let any_expired = deaths
        .iter()
        .any(|(l, _)| *l == Liveness::Dead(Death::Expired));
    if file.length > 0 && erased.saturating_mul(4) >= file.length {
        FileFate::Rewrite(RewriteCause::Erased)
    } else if past_global && any_expired {
        FileFate::Rewrite(RewriteCause::Expired)
    } else {
        FileFate::Keep
    }
}

/// The pass's plan: every file to retire, in the order given, and the most overdue file to
/// rewrite (the earliest span end; the lower tier on a tie).
pub fn plan_files(
    files: &[FileFacts],
    policies: &Policies,
    tombstones: &Tombstones,
    retention_now: u64,
) -> FilePlan {
    let fates: Vec<(&FileFacts, FileFate)> = files
        .iter()
        .map(|f| (f, file_fate(f, policies, tombstones, retention_now)))
        .collect();
    let retire = fates
        .iter()
        .filter(|(_, fate)| *fate == FileFate::Retire)
        .map(|(f, _)| (f.tier, f.span))
        .collect();
    let rewrite = fates
        .iter()
        .filter_map(|(f, fate)| match fate {
            FileFate::Rewrite(cause) => Some((*f, *cause)),
            FileFate::Keep | FileFate::Retire => None,
        })
        .min_by_key(|(f, _)| (f.span.end(f.tier), f.tier))
        .map(|(f, cause)| ((f.tier, f.span), cause));
    FilePlan { retire, rewrite }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::name::{Generation, SystemKey};
    use crate::retention::policy::{
        Override, PerTier, RetentionChange, RetentionPolicy, TierOverride, TierPeriod, TierSetting,
    };

    const HOUR: u64 = 3_600;
    const DAY: u64 = 86_400;
    const NOW: u64 = 1_800_057_600;

    fn owner(name: &str, generation: u64) -> Owner {
        Owner {
            system: SystemKey::try_from(name.as_bytes()).expect("key"),
            generation: Generation::new(generation),
        }
    }

    fn holder(name: &str, generation: u64, bytes: u64) -> Holder {
        Holder {
            owner: Some(owner(name, generation)),
            bytes,
        }
    }

    fn file(tier: Tier, start: u64, length: u64, holders: Vec<Holder>) -> FileFacts {
        FileFacts {
            tier,
            span: SpanStart::new(tier, start).expect("grid"),
            length,
            condition: FileCondition::Sound,
            holders,
        }
    }

    fn damaged(f: FileFacts) -> FileFacts {
        FileFacts {
            condition: FileCondition::Damaged,
            ..f
        }
    }

    fn in_flight(f: FileFacts) -> FileFacts {
        FileFacts {
            condition: FileCondition::InFlight,
            ..f
        }
    }

    /// The default policy; `long` keeps 48 h of raw points and `short` an hour; generation 9 of
    /// `gone` is tombstoned.
    fn world() -> (Policies, Tombstones) {
        let mut policies = Policies::new(RetentionPolicy::default());
        let p = TierPeriod::new(Tier::Raw, Duration::from_secs(48 * HOUR)).expect("bounds");
        let change = RetentionChange::new([p]).expect("one tier");
        policies.change(&owner("long", 1).system, &change, Duration::from_secs(600));
        let hour = TierPeriod::new(Tier::Raw, Duration::from_secs(HOUR)).expect("bounds");
        let short = Override::new(PerTier::from_fn(|tier| match tier {
            Tier::Raw => TierOverride::new(TierSetting::Fixed(hour), None),
            Tier::Minute | Tier::Hour => TierOverride::default(),
        }))
        .expect("tiers");
        policies.restore(owner("short", 1).system, short);
        let mut tombstones = Tombstones::default();
        tombstones.insert(owner("gone", 9));
        (policies, tombstones)
    }

    #[test]
    fn a_file_is_retired_when_every_chunk_in_it_is_dead() {
        let (policies, tombstones) = world();
        let recent = NOW - 2 * HOUR;
        let old = NOW - 30 * HOUR;
        let ancient = NOW - 50 * HOUR;
        let cases = [
            (
                "recent, live",
                file(Tier::Raw, recent, 1_000, vec![holder("a", 1, 900)]),
                FileFate::Keep,
            ),
            (
                "past the global period, no override",
                file(
                    Tier::Raw,
                    old,
                    1_000,
                    vec![holder("a", 1, 500), holder("b", 1, 400)],
                ),
                FileFate::Retire,
            ),
            (
                "every generation tombstoned",
                file(Tier::Raw, recent, 1_000, vec![holder("gone", 9, 900)]),
                FileFate::Retire,
            ),
            (
                "only unmapped generations",
                file(
                    Tier::Raw,
                    recent,
                    1_000,
                    vec![Holder {
                        owner: None,
                        bytes: 900,
                    }],
                ),
                FileFate::Retire,
            ),
            (
                "no chunk at all",
                file(Tier::Raw, recent, 64, vec![]),
                FileFate::Retire,
            ),
            (
                "past even the longest override",
                file(
                    Tier::Raw,
                    ancient,
                    1_000,
                    vec![holder("a", 1, 400), holder("long", 1, 400)],
                ),
                FileFate::Retire,
            ),
            (
                "damaged and dead is still retired",
                damaged(file(Tier::Raw, old, 1_000, vec![holder("a", 1, 900)])),
                FileFate::Retire,
            ),
            (
                "in flight is left alone, however dead",
                in_flight(file(Tier::Raw, old, 1_000, vec![holder("a", 1, 900)])),
                FileFate::Keep,
            ),
        ];
        for (name, f, fate) in cases {
            assert_eq!(file_fate(&f, &policies, &tombstones, NOW), fate, "{name}");
        }
    }

    #[test]
    fn a_file_is_rewritten_when_a_quarter_is_erased_or_only_longer_overrides_keep_it() {
        let (policies, tombstones) = world();
        let recent = NOW - 2 * HOUR;
        let old = NOW - 30 * HOUR;
        let cases = [
            (
                "a quarter tombstoned",
                file(
                    Tier::Raw,
                    recent,
                    1_000,
                    vec![holder("gone", 9, 250), holder("a", 1, 700)],
                ),
                FileFate::Rewrite(RewriteCause::Erased),
            ),
            (
                "just under a quarter tombstoned",
                file(
                    Tier::Raw,
                    recent,
                    1_000,
                    vec![holder("gone", 9, 249), holder("a", 1, 700)],
                ),
                FileFate::Keep,
            ),
            (
                "unmapped bytes count as erased",
                file(
                    Tier::Raw,
                    recent,
                    1_000,
                    vec![
                        Holder {
                            owner: None,
                            bytes: 150,
                        },
                        holder("gone", 9, 100),
                        holder("a", 1, 700),
                    ],
                ),
                FileFate::Rewrite(RewriteCause::Erased),
            ),
            (
                "past global, kept by an override",
                file(
                    Tier::Raw,
                    old,
                    1_000,
                    vec![holder("a", 1, 800), holder("long", 1, 100)],
                ),
                FileFate::Rewrite(RewriteCause::Expired),
            ),
            (
                "before the global period, a shorter override's chunks expired: kept",
                file(
                    Tier::Raw,
                    NOW - 3 * HOUR,
                    1_000,
                    vec![holder("short", 1, 400), holder("a", 1, 500)],
                ),
                FileFate::Keep,
            ),
            (
                "erased and expired at once: erased",
                file(
                    Tier::Raw,
                    old,
                    1_000,
                    vec![
                        holder("gone", 9, 300),
                        holder("a", 1, 300),
                        holder("long", 1, 300),
                    ],
                ),
                FileFate::Rewrite(RewriteCause::Erased),
            ),
            (
                "past global, only the override's chunks left",
                file(Tier::Raw, old, 120, vec![holder("long", 1, 100)]),
                FileFate::Keep,
            ),
            (
                "past global, the dead chunks only tombstoned and few",
                file(
                    Tier::Raw,
                    old,
                    1_000,
                    vec![holder("gone", 9, 10), holder("long", 1, 900)],
                ),
                FileFate::Keep,
            ),
            (
                "damaged is never rewritten",
                damaged(file(
                    Tier::Raw,
                    recent,
                    1_000,
                    vec![holder("gone", 9, 500), holder("a", 1, 400)],
                )),
                FileFate::Keep,
            ),
            (
                "in flight is never rewritten",
                in_flight(file(
                    Tier::Raw,
                    old,
                    1_000,
                    vec![holder("a", 1, 800), holder("long", 1, 100)],
                )),
                FileFate::Keep,
            ),
        ];
        for (name, f, fate) in cases {
            assert_eq!(file_fate(&f, &policies, &tombstones, NOW), fate, "{name}");
        }
    }

    #[test]
    fn a_pass_retires_every_dead_file_and_rewrites_the_most_overdue_one() {
        let (policies, tombstones) = world();
        let erased = |tier: Tier, start: u64| {
            file(
                tier,
                start,
                1_000,
                vec![holder("gone", 9, 500), holder("a", 1, 400)],
            )
        };
        let files = [
            erased(Tier::Raw, NOW - 3 * HOUR),
            file(Tier::Raw, NOW - 30 * HOUR, 1_000, vec![holder("a", 1, 900)]),
            erased(Tier::Hour, NOW - 2 * DAY),
            erased(Tier::Minute, NOW - 2 * DAY),
            file(Tier::Raw, NOW - 40 * HOUR, 1_000, vec![holder("b", 1, 900)]),
            file(Tier::Raw, NOW - 4 * HOUR, 1_000, vec![holder("a", 1, 900)]),
        ];
        let key = |i: usize| (files[i].tier, files[i].span);
        let plan = plan_files(&files, &policies, &tombstones, NOW);
        assert_eq!(
            plan.retire,
            vec![key(1), key(4)],
            "every dead file, in order"
        );
        assert_eq!(
            plan.rewrite,
            Some((key(3), RewriteCause::Erased)),
            "the earliest span end: the minute file ends before the hour file's day ends"
        );

        let none = plan_files(&files[4..], &policies, &tombstones, NOW);
        assert_eq!(none.retire, vec![key(4)]);
        assert_eq!(none.rewrite, None, "nothing to rewrite");

        let by_end = [
            erased(Tier::Hour, NOW - 2 * DAY),
            erased(Tier::Minute, NOW - 2 * DAY + HOUR),
        ];
        assert_eq!(
            plan_files(&by_end, &policies, &tombstones, NOW).rewrite,
            Some(((Tier::Minute, by_end[1].span), RewriteCause::Erased)),
            "a later start that ends first is more overdue"
        );

        let tie = [
            erased(Tier::Minute, NOW - 2 * HOUR),
            erased(Tier::Raw, NOW - 2 * HOUR),
        ];
        assert_eq!(
            plan_files(&tie, &policies, &tombstones, NOW).rewrite,
            Some(((Tier::Raw, tie[1].span), RewriteCause::Erased)),
            "a tie goes to the lower tier"
        );
    }
}
