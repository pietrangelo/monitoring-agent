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

//! The push system id (RFC 0016 §6): the id the agent presents in the push handshake, resolved
//! once per process, in `start`, before the runtime exists. Blocking: reads, and may write, files.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// The environment variable naming the file that keeps the id across re-creations.
pub const ID_FILE_VARIABLE: &str = "SYSTEM_AGENT_ID_FILE";

/// The longest id the hub accepts (RFC 0005).
const MAX_AGENT_ID_BYTES: usize = 255;

/// A push system id the hub will accept: its `SystemId` rule, after trimming ASCII whitespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentId(String);

/// The rule an id broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentIdRule {
    Empty,
    TooLong,
    DotSegment,
}

impl TryFrom<&str> for AgentId {
    type Error = AgentIdRule;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let id = value.trim_ascii();
        match id {
            "" => Err(AgentIdRule::Empty),
            "." | ".." => Err(AgentIdRule::DotSegment),
            _ if id.len() > MAX_AGENT_ID_BYTES => Err(AgentIdRule::TooLong),
            _ => Ok(Self(id.to_owned())),
        }
    }
}

impl AgentId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where a resolved id came from, when it didn't come from the id file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdSource {
    MachineId,
    DbusMachineId,
    HostName,
    Random,
}

/// How the id was resolved: what the startup log line says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Read from the id file.
    FromFile,
    /// Resolved from a source and written to the id file, which keeps it from now on.
    Written(IdSource),
    /// Resolved from a source, with no id file set.
    Resolved(IdSource),
}

/// A resolved push system id, and how it was resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedId {
    pub id: AgentId,
    pub resolution: Resolution,
}

/// Why `SYSTEM_AGENT_ID_FILE` refused startup. No variant carries the path or the file's
/// content.
#[derive(Debug)]
pub enum AgentIdError {
    /// Not an absolute path.
    NotAbsolute,
    /// The file isn't UTF-8.
    NotUtf8,
    /// The file holds an id that breaks the rule.
    Invalid(AgentIdRule),
    /// The file exists but couldn't be read.
    Unreadable(io::Error),
    /// The file was missing and couldn't be written.
    Unwritable(io::Error),
    /// The file couldn't be linked into place, the way a filesystem without hard links refuses.
    NoHardLinks(io::Error),
}

impl AgentIdError {
    /// Whether this is a refused value (exit 78) rather than an I/O failure (exit 1).
    pub fn is_refused_value(&self) -> bool {
        match self {
            Self::NotAbsolute | Self::NotUtf8 | Self::Invalid(_) => true,
            Self::Unreadable(_) | Self::Unwritable(_) | Self::NoHardLinks(_) => false,
        }
    }
}

impl fmt::Display for AgentIdRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "an empty id",
            Self::TooLong => "an id longer than 255 bytes",
            Self::DotSegment => "`.` or `..`, which isn't an id",
        })
    }
}

impl fmt::Display for AgentIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{ID_FILE_VARIABLE} ")?;
        match self {
            Self::NotAbsolute => f.write_str("must be an absolute path"),
            Self::NotUtf8 => f.write_str("names a file that isn't UTF-8"),
            Self::Invalid(rule) => write!(f, "names a file that holds {rule}"),
            Self::Unreadable(err) => write!(f, "names a file that couldn't be read: {err}"),
            Self::Unwritable(err) => write!(f, "names a file that couldn't be written: {err}"),
            Self::NoHardLinks(err) => write!(
                f,
                "names a file on a filesystem that may not support hard links: {err}"
            ),
        }
    }
}

/// `SYSTEM_AGENT_ID_FILE`, parsed: an absolute path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdFilePath(PathBuf);

