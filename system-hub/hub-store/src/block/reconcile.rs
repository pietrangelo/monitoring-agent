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

//! Reconciling the block files with the `blocks` table at open (RFC 0010 §6 step 2a), as a
//! pure decision over what is on disk and what redb holds:
//! - every file in `pending_unlinks` is unlinked if present, and its entry cleared;
//! - a file no row names is deleted only when it is provably a redundant copy: **(a)** its
//!   span has no row, is closed by the persisted hub clock, no tail of the tier is open in it
//!   and its chunks are in `chunks` (a handoff that crashed before its commit); or **(b)** its
//!   span's row holds a lower rewrite number whose own file is present (a rewrite that crashed
//!   before its swap). Any other such file means `hub.redb` is older than `blocks/`: the open
//!   is refused, naming it;
//! - a row whose file is absent opens with its span unreadable.

use std::collections::{BTreeMap, BTreeSet};

use super::name::{BlockName, Rewrite};
use crate::tier::{SpanStart, Tier};

/// Whether a tail of the tier still holds the span open, or every tail has it sealed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TailsInSpan {
    Open,
    AllSealed,
}

/// Whether `chunks` holds chunks of the span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunksInRedb {
    Present,
    Absent,
}

/// What redb says of a span with a row-less file, gathered only for such spans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpanEvidence {
    pub tails: TailsInSpan,
    pub chunks: ChunksInRedb,
}

/// What the open does to the block files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reconciliation {
    /// Files to unlink: those `pending_unlinks` names, and redundant copies.
    pub unlink: Vec<BlockName>,
    /// `pending_unlinks` entries to delete once their files are gone.
    pub clear_pending: Vec<BlockName>,
    /// Rows whose file is absent: their spans open unreadable.
    pub missing: Vec<BlockName>,
}

/// Why the open refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal<E> {
    /// A file no rule explains: `hub.redb` is older than `blocks/`.
    Unexplained(BlockName),
    /// The evidence couldn't be read.
    Evidence(E),
}

/// What redb holds of the block files.
pub struct Catalogued<'a> {
    /// Per span with a row, the rewrite number it names.
    pub rows: &'a BTreeMap<(Tier, SpanStart), Rewrite>,
    pub pending: &'a BTreeSet<BlockName>,
    /// Hub time as last committed (`last_issued`).
    pub hub_now: u64,
}

/// The plan for `files`, the `.blk` files found under `blocks/`. `evidence` is asked only of
/// spans with a row-less file.
pub fn reconcile<E>(
    files: &BTreeSet<BlockName>,
    catalogued: &Catalogued<'_>,
    mut evidence: impl FnMut(Tier, SpanStart) -> Result<SpanEvidence, E>,
) -> Result<Reconciliation, Refusal<E>> {
    let mut plan = Reconciliation::default();
    for name in catalogued.pending {
        if files.contains(name) {
            plan.unlink.push(*name);
        }
        plan.clear_pending.push(*name);
    }
    for (&(tier, span), &rewrite) in catalogued.rows {
        let own = BlockName {
            tier,
            span,
            rewrite,
        };
        if !files.contains(&own) || catalogued.pending.contains(&own) {
            plan.missing.push(own);
        }
    }
    let orphans = files.iter().filter(|f| {
        !catalogued.pending.contains(f)
            && catalogued.rows.get(&(f.tier, f.span)) != Some(&f.rewrite)
    });
    for orphan in orphans {
        if !redundant(orphan, files, catalogued, &mut evidence)? {
            return Err(Refusal::Unexplained(*orphan));
        }
        plan.unlink.push(*orphan);
    }
    Ok(plan)
}

