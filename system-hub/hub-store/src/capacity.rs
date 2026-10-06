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

//! Disk space (RFC 0010 §5, §6, §8), the pure core: the storage cap and the files it retires,
//! the open-time floor of free space, and when `hub.redb` is worth compacting. The volume's
//! sizes are read by the caller (`statvfs`) and passed in.

use crate::retention::{FileCondition, FileKey};
use crate::tier::Tier;

const GIB: u64 = 1 << 30;

/// A share of the volume, 1 to 100 percent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Share(u8);

/// A share outside 1 to 100 percent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidShare;

impl Share {
    pub fn new(percent: u8) -> Result<Share, InvalidShare> {
        match percent {
            1..=100 => Ok(Share(percent)),
            _ => Err(InvalidShare),
        }
    }

    pub fn percent(self) -> u8 {
        self.0
    }
}

/// `HUB_STORAGE_LIMIT`: the bytes the store may take on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageLimit {
    Bytes(u64),
    /// A share of the volume holding the data directory, re-read at every pass.
    Share(Share),
    /// `none`.
    Unlimited,
}

impl Default for StorageLimit {
    /// 80% of the volume.
    fn default() -> StorageLimit {
        StorageLimit::Share(Share(80))
    }
}

/// The cap in force at one pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageCap {
    Bytes(u64),
    Unlimited,
}

impl StorageLimit {
    /// The cap on a volume of `volume_bytes`.
    pub fn cap(self, volume_bytes: u64) -> StorageCap {
        match self {
            StorageLimit::Bytes(bytes) => StorageCap::Bytes(bytes),
            StorageLimit::Share(share) => {
                let bytes = u128::from(volume_bytes) * u128::from(share.0) / 100;
                // At most 100% of a `u64`: always fits.
                StorageCap::Bytes(u64::try_from(bytes).unwrap_or(u64::MAX))
            }
            StorageLimit::Unlimited => StorageCap::Unlimited,
        }
    }
}

/// A block file the cap may retire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapCandidate {
    pub key: FileKey,
    pub length: u64,
    pub condition: FileCondition,
}

/// Whether the cap was met.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapOutcome {
    Met,
    /// Raw and minute files can't bring the total under the cap: by this many bytes.
    Unmet {
        excess: u64,
    },
}

/// The files the cap retires, oldest raw first, then oldest minute, and whether that meets it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapPlan {
    pub retire: Vec<FileKey>,
    pub outcome: CapOutcome,
}

/// While `total` exceeds the cap, retire the oldest raw file, then the oldest minute file;
/// never an hour file, never a file in flight.
pub fn cap_retirements(total: u64, cap: StorageCap, files: &[CapCandidate]) -> CapPlan {
    let StorageCap::Bytes(cap) = cap else {
        return CapPlan {
            retire: Vec::new(),
            outcome: CapOutcome::Met,
        };
    };
    let mut ranked: Vec<(CapRank, &CapCandidate)> = files
        .iter()
        .filter_map(|f| cap_rank(f).map(|rank| (rank, f)))
        .collect();
    ranked.sort_by_key(|(rank, f)| (*rank, f.key.1));
    // Retire in that order while the bytes left still exceed the cap.
    let (left, retire) =
        ranked
            .into_iter()
            .fold((total, Vec::new()), |(left, mut retire), (_, f)| {
                match left > cap {
                    true => {
                        retire.push(f.key);
                        (left.saturating_sub(f.length), retire)
                    }
                    false => (left, retire),
                }
            });
    let outcome = match left.checked_sub(cap) {
        Some(excess) if excess > 0 => CapOutcome::Unmet { excess },
        _ => CapOutcome::Met,
    };
    CapPlan { retire, outcome }
}

/// The order in which the cap takes tiers: every raw file before any minute file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum CapRank {
    Raw,
    Minute,
}

