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

//! The mail intake (RFC 0017): sealed reports that agents without internet access mail
//! through their network's relay, read from a Maildir.

pub mod command;
pub mod config;
pub mod ingest;
pub mod overdue;
pub mod receipt;
pub mod report;
pub mod scan;
pub mod seal;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::clock::unix_now;
use crate::hourly_warning::{HourlyWarning, hourly_warning};
use crate::snapshot::SnapshotTime;
use crate::state::AppState;
use config::MailIntakeConfig;
use scan::ScanPass;

/// How often the Maildir is scanned.
const SCAN_PERIOD: Duration = Duration::from_secs(10);
/// How often the overdue sweep runs.
const OVERDUE_PERIOD: Duration = Duration::from_secs(60);

/// Whether a Maildir has the `new` and `cur` directories the intake reads and an MTA
/// delivers into.
pub fn check_maildir(maildir: &std::path::Path) -> Result<(), std::io::Error> {
    for sub in ["new", "cur"] {
        if !std::fs::metadata(maildir.join(sub))?.is_dir() {
            return Err(std::io::Error::other(format!("{sub} isn't a directory")));
        }
    }
    Ok(())
}

/// Starts the Maildir scan and the overdue sweep for the hub's lifetime, when mail is on.
pub fn start(app: Arc<AppState>, config: MailIntakeConfig) {
    let MailIntakeConfig::On { maildir, key } = config else {
        return;
    };
    tracing::info!(
        "📬 Mail intake on: reading the Maildir in {}",
        config::DIR_VARIABLE
    );
    let key = Arc::new(key);
    let scanning = Arc::clone(&app);
    tokio::spawn(async move {
        let mut warned_at = None;
        let mut ticks = tokio::time::interval(SCAN_PERIOD);
        loop {
            ticks.tick().await;
            let (app, key, maildir) = (Arc::clone(&scanning), Arc::clone(&key), maildir.clone());
            let pass = tokio::task::spawn_blocking(move || {
                scan::scan_once(&app, &key, &maildir, unix_now())
            })
            .await;
            match pass {
                Ok(pass) => warned_at = log_scan(&pass, warned_at, Instant::now()),
                Err(err) => tracing::error!("A mail scan failed: {err}"),
            }
        }
    });
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(OVERDUE_PERIOD);
        loop {
            ticks.tick().await;
            let app = Arc::clone(&app);
            let pass = tokio::task::spawn_blocking(move || {
                let now = SnapshotTime::try_from(unix_now()).map_err(|_| None)?;
                overdue::overdue_pass(&app, now).map_err(Some)
            })
            .await;
            match pass {
                Ok(Ok(0)) => {}
                Ok(Ok(marked)) => {
                    tracing::info!("Mail intake: {marked} overdue system(s) marked offline")
                }
                Ok(Err(Some(err))) => tracing::warn!("The overdue sweep failed: {err}"),
                Ok(Err(None)) => {}
                Err(err) => tracing::error!("An overdue sweep failed: {err}"),
            }
        }
    });
}

/// Logs what a scan stored at `info`, and its refusals at `warn` at most hourly (anyone can
/// mail the address). Returns when it last warned. Names reasons, never content.
fn log_scan(pass: &ScanPass, warned_at: Option<Instant>, now: Instant) -> Option<Instant> {
    if pass.stored > 0 || pass.duplicates > 0 {
        tracing::info!(
            "Mail intake: {} report(s) stored, {} duplicate(s)",
            pass.stored,
            pass.duplicates
        );
    }
    if pass.refused.is_empty() && pass.stuck == 0 {
        return warned_at;
    }
    match hourly_warning(warned_at, now) {
        HourlyWarning::Warn => {
            tracing::warn!(
                "Mail intake: messages refused and deleted: {:?}; {} left for the next scan",
                pass.refused,
                pass.stuck
            );
            Some(now)
        }
        HourlyWarning::Quiet => {
            tracing::debug!("Mail intake: refused {:?}", pass.refused);
            warned_at
        }
    }
}
