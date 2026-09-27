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

//! The agent's execution environment (RFC 0014): the pure domain core of Host Telemetry that
//! decides what the agent's readings are measured against. No I/O lives here.

pub mod cgroup;
pub mod evidence;
pub mod sourcing;
pub mod usage;

use cgroup::{CgroupAccess, CgroupUnreadable, MonitoredCgroup, Pid1Placement};
pub use evidence::EnvironmentEvidence;
use evidence::{ContainerMarker, CpuArchitecture};
use sourcing::ProcessView;

use std::fmt;

/// What the agent runs in, found once at startup (RFC 0014 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionEnvironment {
    BareMetal,
    VirtualMachine {
        hypervisor: Hypervisor,
    },
    Container {
        runtime: Option<ContainerRuntime>,
        /// Whether the agent found a cgroup v2 hierarchy to measure the container by.
        cgroup: CgroupAccess,
    },
    /// No rule matched: an ARM host with no DMI, for example.
    Undetermined,
}

/// A hypervisor the agent can name. `Other` is a hypervisor whose presence is evidenced (the
/// CPU flag) but whose name isn't: a variant, not a sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hypervisor {
    Kvm,
    Qemu,
    Vmware,
    HyperV,
    Wsl,
    Xen,
    VirtualBox,
    AmazonEc2,
    GoogleCompute,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerRuntime {
    Docker,
    Podman,
    Kubernetes,
    Lxc,
    SystemdNspawn,
}

/// Whose load average the kernel reports to the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadScope {
    /// The host's: a container shares its kernel's run queue.
    Host,
    /// The execution environment's own.
    Environment,
}

impl ExecutionEnvironment {
    /// Whose load average the agent reads here.
    pub fn load_scope(&self) -> LoadScope {
        match self {
            Self::Container { .. } => LoadScope::Host,
            Self::BareMetal | Self::VirtualMachine { .. } | Self::Undetermined => {
                LoadScope::Environment
            }
        }
    }

    /// The cgroup whose files the agent reads each tick: a container's, when it has a
    /// readable cgroup v2 hierarchy.
    pub fn monitored_cgroup(&self) -> Option<&MonitoredCgroup> {
        match self {
            Self::Container {
                cgroup: CgroupAccess::V2 { monitored },
                ..
            } => Some(monitored),
            Self::Container {
                cgroup: CgroupAccess::Unreadable(_),
                ..
            }
            | Self::BareMetal
            | Self::VirtualMachine { .. }
            | Self::Undetermined => None,
        }
    }

    /// Whose processes the agent lists here.
    pub fn process_view(&self) -> ProcessView {
        match self {
            Self::Container {
                cgroup: CgroupAccess::V2 { monitored },
                ..
            } => match monitored.pid1() {
                Pid1Placement::Inside => ProcessView::Workload,
                Pid1Placement::Outside => ProcessView::SharedNamespace,
            },
            Self::Container {
                cgroup: CgroupAccess::Unreadable(_),
                ..
            } => ProcessView::UnmeasuredContainer,
            Self::BareMetal | Self::VirtualMachine { .. } | Self::Undetermined => ProcessView::Host,
        }
    }
}

/// The execution environment `evidence` shows, by the first rule that matches (RFC 0014 §3):
/// an explicit container marker, then virtualisation, then x86 bare metal. Only a marker makes
/// a container: no limit or root filesystem is evidence of one.
pub fn classify(evidence: &EnvironmentEvidence) -> ExecutionEnvironment {
    use ExecutionEnvironment::*;
    if evidence.shows_container() {
        Container {
            runtime: container_runtime(evidence),
            cgroup: cgroup::cgroup_access(&evidence.cgroup),
        }
    } else if let Some(hypervisor) = hypervisor(evidence) {
        VirtualMachine { hypervisor }
    } else {
        match evidence.architecture {
            CpuArchitecture::X86 => BareMetal,
            CpuArchitecture::NonX86 => Undetermined,
        }
    }
}

/// A container's runtime, by precedence: Kubernetes, Podman, a named LXC or nspawn marker,
/// then Docker.
fn container_runtime(evidence: &EnvironmentEvidence) -> Option<ContainerRuntime> {
    let markers = || evidence.markers();
    let named = |marker| markers().any(|m| m == marker);
    if evidence.kubernetes {
        Some(ContainerRuntime::Kubernetes)
    } else if evidence.containerenv || named(ContainerMarker::Podman) {
        Some(ContainerRuntime::Podman)
    } else if let Some(runtime) = markers().find_map(ContainerMarker::lxc_or_nspawn) {
        Some(runtime)
    } else if evidence.dockerenv || named(ContainerMarker::Docker) {
        Some(ContainerRuntime::Docker)
    } else {
        None
    }
}

