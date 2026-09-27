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

//! Scrape rounds (RFC 0009 §1, §5): every application report from one pass over the
//! configured applications, under a round id that never rewinds within an agent run. Pure:
//! the scrape loop hands in the reports and the time, and publishes what comes back.

use std::time::Instant;

use super::config::{ApplicationName, ApplicationTarget, ScrapeInterval};
use super::report::{
    ApplicationHealth, ApplicationReport, RawScrape, ScrapeFailure, ScrapeHistory,
};
use crate::alerts::AgentRun;

/// Identifies one scrape round: the agent run, and the round's place in it, from 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundId {
    pub run: AgentRun,
    pub seq: u64,
}

/// Mints the round ids of one agent run, from 1, never rewinding.
#[derive(Debug)]
pub struct RoundSequence {
    run: AgentRun,
    minted: u64,
}

impl RoundSequence {
    pub fn new(run: AgentRun) -> Self {
        Self { run, minted: 0 }
    }

    pub fn mint(&mut self) -> RoundId {
        self.minted += 1;
        RoundId {
            run: self.run,
            seq: self.minted,
        }
    }
}

/// One application's report, under its name.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedReport {
    pub name: ApplicationName,
    pub report: ApplicationReport,
}

/// Every application report from one pass over the configured applications.
#[derive(Debug, Clone, PartialEq)]
pub struct ScrapeRound {
    pub id: RoundId,
    pub interval: ScrapeInterval,
    /// When the round finished, in seconds since the Unix epoch, on the agent's clock: for
    /// local readers only.
    pub scraped_at: u64,
    pub applications: Vec<NamedReport>,
}

/// A change in an application's health worth one log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthChange {
    BecameUnreachable(ScrapeFailure),
    Recovered,
}

/// What changed between an application's previous health (`None` before its first scrape)
/// and its current one. Only reachability matters: a change among reported healths, or a
/// different failure while still unreachable, is not a change.
pub fn health_change(
    previous: Option<ApplicationHealth>,
    current: ApplicationHealth,
) -> Option<HealthChange> {
    use ApplicationHealth::{Reported, Unreachable};
    match (previous, current) {
        (Some(Unreachable(_)), Unreachable(_)) => None,
        (None | Some(Reported(_)), Unreachable(failure)) => {
            Some(HealthChange::BecameUnreachable(failure))
        }
        (Some(Unreachable(_)), Reported(_)) => Some(HealthChange::Recovered),
        (None | Some(Reported(_)), Reported(_)) => None,
    }
}

/// One application as the scrape loop follows it: where it is, its rates' baseline, and its
/// last health.
pub struct WatchedApplication {
    target: ApplicationTarget,
    history: ScrapeHistory,
    last_health: Option<ApplicationHealth>,
}

impl WatchedApplication {
    pub fn new(target: ApplicationTarget) -> Self {
        Self {
            target,
            history: ScrapeHistory::default(),
            last_health: None,
        }
    }

    pub fn target(&self) -> &ApplicationTarget {
        &self.target
    }

    /// Folds one scrape, finished at `now`, into this application's history: its report, and
    /// whether its reachability changed.
    pub fn fold(&mut self, raw: RawScrape, now: Instant) -> (NamedReport, Option<HealthChange>) {
        let (history, report) = std::mem::take(&mut self.history).advance(raw, now);
        self.history = history;
        let health = report.health();
        let change = health_change(self.last_health, health);
        self.last_health = Some(health);
        let named = NamedReport {
            name: self.target.name().clone(),
            report,
        };
        (named, change)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::applications::report::ReportedHealth;

    fn run() -> AgentRun {
        AgentRun::new(uuid::Uuid::from_u128(7))
    }

    #[test]
    fn a_runs_round_ids_count_from_one_and_never_rewind() {
        let mut sequence = RoundSequence::new(run());
        let ids: Vec<RoundId> = (0..5).map(|_| sequence.mint()).collect();
        let seqs: Vec<u64> = ids.iter().map(|id| id.seq).collect();
        assert_eq!(seqs, [1, 2, 3, 4, 5]);
        assert!(
            ids.iter().all(|id| id.run == run()),
            "every id carries the run"
        );
    }

    #[test]
    fn only_a_change_in_reachability_is_a_health_change() {
        use ApplicationHealth::{Reported, Unreachable};
        use HealthChange::{BecameUnreachable, Recovered};
        let up = Reported(ReportedHealth::Up);
        let down = Reported(ReportedHealth::Down);
        let refused = Unreachable(ScrapeFailure::Connect);
        let timed_out = Unreachable(ScrapeFailure::Timeout);
        let cases = [
            ("first scrape reached", None, up, None),
            (
                "first scrape unreachable",
                None,
                refused,
                Some(BecameUnreachable(ScrapeFailure::Connect)),
            ),
            ("still up", Some(up), up, None),
            ("up to down is still reached", Some(up), down, None),
            (
                "reached to unreachable",
                Some(down),
                timed_out,
                Some(BecameUnreachable(ScrapeFailure::Timeout)),
            ),
            (
                "still unreachable, same failure",
                Some(refused),
                refused,
                None,
            ),
            (
                "still unreachable, another failure",
                Some(refused),
                timed_out,
                None,
            ),
            (
                "unreachable to reached",
                Some(refused),
                down,
                Some(Recovered),
            ),
        ];
        for (name, previous, current, expected) in cases {
            assert_eq!(health_change(previous, current), expected, "case: {name}");
        }
    }
}
