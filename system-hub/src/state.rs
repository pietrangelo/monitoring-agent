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

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::applications::{HeldRound, RecentRounds, RoundDigester, SourcePace};
use crate::db::Database;
use crate::snapshot::LiveMetrics;

/// What the hub holds in memory of one system's applications (RFC 0009 §8).
#[derive(Debug, Default)]
pub struct SystemApplications {
    /// The round the dashboard shows. A push disconnect keeps it; it goes stale on its own.
    pub shown: Option<HeldRound>,
    /// The rounds accepted most recently, surviving disconnects, so a re-send is recognised.
    pub recent: RecentRounds,
    /// The poller's pace for this system; `None` until its first stored round (a full bucket).
    pub poll_pace: Option<SourcePace>,
    /// When a refused polled round was last logged at `warn`.
    pub poll_refusal_warned_at: Option<Instant>,
}

pub struct AppState {
    pub db: Arc<Database>,
    /// Per-system live metrics, written with each stored snapshot, keyed by system id. Lock
    /// order: the database mutex, then this; never take the database mutex while holding it.
    pub live_metrics: RwLock<HashMap<String, Arc<LiveMetrics>>>,
    /// Per-system applications, keyed by system id. Lock order: the database mutex, then
    /// this; never take the database mutex while holding it.
    pub live_applications: RwLock<HashMap<String, SystemApplications>>,
    /// The one digest key every ingestion path shares.
    pub digester: RoundDigester,
}

impl AppState {
    pub fn new(db: Arc<Database>) -> Arc<Self> {
        Arc::new(Self {
            db,
            live_metrics: RwLock::new(HashMap::new()),
            live_applications: RwLock::new(HashMap::new()),
            digester: RoundDigester::new(),
        })
    }
}
