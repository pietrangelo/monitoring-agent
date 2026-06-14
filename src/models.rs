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

use serde::Serialize;

// ── System ──────────────────────────────────────────────

#[derive(Serialize)]
pub struct SystemSnapshot {
    pub hostname: String,
    pub os: OsInfo,
    pub kernel: String,
    pub uptime_seconds: u64,
    pub uptime_display: String,
    pub load_average: LoadAverage,
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    pub swap: SwapInfo,
    pub disks: Vec<DiskInfo>,
    pub networks: Vec<NetworkInfo>,
    pub top_processes: Vec<ProcessInfo>,
}

#[derive(Serialize)]
pub struct OsInfo {
    pub name: String,
    pub version: String,
    pub id: String,
    pub pretty_name: String,
}

#[derive(Serialize)]
pub struct LoadAverage {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

#[derive(Serialize)]
pub struct CpuInfo {
    pub model: String,
    pub physical_cores: usize,
    pub logical_cores: usize,
    pub usage_percent: f32,
    pub frequency_mhz: u64,
}

#[derive(Serialize)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
    pub total_display: String,
    pub used_display: String,
    pub usage_percent: f32,
}

#[derive(Serialize)]
pub struct SwapInfo {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub total_display: String,
    pub used_display: String,
    pub usage_percent: f32,
}

#[derive(Serialize)]
pub struct DiskInfo {
    pub mount_point: String,
    pub filesystem: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub total_display: String,
    pub used_display: String,
    pub usage_percent: f32,
}

#[derive(Serialize)]
pub struct NetworkInfo {
    pub interface: String,
    pub mac_address: String,
    pub ip_addresses: Vec<String>,
    pub received_bytes: u64,
    pub transmitted_bytes: u64,
    pub received_display: String,
    pub transmitted_display: String,
}

#[derive(Serialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub cpu_usage: f32,
    pub memory_usage_bytes: u64,
    pub memory_usage_display: String,
    pub memory_percent: f32,
    pub status: String,
}

// ── Packages ────────────────────────────────────────────

#[derive(Serialize)]
pub struct PackageInfo {
    pub name: String,
    pub version: String,
    pub manager: String,
}

// ── Services ────────────────────────────────────────────

#[derive(Serialize)]
pub struct ServiceInfo {
    pub name: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub description: String,
}

// ── Containers (Docker) ────────────────────────────────

#[derive(Serialize)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub state: String,
    pub ports: String,
}

// ── Listening Ports ────────────────────────────────────

#[derive(Serialize)]
pub struct ListeningPort {
    pub protocol: String,
    pub local_address: String,
    pub local_port: u16,
    pub process_name: Option<String>,
    pub pid: Option<u32>,
}

// ── Health ─────────────────────────────────────────────

#[derive(Serialize)]
pub struct HealthStatus {
    pub status: String,
    pub timestamp: String,
    pub version: String,
}

// ── History ────────────────────────────────────────────

#[derive(Serialize, Clone)]
pub struct MetricPoint {
    pub timestamp: u64,
    pub value: f32,
}

#[derive(Serialize)]
pub struct HistoryResponse {
    pub metric: String,
    pub points: Vec<MetricPoint>,
    pub start_time: u64,
    pub end_time: u64,
}
