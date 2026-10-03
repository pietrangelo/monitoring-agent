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

//! The registry fill both push and mail ingestion run after a stored snapshot (RFC 0007 §4,
//! RFC 0014 §8): the system info while it's missing, the memory capacity, and the name taken
//! from the hostname. Blocking: runs on the blocking pool.

use crate::models::SystemId;
use crate::registry::{MemoryCapacity, memory_capacity_refresh, needs_system_info};
use crate::state::AppState;

/// What an agent reports of its system, for the registry fill.
#[derive(Debug)]
pub struct ReportedInfo {
    pub hostname: String,
    pub os_name: String,
    pub kernel: String,
    pub cpu_model: String,
    pub cpu_cores: usize,
    /// `None` when the reported capacity breaks `MemoryCapacity::reported`'s bounds.
    pub memory: Option<MemoryCapacity>,
}

/// Fills in system info while its hostname or OS is missing, refreshes its memory capacity,
/// and replaces a default name with the reported hostname. The status went into the store.
pub fn fill_registry(app: &AppState, system_id: &SystemId, info: &ReportedInfo, via: &str) {
    let id = system_id.as_str();
    let sys = match app.db.get_system(id) {
        Ok(Some(sys)) => sys,
        Ok(None) => return,
        Err(err) => {
            // A row no read can map (RFC 0007 §4): its snapshot is stored, its fill skipped.
            tracing::debug!("{via} from {id:?}: skipping the registry fill: {err}");
            return;
        }
    };
    if needs_system_info(&sys)
        && let Err(err) = app.db.update_system_info(
            id,
            Some(&info.os_name),
            Some(&info.hostname),
            Some(&info.kernel),
            Some(&info.cpu_model),
            Some(info.cpu_cores),
        )
    {
        tracing::warn!("{via} from {id:?}: couldn't record the system info: {err}");
    }
    if let Some(capacity) =
        memory_capacity_refresh(MemoryCapacity::stored(&sys).as_ref(), info.memory.clone())
        && let Err(err) = app.db.update_memory_capacity(id, &capacity)
    {
        tracing::warn!("{via} from {id:?}: couldn't refresh the memory capacity: {err}");
    }
    if system_id.is_default_name(&sys.name)
        && let Err(err) =
            app.db
                .update_system_config(id, Some(&info.hostname), None, None, None, None)
    {
        tracing::warn!("{via} from {id:?}: couldn't rename the system to its hostname: {err}");
    }
}
