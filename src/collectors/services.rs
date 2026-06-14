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


use crate::models::ServiceInfo;
use std::process::Command;

pub fn collect() -> Vec<ServiceInfo> {
    let output = match Command::new("systemctl")
        .args([
            "list-units",
            "--type=service",
            "--all",
            "--no-legend",
            "--no-pager",
            "--output=json",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return vec![],
    };

    let raw = String::from_utf8_lossy(&output);

    raw.lines()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            Some(ServiceInfo {
                name: v.get("unit")?.as_str()?.to_string(),
                load_state: v.get("load")?.as_str()?.to_string(),
                active_state: v.get("active")?.as_str()?.to_string(),
                sub_state: v.get("sub")?.as_str()?.to_string(),
                description: v.get("description")?.as_str()?.to_string(),
            })
        })
        .collect()
}
