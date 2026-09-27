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

//! The monitored cgroup and resource capacity (RFC 0014 §4): where the agent's cgroup v2
//! hierarchy is, which cgroup holds the workload, the parsers for the files read from it, and
//! the capacity rules over their values. `collectors/environment.rs` reads the files.

use std::path::{Component, PathBuf};
use std::time::Duration;

/// The deepest a capacity walk climbs from the monitored cgroup towards the mount point.
pub const MAX_LEVELS: usize = 32;

/// A cgroup's path in the agent's cgroup namespace: its segments under the namespace root,
/// which has none. No segment is empty, `.` or `..`, so a path can't climb out of where it is
/// joined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupPath(Vec<String>);

impl CgroupPath {
    /// The namespace root.
    pub const ROOT: Self = Self(Vec::new());

    /// The path `/proc/<pid>/cgroup` or mountinfo writes, or `None` when it isn't absolute or
    /// climbs with `..` (how the kernel writes a cgroup outside the reader's namespace).
    pub fn parse(path: &str) -> Option<Self> {
        let relative = path.strip_prefix('/')?;
        relative
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(|segment| match segment {
                "." | ".." => None,
                named => Some(named.to_owned()),
            })
            .collect::<Option<Vec<_>>>()
            .map(Self)
    }

    /// This path's segments, from the root down.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    /// This path relative to `prefix`, when `prefix` is this path or one of its ancestors.
    fn strip_prefix(&self, prefix: &Self) -> Option<Self> {
        self.0
            .strip_prefix(prefix.0.as_slice())
            .map(|rest| Self(rest.to_vec()))
    }

    /// Whether `other` is this cgroup or one under it, compared segment by segment.
    fn contains(&self, other: &Self) -> bool {
        other.0.starts_with(&self.0)
    }

    /// This path and each of its ancestors, deepest first, ending at the root, at most
    /// `MAX_LEVELS` of them.
    pub fn ancestry(&self) -> Vec<Self> {
        (0..=self.0.len())
            .rev()
            .take(MAX_LEVELS)
            .map(|depth| Self(self.0[..depth].to_vec()))
            .collect()
    }

    /// This path as the kernel writes it: `/` for the root.
    fn display(&self) -> String {
        format!("/{}", self.0.join("/"))
    }

    /// This path relative to a directory: no leading `/`, empty for the root.
    fn relative(&self) -> PathBuf {
        self.segments().collect()
    }
}

/// The cgroup2 filesystem as the agent's mount namespace shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cgroup2Mount {
    /// Where it is mounted: absolute, with no `..`.
    pub point: PathBuf,
    /// The cgroup the mount shows at `point`, in the agent's cgroup namespace.
    pub root: CgroupPath,
}

/// What the agent found out about its cgroup at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupEvidence {
    /// The agent's cgroup, from `/proc/self/cgroup`.
    pub self_cgroup: Result<CgroupPath, CgroupUnreadable>,
    /// The cgroup2 line of `/proc/self/mountinfo`, if any.
    pub mount: Option<Cgroup2Mount>,
    /// The mount point has a `cgroup.type` file: it shows a non-root cgroup, the root of a
    /// private cgroup namespace.
    pub namespace_root_typed: bool,
    /// PID 1's cgroup from `/proc/1/cgroup`; `None` when unreadable or outside the agent's
    /// cgroup namespace.
    pub pid1_cgroup: Option<CgroupPath>,
}

/// Whether the agent found a cgroup v2 hierarchy it can read, at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CgroupAccess {
    V2 { monitored: MonitoredCgroup },
    Unreadable(CgroupUnreadable),
}

/// Why the agent has no cgroup v2 hierarchy to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupUnreadable {
    /// `/proc/self/cgroup` names only cgroup v1 hierarchies.
    V1Only,
    /// No cgroup2 filesystem is mounted where the agent can see it.
    NoCgroupMount,
    /// `/proc/self/cgroup` can't be read, or is empty.
    SelfCgroupMissing,
    /// The agent's cgroup isn't under the mounted subtree.
    OutsideMountRoot,
}

/// The cgroup holding the whole workload (RFC 0014 §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitoredCgroup {
    /// Where the cgroup2 filesystem is mounted.
    mount_point: PathBuf,
    /// The monitored cgroup under the mount point.
    dir: CgroupPath,
    /// The monitored cgroup in the agent's cgroup namespace.
    path: CgroupPath,
    /// Where PID 1 of the agent's PID namespace sits.
    pid1: Pid1Placement,
}

/// Whether PID 1 of the agent's PID namespace is in the monitored cgroup: when it isn't, the
/// namespace is shared with processes that aren't the workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pid1Placement {
    Inside,
    Outside,
}

