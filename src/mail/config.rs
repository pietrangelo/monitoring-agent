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

//! The mail transport's configuration (RFC 0017 §5, §8): the `MAIL_*` variables, parsed once
//! at startup, before the runtime exists. A refusal names the variable, never its value.

use std::env::VarError;
use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;

use lettre::Address;

use super::batch::SampleInterval;
use super::report::MailInterval;
use super::seal::{MailKey, MailKeyError};

/// Whether the agent mails reports, and how.
pub enum MailConfig {
    Off,
    On(MailSettings),
}

/// Everything the mail client needs. No `Debug`: it holds the key and the relay password.
pub struct MailSettings {
    pub to: Address,
    /// `None`: the client uses `system-agent@<host name>`.
    pub from: Option<Address>,
    pub relay: Relay,
    pub tls: MailTls,
    pub relay_ca: Option<PathBuf>,
    pub credentials: Option<RelayCredentials>,
    pub key: MailKey,
    pub interval: MailInterval,
    pub sample_interval: SampleInterval,
}

/// The SMTP relay's host and port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relay {
    pub host: String,
    pub port: u16,
}

/// How the client talks to the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailTls {
    /// STARTTLS, required: the session fails if the relay doesn't offer it.
    StartTls,
    /// Implicit TLS (port 465).
    Tls,
    /// Plain SMTP: only to a loopback relay.
    None,
}

/// SMTP AUTH credentials. No `Debug`: the password is a secret.
pub struct RelayCredentials {
    pub username: String,
    pub password: String,
}

/// Why the mail configuration refused startup. Names a variable, never a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailConfigError {
    NotUnicode(&'static str),
    Missing(&'static str),
    NotAnAddress(&'static str),
    BadRelay,
    BadTls,
    /// `MAIL_TLS=none` with a relay that isn't loopback.
    PlainToRemote,
    /// Relay credentials with `MAIL_TLS=none`.
    CredentialsInPlain,
    /// Only one of `MAIL_RELAY_USERNAME` and `MAIL_RELAY_PASSWORD`.
    HalfCredentials,
    Key(MailKeyError),
    Interval,
    SampleInterval,
}

impl fmt::Display for MailConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUnicode(var) => write!(f, "{var} isn't valid UTF-8"),
            Self::Missing(var) => write!(f, "{var} must be set when MAIL_TO is"),
            Self::NotAnAddress(var) => write!(f, "{var} isn't a mail address"),
            Self::BadRelay => f.write_str("MAIL_RELAY isn't <host>:<port>"),
            Self::BadTls => f.write_str("MAIL_TLS isn't starttls, tls or none"),
            Self::PlainToRemote => f.write_str("MAIL_TLS=none needs a loopback MAIL_RELAY"),
            Self::CredentialsInPlain => {
                f.write_str("MAIL_RELAY_USERNAME/_PASSWORD need MAIL_TLS starttls or tls")
            }
            Self::HalfCredentials => {
                f.write_str("MAIL_RELAY_USERNAME and MAIL_RELAY_PASSWORD go together")
            }
            Self::Key(MailKeyError::NotBase64) => f.write_str("MAIL_KEY isn't base64"),
            Self::Key(MailKeyError::WrongLength) => f.write_str("MAIL_KEY isn't 32 bytes"),
            Self::Interval => f.write_str("MAIL_INTERVAL isn't 60 to 86400 seconds"),
            Self::SampleInterval => f.write_str(
                "MAIL_SAMPLE_INTERVAL isn't 10 s up to MAIL_INTERVAL, at most 60 samples a report",
            ),
        }
    }
}

impl MailConfig {
    /// Parses the `MAIL_*` variables through `lookup` (`std::env::var` in production).
    pub fn parse(
        lookup: impl Fn(&str) -> Result<String, VarError>,
    ) -> Result<Self, MailConfigError> {
        let read = |var: &'static str| read(&lookup, var);
        let Some(to) = read("MAIL_TO")? else {
            return Ok(Self::Off);
        };
        let required = |var: &'static str| read(var)?.ok_or(MailConfigError::Missing(var));
        let relay = Relay::parse(&required("MAIL_RELAY")?)?;
        let tls = MailTls::parse(read("MAIL_TLS")?.as_deref())?;
        let credentials = credentials(read("MAIL_RELAY_USERNAME")?, read("MAIL_RELAY_PASSWORD")?)?;
        check_plain(tls, &relay, credentials.is_some())?;
        let interval = interval(read("MAIL_INTERVAL")?)?;
        Ok(Self::On(MailSettings {
            to: address("MAIL_TO", &to)?,
            from: read("MAIL_FROM")?
                .map(|from| address("MAIL_FROM", &from))
                .transpose()?,
            relay,
            tls,
            relay_ca: read("MAIL_RELAY_CA")?.map(PathBuf::from),
            credentials,
            key: MailKey::from_base64(&required("MAIL_KEY")?).map_err(MailConfigError::Key)?,
            interval,
            sample_interval: sample_interval(read("MAIL_SAMPLE_INTERVAL")?, interval)?,
        }))
    }
}

