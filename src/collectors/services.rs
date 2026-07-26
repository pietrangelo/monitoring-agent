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

    parse_service_lines(&String::from_utf8_lossy(&output))
}

fn parse_service_lines(raw: &str) -> Vec<ServiceInfo> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_service_lines() {
        let raw = r#"{"unit":"sshd.service","load":"loaded","active":"active","sub":"running","description":"OpenSSH server"}
{"unit":"cron.service","load":"loaded","active":"inactive","sub":"dead","description":"Cron scheduler"}"#;
        let services = parse_service_lines(raw);
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].name, "sshd.service");
        assert_eq!(services[0].load_state, "loaded");
        assert_eq!(services[0].active_state, "active");
        assert_eq!(services[0].sub_state, "running");
        assert_eq!(services[0].description, "OpenSSH server");
    }

    #[test]
    fn skips_malformed_json_lines() {
        let raw = "not json\n{\"unit\":\"ok.service\",\"load\":\"loaded\",\"active\":\"active\",\"sub\":\"running\",\"description\":\"\"}";
        let services = parse_service_lines(raw);
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].name, "ok.service");
    }

    #[test]
    fn skips_json_missing_required_fields() {
        let raw = r#"{"unit":"partial.service","load":"loaded"}"#;
        assert!(parse_service_lines(raw).is_empty());
    }

    #[test]
    fn empty_input_yields_empty_vec() {
        assert!(parse_service_lines("").is_empty());
    }
}