/// Whether a file no row names is provably a redundant copy, by rule (a) or (b).
fn redundant<E>(
    file: &BlockName,
    files: &BTreeSet<BlockName>,
    catalogued: &Catalogued<'_>,
    evidence: &mut impl FnMut(Tier, SpanStart) -> Result<SpanEvidence, E>,
) -> Result<bool, Refusal<E>> {
    match catalogued.rows.get(&(file.tier, file.span)) {
        Some(&row) => {
            let own = BlockName {
                rewrite: row,
                ..*file
            };
            Ok(row < file.rewrite && files.contains(&own) && !catalogued.pending.contains(&own))
        }
        None if file.span.is_closed(file.tier, catalogued.hub_now) => {
            let found = evidence(file.tier, file.span).map_err(Refusal::Evidence)?;
            Ok(found
                == SpanEvidence {
                    tails: TailsInSpan::AllSealed,
                    chunks: ChunksInRedb::Present,
                })
        }
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_800_057_600; // a day boundary: on every tier's grid
    const HOUR: u64 = 3_600;

    fn name(tier: Tier, span: u64, rewrite: u32) -> BlockName {
        BlockName {
            tier,
            span: SpanStart::new(tier, span).expect("grid"),
            rewrite: Rewrite(rewrite),
        }
    }

    fn raw(rewrite: u32) -> BlockName {
        name(Tier::Raw, S, rewrite)
    }

    /// The raw span S is closed once hub time is past S + 1 h + 120 s.
    const CLOSED: u64 = S + HOUR + 121;
    const AT_GRACE: u64 = S + HOUR + 120;

    const HANDED_OFF: SpanEvidence = SpanEvidence {
        tails: TailsInSpan::AllSealed,
        chunks: ChunksInRedb::Present,
    };

    struct Case {
        name: &'static str,
        files: Vec<BlockName>,
        rows: Vec<BlockName>,
        pending: Vec<BlockName>,
        clock: u64,
        evidence: SpanEvidence,
        expected: Result<Reconciliation, Refusal<()>>,
    }

    fn plan(
        unlink: Vec<BlockName>,
        clear: Vec<BlockName>,
        missing: Vec<BlockName>,
    ) -> Result<Reconciliation, Refusal<()>> {
        Ok(Reconciliation {
            unlink,
            clear_pending: clear,
            missing,
        })
    }

    fn run(case: &Case) -> Result<Reconciliation, Refusal<()>> {
        let files: BTreeSet<BlockName> = case.files.iter().copied().collect();
        let rows: BTreeMap<(Tier, SpanStart), Rewrite> = case
            .rows
            .iter()
            .map(|n| ((n.tier, n.span), n.rewrite))
            .collect();
        let pending: BTreeSet<BlockName> = case.pending.iter().copied().collect();
        let evidence = case.evidence;
        reconcile(
            &files,
            &Catalogued {
                rows: &rows,
                pending: &pending,
                hub_now: case.clock,
            },
            |_, _| Ok(evidence),
        )
    }

    #[test]
    fn the_plan_follows_the_rules_of_step_2a() {
        let cases = vec![
            Case {
                name: "every row with its file: nothing to do",
                files: vec![raw(0), name(Tier::Hour, S, 2)],
                rows: vec![raw(0), name(Tier::Hour, S, 2)],
                pending: vec![],
                clock: CLOSED,
                evidence: HANDED_OFF,
                expected: plan(vec![], vec![], vec![]),
            },
            Case {
                name: "a pending unlink still present is unlinked and cleared",
                files: vec![raw(0), raw(1)],
                rows: vec![raw(1)],
                pending: vec![raw(0)],
                clock: CLOSED,
                evidence: HANDED_OFF,
                expected: plan(vec![raw(0)], vec![raw(0)], vec![]),
            },
            Case {
                name: "a retired file (pending, its span with no row) is unlinked without evidence",
                files: vec![raw(0)],
                rows: vec![],
                pending: vec![raw(0)],
                clock: AT_GRACE,
                evidence: SpanEvidence {
                    tails: TailsInSpan::Open,
                    chunks: ChunksInRedb::Absent,
                },
                expected: plan(vec![raw(0)], vec![raw(0)], vec![]),
            },
            Case {
                name: "a pending unlink already gone is only cleared",
                files: vec![raw(1)],
                rows: vec![raw(1)],
                pending: vec![raw(0)],
                clock: CLOSED,
                evidence: HANDED_OFF,
                expected: plan(vec![], vec![raw(0)], vec![]),
            },
            Case {
                name: "(a) a handoff that crashed before its commit",
                files: vec![raw(0)],
                rows: vec![],
                pending: vec![],
                clock: CLOSED,
                evidence: HANDED_OFF,
                expected: plan(vec![raw(0)], vec![], vec![]),
            },
            Case {
                name: "(a) refused: the span isn't closed by the persisted clock",
                files: vec![raw(0)],
                rows: vec![],
                pending: vec![],
                clock: AT_GRACE,
                evidence: HANDED_OFF,
                expected: Err(Refusal::Unexplained(raw(0))),
            },
            Case {
                name: "(a) refused: a tail is still open in the span (hub.redb taken while S was open)",
                files: vec![raw(0)],
                rows: vec![],
                pending: vec![],
                clock: CLOSED,
                evidence: SpanEvidence {
                    tails: TailsInSpan::Open,
                    chunks: ChunksInRedb::Present,
                },
                expected: Err(Refusal::Unexplained(raw(0))),
            },
            Case {
                name: "(a) refused: the span's chunks aren't in redb",
                files: vec![raw(0)],
                rows: vec![],
                pending: vec![],
                clock: CLOSED,
                evidence: SpanEvidence {
                    tails: TailsInSpan::AllSealed,
                    chunks: ChunksInRedb::Absent,
                },
                expected: Err(Refusal::Unexplained(raw(0))),
            },
            Case {
                name: "(b) a rewrite that crashed before its swap",
                files: vec![raw(1), raw(2)],
                rows: vec![raw(1)],
                pending: vec![],
                clock: CLOSED,
                evidence: HANDED_OFF,
                expected: plan(vec![raw(2)], vec![], vec![]),
            },
            Case {
                name: "(b) refused: the row's own file is itself pending (an unsound hub.redb)",
                files: vec![raw(1), raw(2)],
                rows: vec![raw(1)],
                pending: vec![raw(1)],
                clock: CLOSED,
                evidence: HANDED_OFF,
                expected: Err(Refusal::Unexplained(raw(2))),
            },
            Case {
                name: "(b) refused: the row's own file is gone (hub.redb older than a rewrite)",
                files: vec![raw(3)],
                rows: vec![raw(1)],
                pending: vec![],
                clock: CLOSED,
                evidence: HANDED_OFF,
                expected: Err(Refusal::Unexplained(raw(3))),
            },
            Case {
                name: "(b) refused: the row's own file is present only in another tier",
                files: vec![raw(1), name(Tier::Minute, S, 2)],
                rows: vec![raw(1), name(Tier::Minute, S, 1)],
                pending: vec![],
                clock: CLOSED,
                evidence: HANDED_OFF,
                expected: Err(Refusal::Unexplained(name(Tier::Minute, S, 2))),
            },
            Case {
                name: "(b) refused: the row's own file is present only for another span",
                files: vec![name(Tier::Raw, S + HOUR, 1), raw(2)],
                rows: vec![name(Tier::Raw, S + HOUR, 1), raw(1)],
                pending: vec![],
                clock: CLOSED + HOUR,
                evidence: HANDED_OFF,
                expected: Err(Refusal::Unexplained(raw(2))),
            },
            Case {
                name: "refused: a file older than its row and not pending",
                files: vec![raw(0), raw(2)],
                rows: vec![raw(2)],
                pending: vec![],
                clock: CLOSED,
                evidence: HANDED_OFF,
                expected: Err(Refusal::Unexplained(raw(0))),
            },
            Case {
                name: "a row whose file is absent opens unreadable",
                files: vec![name(Tier::Minute, S, 0)],
                rows: vec![raw(0), name(Tier::Minute, S, 0)],
                pending: vec![],
                clock: CLOSED,
                evidence: HANDED_OFF,
                expected: plan(vec![], vec![], vec![raw(0)]),
            },
            Case {
                name: "a row whose file is pending: unlinked, and the row opens unreadable",
                files: vec![raw(0)],
                rows: vec![raw(0)],
                pending: vec![raw(0)],
                clock: CLOSED,
                evidence: HANDED_OFF,
                expected: plan(vec![raw(0)], vec![raw(0)], vec![raw(0)]),
            },
        ];
        for case in &cases {
            assert_eq!(run(case), case.expected, "{}", case.name);
        }
    }

    #[test]
    fn rule_a_reads_each_tiers_own_grace() {
        // The hour span S (a day) closes after S + 1 day + 1 h + 60 s.
        let hour = name(Tier::Hour, S, 0);
        let end = S + 86_400;
        for (clock, expected) in [
            (end + 3_660, Err(Refusal::Unexplained(hour))),
            (end + 3_661, plan(vec![hour], vec![], vec![])),
        ] {
            let case = Case {
                name: "hour",
                files: vec![hour],
                rows: vec![],
                pending: vec![],
                clock,
                evidence: HANDED_OFF,
                expected,
            };
            assert_eq!(run(&case), case.expected, "clock {clock}");
        }
    }

    #[test]
    fn evidence_is_gathered_only_for_spans_with_a_rowless_file_and_its_error_refuses() {
        let files: BTreeSet<BlockName> = [raw(0), name(Tier::Raw, S + HOUR, 0)].into();
        let rows: BTreeMap<(Tier, SpanStart), Rewrite> =
            [((Tier::Raw, raw(0).span), Rewrite(0))].into();
        let pending = BTreeSet::new();
        let catalogued = Catalogued {
            rows: &rows,
            pending: &pending,
            hub_now: CLOSED + 2 * HOUR,
        };
        let mut asked = Vec::new();
        let result = reconcile(&files, &catalogued, |tier, span| {
            asked.push((tier, span));
            Err::<SpanEvidence, &str>("io")
        });
        assert_eq!(result, Err(Refusal::Evidence("io")));
        assert_eq!(
            asked,
            vec![(
                Tier::Raw,
                SpanStart::new(Tier::Raw, S + HOUR).expect("grid")
            )]
        );
    }
}