impl IdFilePath {
    /// Unset or empty means no id file; any other value must be an absolute path. Paths may be
    /// any bytes, so the value is read with `var_os`.
    pub fn from_env(value: Option<OsString>) -> Result<Option<Self>, AgentIdError> {
        match value {
            None => Ok(None),
            Some(value) if value.is_empty() => Ok(None),
            Some(value) => {
                let path = PathBuf::from(value);
                if path.is_absolute() {
                    Ok(Some(Self(path)))
                } else {
                    Err(AgentIdError::NotAbsolute)
                }
            }
        }
    }
}

/// Where the sources after the id file live: the system's paths, or a test's.
#[derive(Debug, Clone)]
pub struct IdSources {
    pub machine_id: PathBuf,
    pub dbus_machine_id: PathBuf,
    pub host_name: PathBuf,
}

impl IdSources {
    /// The paths a running agent reads.
    pub fn system() -> Self {
        Self {
            machine_id: PathBuf::from("/etc/machine-id"),
            dbus_machine_id: PathBuf::from("/var/lib/dbus/machine-id"),
            host_name: PathBuf::from("/proc/sys/kernel/hostname"),
        }
    }
}

/// Resolves the push system id: the id file when it exists, else the machine ids, the host
/// name, then a random UUID. A missing id file is written with what the sources gave.
pub fn resolve_push_id(
    id_file: Option<&IdFilePath>,
    sources: &IdSources,
) -> Result<ResolvedId, AgentIdError> {
    let Some(IdFilePath(path)) = id_file else {
        let (id, source) = from_sources(sources);
        return Ok(ResolvedId {
            id,
            resolution: Resolution::Resolved(source),
        });
    };
    match read_id_file(path)? {
        Some(id) => Ok(from_file(id)),
        None => {
            let (id, source) = from_sources(sources);
            let written = ResolvedId {
                id,
                resolution: Resolution::Written(source),
            };
            persist_new_id(path, written)
        }
    }
}

fn from_file(id: AgentId) -> ResolvedId {
    ResolvedId {
        id,
        resolution: Resolution::FromFile,
    }
}

/// The id file's id, or `None` when there is no file at `path`.
fn read_id_file(path: &Path) -> Result<Option<AgentId>, AgentIdError> {
    match std::fs::read(path) {
        Ok(bytes) => parse_id_file(bytes).map(Some),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(AgentIdError::Unreadable(err)),
    }
}

fn parse_id_file(bytes: Vec<u8>) -> Result<AgentId, AgentIdError> {
    let text = String::from_utf8(bytes).map_err(|_| AgentIdError::NotUtf8)?;
    AgentId::try_from(text.as_str()).map_err(AgentIdError::Invalid)
}

/// The first usable source after the id file, else a new random UUID.
fn from_sources(sources: &IdSources) -> (AgentId, IdSource) {
    let utf8 = |path: &Path| {
        let bytes = std::fs::read(path).ok()?;
        AgentId::try_from(std::str::from_utf8(&bytes).ok()?).ok()
    };
    let lossy = |path: &Path| {
        let bytes = std::fs::read(path).ok()?;
        AgentId::try_from(String::from_utf8_lossy(&bytes).as_ref()).ok()
    };
    utf8(&sources.machine_id)
        .map(|id| (id, IdSource::MachineId))
        .or_else(|| utf8(&sources.dbus_machine_id).map(|id| (id, IdSource::DbusMachineId)))
        .or_else(|| lossy(&sources.host_name).map(|id| (id, IdSource::HostName)))
        .unwrap_or_else(|| (AgentId(uuid::Uuid::new_v4().to_string()), IdSource::Random))
}

/// Links a new id file into place at `path`, never replacing one: if another process created
/// it first, its id is read instead, and that read is final.
fn persist_new_id(path: &Path, resolved: ResolvedId) -> Result<ResolvedId, AgentIdError> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    let mut temp_name = OsString::from(".");
    temp_name.push(path.file_name().unwrap_or_default());
    temp_name.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let temp = dir.join(temp_name);
    let linked = write_temp(&temp, &resolved.id).and_then(|()| {
        std::fs::hard_link(&temp, path).map_err(|err| match err.kind() {
            io::ErrorKind::AlreadyExists => LinkError::Taken,
            _ => LinkError::Failed(link_failure(err)),
        })
    });
    let _ = std::fs::remove_file(&temp);
    match linked {
        Ok(()) => {
            sync_dir(dir);
            Ok(resolved)
        }
        Err(LinkError::Taken) => read_final(path).map(from_file),
        Err(LinkError::Failed(err)) => Err(err),
    }
}

