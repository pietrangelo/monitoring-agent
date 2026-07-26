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

    parse_ss_output(&String::from_utf8_lossy(&output))
}

fn parse_ss_output(output: &str) -> Vec<ListeningPort> {
    let mut ports: Vec<ListeningPort> = output.lines().filter_map(parse_ss_line).collect();
    ports.sort_by_key(|p| p.local_port);
    ports
}

fn parse_ss_line(line: &str) -> Option<ListeningPort> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 4 {
        return None;
    }

    let local = fields.get(3).unwrap_or(&"");
    // `ss -tlnp -H` columns are: State Recv-Q Send-Q Local:Port Peer:Port Process (index 5),
    // not 6 -- verified against real `ss` output.
    let process_raw = fields.get(5).unwrap_or(&"");

    let (addr, port_str) = local.rsplit_once(':')?;
    let port: u16 = port_str.parse().unwrap_or(0);
    if port == 0 {
        return None;
    }

    let protocol = if local.starts_with('[') || local.contains("::") {
        "tcp6"
    } else {
        "tcp"
    };

    let (proc_name, pid) = parse_process_info(process_raw);

    Some(ListeningPort {
        protocol: protocol.to_string(),
        local_address: addr.to_string(),
        local_port: port,
        process_name: proc_name,
        pid,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_process_info_extracts_name_and_pid() {
        let (name, pid) = parse_process_info(r#"users:(("sshd",pid=1234,fd=3))"#);
        assert_eq!(name.as_deref(), Some("sshd"));
        assert_eq!(pid, Some(1234));
    }

    #[test]
    fn parse_process_info_empty_input() {
        let (name, pid) = parse_process_info("");
        assert_eq!(name, None);
        assert_eq!(pid, None);
    }

    #[test]
    fn parse_process_info_no_quotes_returns_none() {
        let (name, pid) = parse_process_info("garbage");
        assert_eq!(name, None);
        assert_eq!(pid, None);
    }

    #[test]
    fn parse_ss_line_tcp4_listening() {
        let line = "LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=100,fd=3))";
        let port = parse_ss_line(line).unwrap();
        assert_eq!(port.protocol, "tcp");
        assert_eq!(port.local_address, "0.0.0.0");
        assert_eq!(port.local_port, 22);
        assert_eq!(port.process_name.as_deref(), Some("sshd"));
        assert_eq!(port.pid, Some(100));
    }

    #[test]
    fn parse_ss_line_tcp6_listening() {
        let line = "LISTEN 0 128 [::]:80 [::]:* users:((\"nginx\",pid=200,fd=6))";
        let port = parse_ss_line(line).unwrap();
        assert_eq!(port.protocol, "tcp6");
        assert_eq!(port.local_port, 80);
    }

    #[test]
    fn parse_ss_line_too_few_fields_is_none() {
        assert!(parse_ss_line("LISTEN 0 128").is_none());
    }

    #[test]
    fn parse_ss_line_port_zero_is_none() {
        let line = "LISTEN 0 128 0.0.0.0:0 0.0.0.0:*";
        assert!(parse_ss_line(line).is_none());
    }

    #[test]
    fn parse_ss_line_non_numeric_port_is_none() {
        let line = "LISTEN 0 128 0.0.0.0:notaport 0.0.0.0:*";
        assert!(parse_ss_line(line).is_none());
    }

    #[test]
    fn parse_ss_line_missing_process_info_yields_none_process() {
        let line = "LISTEN 0 128 0.0.0.0:53 0.0.0.0:*";
        let port = parse_ss_line(line).unwrap();
        assert_eq!(port.process_name, None);
        assert_eq!(port.pid, None);
    }

    #[test]
    fn parse_ss_output_sorts_by_port_and_filters_invalid_lines() {
        let out = "LISTEN 0 1 0.0.0.0:443 0.0.0.0:*\n\
                    garbage line\n\
                    LISTEN 0 1 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=1,fd=1))\n";
        let ports = parse_ss_output(out);
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].local_port, 22);
        assert_eq!(ports[1].local_port, 443);
    }
}