/// The hypervisor the evidence shows, if any: WSL first, then a DMI name, then Xen, then an
/// unnamed one behind the CPU flag.
fn hypervisor(evidence: &EnvironmentEvidence) -> Option<Hypervisor> {
    if evidence.wsl || evidence.markers().any(|m| m == ContainerMarker::Wsl) {
        Some(Hypervisor::Wsl)
    } else if let Some(named) = evidence.dmi_hypervisor {
        Some(named)
    } else if evidence.xen {
        Some(Hypervisor::Xen)
    } else if evidence.cpu_hypervisor_flag {
        Some(Hypervisor::Other)
    } else {
        None
    }
}

impl fmt::Display for ExecutionEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BareMetal => f.write_str("bare metal"),
            Self::VirtualMachine { hypervisor } => {
                write!(f, "virtual machine ({})", hypervisor.name())
            }
            Self::Container {
                runtime: None,
                cgroup,
            } => write!(f, "container, {cgroup}"),
            Self::Container {
                runtime: Some(runtime),
                cgroup,
            } => write!(f, "container ({}), {cgroup}", runtime.name()),
            Self::Undetermined => f.write_str("undetermined"),
        }
    }
}

impl fmt::Display for CgroupAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V2 { monitored } => write!(f, "cgroup v2 at {}", monitored.path_display()),
            Self::Unreadable(why) => write!(f, "cgroup unreadable: {}", why.reason()),
        }
    }
}

impl CgroupUnreadable {
    /// Why, as a log line says it.
    fn reason(self) -> &'static str {
        match self {
            Self::V1Only => "cgroup v1 only",
            Self::NoCgroupMount => "no cgroup2 mount",
            Self::SelfCgroupMissing => "/proc/self/cgroup unreadable",
            Self::OutsideMountRoot => "outside the cgroup2 mount",
        }
    }
}

impl Hypervisor {
    /// How a log line names it.
    fn name(self) -> &'static str {
        match self {
            Self::Kvm => "kvm",
            Self::Qemu => "qemu",
            Self::Vmware => "vmware",
            Self::HyperV => "hyper-v",
            Self::Wsl => "wsl",
            Self::Xen => "xen",
            Self::VirtualBox => "virtualbox",
            Self::AmazonEc2 => "amazon ec2",
            Self::GoogleCompute => "google compute engine",
            Self::Other => "unnamed hypervisor",
        }
    }
}

