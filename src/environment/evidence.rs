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

//! What the agent observed about where it runs, parsed from the files and variables that
//! show it (RFC 0014 §3). The parsers are pure; `collectors/environment.rs` reads the inputs.

use super::cgroup::CgroupEvidence;
use super::{ContainerRuntime, Hypervisor};

/// Every observation `classify` weighs, as plain values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentEvidence {
    /// `/.dockerenv` exists.
    pub dockerenv: bool,
    /// `/run/.containerenv` exists (Podman, Buildah).
    pub containerenv: bool,
    /// The agent's own `container` environment variable.
    pub container_env_var: Option<ContainerMarker>,
    /// `/run/systemd/container`: the service manager's `$container`, which a service's clean
    /// environment doesn't inherit.
    pub systemd_container: Option<ContainerMarker>,
    /// `KUBERNETES_SERVICE_HOST` is set.
    pub kubernetes: bool,
    /// `hypervisor` is among `/proc/cpuinfo`'s CPU flags.
    pub cpu_hypervisor_flag: bool,
    pub architecture: CpuArchitecture,
    /// The hypervisor the DMI vendor or product names, if it names one.
    pub dmi_hypervisor: Option<Hypervisor>,
    /// `/sys/hypervisor/type` reads `xen`.
    pub xen: bool,
    /// The kernel release names Microsoft: a WSL kernel.
    pub wsl: bool,
    /// Where the agent's cgroup is, weighed only for a container.
    pub cgroup: CgroupEvidence,
}

impl EnvironmentEvidence {
    /// The container values found, the agent's own variable first.
    pub(super) fn markers(&self) -> impl Iterator<Item = ContainerMarker> {
        self.container_env_var
            .into_iter()
            .chain(self.systemd_container)
    }

    /// Whether an explicit marker says the agent runs in a container. `wsl` isn't one.
    pub(super) fn shows_container(&self) -> bool {
        self.dockerenv
            || self.containerenv
            || self.kubernetes
            || self.markers().any(ContainerMarker::is_container)
    }
}

/// The agent's compile-time CPU architecture, as far as classification cares: only x86 has
/// a CPU flag that tells a virtual machine from bare metal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuArchitecture {
    X86,
    NonX86,
}

impl CpuArchitecture {
    /// The architecture this binary was built for.
    pub const fn of_build() -> Self {
        if cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
            Self::X86
        } else {
            Self::NonX86
        }
    }
}

/// A `container` value, as systemd and container managers write it. `Other` is a container
/// manager this agent doesn't name: still a container marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerMarker {
    Podman,
    Lxc,
    SystemdNspawn,
    Docker,
    /// Not a container: systemd writes `wsl` on WSL.
    Wsl,
    Other,
}

impl ContainerMarker {
    /// The marker `value` names, or `None` when it's empty.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "" => None,
            "podman" => Some(Self::Podman),
            "lxc" => Some(Self::Lxc),
            "systemd-nspawn" => Some(Self::SystemdNspawn),
            "docker" => Some(Self::Docker),
            "wsl" => Some(Self::Wsl),
            _ => Some(Self::Other),
        }
    }

    fn is_container(self) -> bool {
        match self {
            Self::Podman | Self::Lxc | Self::SystemdNspawn | Self::Docker | Self::Other => true,
            Self::Wsl => false,
        }
    }

    /// The runtime an LXC or nspawn marker names.
    pub(super) fn lxc_or_nspawn(self) -> Option<ContainerRuntime> {
        match self {
            Self::Lxc => Some(ContainerRuntime::Lxc),
            Self::SystemdNspawn => Some(ContainerRuntime::SystemdNspawn),
            Self::Podman | Self::Docker | Self::Wsl | Self::Other => None,
        }
    }
}

/// Whether `/proc/cpuinfo` lists the `hypervisor` CPU flag.
pub fn has_hypervisor_flag(cpuinfo: &str) -> bool {
    cpuinfo
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| key.trim() == "flags")
        .any(|(_, flags)| flags.split_whitespace().any(|flag| flag == "hypervisor"))
}

/// Whether `/proc/sys/kernel/osrelease` is a WSL kernel's.
pub fn is_wsl_release(osrelease: &str) -> bool {
    osrelease.to_ascii_lowercase().contains("microsoft")
}

/// Whether `/sys/hypervisor/type` names Xen.
pub fn is_xen(hypervisor_type: &str) -> bool {
    hypervisor_type.trim() == "xen"
}