/// A variable's value; unset and empty are `None`.
fn read(
    lookup: &impl Fn(&str) -> Result<String, VarError>,
    var: &'static str,
) -> Result<Option<String>, MailConfigError> {
    match lookup(var) {
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => Ok(Some(value.trim().to_owned())),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(MailConfigError::NotUnicode(var)),
    }
}

fn address(var: &'static str, value: &str) -> Result<Address, MailConfigError> {
    Address::from_str(value).map_err(|_| MailConfigError::NotAnAddress(var))
}

impl Relay {
    /// `<host>:<port>`, an IPv6 host in brackets.
    fn parse(value: &str) -> Result<Self, MailConfigError> {
        let (host, port) = value.rsplit_once(':').ok_or(MailConfigError::BadRelay)?;
        let port = port.parse().map_err(|_| MailConfigError::BadRelay)?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() || host.contains(char::is_whitespace) {
            return Err(MailConfigError::BadRelay);
        }
        Ok(Self {
            host: host.to_owned(),
            port,
        })
    }

    /// Whether the relay is on this host.
    fn is_loopback(&self) -> bool {
        self.host == "localhost" || IpAddr::from_str(&self.host).is_ok_and(|ip| ip.is_loopback())
    }
}

impl MailTls {
    fn parse(value: Option<&str>) -> Result<Self, MailConfigError> {
        match value {
            None | Some("starttls") => Ok(Self::StartTls),
            Some("tls") => Ok(Self::Tls),
            Some("none") => Ok(Self::None),
            Some(_) => Err(MailConfigError::BadTls),
        }
    }
}

fn credentials(
    username: Option<String>,
    password: Option<String>,
) -> Result<Option<RelayCredentials>, MailConfigError> {
    match (username, password) {
        (None, None) => Ok(None),
        (Some(username), Some(password)) => Ok(Some(RelayCredentials { username, password })),
        (Some(_), None) | (None, Some(_)) => Err(MailConfigError::HalfCredentials),
    }
}

/// Plain SMTP only to a loopback relay, and never with credentials.
fn check_plain(tls: MailTls, relay: &Relay, credentials: bool) -> Result<(), MailConfigError> {
    match tls {
        MailTls::StartTls | MailTls::Tls => Ok(()),
        MailTls::None if !relay.is_loopback() => Err(MailConfigError::PlainToRemote),
        MailTls::None if credentials => Err(MailConfigError::CredentialsInPlain),
        MailTls::None => Ok(()),
    }
}

fn interval(value: Option<String>) -> Result<MailInterval, MailConfigError> {
    let secs = value.map_or(Ok(300), |value| value.parse());
    secs.ok()
        .and_then(|secs| MailInterval::try_from(secs).ok())
        .ok_or(MailConfigError::Interval)
}

