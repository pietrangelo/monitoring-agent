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

    fn try_from(_value: &str) -> Result<Self, Self::Error> {
        Err(AgentIdRule::Empty)
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
        false
    }
}

impl fmt::Display for AgentIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{ID_FILE_VARIABLE}")
    }
}

/// `SYSTEM_AGENT_ID_FILE`, parsed: an absolute path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdFilePath(PathBuf);

impl IdFilePath {
    /// Unset or empty means no id file; any other value must be an absolute path. Paths may be
    /// any bytes, so the value is read with `var_os`.
    pub fn from_env(_value: Option<OsString>) -> Result<Option<Self>, AgentIdError> {
        Ok(None)
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
    _id_file: Option<&IdFilePath>,
    _sources: &IdSources,
) -> Result<ResolvedId, AgentIdError> {
    Ok(ResolvedId {
        id: AgentId(String::new()),
        resolution: Resolution::Resolved(IdSource::Random),
    })
}

/// Links a new id file into place at `path`, never replacing one: if another process created
/// it first, its id is read instead, and that read is final.
fn persist_new_id(_path: &Path, resolved: ResolvedId) -> Result<ResolvedId, AgentIdError> {
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::ffi::OsStringExt;

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

    /// Sources under `scratch`: each written when given, missing when `None`.
    fn sources(
        scratch: &Scratch,
        machine_id: Option<&[u8]>,
        dbus: Option<&[u8]>,
        host: Option<&[u8]>,
    ) -> IdSources {
        let place = |name: &str, content: Option<&[u8]>| match content {
            Some(content) => scratch.write(name, content),
            None => scratch.path(name),
        };
        IdSources {
            machine_id: place("machine-id", machine_id),
            dbus_machine_id: place("dbus-machine-id", dbus),
            host_name: place("hostname", host),
        }
    }

    /// Built directly, so these tests don't depend on `from_env`, which has its own.
    fn id_file(path: PathBuf) -> IdFilePath {
        IdFilePath(path)
    }

    /// Built directly, so these tests don't depend on the rule, which has its own.
    fn agent_id(id: &str) -> AgentId {
        AgentId(id.to_owned())
    }

    /// RFC 0016 §6: the hub's `SystemId` rule (RFC 0005), on the value after trimming, so the
    /// agent never presents an id the hub refuses.
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
            (
                "uuid",
                "123e4567-e89b-12d3-a456-426614174000",
                Ok("123e4567-e89b-12d3-a456-426614174000"),
            ),
            ("single dot", ".", Err(AgentIdRule::DotSegment)),
            ("double dot", "..", Err(AgentIdRule::DotSegment)),
            ("double dot, padded", " ..\n", Err(AgentIdRule::DotSegment)),
            ("three dots are one segment", "...", Ok("...")),
            ("dotted hostname", "a.b", Ok("a.b")),
            ("leading dot", ".hidden", Ok(".hidden")),
            ("inner space kept", "a b", Ok("a b")),
            ("255 ascii bytes", ascii_255.as_str(), Ok(ascii_255.as_str())),
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
            ("256 ascii bytes", ascii_256.as_str(), Err(AgentIdRule::TooLong)),
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
        let cases: [(&str, Option<OsString>, Result<Option<PathBuf>, ()>); 6] = [
            ("unset", None, Ok(None)),
            ("empty", Some(OsString::new()), Ok(None)),
            (
                "absolute",
                Some("/var/lib/system-agent/id".into()),
                Ok(Some(PathBuf::from("/var/lib/system-agent/id"))),
            ),
            ("relative", Some("system-agent/id".into()), Err(())),
            ("bare name", Some("id".into()), Err(())),
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
    /// random UUID; a source that is missing, empty or breaks the rule is skipped. The host name
    /// is converted lossily, as the `hostname` binary's output was.
    #[test]
    fn with_no_id_file_the_first_usable_source_gives_the_id() {
        let host_64 = "h".repeat(64);
        let too_long = "m".repeat(256);
        type Case<'a> = (
            &'a str,
            Option<&'a [u8]>,
            Option<&'a [u8]>,
            Option<&'a [u8]>,
            IdSource,
            Option<&'a str>,
        );
        let cases: [Case; 9] = [
            (
                "every source present",
                Some(b"machine\n"),
                Some(b"dbus\n"),
                Some(b"host\n"),
                IdSource::MachineId,
                Some("machine"),
            ),
            (
                "an empty machine id",
                Some(b"\n"),
                Some(b"dbus\n"),
                Some(b"host\n"),
                IdSource::DbusMachineId,
                Some("dbus"),
            ),
            (
                "a machine id over 255 bytes",
                Some(too_long.as_bytes()),
                Some(b"dbus\n"),
                Some(b"host\n"),
                IdSource::DbusMachineId,
                Some("dbus"),
            ),
            (
                "no machine ids",
                None,
                Some(b"  \n"),
                Some(b"host\n"),
                IdSource::HostName,
                Some("host"),
            ),
            (
                "a 64-byte host name",
                None,
                None,
                Some(host_64.as_bytes()),
                IdSource::HostName,
                Some(host_64.as_str()),
            ),
            (
                "a host name that isn't UTF-8",
                None,
                None,
                Some(b"host-\xff\n"),
                IdSource::HostName,
                Some("host-\u{fffd}"),
            ),
            (
                "a host name that is a dot segment",
                None,
                None,
                Some(b"..\n"),
                IdSource::Random,
                None,
            ),
            (
                "a machine id that isn't UTF-8",
                Some(b"\xff\xfe"),
                None,
                Some(b"host\n"),
                IdSource::HostName,
                Some("host"),
            ),
            ("no source at all", None, None, None, IdSource::Random, None),
        ];
        for (name, machine_id, dbus, host, source, id) in cases {
            let scratch = Scratch::new();
            let sources = sources(&scratch, machine_id, dbus, host);

            let resolved = resolve_push_id(None, &sources).unwrap_or_else(|err| {
                panic!("case {name}: {err:?}");
            });

            assert_eq!(resolved.resolution, Resolution::Resolved(source), "case {name}");
            match id {
                Some(id) => assert_eq!(resolved.id.as_str(), id, "case {name}"),
                None => assert!(
                    uuid::Uuid::parse_str(resolved.id.as_str()).is_ok(),
                    "case {name}: a UUID, got {:?}",
                    resolved.id
                ),
            }
        }
    }

    /// RFC 0016 §6: a missing id file captures the id the sources give, and from then on the file
    /// is the id, whatever the sources become. No temporary file is left behind.
    #[test]
    fn a_missing_id_file_is_written_once_and_then_wins() {
        let scratch = Scratch::new();
        let file = id_file(scratch.path("id"));
        let first = sources(&scratch, None, None, Some(b"first-host\n"));

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

        let second = sources(&scratch, Some(b"a-machine-id\n"), None, Some(b"second-host\n"));
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

    /// RFC 0016 §6: an id file that can't be used refuses startup, with the error saying why:
    /// a refused value (78) or an I/O failure (1).
    #[test]
    fn an_unusable_id_file_refuses_startup() {
        // (case, what to plant at the id path, is it a refused value)
        let cases: [(&str, fn(&Scratch) -> PathBuf, fn(&AgentIdError) -> bool, bool); 5] = [
            (
                "a dot segment",
                |s| s.write("id", b"..\n"),
                |e| matches!(e, AgentIdError::Invalid(AgentIdRule::DotSegment)),
                true,
            ),
            (
                "only whitespace",
                |s| s.write("id", b" \n"),
                |e| matches!(e, AgentIdError::Invalid(AgentIdRule::Empty)),
                true,
            ),
            (
                "not UTF-8",
                |s| s.write("id", b"\xff\xfe\n"),
                |e| matches!(e, AgentIdError::NotUtf8),
                true,
            ),
            (
                "a directory",
                |s| {
                    let path = s.path("id");
                    fs::create_dir(&path).unwrap();
                    path
                },
                |e| matches!(e, AgentIdError::Unreadable(_)),
                false,
            ),
            (
                "in a directory that doesn't exist",
                |s| s.path("missing").join("id"),
                |e| matches!(e, AgentIdError::Unwritable(_)),
                false,
            ),
        ];
        for (name, plant, is_expected, refused_value) in cases {
            let scratch = Scratch::new();
            let file = id_file(plant(&scratch));
            let sources = sources(&scratch, Some(b"machine\n"), None, None);

            let err = resolve_push_id(Some(&file), &sources).expect_err(name);

            assert!(is_expected(&err), "case {name}: got {err:?}");
            assert_eq!(err.is_refused_value(), refused_value, "case {name}");
            let message = err.to_string();
            assert!(
                message.contains(ID_FILE_VARIABLE),
                "case {name}: names the variable: {message}"
            );
            assert!(
                !message.contains(&*scratch.0.to_string_lossy()),
                "case {name}: never the path: {message}"
            );
        }
    }

    /// RFC 0016 §6: when another process links the file first, its id is read, the file is
    /// never replaced, and the temporary file is removed.
    #[test]
    fn an_id_file_created_by_another_process_first_is_read_not_replaced() {
        let scratch = Scratch::new();
        let path = scratch.write("id", b"the-other-process\n");
        let ours = ResolvedId {
            id: agent_id("ours"),
            resolution: Resolution::Written(IdSource::HostName),
        };

        let resolved = persist_new_id(&path, ours).unwrap();

        assert_eq!(
            resolved,
            ResolvedId {
                id: agent_id("the-other-process"),
                resolution: Resolution::FromFile,
            }
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "the-other-process\n",
            "never replaced"
        );
        assert_eq!(scratch.names(), ["id"], "no temporary file is left");
    }

    /// RFC 0016 §6: a dangling symlink at the id path reads as missing, then its link attempt
    /// finds the name taken, and the read after that is final: `Unreadable`, never a loop.
    #[test]
    fn a_dangling_symlink_at_the_id_path_is_unreadable_after_one_attempt() {
        let scratch = Scratch::new();
        let path = scratch.path("id");
        std::os::unix::fs::symlink(scratch.path("nowhere"), &path).unwrap();
        let sources = sources(&scratch, None, None, Some(b"host\n"));

        let err = resolve_push_id(Some(&id_file(path)), &sources).expect_err("refused");

        assert!(
            matches!(&err, AgentIdError::Unreadable(io) if io.kind() == io::ErrorKind::NotFound),
            "got {err:?}"
        );
        assert!(!err.is_refused_value(), "an I/O failure, exit 1");
        assert_eq!(scratch.names(), ["hostname", "id"], "no temporary file is left");
    }
}