/// The hypervisor a DMI `sys_vendor` and `product_name` name, if either names one. A vendor
/// that also builds physical machines (Microsoft, Amazon, Google) names a hypervisor only
/// with its virtual product.
pub fn dmi_hypervisor(vendor: &str, product: &str) -> Option<Hypervisor> {
    use Hypervisor::*;
    match (vendor.trim(), product.trim()) {
        (_, "KVM") => Some(Kvm),
        ("QEMU", _) => Some(Qemu),
        ("VMware, Inc.", _) => Some(Vmware),
        ("Microsoft Corporation", "Virtual Machine") => Some(HyperV),
        ("Xen", _) => Some(Xen),
        ("innotek GmbH", _) | (_, "VirtualBox") => Some(VirtualBox),
        ("Amazon EC2", product) if !product.ends_with(".metal") => Some(AmazonEc2),
        ("Google", "Google Compute Engine") => Some(GoogleCompute),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_container_value_parses_into_its_marker() {
        let cases = [
            ("podman", "podman", Some(ContainerMarker::Podman)),
            ("lxc", "lxc", Some(ContainerMarker::Lxc)),
            (
                "nspawn",
                "systemd-nspawn",
                Some(ContainerMarker::SystemdNspawn),
            ),
            ("docker", "docker", Some(ContainerMarker::Docker)),
            ("wsl", "wsl", Some(ContainerMarker::Wsl)),
            (
                "systemd's trailing newline",
                "lxc\n",
                Some(ContainerMarker::Lxc),
            ),
            ("an unnamed manager", "oci", Some(ContainerMarker::Other)),
            (
                "lxc under libvirt is another manager",
                "lxc-libvirt",
                Some(ContainerMarker::Other),
            ),
            (
                "names are case-sensitive",
                "Docker",
                Some(ContainerMarker::Other),
            ),
            ("empty", "", None),
            ("only whitespace", " \n", None),
        ];
        for (name, value, expected) in cases {
            assert_eq!(ContainerMarker::parse(value), expected, "{name}");
        }
    }

    #[test]
    fn the_hypervisor_flag_is_a_whole_word_among_the_flags() {
        let cases = [
            (
                "a vm",
                "processor\t: 0\nflags\t\t: fpu vme hypervisor lahf_lm\n",
                true,
            ),
            ("the last flag", "flags\t\t: fpu hypervisor\n", true),
            (
                "bare metal",
                "processor\t: 0\nflags\t\t: fpu vme de pse\n",
                false,
            ),
            (
                "only inside another word",
                "flags\t\t: fpu nothypervisor hypervisorx\n",
                false,
            ),
            (
                "not in the flags line",
                "model name\t: hypervisor edition\nflags\t\t: fpu\n",
                false,
            ),
            (
                "no flags line (arm)",
                "processor\t: 0\nFeatures\t: fp asimd\n",
                false,
            ),
            ("empty", "", false),
        ];
        for (name, cpuinfo, expected) in cases {
            assert_eq!(has_hypervisor_flag(cpuinfo), expected, "{name}");
        }
    }

    #[test]
    fn a_wsl_kernel_names_microsoft_in_its_release() {
        let cases = [
            ("wsl2", "6.18.33.2-microsoft-standard-WSL2\n", true),
            ("wsl1", "4.4.0-19041-Microsoft\n", true),
            ("upper case", "5.10.0-MICROSOFT-standard\n", true),
            ("mixed case", "5.10.0-MicroSoft-standard\n", true),
            ("a distribution kernel", "6.8.0-45-generic\n", false),
            ("empty", "", false),
        ];
        for (name, release, expected) in cases {
            assert_eq!(is_wsl_release(release), expected, "{name}");
        }
    }

    #[test]
    fn only_xen_names_xen() {
        let cases = [
            ("xen", "xen\n", true),
            ("no newline", "xen", true),
            ("another hypervisor", "kvm\n", false),
            ("a longer word", "xenon\n", false),
            ("surrounding whitespace", " xen \n", true),
            ("the kernel writes it lower case", "Xen\n", false),
            ("empty", "", false),
        ];
        for (name, value, expected) in cases {
            assert_eq!(is_xen(value), expected, "{name}");
        }
    }

    #[test]
    fn dmi_names_the_hypervisors_it_knows_and_no_physical_machine() {
        use Hypervisor::*;
        let cases = [
            ("kvm", "QEMU", "KVM", Some(Kvm)),
            ("kvm by product alone", "Red Hat", "KVM", Some(Kvm)),
            ("qemu", "QEMU", "Standard PC (Q35 + ICH9, 2009)", Some(Qemu)),
            (
                "vmware",
                "VMware, Inc.",
                "VMware Virtual Platform",
                Some(Vmware),
            ),
            (
                "hyper-v",
                "Microsoft Corporation",
                "Virtual Machine",
                Some(HyperV),
            ),
            (
                "a surface laptop is not hyper-v",
                "Microsoft Corporation",
                "Surface Laptop 5",
                None,
            ),
            ("xen", "Xen", "HVM domU", Some(Xen)),
            ("virtualbox", "innotek GmbH", "VirtualBox", Some(VirtualBox)),
            ("ec2", "Amazon EC2", "m5.large", Some(AmazonEc2)),
            ("ec2 bare metal is physical", "Amazon EC2", "c5.metal", None),
            (
                "google compute engine",
                "Google",
                "Google Compute Engine",
                Some(GoogleCompute),
            ),
            ("a chromebook is not gce", "Google", "Eve", None),
            ("a physical machine", "Dell Inc.", "PowerEdge R640", None),
            ("trailing newlines", "QEMU\n", "KVM\n", Some(Kvm)),
            ("no dmi", "", "", None),
        ];
        for (name, vendor, product, expected) in cases {
            assert_eq!(dmi_hypervisor(vendor, product), expected, "{name}");
        }
    }
}
