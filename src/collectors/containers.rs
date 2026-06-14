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

use crate::models::ContainerInfo;
use std::process::Command;

pub fn collect() -> Vec<ContainerInfo> {
    let output = match Command::new("docker")
        .args([
            "ps",
            "--all",
            "--format",
            "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.Status}}\t{{.State}}\t{{.Ports}}",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return vec![],
    };

    String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(6, '\t');
            Some(ContainerInfo {
                id: parts.next()?.to_string(),
                name: parts.next()?.to_string(),
                image: parts.next()?.to_string(),
                status: parts.next()?.to_string(),
                state: parts.next()?.to_string(),
                ports: parts.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}