impl ContainerRuntime {
    /// How a log line names it.
    fn name(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
            Self::Kubernetes => "kubernetes",
            Self::Lxc => "lxc",
            Self::SystemdNspawn => "systemd-nspawn",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::cgroup::{Cgroup2Mount, CgroupEvidence, CgroupPath};
    use ContainerRuntime::*;
    use ExecutionEnvironment::*;

    /// An x86 host that shows nothing at all: bare metal.
    fn nothing() -> EnvironmentEvidence {
        EnvironmentEvidence {
            dockerenv: false,
            containerenv: false,
            container_env_var: None,
            systemd_container: None,
            kubernetes: false,
            cpu_hypervisor_flag: false,
            architecture: CpuArchitecture::X86,
            dmi_hypervisor: None,
            xen: false,
            wsl: false,
            cgroup: no_cgroup(),
        }
    }

    /// Cgroup evidence that finds nothing.
    fn no_cgroup() -> CgroupEvidence {
        CgroupEvidence {
            self_cgroup: Err(CgroupUnreadable::SelfCgroupMissing),
            mount: None,
            namespace_root_typed: false,
            pid1_cgroup: None,
        }
    }

    fn container(runtime: Option<ContainerRuntime>) -> ExecutionEnvironment {
        Container {
            runtime,
            cgroup: CgroupAccess::Unreadable(CgroupUnreadable::SelfCgroupMissing),
        }
    }

    fn vm(hypervisor: Hypervisor) -> ExecutionEnvironment {
        VirtualMachine { hypervisor }
    }

    #[test]
    fn classify_takes_the_first_rule_that_matches() {
        let e = nothing;
        let cases = [
            // Rule 1: only an explicit marker makes a container.
            (
                "docker",
                EnvironmentEvidence {
                    dockerenv: true,
                    ..e()
                },
                container(Some(Docker)),
            ),
            (
                "docker by its variable",
                EnvironmentEvidence {
                    container_env_var: Some(ContainerMarker::Docker),
                    ..e()
                },
                container(Some(Docker)),
            ),
            (
                "rootless podman",
                EnvironmentEvidence {
                    containerenv: true,
                    container_env_var: Some(ContainerMarker::Podman),
                    ..e()
                },
                container(Some(Podman)),
            ),
            (
                "podman's file alone",
                EnvironmentEvidence {
                    containerenv: true,
                    ..e()
                },
                container(Some(Podman)),
            ),
            (
                "podman's variable alone",
                EnvironmentEvidence {
                    container_env_var: Some(ContainerMarker::Podman),
                    ..e()
                },
                container(Some(Podman)),
            ),
            (
                "kubernetes",
                EnvironmentEvidence {
                    kubernetes: true,
                    ..e()
                },
                container(Some(Kubernetes)),
            ),
            (
                "kubernetes over docker's file",
                EnvironmentEvidence {
                    kubernetes: true,
                    dockerenv: true,
                    ..e()
                },
                container(Some(Kubernetes)),
            ),
            (
                "kubernetes over podman's file",
                EnvironmentEvidence {
                    kubernetes: true,
                    containerenv: true,
                    ..e()
                },
                container(Some(Kubernetes)),
            ),
            (
                "podman over docker",
                EnvironmentEvidence {
                    containerenv: true,
                    dockerenv: true,
                    ..e()
                },
                container(Some(Podman)),
            ),
            (
                "lxc",
                EnvironmentEvidence {
                    container_env_var: Some(ContainerMarker::Lxc),
                    ..e()
                },
                container(Some(Lxc)),
            ),
            (
                "nspawn",
                EnvironmentEvidence {
                    container_env_var: Some(ContainerMarker::SystemdNspawn),
                    ..e()
                },
                container(Some(SystemdNspawn)),
            ),
            (
                "a manager with no name",
                EnvironmentEvidence {
                    container_env_var: Some(ContainerMarker::Other),
                    ..e()
                },
                container(None),
            ),
            (
                "a service under an init in lxc",
                EnvironmentEvidence {
                    systemd_container: Some(ContainerMarker::Lxc),
                    ..e()
                },
                container(Some(Lxc)),
            ),
            (
                "a service under an init in nspawn",
                EnvironmentEvidence {
                    systemd_container: Some(ContainerMarker::SystemdNspawn),
                    ..e()
                },
                container(Some(SystemdNspawn)),
            ),
            (
                "an unnamed manager, systemd's file",
                EnvironmentEvidence {
                    systemd_container: Some(ContainerMarker::Other),
                    ..e()
                },
                container(None),
            ),
            (
                "a container inside a vm",
                EnvironmentEvidence {
                    dockerenv: true,
                    cpu_hypervisor_flag: true,
                    dmi_hypervisor: Some(Hypervisor::Kvm),
                    ..e()
                },
                container(Some(Docker)),
            ),
            (
                "a container on wsl",
                EnvironmentEvidence {
                    dockerenv: true,
                    wsl: true,
                    cpu_hypervisor_flag: true,
                    ..e()
                },
                container(Some(Docker)),
            ),
            (
                "a container on arm",
                EnvironmentEvidence {
                    dockerenv: true,
                    architecture: CpuArchitecture::NonX86,
                    ..e()
                },
                container(Some(Docker)),
            ),
            // Rule 2: WSL, then any other virtualisation.
            (
                "wsl2",
                EnvironmentEvidence {
                    wsl: true,
                    cpu_hypervisor_flag: true,
                    ..e()
                },
                vm(Hypervisor::Wsl),
            ),
            (
                "systemd's file reads wsl",
                EnvironmentEvidence {
                    systemd_container: Some(ContainerMarker::Wsl),
                    ..e()
                },
                vm(Hypervisor::Wsl),
            ),
            (
                "the variable reads wsl",
                EnvironmentEvidence {
                    container_env_var: Some(ContainerMarker::Wsl),
                    ..e()
                },
                vm(Hypervisor::Wsl),
            ),
            (
                "wsl over hyper-v's dmi",
                EnvironmentEvidence {
                    wsl: true,
                    dmi_hypervisor: Some(Hypervisor::HyperV),
                    ..e()
                },
                vm(Hypervisor::Wsl),
            ),
            (
                "kvm",
                EnvironmentEvidence {
                    cpu_hypervisor_flag: true,
                    dmi_hypervisor: Some(Hypervisor::Kvm),
                    ..e()
                },
                vm(Hypervisor::Kvm),
            ),
            (
                "vmware",
                EnvironmentEvidence {
                    cpu_hypervisor_flag: true,
                    dmi_hypervisor: Some(Hypervisor::Vmware),
                    ..e()
                },
                vm(Hypervisor::Vmware),
            ),
            (
                "hyper-v",
                EnvironmentEvidence {
                    cpu_hypervisor_flag: true,
                    dmi_hypervisor: Some(Hypervisor::HyperV),
                    ..e()
                },
                vm(Hypervisor::HyperV),
            ),
            (
                "xen",
                EnvironmentEvidence { xen: true, ..e() },
                vm(Hypervisor::Xen),
            ),
            (
                "xen with a dmi name",
                EnvironmentEvidence {
                    xen: true,
                    dmi_hypervisor: Some(Hypervisor::AmazonEc2),
                    ..e()
                },
                vm(Hypervisor::AmazonEc2),
            ),
            (
                "an unknown hypervisor",
                EnvironmentEvidence {
                    cpu_hypervisor_flag: true,
                    ..e()
                },
                vm(Hypervisor::Other),
            ),
            (
                "dmi without the flag",
                EnvironmentEvidence {
                    dmi_hypervisor: Some(Hypervisor::Qemu),
                    ..e()
                },
                vm(Hypervisor::Qemu),
            ),
            (
                "an arm vm",
                EnvironmentEvidence {
                    dmi_hypervisor: Some(Hypervisor::Qemu),
                    architecture: CpuArchitecture::NonX86,
                    ..e()
                },
                vm(Hypervisor::Qemu),
            ),
            // Rule 3: x86 with no sign of a hypervisor.
            // A MemoryMax service, an overlay root and an unmarked containerd container all
            // show this: the evidence has no field for a limit or a root filesystem, so none
            // of them can make a container.
            ("x86 bare metal, or no marker", e(), BareMetal),
            // Rule 4.
            (
                "arm with no evidence",
                EnvironmentEvidence {
                    architecture: CpuArchitecture::NonX86,
                    ..e()
                },
                Undetermined,
            ),
        ];
        for (name, evidence, expected) in cases {
            assert_eq!(classify(&evidence), expected, "{name}");
        }
    }

    /// Cgroup evidence of a Docker container: the agent at the root of a private namespace.
    fn docker_cgroup() -> CgroupEvidence {
        CgroupEvidence {
            self_cgroup: Ok(CgroupPath::ROOT),
            mount: Some(Cgroup2Mount {
                point: "/sys/fs/cgroup".into(),
                root: CgroupPath::ROOT,
            }),
            namespace_root_typed: true,
            pid1_cgroup: Some(CgroupPath::ROOT),
        }
    }

    #[test]
    fn only_a_container_is_measured_by_its_cgroup() {
        let marked = EnvironmentEvidence {
            dockerenv: true,
            cgroup: docker_cgroup(),
            ..nothing()
        };
        let Container { cgroup, .. } = classify(&marked) else {
            panic!("a marker makes a container");
        };
        assert_eq!(
            cgroup,
            cgroup::cgroup_access(&docker_cgroup()),
            "the evidence's access"
        );
        assert!(
            matches!(cgroup, CgroupAccess::V2 { .. }),
            "a readable v2 hierarchy"
        );

        // A MemoryMax service: a real, limited cgroup, and no marker. It is monitoring the
        // host, so no cgroup is ever read.
        let service = EnvironmentEvidence {
            cgroup: CgroupEvidence {
                self_cgroup: Ok(CgroupPath::parse("/system.slice/system-agent.service").unwrap()),
                namespace_root_typed: false,
                ..docker_cgroup()
            },
            ..nothing()
        };
        assert_eq!(classify(&service), BareMetal);
        assert_eq!(classify(&service).monitored_cgroup(), None);
        assert!(classify(&marked).monitored_cgroup().is_some());
    }

    #[test]
    fn a_containers_log_line_names_its_cgroup() {
        let with = |cgroup| Container {
            runtime: Some(Docker),
            cgroup,
        };
        let v2 = cgroup::cgroup_access(&docker_cgroup());
        let nested = cgroup::cgroup_access(&CgroupEvidence {
            self_cgroup: Ok(CgroupPath::parse("/system.slice/docker-abc.scope").unwrap()),
            namespace_root_typed: false,
            ..docker_cgroup()
        });
        let stripped = cgroup::cgroup_access(&CgroupEvidence {
            self_cgroup: Ok(CgroupPath::parse("/docker/abc").unwrap()),
            mount: Some(Cgroup2Mount {
                point: "/sys/fs/cgroup".into(),
                root: CgroupPath::parse("/docker/abc").unwrap(),
            }),
            namespace_root_typed: false,
            pid1_cgroup: None,
        });
        let cases = [
            (with(v2), "container (docker), cgroup v2 at /"),
            (
                with(stripped),
                "container (docker), cgroup v2 at /docker/abc",
            ),
            (
                with(nested),
                "container (docker), cgroup v2 at /system.slice/docker-abc.scope",
            ),
            (
                with(CgroupAccess::Unreadable(CgroupUnreadable::V1Only)),
                "container (docker), cgroup unreadable: cgroup v1 only",
            ),
            (
                with(CgroupAccess::Unreadable(CgroupUnreadable::NoCgroupMount)),
                "container (docker), cgroup unreadable: no cgroup2 mount",
            ),
            (
                with(CgroupAccess::Unreadable(CgroupUnreadable::OutsideMountRoot)),
                "container (docker), cgroup unreadable: outside the cgroup2 mount",
            ),
        ];
        for (environment, expected) in cases {
            assert_eq!(environment.to_string(), expected);
        }
    }

    #[test]
    fn whose_processes_the_agent_lists() {
        let with = |cgroup| Container {
            runtime: None,
            cgroup,
        };
        let shared = cgroup::cgroup_access(&CgroupEvidence {
            pid1_cgroup: None,
            ..docker_cgroup()
        });
        let cases = [
            ("bare metal", BareMetal, ProcessView::Host),
            ("a vm", vm(Hypervisor::Kvm), ProcessView::Host),
            ("undetermined", Undetermined, ProcessView::Host),
            (
                "a container with no v2 hierarchy",
                container(Some(Docker)),
                ProcessView::UnmeasuredContainer,
            ),
            (
                "a container whose pid 1 is inside",
                with(cgroup::cgroup_access(&docker_cgroup())),
                ProcessView::Workload,
            ),
            (
                "a container sharing the host's pid namespace",
                with(shared),
                ProcessView::SharedNamespace,
            ),
        ];
        for (name, environment, expected) in cases {
            assert_eq!(environment.process_view(), expected, "{name}");
        }
    }

    #[test]
    fn only_a_container_reports_the_hosts_load() {
        let cases = [
            ("bare metal", BareMetal, LoadScope::Environment),
            ("a vm", vm(Hypervisor::Kvm), LoadScope::Environment),
            ("a container", container(Some(Docker)), LoadScope::Host),
            ("an unnamed container", container(None), LoadScope::Host),
            ("undetermined", Undetermined, LoadScope::Environment),
        ];
        for (name, environment, expected) in cases {
            assert_eq!(environment.load_scope(), expected, "{name}");
        }
    }

    #[test]
    fn an_environment_reads_as_its_log_line_names_it() {
        use Hypervisor::*;
        let cases = [
            (BareMetal, "bare metal"),
            (vm(Kvm), "virtual machine (kvm)"),
            (vm(Qemu), "virtual machine (qemu)"),
            (vm(Vmware), "virtual machine (vmware)"),
            (vm(HyperV), "virtual machine (hyper-v)"),
            (vm(Wsl), "virtual machine (wsl)"),
            (vm(Xen), "virtual machine (xen)"),
            (vm(VirtualBox), "virtual machine (virtualbox)"),
            (vm(AmazonEc2), "virtual machine (amazon ec2)"),
            (vm(GoogleCompute), "virtual machine (google compute engine)"),
            (vm(Other), "virtual machine (unnamed hypervisor)"),
            (
                container(Some(Docker)),
                "container (docker), cgroup unreadable: /proc/self/cgroup unreadable",
            ),
            (
                container(Some(Podman)),
                "container (podman), cgroup unreadable: /proc/self/cgroup unreadable",
            ),
            (
                container(Some(Kubernetes)),
                "container (kubernetes), cgroup unreadable: /proc/self/cgroup unreadable",
            ),
            (
                container(Some(Lxc)),
                "container (lxc), cgroup unreadable: /proc/self/cgroup unreadable",
            ),
            (
                container(Some(SystemdNspawn)),
                "container (systemd-nspawn), cgroup unreadable: /proc/self/cgroup unreadable",
            ),
            (
                container(None),
                "container, cgroup unreadable: /proc/self/cgroup unreadable",
            ),
            (Undetermined, "undetermined"),
        ];
        for (environment, expected) in cases {
            assert_eq!(environment.to_string(), expected, "{environment:?}");
        }
    }
}
