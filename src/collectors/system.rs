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
use std::path::Path;
use std::process::Command;
use sysinfo::{Disks, Networks, System};

/// Processes kept in a snapshot, busiest first.
const TOP_PROCESSES: usize = 20;

/// Everything one collection reads from sysinfo and the OS, before the snapshot's rules are
/// applied. `os` is the one field already resolved: its `lsb_release` fallback runs while
/// reading, and only when the os-release file names no OS.
pub(crate) struct RawReadings {
    pub hostname: Option<String>,
    pub kernel: Option<String>,
    pub os: OsInfo,
    pub uptime_secs: u64,
    pub load: LoadAverage,
    pub cpus: Vec<RawCpu>,
    pub physical_cores: Option<usize>,
    pub memory: RawMemory,
    pub swap: RawSwap,
    pub disks: Vec<RawDisk>,
    pub networks: Vec<RawNetwork>,
    pub processes: Vec<RawProcess>,
}

pub(crate) struct RawCpu {
    pub brand: String,
    pub usage: f32,
    pub frequency_mhz: u64,
}

pub(crate) struct RawMemory {
    pub total: u64,
    pub used: u64,
    pub free: u64,
    pub available: u64,
}

pub(crate) struct RawSwap {
    pub total: u64,
    pub used: u64,
    pub free: u64,
}

pub(crate) struct RawDisk {
    pub mount_point: String,
    pub filesystem: String,
    pub total: u64,
    pub available: u64,
}

pub(crate) struct RawNetwork {
    pub interface: String,
    pub mac_address: String,
    pub ip_addresses: Vec<String>,
    pub received: u64,
    pub transmitted: u64,
}

pub(crate) struct RawProcess {
    pub pid: u32,
    pub name: String,
    pub cpu_usage: f32,
    pub memory: u64,
    pub status: String,
}

/// Refreshes `sys` in place, then reads it and the OS release file under `root`. A long-lived
/// `sys` is what makes CPU usage a reading over the time since its previous refresh.
pub(crate) fn gather(sys: &mut System, root: &Path) -> RawReadings {
    sys.refresh_all();
    let load = System::load_average();
    RawReadings {
        hostname: System::host_name(),
        kernel: System::kernel_version(),
        os: read_os_release(root),
        uptime_secs: System::uptime(),
        load: LoadAverage {
            one: load.one,
            five: load.five,
            fifteen: load.fifteen,
        },
        cpus: sys
            .cpus()
            .iter()
            .map(|c| RawCpu {
                brand: c.brand().to_string(),
                usage: c.cpu_usage(),
                frequency_mhz: c.frequency(),
            })
            .collect(),
        physical_cores: sys.physical_core_count(),
        memory: RawMemory {
            total: sys.total_memory(),
            used: sys.used_memory(),
            free: sys.free_memory(),
            available: sys.available_memory(),
        },
        swap: RawSwap {
            total: sys.total_swap(),
            used: sys.used_swap(),
            free: sys.free_swap(),
        },
        disks: gather_disks(),
        networks: gather_networks(),
        processes: gather_processes(sys),
    }
}

fn gather_disks() -> Vec<RawDisk> {
    Disks::new_with_refreshed_list()
        .iter()
        .map(|disk| RawDisk {
            mount_point: disk.mount_point().to_string_lossy().to_string(),
            filesystem: disk.file_system().to_string_lossy().to_string(),
            total: disk.total_space(),
            available: disk.available_space(),
        })
        .collect()
}

fn gather_networks() -> Vec<RawNetwork> {
    Networks::new_with_refreshed_list()
        .iter()
        .map(|(name, data)| RawNetwork {
            interface: name.to_string(),
            mac_address: data.mac_address().to_string(),
            ip_addresses: data
                .ip_networks()
                .iter()
                .map(|ip| ip.addr.to_string())
                .collect(),
            received: data.total_received(),
            transmitted: data.total_transmitted(),
        })
        .collect()
}

fn gather_processes(sys: &System) -> Vec<RawProcess> {
    sys.processes()
        .values()
        .map(|p| RawProcess {
            pid: p.pid().as_u32(),
            name: p.name().to_string_lossy().to_string(),
            cpu_usage: p.cpu_usage(),
            memory: p.memory(),
            status: format!("{:?}", p.status()),
        })
        .collect()
}

