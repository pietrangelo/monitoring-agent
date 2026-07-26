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

use crate::models::*;
use std::fs;
use std::process::Command;
use sysinfo::{Disks, Networks, System};

pub fn collect() -> SystemSnapshot {
    let mut sys = System::new_all();
    sys.refresh_all();

    let hostname = System::host_name().unwrap_or_else(|| "unknown".into());
    let kernel = System::kernel_version().unwrap_or_else(|| "unknown".into());
    let os = parse_os_release();

    let uptime_secs = System::uptime();
    let uptime_display = format_uptime(uptime_secs);
    let load = System::load_average();

    let cpu = {
        let cpus = sys.cpus();
        let model = cpus
            .first()
            .map(|c| c.brand().to_string())
            .unwrap_or_default();
        let usage = cpus.iter().map(|c| c.cpu_usage()).sum::<f32>() / cpus.len() as f32;
        let freq = cpus.first().map(|c| c.frequency()).unwrap_or(0);
        let phys = sys.physical_core_count().unwrap_or(cpus.len());
        CpuInfo {
            model,
            physical_cores: phys,
            logical_cores: cpus.len(),
            usage_percent: (usage * 10.0).round() / 10.0,
            frequency_mhz: freq,
        }
    };

    let memory = {
        let total = sys.total_memory();
        let used = sys.used_memory();
        let free = sys.free_memory();
        let available = sys.available_memory();
        MemoryInfo {
            total_bytes: total,
            used_bytes: used,
            free_bytes: free,
            available_bytes: available,
            total_display: format_bytes(total),
            used_display: format_bytes(used),
            usage_percent: if total > 0 {
                ((used as f64 / total as f64) * 100.0) as f32
            } else {
                0.0
            },
        }
    };

    let swap = {
        let total = sys.total_swap();
        let used = sys.used_swap();
        let free = sys.free_swap();
        SwapInfo {
            total_bytes: total,
            used_bytes: used,
            free_bytes: free,
            total_display: format_bytes(total),
            used_display: format_bytes(used),
            usage_percent: if total > 0 {
                ((used as f64 / total as f64) * 100.0) as f32
            } else {
                0.0
            },
        }
    };

    let disks = {
        let d = Disks::new_with_refreshed_list();
        d.iter()
            .map(|disk| {
                let total = disk.total_space();
                let free = disk.available_space();
                let used = total.saturating_sub(free);
                DiskInfo {
                    mount_point: disk.mount_point().to_string_lossy().to_string(),
                    filesystem: disk.file_system().to_string_lossy().to_string(),
                    total_bytes: total,
                    used_bytes: used,
                    free_bytes: free,
                    total_display: format_bytes(total),
                    used_display: format_bytes(used),
                    usage_percent: if total > 0 {
                        ((used as f64 / total as f64) * 100.0) as f32
                    } else {
                        0.0
                    },
                }
            })
            .collect()
    };

    let networks = {
        let nets = Networks::new_with_refreshed_list();
        nets.iter()
            .map(|(name, data)| NetworkInfo {
                interface: name.to_string(),
                mac_address: data.mac_address().to_string(),
                ip_addresses: data
                    .ip_networks()
                    .iter()
                    .map(|ip| ip.addr.to_string())
                    .collect(),
                received_bytes: data.total_received(),
                transmitted_bytes: data.total_transmitted(),
                received_display: format_bytes(data.total_received()),
                transmitted_display: format_bytes(data.total_transmitted()),
            })
            .collect()
    };

    let top_processes = {
        let mut procs: Vec<_> = sys.processes().values().collect();
        procs.sort_by(|a, b| {
            b.cpu_usage()
                .partial_cmp(&a.cpu_usage())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        procs
            .iter()
            .take(20)
            .map(|p| ProcessInfo {
                pid: p.pid().as_u32(),
                name: p.name().to_string_lossy().to_string(),
                cpu_usage: (p.cpu_usage() * 10.0).round() / 10.0,
                memory_usage_bytes: p.memory(),
                memory_usage_display: format_bytes(p.memory()),
                memory_percent: if sys.total_memory() > 0 {
                    (p.memory() as f64 / sys.total_memory() as f64 * 100.0) as f32
                } else {
                    0.0
                },
                status: format!("{:?}", p.status()),
            })
            .collect()
    };

    SystemSnapshot {
        hostname,
        os,
        kernel,
        uptime_seconds: uptime_secs,
        uptime_display,
        load_average: LoadAverage {
            one: load.one,
            five: load.five,
            fifteen: load.fifteen,
        },
        cpu,
        memory,
        swap,
        disks,
        networks,
        top_processes,
    }
}

fn parse_os_release() -> OsInfo {
    let content = fs::read_to_string("/etc/os-release").unwrap_or_default();
    let mut os = parse_os_release_content(&content);

    if os.name.is_empty()
        && let Ok(out) = Command::new("lsb_release").args(["-ds"]).output()
    {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.len() > 1 {
            os.name = s.trim_matches('"').to_string();
        }
    }
    if os.pretty_name.is_empty() {
        os.pretty_name = os.name.clone();
    }

    os
}

fn parse_os_release_content(content: &str) -> OsInfo {
    let mut name = String::new();
    let mut version = String::new();
    let mut id = String::new();
    let mut pretty = String::new();

    for line in content.lines() {
        let val = |l: &str| {
            l.split('=')
                .nth(1)
                .unwrap_or("")
                .trim_matches('"')
                .to_string()
        };
        if line.starts_with("NAME=") && !line.starts_with("PRETTY_NAME=") {
            name = val(line);
        } else if line.starts_with("VERSION=") {
            version = val(line);
        } else if line.starts_with("ID=") {
            id = val(line);
        } else if line.starts_with("PRETTY_NAME=") {
            pretty = val(line);
        }
    }

    OsInfo {
        name,
        version,
        id,
        pretty_name: pretty,
    }
}

fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86400;
    let hours = (seconds % 86400) / 3600;
    let mins = (seconds % 3600) / 60;
    if days > 0 {
        format!("{}d {}h {}m", days, hours, mins)
    } else if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else {
        format!("{}m", mins)
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    format!("{:.1} {}", size, UNITS[unit_idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_zero() {
        assert_eq!(format_bytes(0), "0.0 B");
    }

    #[test]
    fn format_bytes_sub_kb() {
        assert_eq!(format_bytes(512), "512.0 B");
    }

    #[test]
    fn format_bytes_exact_kb() {
        assert_eq!(format_bytes(1024), "1.0 KB");
    }

    #[test]
    fn format_bytes_mb_gb_tb() {
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(format_bytes(1024u64.pow(4)), "1.0 TB");
    }

    #[test]
    fn format_bytes_caps_at_petabytes() {
        // Absurdly large value should stay clamped to the largest unit, not panic/index out of range.
        let huge = u64::MAX;
        let out = format_bytes(huge);
        assert!(out.ends_with("PB"));
    }

    #[test]
    fn format_uptime_minutes_only() {
        assert_eq!(format_uptime(90), "1m");
    }

    #[test]
    fn format_uptime_hours_and_minutes() {
        assert_eq!(format_uptime(3 * 3600 + 5 * 60), "3h 5m");
    }

    #[test]
    fn format_uptime_days_hours_minutes() {
        assert_eq!(format_uptime(2 * 86400 + 4 * 3600 + 10 * 60), "2d 4h 10m");
    }

    #[test]
    fn format_uptime_zero() {
        assert_eq!(format_uptime(0), "0m");
    }

    #[test]
    fn parse_os_release_content_full_fields() {
        let content =
            "NAME=\"Ubuntu\"\nVERSION=\"22.04\"\nID=ubuntu\nPRETTY_NAME=\"Ubuntu 22.04 LTS\"\n";
        let os = parse_os_release_content(content);
        assert_eq!(os.name, "Ubuntu");
        assert_eq!(os.version, "22.04");
        assert_eq!(os.id, "ubuntu");
        assert_eq!(os.pretty_name, "Ubuntu 22.04 LTS");
    }

    #[test]
    fn parse_os_release_content_empty_input() {
        let os = parse_os_release_content("");
        assert_eq!(os.name, "");
        assert_eq!(os.version, "");
        assert_eq!(os.id, "");
        assert_eq!(os.pretty_name, "");
    }

    #[test]
    fn parse_os_release_content_ignores_unrelated_lines() {
        let content = "SOME_OTHER_KEY=value\nID=fedora\n";
        let os = parse_os_release_content(content);
        assert_eq!(os.id, "fedora");
        assert_eq!(os.name, "");
    }

    #[test]
    fn parse_os_release_content_name_line_does_not_match_pretty_name() {
        // NAME= must not be confused with PRETTY_NAME= (both start differently but guard against substring bugs).
        let content = "PRETTY_NAME=\"Pretty\"\nNAME=\"Plain\"\n";
        let os = parse_os_release_content(content);
        assert_eq!(os.name, "Plain");
        assert_eq!(os.pretty_name, "Pretty");
    }
}
