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

//! The batch between two mail reports (RFC 0017 §2), pure: which samples to keep, which alert
//! incidents were active, and when an incident may send a report at once. The clocks are the
//! caller's.

use std::time::{Duration, Instant};

use super::report::{
    MailInterval, MailReport, MailedAlert, MailedSnapshot, ReportId, ReportReason,
};
use crate::alerts::{ActiveAlert, AgentRun};
use crate::applications::round::ScrapeRound;

/// The most samples one report carries.
pub const MAX_SAMPLES: usize = 60;
/// The most alerts one report carries.
pub const MAX_ALERTS: usize = 64;
/// The least time between two incident reports.
pub const INCIDENT_SPACING: Duration = Duration::from_secs(60);

/// The time between kept samples (`MAIL_SAMPLE_INTERVAL`): 10 s up to the mail interval, and
/// at most 60 samples a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleInterval(Duration);

/// A sample interval under 10 s, over the mail interval, or giving more than 60 samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleIntervalOutOfRange;

impl SampleInterval {
    pub fn new(secs: u64, mail: MailInterval) -> Result<Self, SampleIntervalOutOfRange> {
        let mail = mail.as_duration().as_secs();
        let fits = secs >= 10 && secs <= mail && mail.div_ceil(secs) <= MAX_SAMPLES as u64;
        match fits {
            true => Ok(Self(Duration::from_secs(secs))),
            false => Err(SampleIntervalOutOfRange),
        }
    }
}

/// Whether an offered sample was kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    Kept,
    Skipped,
}

/// What the agent gathers between two reports.
#[derive(Debug)]
pub struct MailBatch {
    run: AgentRun,
    next_seq: u64,
    sample_interval: SampleInterval,
    last_kept: Option<Instant>,
    samples: Vec<MailedSnapshot>,
    alerts: Vec<MailedAlert>,
}

impl MailBatch {
    pub fn new(run: AgentRun, sample_interval: SampleInterval) -> Self {
        Self {
            run,
            next_seq: 1,
            sample_interval,
            last_kept: None,
            samples: Vec::new(),
            alerts: Vec::new(),
        }
    }

    /// Keeps `sample`, read at `now`, when a sample interval has passed since the last kept
    /// one; past `MAX_SAMPLES`, the oldest is dropped.
    pub fn offer(&mut self, sample: MailedSnapshot, now: Instant) -> Offer {
        let due = self
            .last_kept
            .is_none_or(|last| now.saturating_duration_since(last) >= self.sample_interval.0);
        if !due {
            return Offer::Skipped;
        }
        if self.samples.len() == MAX_SAMPLES {
            self.samples.remove(0);
        }
        self.samples.push(sample);
        self.last_kept = Some(now);
        Offer::Kept
    }

    /// Notes the alerts active on a tick, once each by incident id. Returns whether any is new
    /// to this batch: an incident that may send a report at once.
    pub fn note_alerts(&mut self, alerts: &[ActiveAlert]) -> bool {
        let mut any_new = false;
        for alert in alerts {
            if self.alerts.iter().any(|noted| noted.id == alert.id) {
                continue;
            }
            if self.alerts.len() == MAX_ALERTS {
                self.alerts.remove(0);
            }
            self.alerts.push(MailedAlert::from(alert));
            any_new = true;
        }
        any_new
    }

    /// Closes the batch into the next report, created at `created_at`, and empties it.
    /// `None` when no sample was kept: a report carries at least one.
    pub fn close(
        &mut self,
        interval: MailInterval,
        reason: ReportReason,
        created_at: u64,
        round: Option<ScrapeRound>,
    ) -> Option<MailReport> {
        if self.samples.is_empty() {
            return None;
        }
        let id = ReportId {
            run: self.run,
            seq: self.next_seq,
        };
        self.next_seq += 1;
        Some(MailReport {
            id,
            created_at,
            interval,
            reason,
            snapshots: std::mem::take(&mut self.samples),
            alerts: std::mem::take(&mut self.alerts),
            round,
        })
    }
}

/// When the last incident report was mailed, so incidents send at most one a minute.
#[derive(Debug, Default)]
pub struct IncidentPace {
    last: Option<Instant>,
}

impl IncidentPace {
    /// Whether an incident report may go at `now`; if so, it counts as sent.
    pub fn allows(&mut self, now: Instant) -> bool {
        let allowed = self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= INCIDENT_SPACING);
        if allowed {
            self.last = Some(now);
        }
        allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::fixtures::active_alert;
    use crate::snapshot::fixtures;

    fn mail_interval(secs: u64) -> MailInterval {
        MailInterval::try_from(secs).unwrap()
    }