/// The snapshot one collection's readings make.
pub(crate) fn snapshot_from(raw: RawReadings) -> SystemSnapshot {
    let top_processes = top_processes(raw.processes, raw.memory.total);
    SystemSnapshot {
        hostname: raw.hostname.unwrap_or_else(|| "unknown".into()),
        os: raw.os,
        kernel: raw.kernel.unwrap_or_else(|| "unknown".into()),
        uptime_seconds: raw.uptime_secs,
        uptime_display: format_uptime(raw.uptime_secs),
        load_average: raw.load,
        cpu: cpu_info(&raw.cpus, raw.physical_cores),
        memory: memory_info(&raw.memory),
        swap: swap_info(&raw.swap),
        disks: raw.disks.into_iter().map(disk_info).collect(),
        networks: raw.networks.into_iter().map(network_info).collect(),
        top_processes,
    }
}

fn cpu_info(cpus: &[RawCpu], physical_cores: Option<usize>) -> CpuInfo {
    let usage = cpus.iter().map(|c| c.usage).sum::<f32>() / cpus.len() as f32;
    CpuInfo {
        model: cpus.first().map(|c| c.brand.clone()).unwrap_or_default(),
        physical_cores: physical_cores.unwrap_or(cpus.len()),
        logical_cores: cpus.len(),
        usage_percent: (usage * 10.0).round() / 10.0,
        frequency_mhz: cpus.first().map(|c| c.frequency_mhz).unwrap_or(0),
    }
}

fn memory_info(memory: &RawMemory) -> MemoryInfo {
    MemoryInfo {
        total_bytes: memory.total,
        used_bytes: memory.used,
        free_bytes: memory.free,
        available_bytes: memory.available,
        total_display: format_bytes(memory.total),
        used_display: format_bytes(memory.used),
        usage_percent: percent_of(memory.used, memory.total),
    }
}

fn swap_info(swap: &RawSwap) -> SwapInfo {
    SwapInfo {
        total_bytes: swap.total,
        used_bytes: swap.used,
        free_bytes: swap.free,
        total_display: format_bytes(swap.total),
        used_display: format_bytes(swap.used),
        usage_percent: percent_of(swap.used, swap.total),
    }
}

fn disk_info(disk: RawDisk) -> DiskInfo {
    let used = disk.total.saturating_sub(disk.available);
    DiskInfo {
        mount_point: disk.mount_point,
        filesystem: disk.filesystem,
        total_bytes: disk.total,
        used_bytes: used,
        free_bytes: disk.available,
        total_display: format_bytes(disk.total),
        used_display: format_bytes(used),
        usage_percent: percent_of(used, disk.total),
    }
}

fn network_info(net: RawNetwork) -> NetworkInfo {
    NetworkInfo {
        received_display: format_bytes(net.received),
        transmitted_display: format_bytes(net.transmitted),
        interface: net.interface,
        mac_address: net.mac_address,
        ip_addresses: net.ip_addresses,
        received_bytes: net.received,
        transmitted_bytes: net.transmitted,
    }
}

