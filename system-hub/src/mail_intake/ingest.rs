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

//! Ingesting one mail message (RFC 0017 §4, §6): find the armour in its text, open the sealed
//! report, parse it, check its freshness, and store it in one transaction; then, for the
//! system's newest report, its live metrics, the registry fill and its scrape round. One unit
//! of blocking work per message.

use std::time::Instant;

use super::receipt::{self, RECEIPT_WINDOW_SECS, Recency, Stale};
use super::report::{self, MailReport, MailedSnapshot, ReportRefusal};
use super::seal::{self, MailMasterKey, OpenRefusal};
use crate::applications::{ScrapeRound, SourcePace};
use crate::clock::unix_to_iso8601;
use crate::db::{MailReceipt, MailStored, MailWrite};
use crate::models::{AlertRecord, SystemId, SystemInfo, SystemStatus};
use crate::registry::{LastSeen, MAIL_URL, StatusUpdate};
use crate::registry_fill::{ReportedInfo, fill_registry};
use crate::round_intake::{self, Arrival};
use crate::snapshot::{SnapshotTime, snapshot_rule};
use crate::snapshot_intake::keep_live_metrics;
use crate::state::AppState;

/// What became of an accepted message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ingested {
    Stored {
        recency: Recency,
        expired: usize,
    },
    /// Its report was already accepted: mail is delivered at least once.
    Duplicate,
}

/// Why a message was refused. No variant carries content, an unauthenticated id or a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRefusal {
    /// Not a mail message the parser can read.
    NotAMessage,
    Open(OpenRefusal),
    Report(ReportRefusal),
    Stale(Stale),
    /// The id belongs to a pushed or polled system.
    TransportMismatch,
    /// The database failed; nothing was stored.
    Store,
}

/// Ingests one raw message at `now` (unix seconds on the hub's clock).
pub fn ingest_message(
    app: &AppState,
    master: &MailMasterKey,
    raw: &[u8],
    now: u64,
) -> Result<Ingested, MessageRefusal> {
    let sealed = armoured_report(raw)?;
    let (system_id, plaintext) = seal::open(&sealed, master).map_err(MessageRefusal::Open)?;
    let report = report::decode(&plaintext).map_err(MessageRefusal::Report)?;
    let now = SnapshotTime::try_from(now).map_err(|_| MessageRefusal::Store)?;
    receipt::fresh(report.created_at, now).map_err(MessageRefusal::Stale)?;
    store(app, &system_id, report, now)
}

/// The sealed bytes of the first armour block in the message's decoded text parts.
fn armoured_report(raw: &[u8]) -> Result<Vec<u8>, MessageRefusal> {
    let message = mail_parser::MessageParser::default()
        .parse(raw)
        .ok_or(MessageRefusal::NotAMessage)?;
    let mut refusal = OpenRefusal::BadArmor;
    for text in (0..).map_while(|part| message.body_text(part)) {
        match seal::dearmour(&text) {
            Ok(sealed) => return Ok(sealed),
            Err(OpenRefusal::TooLarge) => refusal = OpenRefusal::TooLarge,
            Err(_) => {}
        }
    }
    Err(MessageRefusal::Open(refusal))
}

/// Stores an opened, fresh report, then, for the system's newest, fills the registry and
/// stores its scrape round.
fn store(
    app: &AppState,
    system_id: &SystemId,
    mut report: MailReport,
    now: SnapshotTime,
) -> Result<Ingested, MessageRefusal> {
    let info = report.snapshots.last().and_then(reported_info);
    let round = (report.round.take()).and_then(|dto| ScrapeRound::try_from(dto).ok());
    let samples = std::mem::take(&mut report.snapshots);
    let row = mail_row(system_id);
    let (write, newest_left_out) = mail_write(system_id, &row, samples, now, &report);
    let created_at = report.created_at;
    let stored = app.db.store_mail_report(
        write,
        |previous| receipt::recency(created_at, previous),
        |snapshot, time| {
            keep_live_metrics(
                app,
                system_id,
                snapshot,
                time,
                &newest_left_out,
                Instant::now(),
            )
        },
    );
    match stored {
        Ok(MailStored::Stored {
            recency,
            expired,
            newest,
        }) => {
            // The replaced live entry is dropped here, outside both locks.
            drop(newest);
            if recency == Recency::Newest {
                after_newest(app, system_id, info, round, now);
            }
            Ok(Ingested::Stored { recency, expired })
        }
        Ok(MailStored::Duplicate) => Ok(Ingested::Duplicate),
        Ok(MailStored::TransportMismatch) => Err(MessageRefusal::TransportMismatch),
        Err(err) => {
            tracing::warn!(
                "Storing a mail report from {:?} failed: {err}",
                system_id.as_str()
            );
            Err(MessageRefusal::Store)
        }
    }
}