    fn batch(sample_secs: u64) -> MailBatch {
        let interval = SampleInterval::new(sample_secs, mail_interval(300));
        assert!(interval.is_ok(), "{sample_secs} s is a sample interval");
        MailBatch::new(AgentRun::new(uuid::Uuid::nil()), interval.unwrap())
    }

    fn sample(collected_at: u64) -> MailedSnapshot {
        let mut sample = MailedSnapshot::from(&fixtures::published());
        sample.collected_at = collected_at;
        sample
    }

    /// RFC 0017 §2: 10 s up to the mail interval, and at most 60 samples a report.
    #[test]
    fn a_sample_interval_fits_its_mail_interval() {
        let cases = [
            (9, 300, false),
            (10, 300, true),
            (5, 300, false),
            (60, 300, true),
            (300, 300, true),
            (301, 300, false),
            (10, 600, true),
            (60, 3600, true),
            (59, 3600, false),
        ];
        for (secs, mail, ok) in cases {
            let got = SampleInterval::new(secs, mail_interval(mail)).is_ok();
            assert_eq!(got, ok, "{secs} s in {mail} s");
        }
    }

    /// RFC 0017 §2: a sample is kept once a sample interval has passed since the last kept one,
    /// the boundary included.
    #[test]
    fn a_sample_is_kept_once_its_interval_has_passed() {
        let start = Instant::now();
        let mut batch = batch(60);
        let offers: Vec<Offer> = [0, 30, 59, 60, 90, 121]
            .iter()
            .map(|secs| batch.offer(sample(*secs), start + Duration::from_secs(*secs)))
            .collect();

        use Offer::{Kept, Skipped};
        assert_eq!(offers, [Kept, Skipped, Skipped, Kept, Skipped, Kept]);
    }

    /// RFC 0017 §2: a close carries the kept samples oldest first, every incident noted since
    /// the last close once each, and the next sequence; then the batch is empty.
    #[test]
    fn a_close_carries_the_batch_and_empties_it() {
        let start = Instant::now();
        let mut batch = batch(60);
        batch.offer(sample(100), start);
        batch.offer(sample(160), start + Duration::from_secs(60));
        let mut other = active_alert();
        other.id = "run-2".into();
        assert!(batch.note_alerts(&[active_alert()]), "a new incident");
        assert!(
            !batch.note_alerts(&[active_alert()]),
            "the same incident again"
        );
        assert!(
            batch.note_alerts(&[other]),
            "another incident, ended by the close"
        );

        let report = batch.close(mail_interval(300), ReportReason::Scheduled, 170, None);

        assert!(report.is_some(), "a report");
        let report = report.unwrap();
        assert_eq!(report.id.seq, 1);
        assert_eq!(report.created_at, 170);
        let times: Vec<u64> = report.snapshots.iter().map(|s| s.collected_at).collect();
        assert_eq!(times, [100, 160]);
        let ids: Vec<&str> = report.alerts.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["run-1", "run-2"]);
        assert_eq!(
            batch
                .close(mail_interval(300), ReportReason::Scheduled, 230, None)
                .map(|r| r.id),
            None,
            "nothing kept since"
        );
        batch.offer(sample(240), start + Duration::from_secs(140));
        let next = batch.close(mail_interval(300), ReportReason::Scheduled, 240, None);
        assert_eq!(next.map(|r| (r.id.seq, r.alerts.len())), Some((2, 0)));
    }

    /// RFC 0017 §2: at most 60 samples and 64 alerts; past them, the oldest go.
    #[test]
    fn a_batch_holds_at_most_60_samples_and_64_alerts() {
        let start = Instant::now();
        let mut batch = batch(10);
        for n in 0..70u64 {
            batch.offer(sample(n * 10), start + Duration::from_secs(n * 10));
        }
        let alerts: Vec<ActiveAlert> = (0..70)
            .map(|n| {
                let mut alert = active_alert();
                alert.id = format!("run-{n}");
                alert
            })
            .collect();
        batch.note_alerts(&alerts);

        let report = batch.close(mail_interval(600), ReportReason::Scheduled, 700, None);

        let report = report.expect("a report");
        assert_eq!(report.snapshots.len(), MAX_SAMPLES);
        assert_eq!(report.snapshots[0].collected_at, 100, "the oldest went");
        assert_eq!(report.alerts.len(), MAX_ALERTS);
    }

    /// RFC 0017 §2: at most one incident report a minute.
    #[test]
    fn incident_reports_are_a_minute_apart() {
        let start = Instant::now();
        let mut pace = IncidentPace::default();
        let at = |secs| start + Duration::from_secs(secs);

        let allowed: Vec<bool> = [0, 30, 59, 60, 61, 125]
            .iter()
            .map(|s| pace.allows(at(*s)))
            .collect();

        assert_eq!(allowed, [true, false, false, true, false, true]);
    }
}
