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

//! Which open push connection is current for each push system, and what the disconnection
//! sweep does with one (RFC 0016 §2, §4). Pure: no lock, no clock read, no I/O. The caller
//! holds it under the presence lock, and passes the time the hub has been up.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use crate::models::{SystemId, SystemStatus};

/// How long after the hub starts a push system with no current connection is left alone: its
/// agent may still be reconnecting (RFC 0010 §10's restart rule).
pub const RECONNECT_GRACE: Duration = Duration::from_secs(120);

/// Identifies one accepted push connection within one hub process. Never persisted: numbers
/// only compare connections of one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionNumber(u64);

/// Held by one connection's task for as long as the task runs, and dropped with it, a panic's
/// unwinding included. The presence entry keeps only a `Weak` of it, so a task that is gone is
/// seen without anything running in a destructor.
#[derive(Debug)]
pub struct ConnectionLease(Arc<ConnectionNumber>);

impl ConnectionLease {
    pub fn number(&self) -> ConnectionNumber {
        *self.0
    }
}

/// A system's current connection: its number, whether its task still runs, and whether a
/// snapshot on it has claimed currency.
#[derive(Debug)]
struct Current {
    number: ConnectionNumber,
    task: Weak<ConnectionNumber>,
    claimed: bool,
}

/// Every push system's current connection, keyed by system id.
#[derive(Debug, Default)]
pub struct PushPresence {
    next: u64,
    current: HashMap<String, Current>,
}

/// What a connection's end means for its system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// The ending connection was current: the caller marks the system offline and evicts its
    /// live metrics.
    Current,
    /// Another connection is current, or none is: nothing changes.
    NotCurrent,
}

/// What the disconnection sweep does with one push system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sweep {
    Leave,
    MarkOffline(OfflineReason),
}

/// Why the sweep marks a push system offline: its `last_error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineReason {
    /// No live current connection, the hub up past the reconnect grace.
    NotConnected,
    /// An id stored before RFC 0005 that breaks `SystemId`: no handshake can make it current.
    InvalidSystemId,
}

impl OfflineReason {
    /// The `last_error` the marking writes.
    pub fn last_error(self) -> &'static str {
        match self {
            Self::NotConnected => "push disconnected",
            Self::InvalidSystemId => "invalid system id",
        }
    }
}

impl Current {
    /// Whether its connection's task still runs.
    fn is_live(&self) -> bool {
        self.task.strong_count() > 0
    }
}

impl PushPresence {
    /// How many systems have an entry. For tests: the map's size is what the sweep bounds.
    #[cfg(test)]
    fn entries(&self) -> usize {
        self.current.len()
    }

    /// A handshake accepted: takes the next number, and makes it current only when the system
    /// has no live current connection.
    pub fn accept(&mut self, system_id: &SystemId) -> ConnectionLease {
        let number = ConnectionNumber(self.next);
        self.next = self.next.wrapping_add(1);
        let lease = ConnectionLease(Arc::new(number));
        let taken = self
            .current
            .get(system_id.as_str())
            .is_some_and(Current::is_live);
        if !taken {
            self.current
                .insert(system_id.as_str().to_owned(), current(&lease, false));
        }
        lease
    }

    /// A snapshot frame on `lease`'s connection: that connection becomes current, and has
    /// claimed.
    pub fn claim(&mut self, system_id: &SystemId, lease: &ConnectionLease) {
        self.current
            .insert(system_id.as_str().to_owned(), current(lease, true));
    }

    /// `lease`'s connection ended. When it was current, its entry is removed. Takes the lease
    /// by value, so nothing can claim with it afterwards.
    pub fn end(&mut self, system_id: &SystemId, lease: ConnectionLease) -> Ending {
        let is_current = self
            .current
            .get(system_id.as_str())
            .is_some_and(|current| current.number == lease.number());
        if is_current {
            self.current.remove(system_id.as_str());
            Ending::Current
        } else {
            Ending::NotCurrent
        }
    }