/// What a report writes: its samples through the snapshot rule, its receipt, the newest
/// report's status and its alert records. Also returns what the rule left out of the newest
/// sample, for its live metrics.
fn mail_write<'a>(
    system_id: &'a SystemId,
    row: &'a SystemInfo,
    samples: Vec<MailedSnapshot>,
    now: SnapshotTime,
    report: &MailReport,
) -> (MailWrite<'a>, crate::snapshot::LeftOut) {
    let mut newest_left_out = crate::snapshot::LeftOut::default();
    let snapshots = samples
        .into_iter()
        .map(|sample| {
            let (snapshot, left_out) = snapshot_rule(sample.reported);
            newest_left_out = left_out;
            (snapshot, sample.time)
        })
        .collect();
    let created_at = u64::try_from(report.created_at.seconds()).unwrap_or_default();
    let write = MailWrite {
        system_id,
        system: row,
        receipt: MailReceipt {
            run: report.id.run.hyphenated().to_string(),
            seq: report.id.seq,
            created_at: report.created_at,
            interval_secs: report.interval.secs(),
            received_at: now,
        },
        snapshots,
        status: StatusUpdate::after_snapshot(LastSeen::ReportedAt(unix_to_iso8601(created_at))),
        alerts: alert_records(system_id, report, now),
        now,
        prune_before: now.cutoff(RECEIPT_WINDOW_SECS),
    };
    (write, newest_left_out)
}

/// The registry row a new mail id gets: unknown until its report is stored.
fn mail_row(system_id: &SystemId) -> SystemInfo {
    SystemInfo {
        id: system_id.as_str().to_owned(),
        name: system_id.default_name(),
        url: MAIL_URL.to_owned(),
        token: String::new(),
        status: SystemStatus::Unknown,
        last_seen: String::new(),
        last_error: None,
        os: None,
        hostname: None,
        kernel: None,
        cpu_model: None,
        cpu_cores: None,
        total_memory_display: None,
        total_memory_bytes: None,
        poll_interval_secs: 10,
        enabled: true,
    }
}

/// One alert record per incident, keyed `<system id>_<incident id>` as the poll keys them.
fn alert_records(system_id: &SystemId, report: &MailReport, now: SnapshotTime) -> Vec<AlertRecord> {
    let stored_at = unix_to_iso8601(u64::try_from(now.seconds()).unwrap_or_default());
    let id = system_id.as_str();
    report
        .alerts
        .iter()
        .map(|alert| AlertRecord {
            id: format!("{id}_{}", alert.id),
            system_id: id.to_owned(),
            system_name: id.to_owned(),
            severity: alert.severity.clone(),
            message: alert.message.clone(),
            current_value: alert.current_value,
            fired_at: alert.fired_at.clone(),
            stored_at: stored_at.clone(),
            acknowledged: false,
        })
        .collect()
}

/// The newest sample's system info and memory capacity, when it carries the info.
fn reported_info(sample: &MailedSnapshot) -> Option<ReportedInfo> {
    let info = sample.info.as_ref()?;
    Some(ReportedInfo {
        hostname: info.hostname.clone(),
        os_name: info.os_name.clone(),
        kernel: info.kernel.clone(),
        cpu_model: info.cpu_model.clone(),
        cpu_cores: info.cpu_cores,
        memory: sample.memory.clone(),
    })
}