fn sample_interval(
    value: Option<String>,
    mail: MailInterval,
) -> Result<SampleInterval, MailConfigError> {
    let secs = value.map_or(Ok(60), |value| value.parse());
    secs.ok()
        .and_then(|secs| SampleInterval::new(secs, mail).ok())
        .ok_or(MailConfigError::SampleInterval)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const KEY: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";

    fn base() -> HashMap<&'static str, String> {
        HashMap::from([
            ("MAIL_TO", "hub@example.org".to_string()),
            ("MAIL_RELAY", "smtp.internal:25".to_string()),
            ("MAIL_KEY", KEY.to_string()),
        ])
    }

    fn parse(vars: &HashMap<&'static str, String>) -> Result<MailConfig, MailConfigError> {
        MailConfig::parse(|key| vars.get(key).cloned().ok_or(VarError::NotPresent))
    }

    fn with(changes: &[(&'static str, Option<&str>)]) -> HashMap<&'static str, String> {
        let mut vars = base();
        for (key, value) in changes {
            match value {
                Some(value) => vars.insert(key, value.to_string()),
                None => vars.remove(key),
            };
        }
        vars
    }

    /// RFC 0017 §5, §8: every variable's rule, each refusal naming its variable.
    #[test]
    fn the_mail_variables_are_parsed_or_refused() {
        use MailConfigError::*;
        let cases: Vec<(
            &str,
            HashMap<&'static str, String>,
            Result<(), MailConfigError>,
        )> = vec![
            ("the defaults", base(), Ok(())),
            (
                "no MAIL_TO: off, whatever else",
                with(&[("MAIL_TO", None), ("MAIL_KEY", Some("x"))]),
                Ok(()),
            ),
            (
                "an empty MAIL_TO: off",
                with(&[("MAIL_TO", Some(""))]),
                Ok(()),
            ),
            (
                "not an address",
                with(&[("MAIL_TO", Some("hub"))]),
                Err(NotAnAddress("MAIL_TO")),
            ),
            (
                "a bad MAIL_FROM",
                with(&[("MAIL_FROM", Some("@"))]),
                Err(NotAnAddress("MAIL_FROM")),
            ),
            (
                "no relay",
                with(&[("MAIL_RELAY", None)]),
                Err(Missing("MAIL_RELAY")),
            ),
            (
                "a relay without a port",
                with(&[("MAIL_RELAY", Some("smtp"))]),
                Err(BadRelay),
            ),
            (
                "a relay with a bad port",
                with(&[("MAIL_RELAY", Some("smtp:99999"))]),
                Err(BadRelay),
            ),
            (
                "an IPv6 relay",
                with(&[("MAIL_RELAY", Some("[fd00::25]:25"))]),
                Ok(()),
            ),
            (
                "no key",
                with(&[("MAIL_KEY", None)]),
                Err(Missing("MAIL_KEY")),
            ),
            (
                "a short key",
                with(&[("MAIL_KEY", Some("AAAA"))]),
                Err(Key(MailKeyError::WrongLength)),
            ),
            ("tls", with(&[("MAIL_TLS", Some("tls"))]), Ok(())),
            (
                "an unknown tls",
                with(&[("MAIL_TLS", Some("ssl"))]),
                Err(BadTls),
            ),
            (
                "none to a remote relay",
                with(&[("MAIL_TLS", Some("none"))]),
                Err(PlainToRemote),
            ),
            (
                "none to loopback",
                with(&[
                    ("MAIL_TLS", Some("none")),
                    ("MAIL_RELAY", Some("127.0.0.1:25")),
                ]),
                Ok(()),
            ),
            (
                "none to localhost",
                with(&[
                    ("MAIL_TLS", Some("none")),
                    ("MAIL_RELAY", Some("localhost:25")),
                ]),
                Ok(()),
            ),
            (
                "credentials in plain",
                with(&[
                    ("MAIL_TLS", Some("none")),
                    ("MAIL_RELAY", Some("127.0.0.1:25")),
                    ("MAIL_RELAY_USERNAME", Some("u")),
                    ("MAIL_RELAY_PASSWORD", Some("leak-marker")),
                ]),
                Err(CredentialsInPlain),
            ),
            (
                "half credentials",
                with(&[("MAIL_RELAY_USERNAME", Some("u"))]),
                Err(HalfCredentials),
            ),
            (
                "an interval of 59 s",
                with(&[("MAIL_INTERVAL", Some("59"))]),
                Err(Interval),
            ),
            (
                "an interval that isn't a number",
                with(&[("MAIL_INTERVAL", Some("5m"))]),
                Err(Interval),
            ),
            (
                "a sample of 9 s",
                with(&[("MAIL_SAMPLE_INTERVAL", Some("9"))]),
                Err(SampleInterval),
            ),
            (
                "61 samples a report",
                with(&[
                    ("MAIL_INTERVAL", Some("3600")),
                    ("MAIL_SAMPLE_INTERVAL", Some("59")),
                ]),
                Err(SampleInterval),
            ),
        ];
        for (name, vars, expected) in cases {
            let got = parse(&vars).map(|_| ());
            assert_eq!(got, expected, "case {name}");
            if let Err(err) = parse(&vars) {
                assert!(
                    !err.to_string().contains("leak-marker"),
                    "case {name}: never a value"
                );
            }
        }
    }

    /// RFC 0017 §8: what the defaults are.
    #[test]
    fn the_defaults_are_starttls_five_minutes_and_a_sample_a_minute() {
        let settings = match parse(&base()) {
            Ok(MailConfig::On(settings)) => Some(settings),
            Ok(MailConfig::Off) | Err(_) => None,
        };
        assert!(settings.is_some(), "mail is on");
        let settings = settings.unwrap();
        assert_eq!(settings.tls, MailTls::StartTls);
        assert_eq!(
            settings.relay,
            Relay {
                host: "smtp.internal".into(),
                port: 25
            }
        );
        assert_eq!(settings.interval.as_duration().as_secs(), 300);
        assert_eq!(settings.to.to_string(), "hub@example.org");
        assert!(settings.from.is_none());
        assert!(settings.credentials.is_none());
    }
}