/// A file's place in the cap's order; `None` for an hour file or a file in flight, which the
/// cap never takes.
fn cap_rank(file: &CapCandidate) -> Option<CapRank> {
    let rank = match file.key.0 {
        Tier::Raw => Some(CapRank::Raw),
        Tier::Minute => Some(CapRank::Minute),
        Tier::Hour => None,
    };
    match file.condition {
        FileCondition::Sound | FileCondition::Damaged => rank,
        FileCondition::InFlight => None,
    }
}

/// The free space the open requires (§6): the larger of 1 GiB and 2% of the volume, raised by
/// twice the bytes a clock rewind will bring back into `hub.redb`.
pub fn floor(volume_bytes: u64, rewind_bytes: u64) -> u64 {
    (volume_bytes / 50)
        .max(GIB)
        .saturating_add(rewind_bytes.saturating_mul(2))
}

/// Free space below the floor: the hub refuses to start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BelowFloor {
    pub free: u64,
    pub floor: u64,
}

pub fn check_floor(free: u64, floor: u64) -> Result<(), BelowFloor> {
    match free < floor {
        true => Err(BelowFloor { free, floor }),
        false => Ok(()),
    }
}

/// Whether `HUB_STORE_COMPACT=1` compacts: reclaimable space over 1 GiB and over 25% of the
/// file.
pub fn compaction_due(file_bytes: u64, reclaimable: u64) -> bool {
    reclaimable > GIB && reclaimable.saturating_mul(4) > file_bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::SpanStart;

    const HOUR: u64 = 3_600;
    const DAY: u64 = 86_400;
    const NOW: u64 = 1_800_057_600;

    fn candidate(tier: Tier, start: u64, length: u64) -> CapCandidate {
        CapCandidate {
            key: (tier, SpanStart::new(tier, start).expect("grid")),
            length,
            condition: FileCondition::Sound,
        }
    }

    #[test]
    fn a_share_is_one_to_a_hundred_percent() {
        let cases = [
            (0, false),
            (1, true),
            (80, true),
            (100, true),
            (101, false),
            (255, false),
        ];
        for (percent, valid) in cases {
            let share = Share::new(percent);
            assert_eq!(share.is_ok(), valid, "{percent}");
            assert_eq!(
                share.map(Share::percent).ok(),
                valid.then_some(percent),
                "{percent}"
            );
            assert_eq!(share.err(), (!valid).then_some(InvalidShare), "{percent}");
        }
    }

    #[test]
    fn the_cap_is_bytes_a_share_of_the_volume_or_none() {
        let volume = 1_000 * GIB;
        let share = |p| StorageLimit::Share(Share::new(p).expect("share"));
        let cases = [
            (
                "bytes ignore the volume",
                StorageLimit::Bytes(5 * GIB),
                volume,
                StorageCap::Bytes(5 * GIB),
            ),
            ("80%", share(80), volume, StorageCap::Bytes(800 * GIB)),
            ("100%", share(100), volume, StorageCap::Bytes(volume)),
            ("1% rounds down", share(1), 199, StorageCap::Bytes(1)),
            ("50% rounds down", share(50), 199, StorageCap::Bytes(99)),
            (
                "80% of the largest volume, without overflow",
                share(80),
                u64::MAX,
                StorageCap::Bytes(14_757_395_258_967_641_292),
            ),
            (
                "on the largest volume",
                share(100),
                u64::MAX,
                StorageCap::Bytes(u64::MAX),
            ),
            (
                "none",
                StorageLimit::Unlimited,
                volume,
                StorageCap::Unlimited,
            ),
            (
                "the default is 80%",
                StorageLimit::default(),
                volume,
                StorageCap::Bytes(800 * GIB),
            ),
        ];
        for (name, limit, volume, cap) in cases {
            assert_eq!(limit.cap(volume), cap, "{name}");
        }
    }

    #[test]
    fn the_cap_retires_oldest_raw_then_oldest_minute_until_it_is_met() {
        let files = [
            candidate(Tier::Minute, NOW - 2 * DAY, 300),
            candidate(Tier::Raw, NOW - 2 * HOUR, 100),
            candidate(Tier::Hour, NOW - 20 * DAY, 10_000),
            candidate(Tier::Raw, NOW - 5 * HOUR, 100),
            candidate(Tier::Minute, NOW - 9 * DAY, 300),
        ];
        let key = |i: usize| files[i].key;
        let cases = [
            (
                "under the cap",
                1_000,
                StorageCap::Bytes(1_000),
                vec![],
                CapOutcome::Met,
            ),
            (
                "unlimited",
                1_000_000,
                StorageCap::Unlimited,
                vec![],
                CapOutcome::Met,
            ),
            (
                "one over: the oldest raw",
                1_001,
                StorageCap::Bytes(1_000),
                vec![key(3)],
                CapOutcome::Met,
            ),
            (
                "every raw file, then the oldest minute",
                1_250,
                StorageCap::Bytes(1_000),
                vec![key(3), key(1), key(4)],
                CapOutcome::Met,
            ),
            (
                "never an hour file",
                12_000,
                StorageCap::Bytes(1_000),
                vec![key(3), key(1), key(4), key(0)],
                CapOutcome::Unmet { excess: 10_200 },
            ),
        ];
        for (name, total, cap, retire, outcome) in cases {
            assert_eq!(
                cap_retirements(total, cap, &files),
                CapPlan { retire, outcome },
                "{name}"
            );
        }
    }

    #[test]
    fn the_cap_skips_files_in_flight_and_retires_damaged_ones() {
        let mut files = [
            candidate(Tier::Raw, NOW - 5 * HOUR, 100),
            candidate(Tier::Raw, NOW - 4 * HOUR, 100),
            candidate(Tier::Raw, NOW - 3 * HOUR, 100),
        ];
        files[0].condition = FileCondition::InFlight;
        files[1].condition = FileCondition::Damaged;
        let plan = cap_retirements(1_250, StorageCap::Bytes(1_000), &files);
        assert_eq!(plan.retire, vec![files[1].key, files[2].key]);
        assert_eq!(plan.outcome, CapOutcome::Unmet { excess: 50 });
    }

    #[test]
    fn the_floor_is_a_gib_or_two_percent_plus_twice_a_rewind() {
        let cases = [
            ("a small volume", 10 * GIB, 0, GIB),
            ("where 2% is a GiB", 50 * GIB, 0, GIB),
            ("a large volume", 1_000 * GIB, 0, 20 * GIB),
            ("2% rounds down", 50 * GIB + 50, 0, GIB + 1),
            ("a rewind raises it", 10 * GIB, 3 * GIB, 7 * GIB),
            ("saturating", u64::MAX, u64::MAX, u64::MAX),
        ];
        for (name, volume, rewind, expected) in cases {
            assert_eq!(floor(volume, rewind), expected, "{name}");
        }
    }

    #[test]
    fn below_the_floor_the_open_is_refused() {
        let cases = [
            (
                GIB - 1,
                GIB,
                Err(BelowFloor {
                    free: GIB - 1,
                    floor: GIB,
                }),
            ),
            (GIB, GIB, Ok(())),
            (GIB + 1, GIB, Ok(())),
            (
                0,
                GIB,
                Err(BelowFloor {
                    free: 0,
                    floor: GIB,
                }),
            ),
        ];
        for (free, floor, expected) in cases {
            assert_eq!(check_floor(free, floor), expected, "{free} of {floor}");
        }
    }

    #[test]
    fn compaction_is_due_over_a_gib_and_a_quarter_of_the_file() {
        let cases = [
            ("both well over", 2 * GIB, GIB + 1, true),
            ("exactly a GiB", 2 * GIB, GIB, false),
            ("exactly a quarter", 4 * GIB + 4, GIB + 1, false),
            ("just over a quarter", 4 * GIB, GIB + 1, true),
            (
                "a large file, little reclaimable",
                100 * GIB,
                2 * GIB,
                false,
            ),
            ("nothing reclaimable", 4 * GIB, 0, false),
            ("an empty file", 0, 0, false),
        ];
        for (name, file, reclaimable, due) in cases {
            assert_eq!(compaction_due(file, reclaimable), due, "{name}");
        }
    }
}
