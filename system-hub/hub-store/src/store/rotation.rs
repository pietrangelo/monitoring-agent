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

//! The rotating tail flush (RFC 0010 §6): every series' tail written once per rotation, a
//! share of them at each commit.

use std::time::{Duration, Instant};

use crate::series::SeriesId;

/// One rotation of the tail flush: every series' tail written once in it, sized by time.
pub(super) struct Rotation {
    pub(super) started_seq: u64,
    started: Instant,
    queue: Vec<SeriesId>,
    done: usize,
}

impl Rotation {
    pub(super) fn start(seq: u64, ids: impl Iterator<Item = SeriesId>) -> Rotation {
        let mut queue: Vec<SeriesId> = ids.collect();
        queue.sort_unstable();
        Rotation {
            started_seq: seq,
            started: Instant::now(),
            queue,
            done: 0,
        }
    }

    /// The series due by now: the share of the queue the elapsed share of the period covers.
    pub(super) fn due(&mut self, period: Duration) -> Vec<SeriesId> {
        let elapsed = self.started.elapsed().as_secs_f64();
        let share = if period.is_zero() {
            1.0
        } else {
            (elapsed / period.as_secs_f64()).min(1.0)
        };
        let target = ((self.queue.len() as f64 * share).ceil() as usize).min(self.queue.len());
        let due = self.queue[self.done.min(target)..target].to_vec();
        self.done = self.done.max(target);
        due
    }

    pub(super) fn finished(&self) -> bool {
        self.done >= self.queue.len()
    }
}