impl MonitoredCgroup {
    /// The monitored cgroup's directory, and each ancestor up to the mount point, deepest
    /// first, as paths relative to the filesystem root.
    pub fn levels(&self) -> Vec<PathBuf> {
        self.dir
            .ancestry()
            .iter()
            .map(|level| self.under_mount(level))
            .collect()
    }

    /// `level`'s directory under the mount point, relative to the filesystem root.
    fn under_mount(&self, level: &CgroupPath) -> PathBuf {
        let mount = self
            .mount_point
            .strip_prefix("/")
            .unwrap_or(&self.mount_point);
        mount.join(level.relative())
    }

    /// The monitored cgroup's own directory, relative to the filesystem root.
    pub fn directory(&self) -> PathBuf {
        self.under_mount(&self.dir)
    }

    pub fn pid1(&self) -> Pid1Placement {
        self.pid1
    }

    /// The monitored cgroup's path in the agent's namespace, as a log line writes it.
    pub fn path_display(&self) -> String {
        self.path.display()
    }
}

/// Where the agent's cgroup is and which cgroup holds its workload. The namespace root is
/// monitored when it is a private namespace's (it has `cgroup.type`), else the agent's own
/// cgroup.
pub fn cgroup_access(evidence: &CgroupEvidence) -> CgroupAccess {
    match monitored_cgroup(evidence) {
        Ok(monitored) => CgroupAccess::V2 { monitored },
        Err(why) => CgroupAccess::Unreadable(why),
    }
}

fn monitored_cgroup(evidence: &CgroupEvidence) -> Result<MonitoredCgroup, CgroupUnreadable> {
    let own = evidence.self_cgroup.clone()?;
    let mount = evidence
        .mount
        .as_ref()
        .ok_or(CgroupUnreadable::NoCgroupMount)?;
    let own_dir = own
        .strip_prefix(&mount.root)
        .ok_or(CgroupUnreadable::OutsideMountRoot)?;
    let private_root = mount.root == CgroupPath::ROOT && evidence.namespace_root_typed;
    let (dir, path) = if private_root {
        (CgroupPath::ROOT, CgroupPath::ROOT)
    } else {
        (own_dir, own)
    };
    let pid1 = match &evidence.pid1_cgroup {
        Some(pid1) if path.contains(pid1) => Pid1Placement::Inside,
        Some(_) | None => Pid1Placement::Outside,
    };
    Ok(MonitoredCgroup {
        mount_point: mount.point.clone(),
        dir,
        path,
        pid1,
    })
}

/// A cgroup's bound on one resource.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResourceLimit<T> {
    Bounded(T),
    /// `max`, or no limit file because the controller isn't enabled there.
    Unbounded,
}

/// A number of CPUs, possibly fractional: always positive and finite.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct CpuCount(f64);

impl CpuCount {
    pub fn new(cpus: f64) -> Option<Self> {
        (cpus.is_finite() && cpus > 0.0).then_some(Self(cpus))
    }

    /// The count of `cores` whole CPUs; a kernel always has one.
    pub fn cores(cores: usize) -> Self {
        Self(cores.max(1) as f64)
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

/// An amount of memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bytes(u64);

impl Bytes {
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }
}

/// A file the agent reads from the monitored cgroup each tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupFile {
    CpuStat,
    CpuMax,
    CpusetCpusEffective,
    MemoryCurrent,
    MemoryStat,
    MemoryMax,
    MemorySwapCurrent,
    MemorySwapMax,
}

impl CgroupFile {
    /// The file's name in a cgroup directory.
    pub fn name(self) -> &'static str {
        match self {
            Self::CpuStat => "cpu.stat",
            Self::CpuMax => "cpu.max",
            Self::CpusetCpusEffective => "cpuset.cpus.effective",
            Self::MemoryCurrent => "memory.current",
            Self::MemoryStat => "memory.stat",
            Self::MemoryMax => "memory.max",
            Self::MemorySwapCurrent => "memory.swap.current",
            Self::MemorySwapMax => "memory.swap.max",
        }
    }
}

/// A cgroup file that is there but gave no value: never guessed around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CgroupReadError {
    /// It couldn't be read: EACCES, EIO, ENODEV. `reason` is for the log only.
    Unreadable { file: CgroupFile, reason: String },
    /// It was read, and isn't what the kernel writes there.
    Malformed(CgroupFile),
    /// It isn't there, though its group's usage file is: not how the kernel says "no limit".
    Missing(CgroupFile),
}

