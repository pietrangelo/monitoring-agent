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

    parse_docker_ps(&String::from_utf8_lossy(&output))
}

fn parse_docker_ps(output: &str) -> Vec<ContainerInfo> {
    output
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_lines() {
        let out = "abc123\tweb\tnginx:latest\tUp 2 hours\trunning\t0.0.0.0:80->80/tcp\n\
                    def456\tdb\tpostgres:16\tExited (0) 1 day ago\texited\t";
        let containers = parse_docker_ps(out);
        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0].id, "abc123");
        assert_eq!(containers[0].name, "web");
        assert_eq!(containers[0].image, "nginx:latest");
        assert_eq!(containers[0].status, "Up 2 hours");
        assert_eq!(containers[0].state, "running");
        assert_eq!(containers[0].ports, "0.0.0.0:80->80/tcp");
        assert_eq!(containers[1].ports, "");
    }

    #[test]
    fn empty_output_yields_empty_vec() {
        assert!(parse_docker_ps("").is_empty());
    }

    #[test]
    fn short_line_missing_required_fields_is_skipped() {
        let out = "onlyid\tonlyname";
        assert!(parse_docker_ps(out).is_empty());
    }

    #[test]
    fn missing_ports_field_defaults_to_empty_string() {
        let out = "abc\tweb\tnginx\tUp\trunning";
        let containers = parse_docker_ps(out);
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].ports, "");
    }
}
