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

//! The head (RFC 0010 §7, §9): every active series' state, in 16 shards. Only the writer
//! thread changes it; queries copy a series' unsealed data out under its shard's lock. A shard
//! lock is a leaf: nothing else is taken while it is held.

use std::collections::HashMap;
use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::series::{SeriesId, SeriesKey};
use crate::state::{Sealed, SeriesState};
use crate::tier::SpanStart;

const SHARDS: usize = 16;

/// One active series.
#[derive(Clone, Debug)]
pub(crate) struct HeadEntry {
    pub id: SeriesId,
    pub state: SeriesState,
    /// Per tier, the last span holding a chunk of the series (sealed, committed or not).
    pub last_spans: [Option<SpanStart>; 3],
    /// Points appended since the series' tail was last written.
    pub dirty: bool,
    /// Chunks sealed and not yet committed: queries read them from here until the commit.
    pub pending: Vec<Sealed>,
}

pub(crate) type Shard = HashMap<SeriesKey, HeadEntry>;

pub(crate) struct Head {
    shards: Vec<Mutex<Shard>>,
}

impl Head {
    pub(crate) fn new() -> Head {
        Head {
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
        }
    }

    /// The shard holding a series. A poisoned lock means the writer panicked, which fails the
    /// store: its data is still readable for what it is.
    pub(crate) fn shard(&self, key: &SeriesKey) -> MutexGuard<'_, Shard> {
        let index = BuildHasherDefault::<DefaultHasher>::default().hash_one(key) as usize % SHARDS;
        self.shards[index]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Every shard in turn, for the writer's whole-head passes (the sweep, a close).
    pub(crate) fn each_shard(&self, mut f: impl FnMut(&mut Shard)) {
        for shard in &self.shards {
            f(&mut shard.lock().unwrap_or_else(PoisonError::into_inner));
        }
    }
}