/// `cpu.max`: `max <period>` is unbounded, `<quota> <period>` is quota/period CPUs.
pub fn parse_cpu_max(content: &str) -> Option<ResourceLimit<CpuCount>> {
    let mut fields = content.split_whitespace();
    let (quota, period) = (fields.next()?, fields.next()?);
    let period: u64 = period.parse().ok().filter(|&p| p > 0)?;
    if fields.next().is_some() {
        return None;
    }
    match quota {
        "max" => Some(ResourceLimit::Unbounded),
        quota => {
            let quota: u64 = quota.parse().ok()?;
            CpuCount::new(quota as f64 / period as f64).map(ResourceLimit::Bounded)
        }
    }
}

/// `memory.max` or `memory.swap.max`: `max`, or a byte count.
pub fn parse_memory_max(content: &str) -> Option<ResourceLimit<Bytes>> {
    match content.trim() {
        "max" => Some(ResourceLimit::Unbounded),
        bytes => parse_bytes(bytes).map(ResourceLimit::Bounded),
    }
}

/// `cpuset.cpus.effective` (`0-3,6`): how many CPUs it lists.
pub fn parse_cpuset(content: &str) -> Option<CpuCount> {
    let count = content
        .trim()
        .split(',')
        .map(cpuset_range_len)
        .sum::<Option<u64>>()?;
    CpuCount::new(count as f64)
}

/// How many CPUs one cpuset entry (`3` or `0-3`) names.
fn cpuset_range_len(entry: &str) -> Option<u64> {
    let (first, last) = entry.split_once('-').unwrap_or((entry, entry));
    let (first, last): (u64, u64) = (first.parse().ok()?, last.parse().ok()?);
    (first <= last).then(|| last - first + 1)
}

/// A single byte count: `memory.current`, `memory.swap.current`.
pub fn parse_bytes(content: &str) -> Option<Bytes> {
    content.trim().parse().ok().map(Bytes)
}

/// `cpu.stat`'s `usage_usec`: the CPU time the cgroup has used.
pub fn parse_cpu_usage(cpu_stat: &str) -> Option<Duration> {
    stat_value(cpu_stat, "usage_usec").map(Duration::from_micros)
}

/// The value of `key` in a flat-keyed stat file (`<key> <u64>` per line).
fn stat_value(stat: &str, key: &str) -> Option<u64> {
    stat.lines()
        .filter_map(|line| line.split_once(' '))
        .find(|(k, _)| *k == key)
        .and_then(|(_, value)| value.trim().parse().ok())
}

/// `memory.stat`'s `inactive_file`: page cache the kernel can reclaim first.
pub fn parse_inactive_file(memory_stat: &str) -> Option<Bytes> {
    stat_value(memory_stat, "inactive_file").map(Bytes)
}

/// The agent's cgroup, from `/proc/self/cgroup`'s `0::` line.
pub fn parse_self_cgroup(content: &str) -> Result<CgroupPath, CgroupUnreadable> {
    match unified_line(content) {
        Some(path) => CgroupPath::parse(path).ok_or(CgroupUnreadable::OutsideMountRoot),
        None if content.trim().is_empty() => Err(CgroupUnreadable::SelfCgroupMissing),
        None => Err(CgroupUnreadable::V1Only),
    }
}

/// The path on a `/proc/<pid>/cgroup` file's `0::` line: the cgroup v2 hierarchy's.
fn unified_line(content: &str) -> Option<&str> {
    content
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::trim)
}

/// PID 1's cgroup, from `/proc/1/cgroup`: `None` without a `0::` line, or outside the
/// agent's cgroup namespace.
pub fn parse_pid1_cgroup(content: &str) -> Option<CgroupPath> {
    unified_line(content).and_then(CgroupPath::parse)
}

/// The first cgroup2 mount in `/proc/self/mountinfo`.
pub fn parse_cgroup2_mount(mountinfo: &str) -> Option<Cgroup2Mount> {
    mountinfo.lines().find_map(cgroup2_mount_line)
}

/// A mountinfo line's mount, when its filesystem type is `cgroup2`: `<id> <parent>
/// <major:minor> <root> <mount point> <options> [optional fields] - <fstype> <source> …`.
fn cgroup2_mount_line(line: &str) -> Option<Cgroup2Mount> {
    let (mount, filesystem) = line.split_once(" - ")?;
    if filesystem.split_whitespace().next()? != "cgroup2" {
        return None;
    }
    let mut fields = mount.split_whitespace().skip(3);
    let (root, point) = (fields.next()?, fields.next()?);
    let point = PathBuf::from(unescape_mountinfo(point));
    let climbs = point.components().any(|c| c == Component::ParentDir);
    (point.is_absolute() && !climbs).then_some(())?;
    Some(Cgroup2Mount {
        point,
        root: CgroupPath::parse(&unescape_mountinfo(root))?,
    })
}

