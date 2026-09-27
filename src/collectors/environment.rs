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

//! Reads the evidence of the agent's execution environment from the files under a root path
//! and from the agent's own environment variables (RFC 0014 §3). It blocks: run it off the
//! runtime. A file that can't be read is no evidence, never an error.

use std::fs;
use std::io;
use std::path::Path;

use crate::environment::EnvironmentEvidence;
use crate::environment::evidence::{self, ContainerMarker, CpuArchitecture};

/// The evidence under `root`, with variables looked up through `var`.
pub fn gather_evidence(
    root: &Path,
    var: impl Fn(&str) -> Option<String>,
    architecture: CpuArchitecture,
) -> EnvironmentEvidence {
    let read = |path: &str| {
        fs::read_to_string(root.join(path))
            .inspect_err(|err| unreadable(path, err))
            .unwrap_or_default()
    };
    let exists = |path: &str| {
        root.join(path)
            .try_exists()
            .inspect_err(|err| unreadable(path, err))
            .unwrap_or(false)
    };
    let set = |key: &str| var(key).is_some_and(|value| !value.is_empty());
    EnvironmentEvidence {
        dockerenv: exists(".dockerenv"),
        containerenv: exists("run/.containerenv"),
        container_env_var: var("container").and_then(|v| ContainerMarker::parse(&v)),
        systemd_container: ContainerMarker::parse(&read("run/systemd/container")),
        kubernetes: set("KUBERNETES_SERVICE_HOST"),
        cpu_hypervisor_flag: evidence::has_hypervisor_flag(&read("proc/cpuinfo")),
        architecture,
        dmi_hypervisor: evidence::dmi_hypervisor(
            &read("sys/class/dmi/id/sys_vendor"),
            &read("sys/class/dmi/id/product_name"),
        ),
        xen: evidence::is_xen(&read("sys/hypervisor/type")),
        wsl: evidence::is_wsl_release(&read("proc/sys/kernel/osrelease")),
    }
}

/// Logs a source that exists but can't be read (a hardened profile, say), so its missing
/// evidence can be told apart from an absent file, which is the common case and stays quiet.
fn unreadable(path: &str, err: &io::Error) {
    if err.kind() != io::ErrorKind::NotFound {
        tracing::debug!("environment evidence /{path} is unreadable: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::Hypervisor;
    use std::path::PathBuf;

    /// A fake root holding `files`, removed when dropped.
    struct FakeRoot(PathBuf);

    impl FakeRoot {
        fn with(name: &str, files: &[(&str, &str)]) -> Self {
            let root = std::env::temp_dir()
                .join(format!("system-agent-env-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            for (path, content) in files {
                let file = root.join(path);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, content).unwrap();
            }
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for FakeRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn no_vars(_: &str) -> Option<String> {
        None
    }

    const VM_CPUINFO: &str = "processor\t: 0\nflags\t\t: fpu vme hypervisor\n";

    #[test]
    fn a_docker_container_on_kvm_shows_every_marker() {
        let root = FakeRoot::with(
            "docker",
            &[
                (".dockerenv", ""),
                ("proc/cpuinfo", VM_CPUINFO),
                ("proc/sys/kernel/osrelease", "6.8.0-45-generic\n"),
                ("sys/class/dmi/id/sys_vendor", "QEMU\n"),
                ("sys/class/dmi/id/product_name", "KVM\n"),
            ],
        );
        let evidence = gather_evidence(&root.0, no_vars, CpuArchitecture::X86);
        assert_eq!(
            evidence,
            EnvironmentEvidence {
                dockerenv: true,
                containerenv: false,
                container_env_var: None,
                systemd_container: None,
                kubernetes: false,
                cpu_hypervisor_flag: true,
                architecture: CpuArchitecture::X86,
                dmi_hypervisor: Some(Hypervisor::Kvm),
                xen: false,
                wsl: false,
            }
        );
    }

    #[test]
    fn a_rootless_podman_container_under_kubernetes_shows_its_variables() {
        let root = FakeRoot::with(
            "podman",
            &[
                ("run/.containerenv", "engine=\"podman-5.0\"\n"),
                ("run/systemd/container", "lxc\n"),
                ("sys/hypervisor/type", "xen\n"),
            ],
        );
        let var = |key: &str| match key {
            "container" => Some("podman".to_owned()),
            "KUBERNETES_SERVICE_HOST" => Some("10.96.0.1".to_owned()),
            _ => None,
        };
        let evidence = gather_evidence(&root.0, var, CpuArchitecture::NonX86);
        assert_eq!(
            evidence,
            EnvironmentEvidence {
                dockerenv: false,
                containerenv: true,
                container_env_var: Some(ContainerMarker::Podman),
                systemd_container: Some(ContainerMarker::Lxc),
                kubernetes: true,
                cpu_hypervisor_flag: false,
                architecture: CpuArchitecture::NonX86,
                dmi_hypervisor: None,
                xen: true,
                wsl: false,
            }
        );
    }

    /// The owner's machine, measured: systemd writes `wsl`, the kernel names Microsoft, the CPU
    /// has the flag and there is no DMI.
    #[test]
    fn wsl_shows_its_kernel_and_systemds_value() {
        let root = FakeRoot::with(
            "wsl",
            &[
                ("run/systemd/container", "wsl\n"),
                ("proc/cpuinfo", VM_CPUINFO),
                (
                    "proc/sys/kernel/osrelease",
                    "6.18.33.2-microsoft-standard-WSL2\n",
                ),
            ],
        );
        let evidence = gather_evidence(&root.0, no_vars, CpuArchitecture::X86);
        assert_eq!(evidence.systemd_container, Some(ContainerMarker::Wsl));
        assert!(evidence.wsl && evidence.cpu_hypervisor_flag);
        assert_eq!(evidence.dmi_hypervisor, None);
    }

    /// An empty root and empty variables are no evidence at all, not an error.
    #[test]
    fn nothing_readable_is_no_evidence() {
        let root = FakeRoot::with("empty", &[]);
        let var = |key: &str| match key {
            "container" | "KUBERNETES_SERVICE_HOST" => Some(String::new()),
            _ => None,
        };
        let evidence = gather_evidence(&root.0, var, CpuArchitecture::X86);
        assert_eq!(
            evidence,
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
            }
        );
    }
}