    /// What the sweep does with a push system stored with `status`, the hub up for `up_for`.
    /// An entry whose task is gone counts as none, and is removed.
    pub fn sweep(&mut self, system_id: &str, status: &SystemStatus, up_for: Duration) -> Sweep {
        let connection = self.connection_of(system_id);
        let disconnected = match (status, connection) {
            (SystemStatus::Offline, _) => false,
            (SystemStatus::Online, Connection::Claimed) => false,
            (SystemStatus::Online, Connection::Unclaimed) => true,
            (SystemStatus::Unknown, Connection::Claimed | Connection::Unclaimed) => false,
            (SystemStatus::Online | SystemStatus::Unknown, Connection::None) => true,
        };
        if !disconnected || up_for < RECONNECT_GRACE {
            return Sweep::Leave;
        }
        match SystemId::try_from(system_id.to_owned()) {
            Ok(_) => Sweep::MarkOffline(OfflineReason::NotConnected),
            Err(_) => Sweep::MarkOffline(OfflineReason::InvalidSystemId),
        }
    }

    /// The system's live current connection, forgetting an entry whose task is gone.
    fn connection_of(&mut self, system_id: &str) -> Connection {
        match self.current.get(system_id) {
            None => Connection::None,
            Some(current) if !current.is_live() => {
                self.current.remove(system_id);
                Connection::None
            }
            Some(current) if current.claimed => Connection::Claimed,
            Some(_) => Connection::Unclaimed,
        }
    }
}

/// What the sweep sees of a system's current connection.
enum Connection {
    None,
    Claimed,
    Unclaimed,
}

