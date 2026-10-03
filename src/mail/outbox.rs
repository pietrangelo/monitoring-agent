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

//! The outbox (RFC 0017 §5), pure: reports waiting for the relay, at most 288 (a day at the
//! default interval), the oldest dropped first; a temporary refusal retries with a backoff from
//! 30 s up to the mail interval, a permanent one drops the report.

use std::collections::VecDeque;
use std::time::Duration;

/// The most reports the outbox holds.
pub const MAX_QUEUED: usize = 288;
/// The first retry's delay.
pub const FIRST_RETRY: Duration = Duration::from_secs(30);

/// How the relay answered a send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendResult {
    Sent,
    /// A 4xx, or a connection or TLS failure: try again later.
    Temporary,
    /// A 5xx: the relay will never take it.
    Permanent,
}

/// Messages waiting for the relay, oldest first.
#[derive(Debug)]
pub struct Outbox<M> {
    queue: VecDeque<M>,
    /// Reports dropped since the last count was taken: for the hourly log line.
    dropped: u64,
    /// The delay before the next attempt after a temporary failure; `None` after a success.
    backoff: Option<Duration>,
}

impl<M> Default for Outbox<M> {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            dropped: 0,
            backoff: None,
        }
    }
}

impl<M> Outbox<M> {
    /// Queues a message; past `MAX_QUEUED`, the oldest is dropped and counted.
    pub fn push(&mut self, message: M) {
        if self.queue.len() == MAX_QUEUED {
            self.queue.pop_front();
            self.dropped += 1;
        }
        self.queue.push_back(message);
    }

    /// The message to send next.
    pub fn front(&self) -> Option<&M> {
        self.queue.front()
    }

    /// Records how sending the front message went, `max` being the mail interval. Returns how
    /// long to wait before the next attempt.
    pub fn after(&mut self, result: SendResult, max: Duration) -> Duration {
        match result {
            SendResult::Sent => {
                self.queue.pop_front();
                self.backoff = None;
                Duration::ZERO
            }
            SendResult::Permanent => {
                self.queue.pop_front();
                self.dropped += 1;
                self.backoff = None;
                Duration::ZERO
            }
            SendResult::Temporary => {
                let next = self.backoff.map_or(FIRST_RETRY, |wait| wait * 2).min(max);
                self.backoff = Some(next);
                next
            }
        }
    }

    /// How many messages were dropped since the last call.
    pub fn take_dropped(&mut self) -> u64 {
        std::mem::take(&mut self.dropped)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: Duration = Duration::from_secs(300);

    /// RFC 0017 §5: at most 288 queued; the oldest goes first, and is counted.
    #[test]
    fn the_outbox_drops_its_oldest_past_288() {
        let mut outbox = Outbox::default();
        for n in 0..(MAX_QUEUED + 2) {
            outbox.push(n);
        }

        assert_eq!(outbox.len(), MAX_QUEUED);
        assert_eq!(outbox.front(), Some(&2));
        assert_eq!(outbox.take_dropped(), 2);
        assert_eq!(outbox.take_dropped(), 0, "counted once");
    }

    /// RFC 0017 §5: a sent message leaves; a temporary failure keeps it and backs off from
    /// 30 s, doubling up to the mail interval; a permanent one drops it.
    #[test]
    fn a_send_is_retried_with_backoff_or_dropped() {
        use SendResult::*;
        let secs = Duration::from_secs;
        let mut outbox = Outbox::default();
        outbox.push("a");
        outbox.push("b");
        let steps = [
            (Temporary, secs(30), Some("a")),
            (Temporary, secs(60), Some("a")),
            (Temporary, secs(120), Some("a")),
            (Temporary, secs(240), Some("a")),
            (Temporary, secs(300), Some("a")),
            (Temporary, secs(300), Some("a")),
            (Sent, Duration::ZERO, Some("b")),
            (Permanent, Duration::ZERO, None),
        ];
        for (at, (result, wait, front)) in steps.into_iter().enumerate() {
            assert_eq!(outbox.after(result, MAX), wait, "step {at}: the wait");
            assert_eq!(outbox.front().copied(), front, "step {at}: what's next");
        }
        assert_eq!(outbox.take_dropped(), 1, "the permanent refusal is counted");
    }
}
