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

//! The receipt rules (RFC 0017 §6, §7), pure: whether a report is fresh enough to take,
//! whether it is its system's newest or a backfill, and whether a mail system is overdue.
//! Times are passed in; the hub's clock is read by the adapter.

use super::report::MailInterval;
use crate::snapshot::SnapshotTime;

/// The replay bound: a report older than this, on the hub's clock, is refused, and receipts
/// older than this are pruned (each system's newest current one aside).
pub const RECEIPT_WINDOW_SECS: u64 = 7 * 86_400;
/// How far ahead of the hub's clock a report may be created.
const FUTURE_SLACK_SECS: u64 = 5 * 60;
/// The margin for mail latency past three missed reports.
const OVERDUE_MARGIN_SECS: u64 = 15 * 60;

/// Why a report isn't fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stale {
    /// Created more than 5 minutes ahead of the hub's clock.
    FromTheFuture,
    /// Created before the receipt window.
    TooOld,
}

/// Whether a report created at `created_at` may be taken at `now`.
pub fn fresh(created_at: SnapshotTime, now: SnapshotTime) -> Result<(), Stale> {
    let age = i128::from(now.seconds()) - i128::from(created_at.seconds());
    if age < -i128::from(FUTURE_SLACK_SECS) {
        Err(Stale::FromTheFuture)
    } else if age > i128::from(RECEIPT_WINDOW_SECS) {
        Err(Stale::TooOld)
    } else {
        Ok(())
    }
}

/// Whether a report is its system's newest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recency {
    /// No current receipt yet, or created no earlier than the newest one: it updates the
    /// system's status, last seen, live metrics and shown round.
    Newest,
    /// Delivered late or out of order: it adds points and alert records only.
    Backfill,
}

/// The recency of a report created at `created_at`, against the system's newest current
/// receipt read before this report's own is inserted.
pub fn recency(created_at: SnapshotTime, previous_newest: Option<SnapshotTime>) -> Recency {
    match previous_newest {
        Some(newest) if created_at.seconds() < newest.seconds() => Recency::Backfill,
        Some(_) | None => Recency::Newest,
    }
}

/// Whether a mail system's reports are on time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailPresence {
    OnTime,
    /// More than `3 × interval + 15 min` since its newest report, or no receipt at all.
    Overdue,
}

/// A mail system's presence at `now`, from its newest current receipt.
pub fn mail_status(
    newest: Option<(SnapshotTime, MailInterval)>,
    now: SnapshotTime,
) -> MailPresence {
    let Some((reported_at, interval)) = newest else {
        return MailPresence::Overdue;
    };
    let limit = i128::from(3 * interval.secs() + OVERDUE_MARGIN_SECS);
    let silent_for = i128::from(now.seconds()) - i128::from(reported_at.seconds());
    match silent_for > limit {
        true => MailPresence::Overdue,
        false => MailPresence::OnTime,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;

    fn at(seconds: u64) -> SnapshotTime {
        SnapshotTime::try_from(seconds).unwrap()
    }

    fn interval(secs: u64) -> MailInterval {
        MailInterval::try_from(secs).unwrap()
    }

    /// RFC 0017 §6: up to 5 minutes ahead, back to the 7-day window, both bounds included.
    #[test]
    fn a_report_is_fresh_from_seven_days_back_to_five_minutes_ahead() {
        let cases = [
            ("now", NOW, Ok(())),
            ("5 min ahead", NOW + 300, Ok(())),
            ("5 min 1 s ahead", NOW + 301, Err(Stale::FromTheFuture)),
            ("7 days old", NOW - RECEIPT_WINDOW_SECS, Ok(())),
            (
                "7 days 1 s old",
                NOW - RECEIPT_WINDOW_SECS - 1,
                Err(Stale::TooOld),
            ),
        ];
        for (name, created_at, expected) in cases {
            assert_eq!(fresh(at(created_at), at(NOW)), expected, "case {name}");
        }
    }

    /// RFC 0017 §6: a system's first report, and any no earlier than the previous newest,
    /// is the newest; an earlier one is a backfill.
    #[test]
    fn a_report_is_newest_unless_an_accepted_one_is_later() {
        let cases = [
            ("the system's first", NOW, None, Recency::Newest),
            ("later than the newest", NOW + 1, Some(NOW), Recency::Newest),
            ("the same second", NOW, Some(NOW), Recency::Newest),
            (
                "earlier than the newest",
                NOW - 1,
                Some(NOW),
                Recency::Backfill,
            ),
        ];
        for (name, created_at, previous, expected) in cases {
            assert_eq!(
                recency(at(created_at), previous.map(at)),
                expected,
                "case {name}"
            );
        }
    }

    /// RFC 0017 §7: overdue once more than `3 × interval + 15 min` has passed, or with no
    /// receipt at all.
    #[test]
    fn a_mail_system_is_overdue_past_three_intervals_and_a_margin() {
        let limit = 3 * 300 + 900;
        let cases = [
            ("no receipt", None, NOW, MailPresence::Overdue),
            ("just reported", Some((NOW, 300)), NOW, MailPresence::OnTime),
            (
                "at the limit",
                Some((NOW, 300)),
                NOW + limit,
                MailPresence::OnTime,
            ),
            (
                "one second past",
                Some((NOW, 300)),
                NOW + limit + 1,
                MailPresence::Overdue,
            ),
            (
                "a day's interval, 2 days on",
                Some((NOW, 86_400)),
                NOW + 2 * 86_400,
                MailPresence::OnTime,
            ),
            (
                "a report from the future",
                Some((NOW + 100, 300)),
                NOW,
                MailPresence::OnTime,
            ),
        ];
        for (name, newest, now, expected) in cases {
            let newest = newest.map(|(time, secs)| (at(time), interval(secs)));
            assert_eq!(mail_status(newest, at(now)), expected, "case {name}");
        }
    }
}
