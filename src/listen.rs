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

//! Where the agent serves: `SYSTEM_AGENT_LISTEN`, parsed once at startup (RFC 0015).

use std::env::VarError;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Where the agent serves: an IP address and a port. Port 0 asks the OS for a free port; the
/// startup line logs the one bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenAddress(SocketAddr);

/// Why `SYSTEM_AGENT_LISTEN` was refused. Carries no part of the value: the raw string is
/// untrusted and never logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenAddressError {
    /// Set, but not `<IPv4>:<port>` or `[<IPv6>]:<port>`.
    Invalid,
    /// Set, but not valid UTF-8.
    NotUnicode,
}

impl ListenAddress {
    pub const VARIABLE: &str = "SYSTEM_AGENT_LISTEN";
    pub const DEFAULT: Self = Self(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9090));

    /// The address `value` (what `std::env::var` returned) names: the default when unset or
    /// empty, else a literal IP address and port. A host name is refused, so startup never
    /// resolves one.
    pub fn from_env(value: Result<String, VarError>) -> Result<Self, ListenAddressError> {
        match value {
            Err(VarError::NotPresent) => Ok(Self::DEFAULT),
            Err(VarError::NotUnicode(_)) => Err(ListenAddressError::NotUnicode),
            Ok(value) if value.is_empty() => Ok(Self::DEFAULT),
            Ok(value) => value
                .parse()
                .map(Self)
                .map_err(|_| ListenAddressError::Invalid),
        }
    }

    pub fn get(self) -> SocketAddr {
        self.0
    }
}

impl fmt::Display for ListenAddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variable = ListenAddress::VARIABLE;
        match self {
            Self::Invalid => write!(
                f,
                "{variable} must be an IP address and a port, such as 0.0.0.0:9090, \
                 127.0.0.1:9090 or [::]:9090"
            ),
            Self::NotUnicode => write!(f, "{variable} is not valid UTF-8"),
        }
    }
}

/// The address to suggest for reaching a service bound at `bound`: `localhost` for an
/// unspecified IP (`0.0.0.0`, `[::]`), which no client can connect to, else the bound one.
pub fn reachable_at(bound: SocketAddr) -> String {
    if bound.ip().is_unspecified() {
        format!("localhost:{}", bound.port())
    } else {
        bound.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_listen_address_is_a_literal_ip_and_port_or_the_default() {
        let set = |value: &str| Ok(value.to_owned());
        let at = |addr: &str| Ok(ListenAddress(addr.parse().expect("a test address")));
        let invalid = Err(ListenAddressError::Invalid);
        // (name, value, expected)
        let cases = [
            (
                "unset",
                Err(VarError::NotPresent),
                Ok(ListenAddress::DEFAULT),
            ),
            ("empty", set(""), Ok(ListenAddress::DEFAULT)),
            ("another port", set("0.0.0.0:9190"), at("0.0.0.0:9190")),
            ("loopback", set("127.0.0.1:9090"), at("127.0.0.1:9090")),
            ("ipv6 loopback", set("[::1]:9091"), at("[::1]:9091")),
            ("ipv6 unspecified", set("[::]:9090"), at("[::]:9090")),
            ("port 0", set("127.0.0.1:0"), at("127.0.0.1:0")),
            (
                "the highest port",
                set("0.0.0.0:65535"),
                at("0.0.0.0:65535"),
            ),
            (
                "leading zeros",
                set("127.0.0.1:09090"),
                at("127.0.0.1:9090"),
            ),
            ("a port alone", set("9090"), invalid),
            ("a host name", set("localhost:9090"), invalid),
            ("no port", set("127.0.0.1"), invalid),
            ("a port past u16", set("127.0.0.1:65536"), invalid),
            ("ipv6 without brackets", set("::1:9090"), invalid),
            ("leading space", set(" 127.0.0.1:9090"), invalid),
            ("trailing space", set("127.0.0.1:9090 "), invalid),
            ("a signed port", set("127.0.0.1:+80"), invalid),
            ("garbage", set("abc"), invalid),
            ("only whitespace", set("   "), invalid),
            (
                "an ipv6 scope id, as std reads it",
                set("[fe80::1%2]:9090"),
                at("[fe80::1%2]:9090"),
            ),
            (
                "not unicode",
                Err(VarError::NotUnicode("\u{fffd}".into())),
                Err(ListenAddressError::NotUnicode),
            ),
        ];
        for (name, value, expected) in cases {
            assert_eq!(ListenAddress::from_env(value), expected, "{name}");
        }
    }

    #[test]
    fn the_default_is_every_ipv4_interface_on_9090() {
        assert_eq!(ListenAddress::DEFAULT.get().to_string(), "0.0.0.0:9090");
    }

    #[test]
    fn a_refusal_names_the_variable_and_what_it_takes() {
        let invalid = ListenAddressError::Invalid.to_string();
        let not_unicode = ListenAddressError::NotUnicode.to_string();
        for message in [&invalid, &not_unicode] {
            assert!(message.contains("SYSTEM_AGENT_LISTEN"), "{message}");
        }
        assert!(
            invalid.contains("0.0.0.0:9090"),
            "shows the expected form: {invalid}"
        );
        assert!(
            !invalid.contains("UTF-8"),
            "not an encoding problem: {invalid}"
        );
        assert!(
            not_unicode.contains("not valid UTF-8"),
            "says what's wrong: {not_unicode}"
        );
        assert!(
            !not_unicode.contains("0.0.0.0:"),
            "not a form problem: {not_unicode}"
        );
    }

    #[test]
    fn a_service_is_suggested_at_localhost_only_when_bound_to_every_interface() {
        // (bound, expected)
        let cases = [
            ("0.0.0.0:9090", "localhost:9090"),
            ("[::]:9090", "localhost:9090"),
            ("127.0.0.1:9090", "127.0.0.1:9090"),
            ("192.168.1.5:9190", "192.168.1.5:9190"),
            ("10.0.0.0:9090", "10.0.0.0:9090"),
            ("100.0.0.0:9090", "100.0.0.0:9090"),
            ("[::1]:9091", "[::1]:9091"),
        ];
        for (bound, expected) in cases {
            let bound: SocketAddr = bound.parse().expect("a test address");
            assert_eq!(reachable_at(bound), expected, "{bound}");
        }
    }
}