/// Why the new id file isn't in place.
enum LinkError {
    /// Another process linked the file first.
    Taken,
    Failed(AgentIdError),
}

fn write_temp(temp: &Path, id: &AgentId) -> Result<(), LinkError> {
    use std::io::Write;
    let write = || {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temp)?;
        file.write_all(format!("{}\n", id.as_str()).as_bytes())?;
        file.sync_all()
    };
    write().map_err(|err| LinkError::Failed(AgentIdError::Unwritable(err)))
}

/// The read after losing the link: any failure, a missing file included, is final.
fn read_final(path: &Path) -> Result<AgentId, AgentIdError> {
    std::fs::read(path)
        .map_err(AgentIdError::Unreadable)
        .and_then(parse_id_file)
}

/// Flushes the directory entry; a failure only risks losing the file in a crash, so it is a
/// warning, not a refusal.
fn sync_dir(dir: &Path) {
    if let Err(err) = std::fs::File::open(dir).and_then(|dir| dir.sync_all()) {
        tracing::warn!("{ID_FILE_VARIABLE}: the id file's directory couldn't be synced: {err}");
    }
}

/// What a failed `hard_link` of the new id file means: a filesystem without hard links (`EPERM`,
/// or an unsupported operation), else a failed write.
fn link_failure(err: io::Error) -> AgentIdError {
    const EPERM: i32 = 1;
    if err.raw_os_error() == Some(EPERM) || err.kind() == io::ErrorKind::Unsupported {
        AgentIdError::NoHardLinks(err)
    } else {
        AgentIdError::Unwritable(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::ffi::OsStringExt;
    use std::sync::{Arc, Barrier};

    /// A directory of its own under the system's temp directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("agent-id-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        fn write(&self, name: &str, content: &[u8]) -> PathBuf {
            let path = self.path(name);
            fs::write(&path, content).unwrap();
            path
        }

        fn dir(&self, name: &str) -> PathBuf {
            let path = self.path(name);
            fs::create_dir(&path).unwrap();
            path
        }

        /// The names in the directory, sorted.
        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// What a test puts at a source's path.
    #[derive(Clone, Copy)]
    enum Plant<'a> {
        Missing,
        File(&'a [u8]),
        /// What Docker's `-v /etc/machine-id:/etc/machine-id` makes on a host without the file.
        Directory,
    }

    /// Sources under `scratch`, each planted as asked.
    fn sources(scratch: &Scratch, machine_id: Plant, dbus: Plant, host: Plant) -> IdSources {
        let place = |name: &str, plant: Plant| match plant {
            Plant::Missing => scratch.path(name),
            Plant::File(content) => scratch.write(name, content),
            Plant::Directory => scratch.dir(name),
        };
        IdSources {
            machine_id: place("machine-id", machine_id),
            dbus_machine_id: place("dbus-machine-id", dbus),
            host_name: place("hostname", host),
        }
    }

    /// Only a host name, in its own scratch directory.
    fn host_only(scratch: &Scratch, host: &[u8]) -> IdSources {
        sources(scratch, Plant::Missing, Plant::Missing, Plant::File(host))
    }

    /// Built directly, so these tests don't depend on `from_env`, which has its own.
    fn id_file(path: PathBuf) -> IdFilePath {
        IdFilePath(path)
    }

    /// Built directly, so these tests don't depend on the rule, which has its own.
    fn agent_id(id: &str) -> AgentId {
        AgentId(id.to_owned())
    }

    /// RFC 0016 §6: the hub's `SystemId` rule (RFC 0005), on the value after trimming ASCII
    /// whitespace, so the agent never presents an id the hub refuses.
    #[test]
    fn an_agent_id_follows_the_hubs_system_id_rule_after_trimming() {
        let ascii_255 = "a".repeat(255);
        let ascii_256 = "a".repeat(256);
        let multi_byte_255 = format!("{}é", "a".repeat(253));
        let multi_byte_256 = format!("{}é", "a".repeat(254));
        let padded_255 = format!("  {ascii_255}\n");
        let cases = [
            ("empty id", "", Err(AgentIdRule::Empty)),
            ("only whitespace", " \t\n", Err(AgentIdRule::Empty)),
            ("one-byte id", "x", Ok("x")),
            (
                "machine id with its newline",
                "0123456789abcdef0123456789abcdef\n",
                Ok("0123456789abcdef0123456789abcdef"),
            ),
            ("a CRLF line", "abc\r\n", Ok("abc")),
            // ASCII trimming only: a no-break space and a vertical tab aren't ASCII
            // whitespace for `trim_ascii`, and stay part of the id, as the hub would keep them.
            ("a no-break space is kept", "\u{a0}id", Ok("\u{a0}id")),
            ("a vertical tab is kept", "\x0bid", Ok("\x0bid")),
            ("single dot", ".", Err(AgentIdRule::DotSegment)),
            ("double dot", "..", Err(AgentIdRule::DotSegment)),
            ("double dot, padded", " ..\n", Err(AgentIdRule::DotSegment)),
            ("three dots are one segment", "...", Ok("...")),
            ("dotted hostname", "a.b", Ok("a.b")),
            ("leading dot", ".hidden", Ok(".hidden")),
            ("inner space kept", "a b", Ok("a b")),
            (
                "255 ascii bytes",
                ascii_255.as_str(),
                Ok(ascii_255.as_str()),
            ),
            (
                "255 bytes once trimmed",
                padded_255.as_str(),
                Ok(ascii_255.as_str()),
            ),
            (
                "255 bytes with a multi-byte character",
                multi_byte_255.as_str(),
                Ok(multi_byte_255.as_str()),
            ),
            (
                "256 ascii bytes",
                ascii_256.as_str(),
                Err(AgentIdRule::TooLong),
            ),
            (
                "256 bytes in 255 characters",
                multi_byte_256.as_str(),
                Err(AgentIdRule::TooLong),
            ),
        ];
        for (name, input, expected) in cases {
            let got = AgentId::try_from(input);
            assert_eq!(
                got.as_ref().map(AgentId::as_str).map_err(|rule| *rule),
                expected,
                "case {name}"
            );
        }
    }

    /// RFC 0016 §6: unset or empty is no id file; a set value must be absolute, and may be any
    /// bytes.
    #[test]
    fn the_id_file_variable_is_unset_empty_or_an_absolute_path() {
        let not_utf8 = OsString::from_vec(b"/var/lib/agent-\xff/id".to_vec());
        let cases: [(&str, Option<OsString>, Result<Option<PathBuf>, ()>); 7] = [
            ("unset", None, Ok(None)),
            ("empty", Some(OsString::new()), Ok(None)),
            (
                "absolute",
                Some("/var/lib/system-agent/id".into()),
                Ok(Some(PathBuf::from("/var/lib/system-agent/id"))),
            ),
            ("relative", Some("system-agent/id".into()), Err(())),
            ("bare name", Some("id".into()), Err(())),
            ("a space is a value, not unset", Some(" ".into()), Err(())),
            (
                "absolute, not UTF-8",
                Some(not_utf8.clone()),
                Ok(Some(PathBuf::from(not_utf8))),
            ),
        ];
        for (name, value, expected) in cases {
            let got = IdFilePath::from_env(value);
            match (got, expected) {
                (Ok(path), Ok(expected)) => {
                    assert_eq!(path.map(|p| p.0), expected, "case {name}");
                }
                (Err(AgentIdError::NotAbsolute), Err(())) => {}
                (got, expected) => panic!("case {name}: got {got:?}, expected {expected:?}"),
            }
        }
    }

    /// RFC 0016 §6: with no id file, the machine id, the dbus machine id, the host name, then a
    /// random UUID; a source that is missing, unreadable, not UTF-8 (the machine ids), empty or
    /// breaking the rule is skipped. The host name is converted lossily, as the `hostname`
    /// binary's output was.
    #[test]
    fn with_no_id_file_the_first_usable_source_gives_the_id() {
        use Plant::{Directory, File, Missing};
        let host_64 = "h".repeat(64);
        let too_long = "m".repeat(256);
        let host = File(b"host\n");
        let cases: [(&str, Plant, Plant, Plant, IdSource, &str); 10] = [
            (
                "every source present",
                File(b"machine\n"),
                File(b"dbus\n"),
                host,
                IdSource::MachineId,
                "machine",
            ),
            (
                "an empty machine id",
                File(b"\n"),
                File(b"dbus\n"),
                host,
                IdSource::DbusMachineId,
                "dbus",
            ),
            (
                "a machine id over 255 bytes",
                File(too_long.as_bytes()),
                File(b"dbus\n"),
                host,
                IdSource::DbusMachineId,
                "dbus",
            ),
            (
                "a machine id that is a directory",
                Directory,
                File(b"dbus\n"),
                host,
                IdSource::DbusMachineId,
                "dbus",
            ),
            (
                "a machine id that isn't UTF-8",
                File(b"\xff\xfe"),
                Missing,
                host,
                IdSource::HostName,
                "host",
            ),
            (
                "a blank dbus machine id",
                Missing,
                File(b"  \n"),
                host,
                IdSource::HostName,
                "host",
            ),
            (
                "a dbus machine id that is a dot segment",
                Missing,
                File(b"..\n"),
                host,
                IdSource::HostName,
                "host",
            ),
            (
                "a dbus machine id that isn't UTF-8",
                Missing,
                File(b"\xc3"),
                host,
                IdSource::HostName,
                "host",
            ),
            (
                "a 64-byte host name",
                Missing,
                Missing,
                File(host_64.as_bytes()),
                IdSource::HostName,
                host_64.as_str(),
            ),
            (
                "a host name that isn't UTF-8",
                Missing,
                Directory,
                File(b"host-\xff\n"),
                IdSource::HostName,
                "host-\u{fffd}",
            ),
        ];
        for (name, machine_id, dbus, host, source, id) in cases {
            let scratch = Scratch::new();
            let sources = sources(&scratch, machine_id, dbus, host);

            let resolved = resolve_push_id(None, &sources).unwrap_or_else(|err| {
                panic!("case {name}: {err:?}");
            });

            assert_eq!(
                resolved,
                ResolvedId {
                    id: agent_id(id),
                    resolution: Resolution::Resolved(source),
                },
                "case {name}"
            );
        }
    }

    /// RFC 0016 §6: with no usable source, a random (v4) UUID, a new one for each resolution:
    /// a fixed fallback would make every such host one system on the hub.
    #[test]
    fn with_no_usable_source_each_resolution_draws_a_new_random_uuid() {
        use Plant::{Directory, File, Missing};
        let cases: [(&str, Plant, Plant, Plant); 3] = [
            ("no source at all", Missing, Missing, Missing),
            (
                "a host name that is a dot segment",
                Missing,
                Missing,
                File(b"..\n"),
            ),
            (
                "unreadable machine ids, a blank host name",
                Directory,
                Directory,
                File(b" \n"),
            ),
        ];
        for (name, machine_id, dbus, host) in cases {
            let scratch = Scratch::new();
            let sources = sources(&scratch, machine_id, dbus, host);

            let first = resolve_push_id(None, &sources).unwrap();
            let second = resolve_push_id(None, &sources).unwrap();

            for resolved in [&first, &second] {
                assert_eq!(
                    resolved.resolution,
                    Resolution::Resolved(IdSource::Random),
                    "case {name}"
                );
                let uuid = uuid::Uuid::parse_str(resolved.id.as_str());
                assert_eq!(
                    uuid.ok().and_then(|uuid| uuid.get_version()),
                    Some(uuid::Version::Random),
                    "case {name}: a v4 UUID, got {:?}",
                    resolved.id
                );
                assert_eq!(resolved.id.as_str().len(), 36, "case {name}: hyphenated");
            }
            assert_ne!(first.id, second.id, "case {name}: a new UUID each time");
        }
    }

    /// RFC 0016 §6: a missing id file captures the id the sources give, and from then on the file
    /// is the id, whatever the sources become. No temporary file is left behind.
    #[test]
    fn a_missing_id_file_is_written_once_and_then_wins() {
        let scratch = Scratch::new();
        let file = id_file(scratch.path("id"));
        let first = host_only(&scratch, b"first-host\n");

        let written = resolve_push_id(Some(&file), &first).unwrap();

        assert_eq!(
            written,
            ResolvedId {
                id: agent_id("first-host"),
                resolution: Resolution::Written(IdSource::HostName),
            }
        );
        let stored = fs::read_to_string(scratch.path("id")).unwrap();
        assert_eq!(stored.trim(), "first-host", "the file holds the id");
        assert_eq!(
            scratch.names(),
            ["hostname", "id"],
            "no temporary file is left"
        );

        let second = sources(
            &scratch,
            Plant::File(b"a-machine-id\n"),
            Plant::Missing,
            Plant::File(b"second-host\n"),
        );
        let read = resolve_push_id(Some(&file), &second).unwrap();

        assert_eq!(
            read,
            ResolvedId {
                id: agent_id("first-host"),
                resolution: Resolution::FromFile,
            },
            "the file wins over the machine id and a changed host name"
        );
    }

    /// RFC 0016 §6: agents racing to create one id file all end up with one id, the one the file
    /// holds, and no file is ever replaced or left half-written: whoever links first wins, and
    /// every other reads what it linked.
    #[test]
    fn agents_racing_for_a_missing_id_file_all_end_up_with_the_one_it_holds() {
        const RACERS: usize = 8;
        for round in 0..20 {
            let shared = Scratch::new();
            let path = shared.path("id");
            let start = Arc::new(Barrier::new(RACERS));
            let racers: Vec<_> = (0..RACERS)
                .map(|racer| {
                    let (path, start) = (path.clone(), Arc::clone(&start));
                    std::thread::spawn(move || {
                        let own = Scratch::new();
                        let host = format!("host-{racer}\n");
                        let sources = host_only(&own, host.as_bytes());
                        start.wait();
                        resolve_push_id(Some(&id_file(path)), &sources)
                    })
                })
                .collect();
            let results: Vec<ResolvedId> = racers
                .into_iter()
                .map(|racer| racer.join().unwrap().unwrap())
                .collect();

            let stored = fs::read_to_string(&path);
            assert!(stored.is_ok(), "round {round}: the file was written: {stored:?}");
            let winner = agent_id(stored.unwrap_or_default().trim());
            assert!(
                results.iter().all(|resolved| resolved.id == winner),
                "round {round}: every racer holds the file's id {winner:?}: {results:?}"
            );
            let written = results
                .iter()
                .filter(|resolved| matches!(resolved.resolution, Resolution::Written(_)))
                .count();
            assert_eq!(written, 1, "round {round}: one racer wrote it: {results:?}");
            assert_eq!(
                shared.names(),
                ["id"],
                "round {round}: no temporary file is left"
            );
        }
    }

    /// RFC 0016 §6: an id file that can't be used refuses startup, and the error says why,
    /// never with the path.
    #[test]
    fn an_unusable_id_file_refuses_startup() {
        // (case, what to plant at the id path, the expected error, what its message says)
        type Check = fn(&AgentIdError) -> bool;
        let cases: [(&str, fn(&Scratch) -> PathBuf, Check, &str); 7] = [
            (
                "a dot segment",
                |s| s.write("id", b"..\n"),
                |e| matches!(e, AgentIdError::Invalid(AgentIdRule::DotSegment)),
                "`.` or `..`",
            ),
            (
                "only whitespace",
                |s| s.write("id", b" \n"),
                |e| matches!(e, AgentIdError::Invalid(AgentIdRule::Empty)),
                "an empty id",
            ),
            (
                "a zero-byte file",
                |s| s.write("id", b""),
                |e| matches!(e, AgentIdError::Invalid(AgentIdRule::Empty)),
                "an empty id",
            ),
            (
                "an id over 255 bytes",
                |s| s.write("id", "i".repeat(256).as_bytes()),
                |e| matches!(e, AgentIdError::Invalid(AgentIdRule::TooLong)),
                "longer than 255 bytes",
            ),
            (
                "not UTF-8",
                |s| s.write("id", b"\xff\xfe\n"),
                |e| matches!(e, AgentIdError::NotUtf8),
                "UTF-8",
            ),
            (
                "a directory",
                |s| s.dir("id"),
                |e| matches!(e, AgentIdError::Unreadable(_)),
                "couldn't be read",
            ),
            (
                "in a directory that doesn't exist",
                |s| s.path("missing").join("id"),
                |e| matches!(e, AgentIdError::Unwritable(_)),
                "couldn't be written",
            ),
        ];
        for (name, plant, is_expected, says) in cases {
            let scratch = Scratch::new();
            let path = plant(&scratch);
            let before = fs::read(&path).ok();
            let sources = sources(
                &scratch,
                Plant::File(b"machine\n"),
                Plant::Missing,
                Plant::Missing,
            );

            let err = resolve_push_id(Some(&id_file(path.clone())), &sources).expect_err(name);

            assert!(is_expected(&err), "case {name}: got {err:?}");
            let message = err.to_string();
            assert!(
                message.contains(ID_FILE_VARIABLE) && message.contains(says),
                "case {name}: names the variable and says {says:?}: {message}"
            );
            assert!(
                !message.contains(&*scratch.0.to_string_lossy()),
                "case {name}: never the path: {message}"
            );
            assert_eq!(
                fs::read(&path).ok(),
                before,
                "case {name}: the file is untouched"
            );
        }
    }

    /// RFC 0016 §6: which refusals are refused values (exit 78) and which are I/O failures
    /// (exit 1), and what each message says.
    #[test]
    fn each_refusal_is_a_refused_value_or_an_io_failure() {
        let io = || io::Error::other("boom");
        let cases = [
            (
                "not absolute",
                AgentIdError::NotAbsolute,
                true,
                "absolute path",
            ),
            ("not UTF-8", AgentIdError::NotUtf8, true, "UTF-8"),
            (
                "breaking the rule",
                AgentIdError::Invalid(AgentIdRule::DotSegment),
                true,
                "`.` or `..`",
            ),
            (
                "unreadable",
                AgentIdError::Unreadable(io()),
                false,
                "couldn't be read: boom",
            ),
            (
                "unwritable",
                AgentIdError::Unwritable(io()),
                false,
                "couldn't be written: boom",
            ),
            (
                "no hard links",
                AgentIdError::NoHardLinks(io()),
                false,
                "may not support hard links: boom",
            ),
        ];
        for (name, err, refused_value, says) in cases {
            assert_eq!(err.is_refused_value(), refused_value, "case {name}");
            let message = err.to_string();
            assert!(
                message.starts_with(ID_FILE_VARIABLE) && message.contains(says),
                "case {name}: says {says:?}: {message}"
            );
        }
    }

    /// RFC 0016 §6: a `link(2)` refused the way a filesystem without hard links refuses is
    /// reported as such, so the operator doesn't chase ownership; any other failure is a failed
    /// write.
    #[test]
    fn a_link_refused_for_want_of_hard_links_is_named() {
        const EPERM: i32 = 1;
        const EACCES: i32 = 13;
        const ENOSPC: i32 = 28;
        let cases: [(&str, io::Error, bool); 4] = [
            ("EPERM", io::Error::from_raw_os_error(EPERM), true),
            (
                "unsupported",
                io::Error::from(io::ErrorKind::Unsupported),
                true,
            ),
            ("EACCES", io::Error::from_raw_os_error(EACCES), false),
            ("ENOSPC", io::Error::from_raw_os_error(ENOSPC), false),
        ];
        for (name, err, no_hard_links) in cases {
            let got = link_failure(err);
            assert_eq!(
                matches!(got, AgentIdError::NoHardLinks(_)),
                no_hard_links,
                "case {name}: {got:?}"
            );
            assert_eq!(
                matches!(got, AgentIdError::Unwritable(_)),
                !no_hard_links,
                "case {name}: {got:?}"
            );
        }
    }

    /// RFC 0016 §6: when another process linked the file first, the read that follows is final
    /// and applies the file's rules: its id is used and never replaced; an invalid one is
    /// refused as a value; a dangling symlink is `Unreadable`, never another write.
    #[test]
    fn after_losing_the_link_the_read_is_final() {
        type Check = fn(&Result<ResolvedId, AgentIdError>) -> bool;
        let cases: [(&str, fn(&Scratch) -> PathBuf, Check); 3] = [
            (
                "a valid file",
                |s| s.write("id", b"the-other-process\n"),
                |r| {
                    matches!(r, Ok(ResolvedId { id, resolution: Resolution::FromFile })
                        if id.as_str() == "the-other-process")
                },
            ),
            (
                "an invalid file",
                |s| s.write("id", b"..\n"),
                |r| matches!(r, Err(AgentIdError::Invalid(AgentIdRule::DotSegment))),
            ),
            (
                "a dangling symlink",
                |s| {
                    let path = s.path("id");
                    std::os::unix::fs::symlink(s.path("nowhere"), &path).unwrap();
                    path
                },
                |r| {
                    matches!(r, Err(AgentIdError::Unreadable(io))
                        if io.kind() == io::ErrorKind::NotFound)
                },
            ),
        ];
        for (name, plant, is_expected) in cases {
            let scratch = Scratch::new();
            let path = plant(&scratch);
            let before = fs::read(&path).ok();
            let ours = ResolvedId {
                id: agent_id("ours"),
                resolution: Resolution::Written(IdSource::HostName),
            };

            let got = persist_new_id(&path, ours);

            assert!(is_expected(&got), "case {name}: got {got:?}");
            assert_eq!(fs::read(&path).ok(), before, "case {name}: never replaced");
            assert_eq!(
                scratch.names(),
                ["id"],
                "case {name}: no temporary file is left"
            );
            assert!(
                !scratch.path("nowhere").exists(),
                "case {name}: nothing written through the symlink"
            );
        }
    }

    /// RFC 0016 §6: the whole resolution over a dangling symlink at the id path: it reads as
    /// missing, the link finds the name taken, and the read after that is final.
    #[test]
    fn a_dangling_symlink_at_the_id_path_is_unreadable() {
        let scratch = Scratch::new();
        let path = scratch.path("id");
        std::os::unix::fs::symlink(scratch.path("nowhere"), &path).unwrap();
        let sources = host_only(&scratch, b"host\n");

        let err = resolve_push_id(Some(&id_file(path)), &sources).expect_err("refused");

        assert!(
            matches!(&err, AgentIdError::Unreadable(io) if io.kind() == io::ErrorKind::NotFound),
            "got {err:?}"
        );
        assert_eq!(
            scratch.names(),
            ["hostname", "id"],
            "no temporary file is left"
        );
        assert!(
            !scratch.path("nowhere").exists(),
            "nothing written through it"
        );
    }
}