/// A mountinfo field with the kernel's octal escapes (`\040` for a space) decoded.
fn unescape_mountinfo(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let escape = rest.get(at + 1..at + 4);
        match escape.and_then(|octal| u8::from_str_radix(octal, 8).ok()) {
            Some(byte) => {
                out.push(char::from(byte));
                rest = &rest[at + 4..];
            }
            None => {
                out.push('\\');
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// How many CPUs the monitored environment may use: the least of the host's, every bounded
/// quota and the cpuset.
pub fn cpu_capacity(
    host: CpuCount,
    quotas: &[ResourceLimit<CpuCount>],
    cpuset: ResourceLimit<CpuCount>,
) -> CpuCount {
    quotas
        .iter()
        .chain([&cpuset])
        .filter_map(|limit| match limit {
            ResourceLimit::Bounded(cpus) => Some(*cpus),
            ResourceLimit::Unbounded => None,
        })
        .fold(host, |least, cpus| if cpus < least { cpus } else { least })
}

/// The tightest of `limits`: unbounded when none is bounded.
pub fn tightest(limits: &[ResourceLimit<Bytes>]) -> ResourceLimit<Bytes> {
    limits
        .iter()
        .filter_map(|limit| match limit {
            ResourceLimit::Bounded(bytes) => Some(*bytes),
            ResourceLimit::Unbounded => None,
        })
        .min()
        .map_or(ResourceLimit::Unbounded, ResourceLimit::Bounded)
}

/// How much memory (or swap) the monitored environment may use: the least of the host's and
/// every bounded limit.
pub fn memory_capacity(host: Bytes, limits: &[ResourceLimit<Bytes>]) -> Bytes {
    match tightest(limits) {
        ResourceLimit::Bounded(limit) => limit.min(host),
        ResourceLimit::Unbounded => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ResourceLimit::{Bounded, Unbounded};

    fn path(p: &str) -> CgroupPath {
        CgroupPath::parse(p).expect("a valid cgroup path")
    }

    fn cpus(n: f64) -> CpuCount {
        CpuCount::new(n).expect("a positive count")
    }

    #[test]
    fn a_cgroup_path_is_absolute_and_never_climbs() {
        let cases: [(&str, Option<&[&str]>); 9] = [
            ("/", Some(&[])),
            ("/a", Some(&["a"])),
            (
                "/system.slice/x.service",
                Some(&["system.slice", "x.service"]),
            ),
            ("/a//b/", Some(&["a", "b"])),
            ("", None),
            ("a/b", None),
            ("/../../..", None),
            ("/a/../b", None),
            ("/a/./b", None),
        ];
        for (raw, expected) in cases {
            let got =
                CgroupPath::parse(raw).map(|p| p.segments().map(str::to_owned).collect::<Vec<_>>());
            let expected = expected.map(|s| s.iter().map(|x| x.to_string()).collect::<Vec<_>>());
            assert_eq!(got, expected, "{raw:?}");
        }
    }

    #[test]
    fn a_path_contains_itself_and_what_is_under_it_segment_by_segment() {
        let cases = [
            ("the root holds everything", "/", "/a/b", true),
            ("itself", "/foo", "/foo", true),
            ("a child", "/foo", "/foo/bar", true),
            ("a sibling sharing a prefix", "/foo", "/foobar", false),
            ("a parent", "/foo/bar", "/foo", false),
            ("elsewhere", "/foo", "/baz", false),
        ];
        for (name, outer, inner, expected) in cases {
            assert_eq!(path(outer).contains(&path(inner)), expected, "{name}");
        }
    }

    #[test]
    fn ancestry_climbs_to_the_root_and_stops_at_the_level_cap() {
        let names = |p: &CgroupPath| p.segments().collect::<Vec<_>>().join("/");
        let got: Vec<String> = path("/a/b/c").ancestry().iter().map(names).collect();
        assert_eq!(got, ["a/b/c", "a/b", "a", ""]);
        assert_eq!(CgroupPath::ROOT.ancestry(), [CgroupPath::ROOT]);
        let deep = path(&"/x".repeat(40));
        let ancestry = deep.ancestry();
        assert_eq!(ancestry.len(), MAX_LEVELS, "capped");
        assert_eq!(ancestry[0], deep, "deepest first");
        assert_eq!(ancestry[1], path(&"/x".repeat(39)), "then its parent");
        assert_eq!(
            ancestry[MAX_LEVELS - 1],
            path(&"/x".repeat(40 - MAX_LEVELS + 1)),
            "stopping short of the root"
        );
    }

    #[test]
    fn cpu_max_reads_a_quota_over_its_period() {
        let cases = [
            ("unbounded", "max 100000\n", Some(Unbounded)),
            (
                "one and a half",
                "150000 100000\n",
                Some(Bounded(cpus(1.5))),
            ),
            ("a tenth", "10000 100000", Some(Bounded(cpus(0.1)))),
            ("zero period", "150000 0", None),
            ("zero quota", "0 100000", None),
            ("only max", "max", None),
            ("max over a zero period", "max 0", None),
            ("max over garbage", "max junk", None),
            ("a third field", "150000 100000 7", None),
            ("garbage", "lots", None),
            ("negative", "-1 100000", None),
            ("empty", "", None),
        ];
        for (name, content, expected) in cases {
            assert_eq!(parse_cpu_max(content), expected, "{name}");
        }
    }

    #[test]
    fn memory_max_reads_max_or_a_byte_count() {
        let cases = [
            ("unbounded", "max\n", Some(Unbounded)),
            (
                "512 MiB",
                "536870912\n",
                Some(Bounded(Bytes::new(536_870_912))),
            ),
            ("zero", "0", Some(Bounded(Bytes::new(0)))),
            ("overflow", "18446744073709551616", None),
            ("negative", "-5", None),
            ("garbage", "lots", None),
            ("empty", "", None),
        ];
        for (name, content, expected) in cases {
            assert_eq!(parse_memory_max(content), expected, "{name}");
        }
    }

    #[test]
    fn a_cpuset_counts_every_cpu_it_lists() {
        let cases = [
            ("ranges and singles", "0-3,6\n", Some(cpus(5.0))),
            ("one cpu", "0", Some(cpus(1.0))),
            ("a one-cpu range", "4-4", Some(cpus(1.0))),
            ("two ranges", "0-1,8-11", Some(cpus(6.0))),
            ("empty", "\n", None),
            ("reversed range", "3-0", None),
            ("garbage", "a-b", None),
            ("a trailing comma", "0-3,", None),
        ];
        for (name, content, expected) in cases {
            assert_eq!(parse_cpuset(content), expected, "{name}");
        }
    }

    #[test]
    fn byte_counts_and_stat_lines_are_read_strictly() {
        assert_eq!(parse_bytes("1048576\n"), Some(Bytes::new(1_048_576)));
        assert_eq!(parse_bytes("max"), None);
        assert_eq!(parse_bytes(""), None);

        let cpu_stat = "usage_usec 12090780000\nuser_usec 7951916000\nsystem_usec 4138864000\n";
        assert_eq!(
            parse_cpu_usage(cpu_stat),
            Some(Duration::from_micros(12_090_780_000))
        );
        assert_eq!(
            parse_cpu_usage("nr_periods 0\nusage_usec 5\nuser_usec 3\n"),
            Some(Duration::from_micros(5)),
            "usage_usec anywhere in the file"
        );
        assert_eq!(parse_cpu_usage("user_usec 5\n"), None, "no usage line");
        assert_eq!(parse_cpu_usage("usage_usec x\n"), None, "garbage");
        assert_eq!(parse_cpu_usage("usage_usec_x 5\n"), None, "another key");

        let memory_stat = "anon 100\nfile 900\nactive_file 300\ninactive_file 600\n";
        assert_eq!(parse_inactive_file(memory_stat), Some(Bytes::new(600)));
        assert_eq!(
            parse_inactive_file("anon 1\ninactive_file 600\nactive_file 300\n"),
            Some(Bytes::new(600)),
            "mid-file, as the kernel writes it"
        );
        assert_eq!(
            parse_inactive_file("inactive_file_x 5\ninactive_file 7\n"),
            Some(Bytes::new(7)),
            "the exact key"
        );
        assert_eq!(parse_inactive_file("anon 100\n"), None, "no line");
        assert_eq!(parse_inactive_file("inactive_file -1\n"), None, "garbage");
    }

    #[test]
    fn self_cgroup_is_the_unified_line() {
        use CgroupUnreadable::*;
        let cases = [
            ("docker", "0::/\n", Ok(CgroupPath::ROOT)),
            (
                "a nested path",
                "0::/system.slice/x.service\n",
                Ok(path("/system.slice/x.service")),
            ),
            (
                "hybrid, with the unified line last",
                "12:memory:/x\n1:name=systemd:/x\n0::/x\n",
                Ok(path("/x")),
            ),
            ("v1 only", "12:memory:/x\n1:name=systemd:/x\n", Err(V1Only)),
            ("empty", "", Err(SelfCgroupMissing)),
            ("a climbing path", "0::/../x\n", Err(OutsideMountRoot)),
        ];
        for (name, content, expected) in cases {
            assert_eq!(parse_self_cgroup(content), expected, "{name}");
        }
    }

    #[test]
    fn pid1_cgroup_is_kept_only_inside_the_namespace() {
        let cases = [
            ("the namespace root", "0::/\n", Some(CgroupPath::ROOT)),
            (
                "under an init",
                "0::/init.scope\n",
                Some(path("/init.scope")),
            ),
            ("the host's init", "0::/../../..\n", None),
            ("a pod's pause container", "0::/../abc\n", None),
            ("v1 only", "1:name=systemd:/\n", None),
            ("empty", "", None),
        ];
        for (name, content, expected) in cases {
            assert_eq!(parse_pid1_cgroup(content), expected, "{name}");
        }
    }

    #[test]
    fn the_cgroup2_mount_is_read_from_mountinfo() {
        let measured = "122 113 0:25 / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime shared:23 - cgroup2 cgroup2 rw\n";
        let cases = [
            (
                "the root mounted at the usual place",
                measured.to_owned(),
                Some(("/sys/fs/cgroup", "/")),
            ),
            (
                "a subtree, after another mount",
                "21 1 8:1 / / rw - ext4 /dev/sda1 rw\n\
                 30 21 0:26 /docker/abc /sys/fs/cgroup ro - cgroup2 cgroup rw\n"
                    .to_owned(),
                Some(("/sys/fs/cgroup", "/docker/abc")),
            ),
            (
                "a tmpfs whose mount point mentions cgroup2, before the real one",
                "40 21 0:30 / /sys/fs/cgroup2 rw - tmpfs tmpfs rw\n\
                 30 21 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n"
                    .to_owned(),
                Some(("/sys/fs/cgroup", "/")),
            ),
            (
                "the first of two cgroup2 mounts",
                "30 21 0:26 / /sys/fs/cgroup rw shared:9 - cgroup2 cgroup2 rw\n\
                 31 21 0:26 /nested /mnt/other rw - cgroup2 cgroup2 rw\n"
                    .to_owned(),
                Some(("/sys/fs/cgroup", "/")),
            ),
            (
                "an fstype named cgroup2 only in the source",
                "40 21 0:30 / /mnt/x rw - tmpfs cgroup2 rw\n".to_owned(),
                None,
            ),
            (
                "no optional fields",
                "30 21 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n".to_owned(),
                Some(("/sys/fs/cgroup", "/")),
            ),
            (
                "an escaped space in the mount point",
                "30 21 0:26 / /mnt/cg\\040two rw - cgroup2 none rw\n".to_owned(),
                Some(("/mnt/cg two", "/")),
            ),
            (
                "escaped tab and backslash, in the root too",
                "30 21 0:26 /a\\011b /mnt/c\\134d rw - cgroup2 none rw\n".to_owned(),
                Some(("/mnt/c\\d", "/a\tb")),
            ),
            (
                "cgroup v1 only",
                "25 21 0:22 / /sys/fs/cgroup/memory rw - cgroup cgroup rw,memory\n".to_owned(),
                None,
            ),
            (
                "a relative mount point",
                "30 21 0:26 / sys/fs/cgroup rw - cgroup2 cgroup2 rw\n".to_owned(),
                None,
            ),
            (
                "a climbing mount point",
                "30 21 0:26 / /sys/../etc rw - cgroup2 cgroup2 rw\n".to_owned(),
                None,
            ),
            (
                "a climbing root",
                "30 21 0:26 /.. /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n".to_owned(),
                None,
            ),
            ("empty", String::new(), None),
        ];
        for (name, content, expected) in cases {
            let expected = expected.map(|(point, root)| Cgroup2Mount {
                point: PathBuf::from(point),
                root: path(root),
            });
            assert_eq!(parse_cgroup2_mount(&content), expected, "{name}");
        }
    }

    fn evidence(self_cgroup: &str, root: &str, typed: bool, pid1: Option<&str>) -> CgroupEvidence {
        CgroupEvidence {
            self_cgroup: Ok(path(self_cgroup)),
            mount: Some(Cgroup2Mount {
                point: PathBuf::from("/sys/fs/cgroup"),
                root: path(root),
            }),
            namespace_root_typed: typed,
            pid1_cgroup: pid1.map(path),
        }
    }

    fn monitored(dir: &str, at: &str, pid1: Pid1Placement) -> CgroupAccess {
        CgroupAccess::V2 {
            monitored: MonitoredCgroup {
                mount_point: PathBuf::from("/sys/fs/cgroup"),
                dir: path(dir),
                path: path(at),
                pid1,
            },
        }
    }

    #[test]
    fn the_monitored_cgroup_is_the_private_namespace_root_else_the_agents_own() {
        use CgroupUnreadable::*;
        use Pid1Placement::*;
        let cases = [
            (
                "docker: the agent is pid 1 at the namespace root",
                evidence("/", "/", true, Some("/")),
                monitored("/", "/", Inside),
            ),
            (
                "an init in the container: the namespace root, not the agent's service",
                evidence("/system.slice/x.service", "/", true, Some("/init.scope")),
                monitored("/", "/", Inside),
            ),
            (
                "--cgroupns=host: the agent's own cgroup",
                evidence(
                    "/system.slice/docker-abc.scope",
                    "/",
                    false,
                    Some("/system.slice/docker-abc.scope"),
                ),
                monitored(
                    "/system.slice/docker-abc.scope",
                    "/system.slice/docker-abc.scope",
                    Inside,
                ),
            ),
            (
                "a mount root that prefixes the agent's path is stripped",
                evidence("/docker/abc", "/docker/abc", false, Some("/docker/abc")),
                monitored("/", "/docker/abc", Inside),
            ),
            (
                "a nested agent under a stripped mount root: its own cgroup, pid 1 above it",
                evidence("/docker/abc/sub", "/docker/abc", true, Some("/docker/abc")),
                monitored("/sub", "/docker/abc/sub", Outside),
            ),
            (
                "a non-root mount root is never the namespace root, even typed",
                evidence("/docker/abc", "/docker/abc", true, Some("/docker/abc")),
                monitored("/", "/docker/abc", Inside),
            ),
            (
                "a mount root that isn't a prefix",
                evidence("/other", "/docker/abc", false, None),
                CgroupAccess::Unreadable(OutsideMountRoot),
            ),
            (
                "a mount root sharing only a name prefix",
                evidence("/docker/abcd", "/docker/abc", false, None),
                CgroupAccess::Unreadable(OutsideMountRoot),
            ),
            (
                "pid 1 nested under the monitored cgroup",
                evidence("/", "/", true, Some("/a/b")),
                monitored("/", "/", Inside),
            ),
            (
                "--pid=host: pid 1 outside the namespace",
                evidence("/", "/", true, None),
                monitored("/", "/", Outside),
            ),
            (
                "pid 1 in a sibling sharing a name prefix",
                evidence("/foo", "/", false, Some("/foobar")),
                monitored("/foo", "/foo", Outside),
            ),
            (
                "pid 1 above the agent's own cgroup",
                evidence("/foo/agent", "/", false, Some("/foo")),
                monitored("/foo/agent", "/foo/agent", Outside),
            ),
        ];
        for (name, evidence, expected) in cases {
            assert_eq!(cgroup_access(&evidence), expected, "{name}");
        }
    }

    #[test]
    fn no_readable_hierarchy_says_why() {
        use CgroupUnreadable::*;
        let base = evidence("/", "/", true, Some("/"));
        let cases = [
            (
                "v1 only",
                CgroupEvidence {
                    self_cgroup: Err(V1Only),
                    ..base.clone()
                },
                V1Only,
            ),
            (
                "no self cgroup",
                CgroupEvidence {
                    self_cgroup: Err(SelfCgroupMissing),
                    ..base.clone()
                },
                SelfCgroupMissing,
            ),
            (
                "no mount",
                CgroupEvidence {
                    mount: None,
                    ..base.clone()
                },
                NoCgroupMount,
            ),
        ];
        for (name, evidence, expected) in cases {
            assert_eq!(
                cgroup_access(&evidence),
                CgroupAccess::Unreadable(expected),
                "{name}"
            );
        }
    }

    #[test]
    fn levels_climb_from_the_monitored_cgroup_to_the_mount_point() {
        let CgroupAccess::V2 { monitored: m } = monitored("/a/b", "/a/b", Pid1Placement::Inside)
        else {
            unreachable!()
        };
        assert_eq!(
            m.levels(),
            ["sys/fs/cgroup/a/b", "sys/fs/cgroup/a", "sys/fs/cgroup"].map(PathBuf::from)
        );
        let CgroupAccess::V2 { monitored: m } = monitored_root() else {
            unreachable!()
        };
        assert_eq!(m.levels(), [PathBuf::from("sys/fs/cgroup")]);
        assert_eq!(m.directory(), PathBuf::from("sys/fs/cgroup"));
        assert_eq!(m.path_display(), "/");

        // A stripped mount root: the files are at the mount point, not under the path.
        let stripped = monitored("/", "/docker/abc", Pid1Placement::Inside);
        let CgroupAccess::V2 { monitored: m } = stripped else {
            unreachable!()
        };
        assert_eq!(m.levels(), [PathBuf::from("sys/fs/cgroup")]);
        assert_eq!(m.directory(), PathBuf::from("sys/fs/cgroup"));

        // Another mount point.
        let CgroupAccess::V2 { monitored: m } = cgroup_access(&CgroupEvidence {
            mount: Some(Cgroup2Mount {
                point: PathBuf::from("/mnt/cg"),
                root: CgroupPath::ROOT,
            }),
            ..evidence("/a", "/", false, Some("/a"))
        }) else {
            unreachable!()
        };
        assert_eq!(m.levels(), ["mnt/cg/a", "mnt/cg"].map(PathBuf::from));
        assert_eq!(m.directory(), PathBuf::from("mnt/cg/a"));
        assert_eq!(m.path_display(), "/a");
    }

    #[test]
    fn the_monitored_path_is_named_in_the_namespace_not_under_the_mount() {
        let cases = [
            ("the private root", evidence("/", "/", true, Some("/")), "/"),
            (
                "a stripped mount root",
                evidence("/docker/abc", "/docker/abc", false, Some("/docker/abc")),
                "/docker/abc",
            ),
            (
                "the agent's own cgroup",
                evidence("/system.slice/x.scope", "/", false, None),
                "/system.slice/x.scope",
            ),
        ];
        for (name, evidence, expected) in cases {
            let CgroupAccess::V2 { monitored } = cgroup_access(&evidence) else {
                panic!("{name}: readable");
            };
            assert_eq!(monitored.path_display(), expected, "{name}");
        }
    }

    fn monitored_root() -> CgroupAccess {
        monitored("/", "/", Pid1Placement::Inside)
    }

    #[test]
    fn cpu_capacity_is_the_least_of_the_host_every_quota_and_the_cpuset() {
        let cases = [
            (
                "nothing bounded",
                8.0,
                vec![Unbounded, Unbounded],
                Unbounded,
                8.0,
            ),
            ("a quota", 8.0, vec![Bounded(cpus(1.5))], Unbounded, 1.5),
            (
                "a parent's quota binds an unbounded child",
                8.0,
                vec![Unbounded, Bounded(cpus(2.0))],
                Unbounded,
                2.0,
            ),
            (
                "the tighter of two quotas",
                8.0,
                vec![Bounded(cpus(3.0)), Bounded(cpus(0.5))],
                Unbounded,
                0.5,
            ),
            (
                "a cpuset under the quota",
                8.0,
                vec![Bounded(cpus(4.0))],
                Bounded(cpus(2.0)),
                2.0,
            ),
            (
                "a quota above the host",
                2.0,
                vec![Bounded(cpus(6.0))],
                Unbounded,
                2.0,
            ),
            (
                "a cpuset looser than the quota",
                8.0,
                vec![Bounded(cpus(1.5))],
                Bounded(cpus(4.0)),
                1.5,
            ),
            (
                "a cpuset above the host",
                2.0,
                vec![Unbounded],
                Bounded(cpus(4.0)),
                2.0,
            ),
            ("no levels at all", 4.0, vec![], Unbounded, 4.0),
        ];
        for (name, host, quotas, cpuset, expected) in cases {
            assert_eq!(
                cpu_capacity(cpus(host), &quotas, cpuset),
                cpus(expected),
                "{name}"
            );
        }
    }

    #[test]
    fn memory_capacity_is_the_least_of_the_host_and_every_limit() {
        let b = Bytes::new;
        let cases = [
            ("unbounded", 1000, vec![Unbounded], 1000, Unbounded),
            ("a limit", 1000, vec![Bounded(b(512))], 512, Bounded(b(512))),
            (
                "a parent's limit binds",
                1000,
                vec![Unbounded, Bounded(b(300))],
                300,
                Bounded(b(300)),
            ),
            (
                "a limit above host ram",
                1000,
                vec![Bounded(b(4000))],
                1000,
                Bounded(b(4000)),
            ),
            (
                "the tighter of two",
                1000,
                vec![Bounded(b(700)), Bounded(b(600))],
                600,
                Bounded(b(600)),
            ),
            ("no levels at all", 1000, vec![], 1000, Unbounded),
        ];
        for (name, host, limits, capacity, limit) in cases {
            assert_eq!(memory_capacity(b(host), &limits), b(capacity), "{name}");
            assert_eq!(tightest(&limits), limit, "{name}: the limit");
        }
    }
}
