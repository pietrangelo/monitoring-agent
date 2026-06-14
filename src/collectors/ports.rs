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

use crate::models::ListeningPort;
use std::process::Command;

pub fn collect() -> Vec<ListeningPort> {
    let output = match Command::new("ss").args(["-tlnp", "-H"]).output() {
        Ok(o) if o.status.success() => o.stdout,
        _ => return vec![],
    };

    let mut ports: Vec<ListeningPort> = Vec::new();
    for line in String::from_utf8_lossy(&output).lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 4 {
            continue;
        }

        let local = fields.get(3).unwrap_or(&"");
        let process_raw = fields.get(6).unwrap_or(&"");

        let (addr, port_str) = match local.rsplit_once(':') {
            Some(p) => p,
            None => continue,
        };
        let port: u16 = port_str.parse().unwrap_or(0);
        if port == 0 {
            continue;
        }

        let protocol = if local.starts_with('[') || local.contains("::") {
            "tcp6"
        } else {
            "tcp"
        };

        let (proc_name, pid) = parse_process_info(process_raw);

        ports.push(ListeningPort {
            protocol: protocol.to_string(),
            local_address: addr.to_string(),
            local_port: port,
            process_name: proc_name,
            pid,
        });
    }

    ports.sort_by_key(|p| p.local_port);
    ports
}

fn parse_process_info(raw: &str) -> (Option<String>, Option<u32>) {
    let raw = raw.trim();
    if raw.is_empty() {
        return (None, None);
    }

    let inner = raw.trim_start_matches("users:((").trim_end_matches("))");

    let first = inner.split(')').next().unwrap_or("");
    let first = first.trim_start_matches('"');

    if let Some((name, rest)) = first.split_once('"') {
        let pid = rest
            .trim_start_matches(",pid=")
            .split(',')
            .next()
            .and_then(|s| s.parse().ok());
        (Some(name.to_string()), pid)
    } else {
        (None, None)
    }
}