fn current(lease: &ConnectionLease, claimed: bool) -> Current {
    Current {
        number: lease.number(),
        task: Arc::downgrade(&lease.0),
        claimed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(id: &str) -> SystemId {
        SystemId::try_from(id.to_owned()).unwrap()
    }

    const PAST_GRACE: Duration = Duration::from_secs(120);

    /// RFC 0016 §2: every accepted connection gets a new number.
    #[test]
    fn every_accepted_connection_gets_a_new_number() {
        let mut presence = PushPresence::default();

        let numbers: Vec<ConnectionNumber> = ["a", "a", "b", "a"]
            .iter()
            .map(|system| presence.accept(&id(system)).number())
            .collect();

        for (i, number) in numbers.iter().enumerate() {
            assert!(
                !numbers[..i].contains(number),
                "connection {i} reuses a number: {numbers:?}"
            );
        }
    }

    /// RFC 0016 §2: a handshake makes its connection current only when the system has no live
    /// current connection; between open connections only a snapshot moves currency. Observed
    /// through `end`: only the current connection's end is `Current`.
    #[test]
    fn a_handshake_takes_currency_only_from_no_live_connection() {
        let sys = id("sys-1");
        // (case, whether a first connection is accepted before, and whether its task is gone)
        let cases = [
            ("no connection before", false, false, Ending::Current),
            (
                "a live connection is current",
                true,
                false,
                Ending::NotCurrent,
            ),
            (
                "the current one's task is gone",
                true,
                true,
                Ending::Current,
            ),
        ];
        for (name, first, first_gone, expected) in cases {
            let mut presence = PushPresence::default();
            let earlier = first.then(|| presence.accept(&sys));
            if first_gone {
                drop(earlier);
            }
            let second = presence.accept(&sys);

            assert_eq!(
                presence.end(&sys, second),
                expected,
                "case {name}: the new connection's end"
            );
        }
    }

    /// RFC 0016 §2, as sequences over one system: a snapshot claims currency; only the current
    /// connection's end is `Current`, and it removes the entry; newest exits first.
    #[test]
    fn a_snapshot_claims_currency_and_only_the_current_end_counts() {
        #[derive(Clone, Copy)]
        enum Step {
            Accept,
            Claim(usize),
            End(usize, Ending),
        }
        use Ending::{Current, NotCurrent};
        use Step::{Accept, Claim, End};
        // An end can't be counted twice: `end` takes the lease by value.
        let cases: [(&str, &[Step]); 4] = [
            (
                "a reconnect claims, then the old connection times out",
                &[
                    Accept,
                    Claim(0),
                    Accept,
                    Claim(1),
                    End(0, NotCurrent),
                    End(1, Current),
                ],
            ),
            (
                "a connection that never claims can't take currency",
                &[
                    Accept,
                    Claim(0),
                    Accept,
                    End(0, Current),
                    End(1, NotCurrent),
                ],
            ),
            (
                "after the current end, a snapshot on the other claims",
                &[
                    Accept,
                    Claim(0),
                    Accept,
                    End(0, Current),
                    Claim(1),
                    End(1, Current),
                ],
            ),
            (
                "newest exits first",
                &[
                    Accept,
                    Claim(0),
                    Accept,
                    Claim(1),
                    End(1, Current),
                    Claim(0),
                    End(0, Current),
                ],
            ),
        ];
        let sys = id("sys-1");
        for (name, steps) in cases {
            let mut presence = PushPresence::default();
            let mut leases: Vec<Option<ConnectionLease>> = Vec::new();
            for (at, step) in steps.iter().enumerate() {
                match *step {
                    Accept => leases.push(Some(presence.accept(&sys))),
                    Claim(n) => presence.claim(&sys, leases[n].as_ref().expect("not ended")),
                    End(n, expected) => assert_eq!(
                        presence.end(&sys, leases[n].take().expect("ended once")),
                        expected,
                        "case {name}, step {at}"
                    ),
                }
            }
        }
    }

    /// RFC 0016 §2: a connection's end concerns only its own system.
    #[test]
    fn an_end_concerns_only_its_own_system() {
        // (case, the system the lease was accepted for, the system its end names, the outcome)
        let cases = [
            ("another system's end", "a", "b", Ending::NotCurrent),
            ("an unknown system", "a", "unknown", Ending::NotCurrent),
            ("its own system", "a", "a", Ending::Current),
        ];
        for (name, accepted_for, ended_for, expected) in cases {
            let mut presence = PushPresence::default();
            let lease = presence.accept(&id(accepted_for));
            let other = presence.accept(&id("b"));
            let other_lease = (accepted_for != "b").then_some(other);

            assert_eq!(presence.end(&id(ended_for), lease), expected, "case {name}");
            if let Some(other) = other_lease {
                assert_eq!(
                    presence.end(&id("b"), other),
                    Ending::Current,
                    "case {name}: b's own connection is still current"
                );
            }
        }
    }

    /// RFC 0016 §4: the sweep's table.
    #[test]
    fn the_sweep_marks_offline_only_a_disconnected_push_system_past_the_grace() {
        #[derive(Clone, Copy)]
        enum Connection {
            None,
            /// Accepted, and a snapshot on it claimed.
            Live,
            /// Accepted, and no snapshot claimed on it.
            LiveUnclaimed,
            TaskGone,
        }
        let just_under = PAST_GRACE - Duration::from_secs(1);
        let not_connected = Sweep::MarkOffline(OfflineReason::NotConnected);
        let cases = [
            (
                "offline, none",
                SystemStatus::Offline,
                Connection::None,
                PAST_GRACE,
                Sweep::Leave,
            ),
            (
                "offline, live",
                SystemStatus::Offline,
                Connection::Live,
                PAST_GRACE,
                Sweep::Leave,
            ),
            (
                "online, live, claimed",
                SystemStatus::Online,
                Connection::Live,
                PAST_GRACE,
                Sweep::Leave,
            ),
            (
                "online, live, never claimed, in the grace",
                SystemStatus::Online,
                Connection::LiveUnclaimed,
                just_under,
                Sweep::Leave,
            ),
            (
                "online, live, never claimed, at the grace",
                SystemStatus::Online,
                Connection::LiveUnclaimed,
                PAST_GRACE,
                not_connected,
            ),
            (
                "unknown, live, claimed",
                SystemStatus::Unknown,
                Connection::Live,
                PAST_GRACE,
                Sweep::Leave,
            ),
            (
                "unknown, live, never claimed",
                SystemStatus::Unknown,
                Connection::LiveUnclaimed,
                PAST_GRACE,
                Sweep::Leave,
            ),
            (
                "offline, live, never claimed",
                SystemStatus::Offline,
                Connection::LiveUnclaimed,
                PAST_GRACE,
                Sweep::Leave,
            ),
            (
                "online, none, in the grace",
                SystemStatus::Online,
                Connection::None,
                just_under,
                Sweep::Leave,
            ),
            (
                "online, none, at the grace",
                SystemStatus::Online,
                Connection::None,
                PAST_GRACE,
                not_connected,
            ),
            (
                "unknown, none, at the grace",
                SystemStatus::Unknown,
                Connection::None,
                PAST_GRACE,
                not_connected,
            ),
            (
                "online, task gone, at the grace",
                SystemStatus::Online,
                Connection::TaskGone,
                PAST_GRACE,
                not_connected,
            ),
            (
                "online, task gone, in the grace",
                SystemStatus::Online,
                Connection::TaskGone,
                just_under,
                Sweep::Leave,
            ),
            (
                "online, none, long up",
                SystemStatus::Online,
                Connection::None,
                Duration::from_secs(86_400),
                not_connected,
            ),
        ];
        let sys = id("sys-1");
        for (name, status, connection, up_for, expected) in cases {
            let mut presence = PushPresence::default();
            let lease = match connection {
                Connection::None => None,
                Connection::Live => {
                    let lease = presence.accept(&sys);
                    presence.claim(&sys, &lease);
                    Some(lease)
                }
                Connection::LiveUnclaimed => Some(presence.accept(&sys)),
                Connection::TaskGone => {
                    drop(presence.accept(&sys));
                    None
                }
            };

            assert_eq!(
                presence.sweep(sys.as_str(), &status, up_for),
                expected,
                "case {name}"
            );
            drop(lease);
        }
    }

    /// RFC 0016 §4: an id that breaks `SystemId` (stored before RFC 0005) is marked offline with
    /// its own reason.
    #[test]
    fn the_sweep_names_an_invalid_system_id() {
        let long = "a".repeat(256);
        let mut presence = PushPresence::default();
        for invalid in ["..", ".", "", long.as_str()] {
            assert_eq!(
                presence.sweep(invalid, &SystemStatus::Online, PAST_GRACE),
                Sweep::MarkOffline(OfflineReason::InvalidSystemId),
                "id {:?}",
                &invalid[..invalid.len().min(8)]
            );
        }
    }

    /// RFC 0016 §4: the sweep removes an entry whose task is gone, and keeps a live one, so
    /// the presence map holds no more than the running connections.
    #[test]
    fn the_sweep_forgets_an_entry_whose_task_is_gone() {
        let sys = id("sys-1");
        // (case, whether the task is gone, whether the sweep runs, entries after)
        let cases = [
            ("task gone, no sweep: the entry stays", true, false, 1),
            ("task gone, a sweep: the entry is gone", true, true, 0),
            ("task live, a sweep: the entry stays", false, true, 1),
        ];
        for (name, gone, swept, entries) in cases {
            let mut presence = PushPresence::default();
            let lease = presence.accept(&sys);
            let kept = (!gone).then_some(lease);

            if swept {
                presence.sweep(sys.as_str(), &SystemStatus::Offline, Duration::ZERO);
            }

            assert_eq!(presence.entries(), entries, "case {name}");
            drop(kept);
        }
    }

    /// RFC 0016 §4: the texts the markings write, the same as a connection's end and a poll of
    /// an invalid id.
    #[test]
    fn each_offline_reason_has_its_last_error() {
        let cases = [
            (OfflineReason::NotConnected, "push disconnected"),
            (OfflineReason::InvalidSystemId, "invalid system id"),
        ];
        for (reason, text) in cases {
            assert_eq!(reason.last_error(), text, "{reason:?}");
        }
    }
}
