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

use crate::environment::cgroup::{Bytes, CgroupAccess, ResourceLimit};
use crate::environment::sourcing::ReadingSource;
use crate::environment::{ContainerRuntime, ExecutionEnvironment, Hypervisor, LoadScope};

// ── Execution environment ───────────────────────────────

/// `/api/system`'s `environment` object (RFC 0014 §7): the wire shape of an
/// `ExecutionEnvironment`, converted at the edge.
#[derive(Serialize)]
pub struct EnvironmentInfo {
    kind: EnvironmentKind,
    /// A container's runtime; `null` when unknown or not a container.
    runtime: Option<RuntimeName>,
    /// A virtual machine's hypervisor, `other` when unnamed; `null` when not a virtual machine.
    hypervisor: Option<HypervisorName>,
    /// A container's cgroup: `v2` or `unreadable`; `null` when not a container.
    cgroup: Option<CgroupName>,
    load_scope: LoadScopeName,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum CgroupName {
    V2,
    Unreadable,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum EnvironmentKind {
    BareMetal,
    VirtualMachine,
    Container,
    Undetermined,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeName {
    Docker,
    Podman,
    Kubernetes,
    Lxc,
    SystemdNspawn,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum HypervisorName {
    Kvm,
    Qemu,
    Vmware,
    #[serde(rename = "hyperv")]
    HyperV,
    Wsl,
    Xen,
    #[serde(rename = "virtualbox")]
    VirtualBox,
    AmazonEc2,
    GoogleCompute,
    Other,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum LoadScopeName {
    Host,
    Environment,
}

impl From<&ExecutionEnvironment> for EnvironmentInfo {
    fn from(environment: &ExecutionEnvironment) -> Self {
        let (kind, runtime, hypervisor, cgroup) = match environment {
            ExecutionEnvironment::BareMetal => (EnvironmentKind::BareMetal, None, None, None),
            ExecutionEnvironment::VirtualMachine { hypervisor } => (
                EnvironmentKind::VirtualMachine,
                None,
                Some((*hypervisor).into()),
                None,
            ),
            ExecutionEnvironment::Container { runtime, cgroup } => (
                EnvironmentKind::Container,
                runtime.map(RuntimeName::from),
                None,
                Some(cgroup.into()),
            ),
            ExecutionEnvironment::Undetermined => (EnvironmentKind::Undetermined, None, None, None),
        };
        Self {
            kind,
            runtime,
            hypervisor,
            cgroup,
            load_scope: environment.load_scope().into(),
        }
    }
}

impl From<&CgroupAccess> for CgroupName {
    fn from(access: &CgroupAccess) -> Self {
        match access {
            CgroupAccess::V2 { .. } => Self::V2,
            CgroupAccess::Unreadable(_) => Self::Unreadable,
        }
    }
}

impl From<ContainerRuntime> for RuntimeName {
    fn from(runtime: ContainerRuntime) -> Self {
        match runtime {
            ContainerRuntime::Docker => Self::Docker,
            ContainerRuntime::Podman => Self::Podman,
            ContainerRuntime::Kubernetes => Self::Kubernetes,
            ContainerRuntime::Lxc => Self::Lxc,
            ContainerRuntime::SystemdNspawn => Self::SystemdNspawn,
        }
    }
}

impl From<Hypervisor> for HypervisorName {
    fn from(hypervisor: Hypervisor) -> Self {
        match hypervisor {
            Hypervisor::Kvm => Self::Kvm,
            Hypervisor::Qemu => Self::Qemu,
            Hypervisor::Vmware => Self::Vmware,
            Hypervisor::HyperV => Self::HyperV,
            Hypervisor::Wsl => Self::Wsl,
            Hypervisor::Xen => Self::Xen,
            Hypervisor::VirtualBox => Self::VirtualBox,
            Hypervisor::AmazonEc2 => Self::AmazonEc2,
            Hypervisor::GoogleCompute => Self::GoogleCompute,
            Hypervisor::Other => Self::Other,
        }
    }
}

impl From<LoadScope> for LoadScopeName {
    fn from(scope: LoadScope) -> Self {
        match scope {
            LoadScope::Host => Self::Host,
            LoadScope::Environment => Self::Environment,
        }
    }
}

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
    /// CPUs the monitored environment may use: the host's cores outside a container.
    pub capacity_cpus: f64,
    /// The share of CPU time a hypervisor withheld since the previous snapshot, host-wide;
    /// `null` when it can't be measured.
    pub steal_percent: Option<f32>,
    pub source: ReadingSourceName,
}

/// Where a reading group's values came from, as the API names it (RFC 0014 §7).
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadingSourceName {
    Cgroup,
    Kernel,
    Unavailable,
}

/// A cgroup's memory or swap limit on the wire: `{"bounded": <bytes>}` or `"unbounded"`.
#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LimitInfo {
    Bounded(u64),
    Unbounded,
}

impl From<ReadingSource> for ReadingSourceName {
    fn from(source: ReadingSource) -> Self {
        match source {
            ReadingSource::Cgroup => Self::Cgroup,
            ReadingSource::Kernel => Self::Kernel,
            ReadingSource::Unavailable => Self::Unavailable,
        }
    }
}

impl From<ResourceLimit<Bytes>> for LimitInfo {
    fn from(limit: ResourceLimit<Bytes>) -> Self {
        match limit {
            ResourceLimit::Bounded(bytes) => Self::Bounded(bytes.get()),
            ResourceLimit::Unbounded => Self::Unbounded,
        }
    }
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
    /// The monitored cgroup's tightest limit; absent outside a container's cgroup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<LimitInfo>,
    pub source: ReadingSourceName,
}

#[derive(Serialize)]
pub struct SwapInfo {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub total_display: String,
    pub used_display: String,
    pub usage_percent: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<LimitInfo>,
    pub source: ReadingSourceName,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sources_and_limits_are_named_on_the_wire() {
        let sources = [
            (ReadingSource::Cgroup, json!("cgroup")),
            (ReadingSource::Kernel, json!("kernel")),
            (ReadingSource::Unavailable, json!("unavailable")),
        ];
        for (source, expected) in sources {
            let got = serde_json::to_value(ReadingSourceName::from(source)).unwrap();
            assert_eq!(got, expected, "{source:?}");
        }
        let limits = [
            (
                ResourceLimit::Bounded(Bytes::new(536_870_912)),
                json!({ "bounded": 536_870_912 }),
            ),
            (
                ResourceLimit::Bounded(Bytes::new(0)),
                json!({ "bounded": 0 }),
            ),
            (ResourceLimit::Unbounded, json!("unbounded")),
        ];
        for (limit, expected) in limits {
            let got = serde_json::to_value(LimitInfo::from(limit)).unwrap();
            assert_eq!(got, expected, "{limit:?}");
        }
    }

    #[test]
    fn an_environment_goes_on_the_wire_by_its_kind_runtime_and_hypervisor() {
        use ExecutionEnvironment::*;
        let vm = |hypervisor| VirtualMachine { hypervisor };
        use crate::environment::cgroup::{
            Cgroup2Mount, CgroupAccess, CgroupEvidence, CgroupPath, CgroupUnreadable, cgroup_access,
        };
        let unreadable = CgroupAccess::Unreadable(CgroupUnreadable::OutsideMountRoot);
        let container = |runtime| Container {
            runtime,
            cgroup: unreadable.clone(),
        };
        let v2 = cgroup_access(&CgroupEvidence {
            self_cgroup: Ok(CgroupPath::ROOT),
            mount: Some(Cgroup2Mount {
                point: "/sys/fs/cgroup".into(),
                root: CgroupPath::ROOT,
            }),
            namespace_root_typed: true,
            pid1_cgroup: Some(CgroupPath::ROOT),
        });
        let wire = |kind: &str,
                    runtime: Option<&str>,
                    hypervisor: Option<&str>,
                    cgroup: Option<&str>,
                    load: &str| json!({ "kind": kind, "runtime": runtime, "hypervisor": hypervisor, "cgroup": cgroup, "load_scope": load });
        let cases = [
            (
                BareMetal,
                wire("bare_metal", None, None, None, "environment"),
            ),
            (
                Undetermined,
                wire("undetermined", None, None, None, "environment"),
            ),
            (
                container(None),
                wire("container", None, None, Some("unreadable"), "host"),
            ),
            (
                Container {
                    runtime: None,
                    cgroup: v2,
                },
                wire("container", None, None, Some("v2"), "host"),
            ),
        ]
        .into_iter()
        .chain(
            [
                (Hypervisor::Kvm, "kvm"),
                (Hypervisor::Qemu, "qemu"),
                (Hypervisor::Vmware, "vmware"),
                (Hypervisor::HyperV, "hyperv"),
                (Hypervisor::Wsl, "wsl"),
                (Hypervisor::Xen, "xen"),
                (Hypervisor::VirtualBox, "virtualbox"),
                (Hypervisor::AmazonEc2, "amazon_ec2"),
                (Hypervisor::GoogleCompute, "google_compute"),
                (Hypervisor::Other, "other"),
            ]
            .map(|(h, name)| {
                (
                    vm(h),
                    wire("virtual_machine", None, Some(name), None, "environment"),
                )
            }),
        )
        .chain(
            [
                (ContainerRuntime::Docker, "docker"),
                (ContainerRuntime::Podman, "podman"),
                (ContainerRuntime::Kubernetes, "kubernetes"),
                (ContainerRuntime::Lxc, "lxc"),
                (ContainerRuntime::SystemdNspawn, "systemd_nspawn"),
            ]
            .map(|(r, name)| {
                (
                    container(Some(r)),
                    wire("container", Some(name), None, Some("unreadable"), "host"),
                )
            }),
        );
        for (environment, expected) in cases {
            let got = serde_json::to_value(EnvironmentInfo::from(&environment)).unwrap();
            assert_eq!(got, expected, "{environment:?}");
        }
    }
}
