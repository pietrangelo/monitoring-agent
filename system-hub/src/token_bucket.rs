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

//! A token bucket on the monotonic clock, with its capacity and its refill period as
//! parameters (RFC 0007 §3). Pure: the caller hands in the time. RFC 0009's source pace and
//! RFC 0007's decode budget each wrap one; the bucket has no domain term of its own.

use std::num::NonZeroU8;
use std::time::{Duration, Instant};

/// How a token bucket fills: it holds at most `capacity` tokens, and gains one each `period`.
/// A zero period refills it at every take, so it never refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refill {
    capacity: NonZeroU8,
    period: Duration,
}

impl Refill {
    pub const fn new(capacity: NonZeroU8, period: Duration) -> Self {
        Self { capacity, period }
    }
}

/// A bucket had no token to spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Empty;

/// A token bucket: a value, spent by `take`, which returns the bucket that remains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBucket {
    refill: Refill,
    tokens: u8,
    last_refill: Instant,
}

impl TokenBucket {
    /// A full bucket, its refill clock starting at `now`.
    pub fn full(refill: Refill, now: Instant) -> Self {
        Self {
            refill,
            tokens: refill.capacity.get(),
            last_refill: now,
        }
    }

    /// Refills for the time elapsed, then spends one token.
    pub fn take(self, now: Instant) -> Result<Self, Empty> {
        let refilled = self.refill(now);
        let tokens = refilled.tokens.checked_sub(1).ok_or(Empty)?;
        Ok(Self { tokens, ..refilled })
    }

    /// Adds one token per whole period elapsed, keeping the remainder. A full bucket banks
    /// nothing: its clock restarts at `now`. Past the capacity, or with a zero period, the
    /// bucket is full.
    fn refill(self, now: Instant) -> Self {
        let capacity = self.refill.capacity.get();
        let missing = capacity.saturating_sub(self.tokens);
        let elapsed = now.saturating_duration_since(self.last_refill);
        // A zero period has no quotient: the bucket refills at every take, so it never refuses.
        let periods = elapsed
            .as_nanos()
            .checked_div(self.refill.period.as_nanos());
        match periods.map(u8::try_from) {
            Some(Ok(periods)) if periods < missing => Self {
                tokens: self.tokens + periods,
                last_refill: self.last_refill + self.refill.period * u32::from(periods),
                ..self
            },
            _ => Self {
                tokens: capacity,
                last_refill: now.max(self.last_refill),
                ..self
            },
        }
    }
}