/// The newest report's registry fill and scrape round. A round's own duplicate rule makes a
/// replayed round a duplicate; each report brings at most one, so a fresh pace never limits it.
fn after_newest(
    app: &AppState,
    system_id: &SystemId,
    info: Option<ReportedInfo>,
    round: Option<ScrapeRound>,
    now: SnapshotTime,
) {
    if let Some(info) = info {
        fill_registry(app, system_id, &info, "Mail");
    }
    let Some(round) = round else {
        return;
    };
    let arrival = Arrival {
        now: Instant::now(),
        received_at: u64::try_from(now.seconds()).unwrap_or_default(),
    };
    let (stored, _pace) =
        round_intake::store_round(app, system_id, round, SourcePace::new(arrival.now), arrival);
    if let Err(err) = stored {
        tracing::warn!(
            "Storing a mailed round from {:?} failed: {err}",
            system_id.as_str()
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::Database;
    use crate::mail_intake::report::tests::{CREATED, sample, with};
    use crate::mail_intake::seal::tests::seal_for_test;
    use std::sync::Arc;

    pub(crate) const NOW: u64 = CREATED + 60;

    fn app() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Arc::new(Database::new(path.to_str().unwrap()).unwrap());
        (AppState::new(db).unwrap(), dir)
    }

    pub(crate) fn master() -> MailMasterKey {
        MailMasterKey::from_base64(&base64_of(&[1; 32])).unwrap()
    }

    fn base64_of(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn key_for(id: &str) -> [u8; 32] {
        let key = master().derive(&SystemId::try_from(id.to_owned()).unwrap());
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(key.to_base64())
            .unwrap();
        bytes.try_into().unwrap()
    }

    /// A message whose text body is `body`, as a relay delivers it.
    fn message(body: &str, encoding: &str) -> Vec<u8> {
        format!(
            "From: agent@web-01\r\nTo: hub@example\r\nSubject: system-agent report\r\n\
             MIME-Version: 1.0\r\nContent-Type: text/plain; charset=us-ascii\r\n\
             Content-Transfer-Encoding: {encoding}\r\n\r\n{body}\r\n"
        )
        .into_bytes()
    }

    fn armour(sealed: &[u8]) -> String {
        let encoded = base64_of(sealed);
        let lines: Vec<&str> = (encoded.as_bytes().chunks(76))
            .map(|line| std::str::from_utf8(line).unwrap())
            .collect();
        format!(
            "-----BEGIN SYSTEM-AGENT REPORT-----\r\n{}\r\n-----END SYSTEM-AGENT REPORT-----",
            lines.join("\r\n")
        )
    }

    /// A sealed report for `id`, as `change` leaves the base report.
    fn sealed_for(id: &str, change: impl FnOnce(&mut report::tests::TestReport)) -> Vec<u8> {
        seal_for_test(id.as_bytes(), &with(change), &key_for(id))
    }

    fn mail_for(id: &str, change: impl FnOnce(&mut report::tests::TestReport)) -> Vec<u8> {
        message(&armour(&sealed_for(id, change)), "7bit")
    }

    /// A valid message for `id`: report `seq`, created at `CREATED`.
    pub(crate) fn valid_mail(id: &str, seq: u64) -> Vec<u8> {
        mail_for(id, |r| r.seq = seq)
    }

    /// RFC 0017 §6: a first report registers a mail system, stores its samples and alerts,
    /// marks it online, fills the registry and keeps its newest sample as live metrics.
    #[test]
    fn a_first_report_registers_and_stores_its_system() {
        let (app, _dir) = app();
        let raw = mail_for("web-01", |r| {
            r.snapshots = vec![sample(CREATED - 60), sample(CREATED)];
        });

        let ingested = ingest_message(&app, &master(), &raw, NOW);

        assert_eq!(
            ingested,
            Ok(Ingested::Stored {
                recency: Recency::Newest,
                expired: 0
            })
        );
        let sys = app.db.get_system("web-01").unwrap().unwrap();
        assert_eq!(sys.url, "mail://");
        assert_eq!(sys.status, SystemStatus::Online);
        assert_eq!(sys.last_seen, unix_to_iso8601(CREATED));
        assert_eq!(
            app.db.get_metrics("web-01", "cpu", 10, None).unwrap().len(),
            2
        );
        let alerts = app.db.get_alerts(Some("web-01"), None, 10).unwrap();
        assert_eq!(
            alerts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["web-01_run-1"]
        );
        assert!(app.live_metrics.read().unwrap().contains_key("web-01"));
    }

    /// RFC 0017 §4: the armour is found in the decoded text, whatever transfer encoding a
    /// relay applied and whatever it added around the block.
    #[test]
    fn the_armour_is_found_through_a_relays_encoding_and_footer() {
        let (app, _dir) = app();
        let armoured = armour(&sealed_for("web-01", |_| {}));
        let quoted = armoured.replace('=', "=3D");
        let body = format!("{quoted}\r\n\r\n-- \r\nScanned by the gateway.");

        let ingested = ingest_message(&app, &master(), &message(&body, "quoted-printable"), NOW);

        assert!(
            matches!(ingested, Ok(Ingested::Stored { .. })),
            "{ingested:?}"
        );
    }

    /// RFC 0017 §6: every refusal, with its reason, and nothing stored.
    #[test]
    fn a_message_that_cant_be_taken_is_refused_with_its_reason() {
        let other_key = seal_for_test(b"web-01", &with(|_| {}), &key_for("web-02"));
        let cases: Vec<(&str, Vec<u8>, MessageRefusal)> = vec![
            (
                "no armour",
                message("hello", "7bit"),
                MessageRefusal::Open(OpenRefusal::BadArmor),
            ),
            (
                "another system's key",
                message(&armour(&other_key), "7bit"),
                MessageRefusal::Open(OpenRefusal::NotAuthentic),
            ),
            (
                "an authentic report that doesn't parse",
                mail_for("web-01", |r| r.kind = "mail-report.v9".into()),
                MessageRefusal::Report(ReportRefusal::UnknownKind),
            ),
            (
                "a report from the future",
                mail_for("web-01", |r| {
                    r.created_at = NOW + 301;
                    r.snapshots = vec![sample(NOW + 301)];
                }),
                MessageRefusal::Stale(Stale::FromTheFuture),
            ),
            (
                "a report older than the window",
                mail_for("web-01", |r| {
                    r.created_at = NOW - RECEIPT_WINDOW_SECS - 1;
                    r.snapshots = vec![sample(NOW - RECEIPT_WINDOW_SECS - 1)];
                }),
                MessageRefusal::Stale(Stale::TooOld),
            ),
        ];
        for (name, raw, expected) in cases {
            let (app, _dir) = app();

            assert_eq!(
                ingest_message(&app, &master(), &raw, NOW),
                Err(expected),
                "case {name}"
            );
            assert!(
                app.db.get_system("web-01").unwrap().is_none(),
                "case {name}: nothing stored"
            );
        }
    }

    /// RFC 0017 §6: a duplicate stores nothing, and a report for a pushed system's id is a
    /// transport mismatch.
    #[test]
    fn duplicates_and_other_transports_store_nothing() {
        let (app, _dir) = app();
        let raw = mail_for("web-01", |_| {});
        assert!(ingest_message(&app, &master(), &raw, NOW).is_ok());

        assert_eq!(
            ingest_message(&app, &master(), &raw, NOW),
            Ok(Ingested::Duplicate)
        );
        assert_eq!(
            app.db.get_metrics("web-01", "cpu", 10, None).unwrap().len(),
            1
        );

        let mut pushed = app.db.get_system("web-01").unwrap().unwrap();
        pushed.id = "pushed".into();
        pushed.url = "push://".into();
        app.db.insert_system(&pushed).unwrap();
        let for_pushed = mail_for("pushed", |_| {});
        assert_eq!(
            ingest_message(&app, &master(), &for_pushed, NOW),
            Err(MessageRefusal::TransportMismatch)
        );
    }

    /// RFC 0017 §6: a late report adds its points but leaves the live metrics of the newer
    /// one.
    #[test]
    fn a_backfill_report_leaves_the_live_metrics() {
        let (app, _dir) = app();
        let first = ingest_message(&app, &master(), &mail_for("web-01", |r| r.seq = 2), NOW);
        assert!(first.is_ok(), "the newer report is stored: {first:?}");
        let live = Arc::clone(app.live_metrics.read().unwrap().get("web-01").unwrap());

        let late = mail_for("web-01", |r| {
            r.seq = 1;
            r.created_at = CREATED - 300;
            r.snapshots = vec![sample(CREATED - 300)];
        });
        let ingested = ingest_message(&app, &master(), &late, NOW);

        assert_eq!(
            ingested,
            Ok(Ingested::Stored {
                recency: Recency::Backfill,
                expired: 0
            })
        );
        let now_live = Arc::clone(app.live_metrics.read().unwrap().get("web-01").unwrap());
        assert!(
            Arc::ptr_eq(&live, &now_live),
            "the newer report's entry is kept"
        );
        assert_eq!(
            app.db.get_metrics("web-01", "cpu", 10, None).unwrap().len(),
            2
        );
    }
}