/// The busiest processes first, with memory taken as a share of `total_memory`.
fn top_processes(mut processes: Vec<RawProcess>, total_memory: u64) -> Vec<ProcessInfo> {
    processes.sort_by(|a, b| {
        b.cpu_usage
            .partial_cmp(&a.cpu_usage)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    processes
        .into_iter()
        .take(TOP_PROCESSES)
        .map(|p| ProcessInfo {
            pid: p.pid,
            cpu_usage: (p.cpu_usage * 10.0).round() / 10.0,
            memory_usage_display: format_bytes(p.memory),
            memory_percent: percent_of(p.memory, total_memory),
            memory_usage_bytes: p.memory,
            name: p.name,
            status: p.status,
        })
        .collect()
}

/// `part` as a percentage of `whole`, 0 when `whole` is 0.
fn percent_of(part: u64, whole: u64) -> f32 {
    if whole > 0 {
        (part as f64 / whole as f64 * 100.0) as f32
    } else {
        0.0
    }
}

fn read_os_release(root: &Path) -> OsInfo {
    let content = fs::read_to_string(root.join("etc/os-release")).unwrap_or_default();
    os_from(&content, lsb_description)
}

fn lsb_description() -> Option<String> {
    let out = Command::new("lsb_release").args(["-ds"]).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The OS described by an os-release file, asking `lsb` (run only when the file names no
/// OS) for a description to fall back on.
fn os_from(content: &str, lsb: impl FnOnce() -> Option<String>) -> OsInfo {
    let mut os = parse_os_release_content(content);
    if os.name.is_empty()
        && let Some(description) = lsb()
    {
        let s = description.trim();
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
pub(crate) mod fixtures {
    use super::*;

    pub fn cpu(brand: &str, usage: f32, frequency_mhz: u64) -> RawCpu {
        RawCpu {
            brand: brand.into(),
            usage,
            frequency_mhz,
        }
    }

    pub fn process(pid: u32, cpu_usage: f32, memory: u64) -> RawProcess {
        RawProcess {
            pid,
            name: format!("p{pid}"),
            cpu_usage,
            memory,
            status: "Run".into(),
        }
    }

    pub fn raw() -> RawReadings {
        RawReadings {
            hostname: Some("host-a".into()),
            kernel: Some("6.6.1".into()),
            os: parse_os_release_content("PRETTY_NAME=\"Debian 12\"\n"),
            uptime_secs: 2 * 86400 + 3600,
            load: LoadAverage {
                one: 0.5,
                five: 0.25,
                fifteen: 0.125,
            },
            cpus: vec![cpu("Xeon", 10.04, 2400), cpu("Other", 20.0, 1200)],
            physical_cores: Some(1),
            memory: RawMemory {
                total: 4096,
                used: 1024,
                free: 2048,
                available: 3072,
            },
            swap: RawSwap {
                total: 1000,
                used: 250,
                free: 750,
            },
            disks: vec![],
            networks: vec![],
            processes: vec![],
        }
    }

    /// `raw()` with every CPU at `usage`, so the snapshot's CPU usage is `usage`.
    pub fn raw_with_cpu(usage: f32) -> RawReadings {
        RawReadings {
            cpus: vec![cpu("Xeon", usage, 2400)],
            ..raw()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
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
    fn snapshot_carries_identity_and_uptime() {
        let cases = [
            (
                "both known",
                Some("host-a"),
                Some("6.6.1"),
                ("host-a", "6.6.1"),
            ),
            ("both unknown", None, None, ("unknown", "unknown")),
            (
                "only the hostname known",
                Some("host-a"),
                None,
                ("host-a", "unknown"),
            ),
            (
                "only the kernel known",
                None,
                Some("6.6.1"),
                ("unknown", "6.6.1"),
            ),
        ];
        for (name, hostname, kernel, expected) in cases {
            let snap = snapshot_from(RawReadings {
                hostname: hostname.map(Into::into),
                kernel: kernel.map(Into::into),
                ..raw()
            });
            assert_eq!(
                (snap.hostname.as_str(), snap.kernel.as_str()),
                expected,
                "{name}"
            );
            assert_eq!(snap.uptime_seconds, 2 * 86400 + 3600, "{name}");
            assert_eq!(snap.uptime_display, "2d 1h 0m", "{name}");
            assert_eq!(snap.os.pretty_name, "Debian 12", "{name}");
            let load = &snap.load_average;
            assert_eq!(
                (load.one, load.five, load.fifteen),
                (0.5, 0.25, 0.125),
                "{name}"
            );
        }
    }

    #[test]
    fn cpu_usage_is_the_mean_of_every_cpu_rounded_to_a_tenth() {
        let cases = [
            (
                "two cpus, physical count known",
                vec![cpu("Xeon", 10.04, 2400), cpu("Other", 20.0, 1200)],
                Some(1),
                ("Xeon", 1, 2, 2400),
            ),
            (
                "physical count unknown falls back to the logical count",
                vec![
                    cpu("Arm", 50.0, 0),
                    cpu("Arm", 50.0, 0),
                    cpu("Arm", 50.0, 0),
                ],
                None,
                ("Arm", 3, 3, 0),
            ),
            ("no cpus", vec![], None, ("", 0, 0, 0)),
        ];
        for (name, cpus, physical, (model, phys, logical, freq)) in cases {
            let c = snapshot_from(RawReadings {
                cpus,
                physical_cores: physical,
                ..raw()
            })
            .cpu;
            assert_eq!(c.model, model, "{name}");
            assert_eq!(c.physical_cores, phys, "{name}");
            assert_eq!(c.logical_cores, logical, "{name}");
            assert_eq!(c.frequency_mhz, freq, "{name}");
        }
        let mean = |cpus| {
            snapshot_from(RawReadings { cpus, ..raw() })
                .cpu
                .usage_percent
        };
        assert_eq!(mean(vec![cpu("a", 10.04, 0), cpu("b", 20.0, 0)]), 15.0);
        assert_eq!(mean(vec![cpu("a", 33.36, 0)]), 33.4);
        // Surprising, pinned as found: no cpus divides 0 by 0.
        assert!(mean(vec![]).is_nan());
    }

    #[test]
    fn memory_and_swap_usage_are_shares_of_their_own_totals() {
        let cases = [
            ("a quarter, half", (4096, 1024, 25.0), (1000, 500, 50.0)),
            ("full, a tenth", (1000, 1000, 100.0), (2000, 200, 10.0)),
            ("unrounded, empty", (3, 1, 33.333332), (4, 0, 0.0)),
            ("one-byte totals", (1, 1, 100.0), (1, 0, 0.0)),
            ("no totals", (0, 0, 0.0), (0, 0, 0.0)),
        ];
        for (name, (mem_total, mem_used, mem_pct), (swap_total, swap_used, swap_pct)) in cases {
            let snap = snapshot_from(RawReadings {
                memory: RawMemory {
                    total: mem_total,
                    used: mem_used,
                    free: 7,
                    available: 9,
                },
                swap: RawSwap {
                    total: swap_total,
                    used: swap_used,
                    free: 5,
                },
                ..raw()
            });
            let (m, w) = (&snap.memory, &snap.swap);
            assert_eq!(m.usage_percent, mem_pct, "{name}: memory");
            assert_eq!(w.usage_percent, swap_pct, "{name}: swap");
            assert_eq!(
                (m.total_bytes, m.used_bytes),
                (mem_total, mem_used),
                "{name}"
            );
            assert_eq!(
                (w.total_bytes, w.used_bytes),
                (swap_total, swap_used),
                "{name}"
            );
            assert_eq!(
                (m.free_bytes, m.available_bytes, w.free_bytes),
                (7, 9, 5),
                "{name}"
            );
            assert_eq!(m.total_display, format_bytes(mem_total), "{name}");
            assert_eq!(m.used_display, format_bytes(mem_used), "{name}");
            assert_eq!(w.total_display, format_bytes(swap_total), "{name}");
            assert_eq!(w.used_display, format_bytes(swap_used), "{name}");
        }
    }

    #[test]
    fn disk_usage_is_total_minus_available() {
        let cases: [(&str, u64, u64, (u64, f32)); 3] = [
            ("half used", 2048, 1024, (1024, 50.0)),
            ("more available than total saturates", 100, 150, (0, 0.0)),
            ("empty disk", 0, 0, (0, 0.0)),
        ];
        for (name, total, available, (used, percent)) in cases {
            let snap = snapshot_from(RawReadings {
                disks: vec![RawDisk {
                    mount_point: "/data".into(),
                    filesystem: "ext4".into(),
                    total,
                    available,
                }],
                ..raw()
            });
            let d = &snap.disks[0];
            assert_eq!(
                (d.mount_point.as_str(), d.filesystem.as_str()),
                ("/data", "ext4"),
                "{name}"
            );
            assert_eq!(
                (d.total_bytes, d.used_bytes, d.free_bytes),
                (total, used, available),
                "{name}"
            );
            assert_eq!(d.usage_percent, percent, "{name}");
            assert_eq!(d.total_display, format_bytes(total), "{name}");
            assert_eq!(d.used_display, format_bytes(used), "{name}");
        }
    }

    #[test]
    fn every_disk_is_kept_in_order() {
        let disk = |mount: &str, total| RawDisk {
            mount_point: mount.into(),
            filesystem: "xfs".into(),
            total,
            available: 0,
        };
        let snap = snapshot_from(RawReadings {
            disks: vec![disk("/", 10), disk("/home", 20), disk("/var", 30)],
            ..raw()
        });
        let disks: Vec<(&str, u64)> = snap
            .disks
            .iter()
            .map(|d| (d.mount_point.as_str(), d.total_bytes))
            .collect();
        assert_eq!(disks, vec![("/", 10), ("/home", 20), ("/var", 30)]);
    }

    #[test]
    fn network_totals_are_shown_in_bytes() {
        let snap = snapshot_from(RawReadings {
            networks: vec![
                RawNetwork {
                    interface: "eth0".into(),
                    mac_address: "00:11:22:33:44:55".into(),
                    ip_addresses: vec!["10.0.0.2".into(), "fe80::1".into()],
                    received: 2048,
                    transmitted: 3 * 1024 * 1024,
                },
                RawNetwork {
                    interface: "lo".into(),
                    mac_address: "00:00:00:00:00:00".into(),
                    ip_addresses: vec![],
                    received: 0,
                    transmitted: 0,
                },
            ],
            ..raw()
        });
        let n = &snap.networks[0];
        assert_eq!(n.interface, "eth0");
        assert_eq!(n.mac_address, "00:11:22:33:44:55");
        assert_eq!(
            n.ip_addresses,
            vec!["10.0.0.2".to_string(), "fe80::1".to_string()]
        );
        assert_eq!(
            (n.received_bytes, n.transmitted_bytes),
            (2048, 3 * 1024 * 1024)
        );
        assert_eq!(
            (n.received_display.as_str(), n.transmitted_display.as_str()),
            ("2.0 KB", "3.0 MB")
        );
        let lo = &snap.networks[1];
        assert_eq!((lo.interface.as_str(), lo.ip_addresses.len()), ("lo", 0));
        assert_eq!(snap.networks.len(), 2);
    }

    #[test]
    fn top_processes_are_the_twenty_busiest_with_memory_over_total_memory() {
        let processes = (1..=25)
            .map(|pid| process(pid, pid as f32 + 0.04, 1024))
            .collect();
        let top = top_processes(processes, 4096);
        let pids: Vec<u32> = top.iter().map(|p| p.pid).collect();
        assert_eq!(pids, (6..=25).rev().collect::<Vec<u32>>());
        let first = &top[0];
        assert_eq!(first.name, "p25");
        assert_eq!(first.cpu_usage, 25.0);
        assert_eq!(first.memory_usage_bytes, 1024);
        assert_eq!(first.memory_usage_display, "1.0 KB");
        assert_eq!(first.memory_percent, 25.0);
        assert_eq!(first.status, "Run");

        let cases: [(&str, u64, f32); 2] = [
            ("over total memory", 2048, 50.0),
            ("no total memory", 0, 0.0),
        ];
        for (name, total, expected) in cases {
            let top = top_processes(vec![process(1, 1.0, 1024)], total);
            assert_eq!(top[0].memory_percent, expected, "{name}");
        }
        assert!(top_processes(vec![], 4096).is_empty());

        // A process whose CPU usage isn't a number compares equal to everything: the sort
        // neither panics nor drops it.
        let with_nan = vec![
            process(1, f32::NAN, 0),
            process(2, 5.0, 0),
            process(3, 1.0, 0),
        ];
        let mut pids: Vec<u32> = top_processes(with_nan, 4096)
            .iter()
            .map(|p| p.pid)
            .collect();
        pids.sort();
        assert_eq!(pids, vec![1, 2, 3]);
    }

    #[test]
    fn snapshot_keeps_the_top_processes_over_its_memory_total() {
        let snap = snapshot_from(RawReadings {
            processes: vec![process(7, 1.0, 1024), process(9, 2.0, 2048)],
            ..raw()
        });
        let pids: Vec<(u32, f32)> = snap
            .top_processes
            .iter()
            .map(|p| (p.pid, p.memory_percent))
            .collect();
        assert_eq!(pids, vec![(9, 50.0), (7, 25.0)]);
    }

    #[test]
    fn os_falls_back_to_lsb_release_only_when_the_file_names_no_os() {
        let cases = [
            (
                "named by the file",
                "NAME=\"Fedora\"\nPRETTY_NAME=\"Fedora 40\"\n",
                Some("Other"),
                ("Fedora", "Fedora 40"),
                false,
            ),
            (
                "pretty name falls back to the name",
                "NAME=Alpine\n",
                None,
                ("Alpine", "Alpine"),
                false,
            ),
            (
                "lsb description, quoted and padded",
                "",
                Some("  \"Arch Linux\"\n"),
                ("Arch Linux", "Arch Linux"),
                true,
            ),
            ("lsb description too short", "", Some("x\n"), ("", ""), true),
            (
                "lsb description of two characters",
                "",
                Some("xy\n"),
                ("xy", "xy"),
                true,
            ),
            (
                "quotes count toward the length before they're trimmed",
                "",
                Some("\"x\"\n"),
                ("x", "x"),
                true,
            ),
            ("lsb unavailable", "", None, ("", ""), true),
            (
                "file with only a pretty name",
                "PRETTY_NAME=\"Mystery\"\n",
                Some("Lsb OS"),
                ("Lsb OS", "Mystery"),
                true,
            ),
        ];
        for (name, content, lsb, (os_name, pretty), asks_lsb) in cases {
            let mut asked = false;
            let os = os_from(content, || {
                asked = true;
                lsb.map(Into::into)
            });
            assert_eq!(
                (os.name.as_str(), os.pretty_name.as_str()),
                (os_name, pretty),
                "{name}"
            );
            assert_eq!(asked, asks_lsb, "{name}: whether lsb_release was asked");
        }
    }

    #[test]
    fn os_release_is_read_under_the_root() {
        let root =
            std::env::temp_dir().join(format!("system-agent-os-root-{}", std::process::id()));
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(
            root.join("etc/os-release"),
            "NAME=\"Rooted\"\nVERSION=\"1\"\nID=rooted\n",
        )
        .unwrap();
        let os = read_os_release(&root);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            (os.name.as_str(), os.version.as_str(), os.id.as_str()),
            ("Rooted", "1", "rooted")
        );
        assert_eq!(os.pretty_name, "Rooted");
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
