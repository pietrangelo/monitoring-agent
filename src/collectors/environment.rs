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
//! and from the agent's own environment variables (RFC 0014 §3), and each tick's files from
//! the monitored cgroup (§4). It blocks: run it off the runtime. At startup a file that can't
//! be read is no evidence, never an error; a cgroup file that can't be read on a tick fails
//! its reading group.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::environment::EnvironmentEvidence;
use crate::environment::cgroup::{
    self, CgroupEvidence, CgroupFile, CgroupReadError, CgroupUnreadable, MonitoredCgroup,
    ResourceLimit,
};
use crate::environment::evidence::{self, ContainerMarker, CpuArchitecture};
use crate::environment::sourcing::{CgroupRead, CgroupReadings, CpuFiles, MemoryFiles, SwapFiles};
use crate::environment::usage::CpuCounters;

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
        cgroup: gather_cgroup_evidence(root),
    }
}

/// Where the agent's cgroup is, from `/proc/self/cgroup`, `/proc/self/mountinfo` and
/// `/proc/1/cgroup` under `root`.
pub fn gather_cgroup_evidence(root: &Path) -> CgroupEvidence {
    let read = |path: &str| {
        fs::read_to_string(root.join(path))
            .inspect_err(|err| unreadable(path, err))
            .ok()
    };
    let self_cgroup = read("proc/self/cgroup")
        .map_or(Err(CgroupUnreadable::SelfCgroupMissing), |content| {
            cgroup::parse_self_cgroup(&content)
        });
    let mount = read("proc/self/mountinfo").and_then(|m| cgroup::parse_cgroup2_mount(&m));
    let namespace_root_typed = mount.as_ref().is_some_and(|mount| {
        let point = mount.point.strip_prefix("/").unwrap_or(&mount.point);
        root.join(point).join("cgroup.type").is_file()
    });
    CgroupEvidence {
        self_cgroup,
        mount,
        namespace_root_typed,
        pid1_cgroup: read("proc/1/cgroup").and_then(|c| cgroup::parse_pid1_cgroup(&c)),
    }
}

/// This tick's files from `monitored` under `root`, the CPU counters stamped `read_at`.
pub fn read_cgroup(root: &Path, monitored: &MonitoredCgroup, read_at: Instant) -> CgroupReadings {
    let levels: Vec<PathBuf> = monitored.levels().iter().map(|l| root.join(l)).collect();
    let own = &root.join(monitored.directory());
    CgroupReadings {
        cpu: group(own, CgroupFile::CpuStat, cgroup::parse_cpu_usage, |usage| {
            Ok(CpuFiles {
                counters: CpuCounters { usage, read_at },
                quotas: limits(&levels, CgroupFile::CpuMax, cgroup::parse_cpu_max)?,
                cpuset: limit(own, CgroupFile::CpusetCpusEffective, |c| {
                    cgroup::parse_cpuset(c).map(ResourceLimit::Bounded)
                })?,
            })
        }),
        memory: group(
            own,
            CgroupFile::MemoryCurrent,
            cgroup::parse_bytes,
            |current| {
                Ok(MemoryFiles {
                    current,
                    inactive_file: required(
                        own,
                        CgroupFile::MemoryStat,
                        cgroup::parse_inactive_file,
                    )?,
                    limits: limits(&levels, CgroupFile::MemoryMax, cgroup::parse_memory_max)?,
                })
            },
        ),
        swap: group(
            own,
            CgroupFile::MemorySwapCurrent,
            cgroup::parse_bytes,
            |current| {
                Ok(SwapFiles {
                    current,
                    limits: limits(&levels, CgroupFile::MemorySwapMax, cgroup::parse_memory_max)?,
                })
            },
        ),
    }
}

/// A reading group: `Absent` when its usage file isn't in `dir`, else its usage and the rest of
/// its files, or the first file that failed.
fn group<U, T>(
    dir: &Path,
    usage_file: CgroupFile,
    parse: fn(&str) -> Option<U>,
    rest: impl FnOnce(U) -> Result<T, CgroupReadError>,
) -> CgroupRead<T> {
    match parsed(dir, usage_file, parse) {
        Ok(None) => CgroupRead::Absent,
        Ok(Some(usage)) => rest(usage).map_or_else(CgroupRead::Failed, CgroupRead::Read),
        Err(err) => CgroupRead::Failed(err),
    }
}

/// `file` in `dir`, parsed: `None` when it isn't there.
fn parsed<T>(
    dir: &Path,
    file: CgroupFile,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Result<Option<T>, CgroupReadError> {
    match fs::read_to_string(dir.join(file.name())) {
        Ok(content) => parse(&content)
            .map(Some)
            .ok_or(CgroupReadError::Malformed(file)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(CgroupReadError::Unreadable {
            file,
            reason: err.to_string(),
        }),
    }
}

/// `file` in `dir`, parsed; its absence is a failure too.
fn required<T>(
    dir: &Path,
    file: CgroupFile,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Result<T, CgroupReadError> {
    parsed(dir, file, parse)?.ok_or(CgroupReadError::Missing(file))
}

/// One limit file in `dir`: unbounded when it isn't there.
fn limit<T>(
    dir: &Path,
    file: CgroupFile,
    parse: impl FnOnce(&str) -> Option<ResourceLimit<T>>,
) -> Result<ResourceLimit<T>, CgroupReadError> {
    Ok(parsed(dir, file, parse)?.unwrap_or(ResourceLimit::Unbounded))
}

/// A limit file at every level, deepest first.
fn limits<T>(
    levels: &[PathBuf],
    file: CgroupFile,
    parse: fn(&str) -> Option<ResourceLimit<T>>,
) -> Result<Vec<ResourceLimit<T>>, CgroupReadError> {
    levels.iter().map(|dir| limit(dir, file, parse)).collect()
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
    use crate::environment::cgroup::{
        Bytes, Cgroup2Mount, CgroupAccess, CgroupFile, CgroupPath, CgroupReadError, CpuCount,
        ResourceLimit, cgroup_access,
    };
    use crate::environment::sourcing::{CpuFiles, MemoryFiles, SwapFiles};
    use crate::environment::usage::CpuCounters;
    use std::path::PathBuf;
    use std::time::Duration;

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
                cgroup: nothing_cgroup(),
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
                cgroup: nothing_cgroup(),
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
                cgroup: nothing_cgroup(),
            }
        );
    }

    fn nothing_cgroup() -> CgroupEvidence {
        CgroupEvidence {
            self_cgroup: Err(CgroupUnreadable::SelfCgroupMissing),
            mount: None,
            namespace_root_typed: false,
            pid1_cgroup: None,
        }
    }

    const MOUNTINFO: &str = "122 113 0:25 / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime shared:23 - cgroup2 cgroup2 rw\n";

    #[test]
    fn cgroup_evidence_of_a_docker_container() {
        let root = FakeRoot::with(
            "cg-docker",
            &[
                ("proc/self/cgroup", "0::/\n"),
                ("proc/self/mountinfo", MOUNTINFO),
                ("proc/1/cgroup", "0::/\n"),
                ("sys/fs/cgroup/cgroup.type", "domain\n"),
            ],
        );
        assert_eq!(
            gather_cgroup_evidence(&root.0),
            CgroupEvidence {
                self_cgroup: Ok(CgroupPath::ROOT),
                mount: Some(Cgroup2Mount {
                    point: "/sys/fs/cgroup".into(),
                    root: CgroupPath::ROOT,
                }),
                namespace_root_typed: true,
                pid1_cgroup: Some(CgroupPath::ROOT),
            }
        );
    }

    #[test]
    fn cgroup_evidence_of_the_host_namespace_and_a_missing_mount() {
        let root = FakeRoot::with(
            "cg-host",
            &[
                ("proc/self/cgroup", "0::/system.slice/docker-abc.scope\n"),
                ("proc/self/mountinfo", MOUNTINFO),
                ("proc/1/cgroup", "0::/../../..\n"),
                ("sys/fs/cgroup/cgroup.procs", ""),
            ],
        );
        let got = gather_cgroup_evidence(&root.0);
        assert_eq!(
            got.self_cgroup,
            Ok(CgroupPath::parse("/system.slice/docker-abc.scope").unwrap())
        );
        assert!(
            !got.namespace_root_typed,
            "the host's root cgroup has no cgroup.type"
        );
        assert_eq!(got.pid1_cgroup, None, "pid 1 outside the namespace");

        let unmounted = FakeRoot::with(
            "cg-unmounted",
            &[
                ("proc/self/cgroup", "12:memory:/x\n"),
                (
                    "proc/self/mountinfo",
                    "21 1 8:1 / / rw - ext4 /dev/sda1 rw\n",
                ),
            ],
        );
        let got = gather_cgroup_evidence(&unmounted.0);
        assert_eq!(got.self_cgroup, Err(CgroupUnreadable::V1Only));
        assert_eq!(got.mount, None);
    }

    #[test]
    fn cgroup_type_is_looked_for_at_the_mount_point_whatever_pid_1_is() {
        // (name, pid 1's cgroup, mount point, cgroup.type at, expected)
        let cases = [
            (
                "an init container",
                "0::/init.scope\n",
                "/sys/fs/cgroup",
                Some("sys/fs/cgroup"),
                true,
            ),
            (
                "pid 1 at the root, no cgroup.type",
                "0::/\n",
                "/sys/fs/cgroup",
                None,
                false,
            ),
            (
                "another mount point",
                "0::/\n",
                "/mnt/cg",
                Some("mnt/cg"),
                true,
            ),
            (
                "cgroup.type only at the usual place",
                "0::/\n",
                "/mnt/cg",
                Some("sys/fs/cgroup"),
                false,
            ),
        ];
        for (name, pid1, point, typed_at, expected) in cases {
            let mountinfo = format!("30 21 0:26 / {point} rw - cgroup2 cgroup2 rw\n");
            let typed_file = typed_at.map(|dir| format!("{dir}/cgroup.type"));
            let mut files = vec![
                ("proc/self/cgroup", "0::/\n"),
                ("proc/self/mountinfo", mountinfo.as_str()),
                ("proc/1/cgroup", pid1),
            ];
            files.extend(typed_file.as_deref().map(|file| (file, "domain\n")));
            let root = FakeRoot::with(&format!("cg-typed-{expected}-{}", name.len()), &files);
            assert_eq!(
                gather_cgroup_evidence(&root.0).namespace_root_typed,
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn a_stripped_mount_roots_files_are_read_at_the_mount_point() {
        let root = FakeRoot::with(
            "cg-read-stripped",
            &[
                ("proc/self/cgroup", "0::/docker/abc\n"),
                (
                    "proc/self/mountinfo",
                    "30 21 0:26 /docker/abc /mnt/cg ro - cgroup2 cgroup rw\n",
                ),
                ("proc/1/cgroup", "0::/docker/abc\n"),
                ("mnt/cg/memory.current", "100\n"),
                ("mnt/cg/memory.stat", "inactive_file 0\n"),
                ("mnt/cg/memory.max", "1000\n"),
            ],
        );
        let got = read_cgroup(&root.0, &monitored_at(&root.0), Instant::now());
        assert_eq!(
            got.memory,
            CgroupRead::Read(MemoryFiles {
                current: Bytes::new(100),
                inactive_file: Bytes::new(0),
                limits: vec![ResourceLimit::Bounded(Bytes::new(1000))],
            })
        );
    }

    /// The monitored cgroup of a container whose files live under `root`.
    fn monitored_at(root: &Path) -> MonitoredCgroup {
        match cgroup_access(&gather_cgroup_evidence(root)) {
            CgroupAccess::V2 { monitored } => monitored,
            CgroupAccess::Unreadable(why) => panic!("the fixture is readable: {why:?}"),
        }
    }

    fn cpus(n: f64) -> CpuCount {
        CpuCount::new(n).unwrap()
    }

    /// `docker run --cpus=1.5 --memory=512m`, as measured.
    const DOCKER_FILES: &[(&str, &str)] = &[
        ("proc/self/cgroup", "0::/\n"),
        ("proc/self/mountinfo", MOUNTINFO),
        ("proc/1/cgroup", "0::/\n"),
        ("sys/fs/cgroup/cgroup.type", "domain\n"),
        (
            "sys/fs/cgroup/cpu.stat",
            "usage_usec 4500000\nuser_usec 4000000\n",
        ),
        ("sys/fs/cgroup/cpu.max", "150000 100000\n"),
        ("sys/fs/cgroup/cpuset.cpus.effective", "0-7\n"),
        ("sys/fs/cgroup/memory.current", "104857600\n"),
        (
            "sys/fs/cgroup/memory.stat",
            "anon 80000000\ninactive_file 20971520\n",
        ),
        ("sys/fs/cgroup/memory.max", "536870912\n"),
        ("sys/fs/cgroup/memory.swap.current", "0\n"),
        ("sys/fs/cgroup/memory.swap.max", "268435456\n"),
    ];

    #[test]
    fn a_docker_containers_cgroup_files_are_read_whole() {
        let root = FakeRoot::with("cg-read-docker", DOCKER_FILES);
        let at = Instant::now();
        let got = read_cgroup(&root.0, &monitored_at(&root.0), at);
        let limit = ResourceLimit::Bounded(Bytes::new(536_870_912));
        assert_eq!(
            got,
            CgroupReadings {
                cpu: CgroupRead::Read(CpuFiles {
                    counters: CpuCounters {
                        usage: Duration::from_micros(4_500_000),
                        read_at: at,
                    },
                    quotas: vec![ResourceLimit::Bounded(cpus(1.5))],
                    cpuset: ResourceLimit::Bounded(cpus(8.0)),
                }),
                memory: CgroupRead::Read(MemoryFiles {
                    current: Bytes::new(104_857_600),
                    inactive_file: Bytes::new(20_971_520),
                    limits: vec![limit],
                }),
                swap: CgroupRead::Read(SwapFiles {
                    current: Bytes::new(0),
                    limits: vec![ResourceLimit::Bounded(Bytes::new(268_435_456))],
                }),
            }
        );
    }

    #[test]
    fn under_the_host_namespace_every_ancestors_limit_is_read() {
        let root = FakeRoot::with(
            "cg-read-host",
            &[
                ("proc/self/cgroup", "0::/system.slice/docker-abc.scope\n"),
                ("proc/self/mountinfo", MOUNTINFO),
                ("proc/1/cgroup", "0::/system.slice/docker-abc.scope\n"),
                (
                    "sys/fs/cgroup/system.slice/docker-abc.scope/cpu.stat",
                    "usage_usec 10\n",
                ),
                (
                    "sys/fs/cgroup/system.slice/docker-abc.scope/cpu.max",
                    "max 100000\n",
                ),
                ("sys/fs/cgroup/system.slice/cpu.max", "50000 100000\n"),
                (
                    "sys/fs/cgroup/system.slice/docker-abc.scope/memory.current",
                    "100\n",
                ),
                (
                    "sys/fs/cgroup/system.slice/docker-abc.scope/memory.stat",
                    "inactive_file 0\n",
                ),
                (
                    "sys/fs/cgroup/system.slice/docker-abc.scope/memory.max",
                    "max\n",
                ),
                ("sys/fs/cgroup/system.slice/memory.max", "1000\n"),
                (
                    "sys/fs/cgroup/system.slice/docker-abc.scope/memory.swap.current",
                    "0\n",
                ),
                (
                    "sys/fs/cgroup/system.slice/docker-abc.scope/memory.swap.max",
                    "max\n",
                ),
                ("sys/fs/cgroup/system.slice/memory.swap.max", "500\n"),
                (
                    "sys/fs/cgroup/system.slice/docker-abc.scope/cpuset.cpus.effective",
                    "0-1\n",
                ),
                ("sys/fs/cgroup/cpuset.cpus.effective", "0-7\n"),
            ],
        );
        let got = read_cgroup(&root.0, &monitored_at(&root.0), Instant::now());
        let CgroupRead::Read(cpu) = got.cpu else {
            panic!("cpu read: {:?}", got.cpu);
        };
        assert_eq!(
            cpu.quotas,
            [
                ResourceLimit::Unbounded,
                ResourceLimit::Bounded(cpus(0.5)),
                ResourceLimit::Unbounded
            ],
            "the scope's, the slice's, and the root's absent file"
        );
        assert_eq!(
            cpu.cpuset,
            ResourceLimit::Bounded(cpus(2.0)),
            "the monitored cgroup's cpuset"
        );
        let CgroupRead::Read(memory) = got.memory else {
            panic!("memory read: {:?}", got.memory);
        };
        assert_eq!(
            memory.limits,
            [
                ResourceLimit::Unbounded,
                ResourceLimit::Bounded(Bytes::new(1000)),
                ResourceLimit::Unbounded
            ]
        );
        let CgroupRead::Read(swap) = got.swap else {
            panic!("swap read: {:?}", got.swap);
        };
        assert_eq!(
            swap.limits,
            [
                ResourceLimit::Unbounded,
                ResourceLimit::Bounded(Bytes::new(500)),
                ResourceLimit::Unbounded
            ]
        );
    }

    #[test]
    fn a_broken_file_fails_its_group_only() {
        let malformed = FakeRoot::with(
            "cg-read-malformed",
            &[DOCKER_FILES, &[("sys/fs/cgroup/memory.max", "lots\n")]].concat(),
        );
        let got = read_cgroup(&malformed.0, &monitored_at(&malformed.0), Instant::now());
        assert_eq!(
            got.memory,
            CgroupRead::Failed(CgroupReadError::Malformed(CgroupFile::MemoryMax))
        );
        assert!(matches!(got.cpu, CgroupRead::Read(_)), "cpu is unaffected");
        assert!(
            matches!(got.swap, CgroupRead::Read(_)),
            "swap is unaffected"
        );

        // A directory where the file should be reads as EISDIR: there, but unreadable.
        let unreadable = FakeRoot::with("cg-read-unreadable", DOCKER_FILES);
        let stat = unreadable.0.join("sys/fs/cgroup/cpu.max");
        std::fs::remove_file(&stat).unwrap();
        std::fs::create_dir(&stat).unwrap();
        let got = read_cgroup(&unreadable.0, &monitored_at(&unreadable.0), Instant::now());
        assert!(
            matches!(
                got.cpu,
                CgroupRead::Failed(CgroupReadError::Unreadable {
                    file: CgroupFile::CpuMax,
                    ..
                })
            ),
            "{:?}",
            got.cpu
        );

        // memory.stat missing beside memory.current isn't "no controller": it fails.
        let no_stat = FakeRoot::with("cg-read-nostat", DOCKER_FILES);
        std::fs::remove_file(no_stat.0.join("sys/fs/cgroup/memory.stat")).unwrap();
        let got = read_cgroup(&no_stat.0, &monitored_at(&no_stat.0), Instant::now());
        assert_eq!(
            got.memory,
            CgroupRead::Failed(CgroupReadError::Missing(CgroupFile::MemoryStat))
        );
    }

    #[test]
    fn absent_controllers_read_as_absent_or_unbounded() {
        // Rootless podman under a user session: memory and pids only.
        let root = FakeRoot::with(
            "cg-read-rootless",
            &[
                ("proc/self/cgroup", "0::/\n"),
                ("proc/self/mountinfo", MOUNTINFO),
                ("proc/1/cgroup", "0::/\n"),
                ("sys/fs/cgroup/cgroup.type", "domain\n"),
                ("sys/fs/cgroup/memory.current", "100\n"),
                ("sys/fs/cgroup/memory.stat", "inactive_file 0\n"),
            ],
        );
        let got = read_cgroup(&root.0, &monitored_at(&root.0), Instant::now());
        assert_eq!(got.cpu, CgroupRead::Absent, "no cpu.stat");
        assert_eq!(
            got.memory,
            CgroupRead::Read(MemoryFiles {
                current: Bytes::new(100),
                inactive_file: Bytes::new(0),
                limits: vec![ResourceLimit::Unbounded],
            }),
            "no memory.max is unbounded"
        );
        assert_eq!(got.swap, CgroupRead::Absent);
    }
}
