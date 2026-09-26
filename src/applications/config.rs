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

//! Which applications to scrape, parsed once at startup from the environment into types that
//! can't hold an invalid setting (RFC 0009 §2). Pure: the environment comes in through a
//! lookup function, so `main` passes `std::env::var` and tests pass a table.

use std::collections::HashSet;
use std::env::VarError;
use std::fmt;
use std::ops::RangeInclusive;
use std::time::Duration;

/// Names the applications to scrape: comma-separated `name=actuator-base-url` pairs.
const APPS_VARIABLE: &str = "SPRING_BOOT_APPS";
/// Seconds between scrape rounds.
const INTERVAL_VARIABLE: &str = "SPRING_BOOT_SCRAPE_INTERVAL";
/// The most applications one agent scrapes.
const MAX_APPLICATIONS: usize = 16;

/// A Spring Boot application's name, as the operator gave it: 1 to 64 bytes of
/// `[A-Za-z0-9_.-]`, and not `.` or `..`. So it needs no escaping in a URL path segment, an
/// environment variable name, or a hub metric name (`app:<name>:<gauge>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationName(String);

impl ApplicationName {
    const MAX_LEN: usize = 64;

    fn parse(value: &str) -> Option<Self> {
        let allowed = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-');
        let valid = (1..=Self::MAX_LEN).contains(&value.len())
            && value.bytes().all(allowed)
            && value != "."
            && value != "..";
        valid.then(|| Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The `<KEY>` of this application's credential variables: upper case, with `-` and `.`
    /// read as `_`. Two names with the same key would share credentials, so they can't coexist.
    fn credentials_key(&self) -> String {
        self.0
            .chars()
            .map(|c| match c {
                '-' | '.' => '_',
                c => c.to_ascii_uppercase(),
            })
            .collect()
    }
}

/// Where an application's Actuator endpoints live: an absolute http(s) URL with no userinfo,
/// query or fragment, whose path ends in `/`, so endpoint paths join *under* it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActuatorBaseUrl(url::Url);

impl ActuatorBaseUrl {
    fn parse(value: &str) -> Result<Self, UrlProblem> {
        let mut url = url::Url::parse(value).map_err(|_| UrlProblem::NotAUrl)?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(UrlProblem::NotHttp);
        }
        // Credentials belong in the password variable, where a logged URL can't leak them.
        if !url.username().is_empty() || url.password().is_some() {
            return Err(UrlProblem::HasUserinfo);
        }
        if url.query().is_some() {
            return Err(UrlProblem::HasQuery);
        }
        if url.fragment().is_some() {
            return Err(UrlProblem::HasFragment);
        }
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        Ok(Self(url))
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the actuator adapter, RFC 0009 commit 2")
    )]
    pub fn as_url(&self) -> &url::Url {
        &self.0
    }

    /// Whether a request to this URL crosses the network unencrypted: plain `http://` to a
    /// host that isn't a loopback address.
    fn is_plaintext_to_another_host(&self) -> bool {
        self.0.scheme() == "http" && !is_loopback(self.0.host())
    }
}

fn is_loopback(host: Option<url::Host<&str>>) -> bool {
    match host {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// A username and password HTTP Basic can carry (RFC 7617 §2): no `:` in the username, and
/// no control character in either. Built only by `parse`, so holding one proves the rule.
/// No `Debug`, `Display` or `Serialize`: the only way out is the Basic header the actuator
/// adapter builds.
pub struct BasicCredentials {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the actuator adapter, RFC 0009 commit 2")
    )]
    username: String,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the actuator adapter, RFC 0009 commit 2")
    )]
    password: String,
}

/// Which of the two credential values a problem is in.
enum CredentialField {
    Username,
    Password,
}

impl BasicCredentials {
    /// Checks the username (a control character first, then a colon), then the password.
    fn parse(
        username: String,
        password: String,
    ) -> Result<Self, (CredentialField, CredentialProblem)> {
        use CredentialProblem as P;
        if has_control_character(&username) {
            return Err((CredentialField::Username, P::ControlCharacter));
        }
        if username.contains(':') {
            return Err((CredentialField::Username, P::Colon));
        }
        if has_control_character(&password) {
            return Err((CredentialField::Password, P::ControlCharacter));
        }
        Ok(Self { username, password })
    }
}

fn has_control_character(value: &str) -> bool {
    value.chars().any(char::is_control)
}

/// How the agent authenticates to one application's Actuator.
pub enum ActuatorCredentials {
    None,
    Basic(
        #[cfg_attr(
            not(test),
            expect(dead_code, reason = "read by the actuator adapter, RFC 0009 commit 2")
        )]
        BasicCredentials,
    ),
}

impl ActuatorCredentials {
    /// Reads `SPRING_BOOT_APP_<KEY>_USERNAME` and `_PASSWORD`: both or neither, then usable.
    fn read(
        name: &ApplicationName,
        lookup: &impl Fn(&str) -> Result<String, VarError>,
    ) -> Result<Self, ApplicationsConfigError> {
        let key = name.credentials_key();
        let username_variable = format!("SPRING_BOOT_APP_{key}_USERNAME");
        let password_variable = format!("SPRING_BOOT_APP_{key}_PASSWORD");
        let username = read_variable(lookup, &username_variable)?;
        let password = read_variable(lookup, &password_variable)?;
        match (username, password) {
            (None, None) => Ok(Self::None),
            (Some(username), Some(password)) => BasicCredentials::parse(username, password)
                .map(Self::Basic)
                .map_err(
                    |(field, problem)| ApplicationsConfigError::InvalidCredential {
                        variable: match field {
                            CredentialField::Username => username_variable,
                            CredentialField::Password => password_variable,
                        },
                        problem,
                    },
                ),
            (Some(_), None) => Err(ApplicationsConfigError::IncompleteCredentials {
                missing: password_variable,
            }),
            (None, Some(_)) => Err(ApplicationsConfigError::IncompleteCredentials {
                missing: username_variable,
            }),
        }
    }
}

/// One application to scrape.
pub struct ApplicationTarget {
    name: ApplicationName,
    base_url: ActuatorBaseUrl,
    credentials: ActuatorCredentials,
}

impl ApplicationTarget {
    /// Parses the pair at 1-based `position` of `SPRING_BOOT_APPS`, and reads its credentials.
    fn parse(
        position: usize,
        pair: &str,
        lookup: &impl Fn(&str) -> Result<String, VarError>,
    ) -> Result<Self, ApplicationsConfigError> {
        use ApplicationsConfigError as E;
        let (name, url) = pair
            .split_once('=')
            .map(|(name, url)| (name.trim(), url.trim()))
            .filter(|(name, url)| !name.is_empty() && !url.is_empty())
            .ok_or(E::MalformedPair { position })?;
        let name = ApplicationName::parse(name).ok_or(E::InvalidName { position })?;
        let base_url =
            ActuatorBaseUrl::parse(url).map_err(|problem| E::InvalidUrl { position, problem })?;
        let credentials = ActuatorCredentials::read(&name, lookup)?;
        Ok(Self {
            name,
            base_url,
            credentials,
        })
    }

    pub fn name(&self) -> &ApplicationName {
        &self.name
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the actuator adapter, RFC 0009 commit 2")
    )]
    pub fn base_url(&self) -> &ActuatorBaseUrl {
        &self.base_url
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the actuator adapter, RFC 0009 commit 2")
    )]
    pub fn credentials(&self) -> &ActuatorCredentials {
        &self.credentials
    }

    fn sends_plaintext_credentials(&self) -> bool {
        matches!(self.credentials, ActuatorCredentials::Basic(_))
            && self.base_url.is_plaintext_to_another_host()
    }
}

/// Time between the end of one scrape round and the start of the next: 10 s to 3600 s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrapeInterval(Duration);

impl ScrapeInterval {
    const DEFAULT_SECS: u64 = 15;
    const SECS: RangeInclusive<u64> = 10..=3600;

    /// Parses the trimmed, non-empty value of `SPRING_BOOT_SCRAPE_INTERVAL`, or the default.
    fn parse(value: Option<String>) -> Result<Self, ApplicationsConfigError> {
        let secs = match value {
            None => Self::DEFAULT_SECS,
            Some(value) => value
                .parse()
                .ok()
                .filter(|secs| Self::SECS.contains(secs))
                .ok_or(ApplicationsConfigError::InvalidInterval)?,
        };
        Ok(Self(Duration::from_secs(secs)))
    }

    pub fn as_duration(self) -> Duration {
        self.0
    }
}

/// The applications to scrape, and how often. At most 16, with distinct credential keys.
pub struct Applications {
    targets: Vec<ApplicationTarget>,
    interval: ScrapeInterval,
}

impl Applications {
    pub fn targets(&self) -> &[ApplicationTarget] {
        &self.targets
    }

    pub fn interval(&self) -> ScrapeInterval {
        self.interval
    }
}

/// Whether the agent scrapes applications at all.
pub enum ApplicationsConfig {
    /// `SPRING_BOOT_APPS` is unset, empty or blank: no scrape loop runs.
    Off,
    On(Applications),
}

/// What makes a URL unusable as an actuator base URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlProblem {
    NotAUrl,
    NotHttp,
    HasUserinfo,
    HasQuery,
    HasFragment,
}

impl fmt::Display for UrlProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotAUrl => "the actuator base URL is not an absolute URL",
            Self::NotHttp => "the actuator base URL is not http or https",
            Self::HasUserinfo => {
                "the actuator base URL carries credentials; set \
                 SPRING_BOOT_APP_<NAME>_USERNAME and _PASSWORD instead"
            }
            Self::HasQuery => "the actuator base URL has a query",
            Self::HasFragment => "the actuator base URL has a fragment",
        })
    }
}

/// What makes a credential unusable in an HTTP Basic header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialProblem {
    /// A `:` in the username, where Basic ends the username.
    Colon,
    /// A control character (`char::is_control`: C0, DEL and C1) in either value.
    ControlCharacter,
}

impl fmt::Display for CredentialProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Colon => "a colon, which HTTP Basic can't carry in a username",
            Self::ControlCharacter => "a control character, which HTTP Basic can't carry",
        })
    }
}

/// Why the applications configuration can't be used. It names variables and positions in
/// `SPRING_BOOT_APPS`, never a value, so it can be logged as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplicationsConfigError {
    /// The variable is set but isn't valid UTF-8.
    NotUnicode { variable: String },
    /// More applications than the agent scrapes.
    TooMany { count: usize },
    /// The pair at this 1-based position has no `=`, or an empty name or URL.
    MalformedPair { position: usize },
    /// The pair at this position has a name that breaks the `ApplicationName` rule.
    InvalidName { position: usize },
    /// The pair at this position has a URL that can't be an actuator base URL.
    InvalidUrl {
        position: usize,
        problem: UrlProblem,
    },
    /// The pair at this position repeats an earlier name, or one whose credential variables
    /// would be the same.
    DuplicateName { position: usize },
    /// One of an application's two credential variables is set without the other.
    IncompleteCredentials { missing: String },
    /// A credential HTTP Basic can't carry (RFC 7617 §2): a username with `:`, or either
    /// value with a control character.
    InvalidCredential {
        variable: String,
        problem: CredentialProblem,
    },
    /// `SPRING_BOOT_SCRAPE_INTERVAL` is not a whole number of seconds from 10 to 3600.
    InvalidInterval,
}

impl fmt::Display for ApplicationsConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUnicode { variable } => write!(f, "{variable} is set but is not valid UTF-8"),
            Self::TooMany { count } => write!(
                f,
                "{APPS_VARIABLE} names {count} applications; the agent scrapes at most \
                 {MAX_APPLICATIONS}"
            ),
            Self::MalformedPair { position } => write!(
                f,
                "{APPS_VARIABLE} entry {position} is not a name=actuator-base-url pair"
            ),
            Self::InvalidName { position } => write!(
                f,
                "{APPS_VARIABLE} entry {position} has an invalid name: use 1 to {} of \
                 A-Z a-z 0-9 _ . -, and not . or ..",
                ApplicationName::MAX_LEN
            ),
            Self::InvalidUrl { position, problem } => {
                write!(f, "{APPS_VARIABLE} entry {position}: {problem}")
            }
            Self::DuplicateName { position } => write!(
                f,
                "{APPS_VARIABLE} entry {position} repeats an earlier name (names equal apart \
                 from case, - and . would share credential variables)"
            ),
            Self::IncompleteCredentials { missing } => write!(
                f,
                "{missing} is not set, but its application's other credential variable is"
            ),
            Self::InvalidCredential { variable, problem } => {
                write!(f, "{variable} contains {problem}")
            }
            Self::InvalidInterval => write!(
                f,
                "{INTERVAL_VARIABLE} is not a whole number of seconds from {} to {}",
                ScrapeInterval::SECS.start(),
                ScrapeInterval::SECS.end()
            ),
        }
    }
}

/// Reads `variable`: `None` when unset or empty.
fn read_variable(
    lookup: &impl Fn(&str) -> Result<String, VarError>,
    variable: &str,
) -> Result<Option<String>, ApplicationsConfigError> {
    match lookup(variable) {
        Ok(value) => Ok(Some(value).filter(|value| !value.is_empty())),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(ApplicationsConfigError::NotUnicode {
            variable: variable.to_string(),
        }),
    }
}

/// Reads `variable` trimmed: `None` when unset, empty or blank.
fn read_variable_trimmed(
    lookup: &impl Fn(&str) -> Result<String, VarError>,
    variable: &str,
) -> Result<Option<String>, ApplicationsConfigError> {
    Ok(read_variable(lookup, variable)?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

impl ApplicationsConfig {
    /// Parses the applications configuration, reading variables through `lookup`
    /// (`std::env::var` in production).
    pub fn parse(
        lookup: impl Fn(&str) -> Result<String, VarError>,
    ) -> Result<Self, ApplicationsConfigError> {
        let Some(apps) = read_variable_trimmed(&lookup, APPS_VARIABLE)? else {
            return Ok(Self::Off);
        };
        let pairs: Vec<&str> = apps.split(',').map(str::trim).collect();
        if pairs.len() > MAX_APPLICATIONS {
            return Err(ApplicationsConfigError::TooMany { count: pairs.len() });
        }
        let targets = parse_targets(&pairs, &lookup)?;
        let interval = ScrapeInterval::parse(read_variable_trimmed(&lookup, INTERVAL_VARIABLE)?)?;
        Ok(Self::On(Applications { targets, interval }))
    }

    /// The applications whose Basic credentials would cross the network in plain text: an
    /// `http://` base URL whose host isn't a loopback address.
    pub fn plaintext_credentials(&self) -> Vec<&ApplicationName> {
        match self {
            Self::Off => Vec::new(),
            Self::On(apps) => apps
                .targets
                .iter()
                .filter(|target| target.sends_plaintext_credentials())
                .map(ApplicationTarget::name)
                .collect(),
        }
    }
}

/// Parses every pair, refusing a name whose credential key an earlier pair already took.
fn parse_targets(
    pairs: &[&str],
    lookup: &impl Fn(&str) -> Result<String, VarError>,
) -> Result<Vec<ApplicationTarget>, ApplicationsConfigError> {
    let mut keys = HashSet::new();
    let mut targets = Vec::with_capacity(pairs.len());
    for (index, pair) in pairs.iter().enumerate() {
        let position = index + 1;
        let target = ApplicationTarget::parse(position, pair, lookup)?;
        if !keys.insert(target.name.credentials_key()) {
            return Err(ApplicationsConfigError::DuplicateName { position });
        }
        targets.push(target);
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    /// An environment for `parse`: plain values, plus variables that are set but not UTF-8.
    fn env(
        vars: &[(&str, &str)],
        not_unicode: &[&str],
    ) -> impl Fn(&str) -> Result<String, VarError> + use<> {
        let mut table: HashMap<String, Result<String, VarError>> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), Ok(v.to_string())))
            .collect();
        for name in not_unicode {
            table.insert(
                name.to_string(),
                Err(VarError::NotUnicode(OsString::from_vec(
                    b"leak-marker-\xff".to_vec(),
                ))),
            );
        }
        move |key| table.get(key).cloned().unwrap_or(Err(VarError::NotPresent))
    }

    /// One application as parsed: name, base URL, and Basic username and password, if any.
    type TargetSummary = (String, String, Option<(String, String)>);

    /// What a parsed configuration says, in comparable form.
    #[derive(Debug, PartialEq)]
    enum Summary {
        Off,
        On {
            targets: Vec<TargetSummary>,
            interval_secs: u64,
        },
    }

    fn summary(config: &ApplicationsConfig) -> Summary {
        match config {
            ApplicationsConfig::Off => Summary::Off,
            ApplicationsConfig::On(apps) => Summary::On {
                targets: apps
                    .targets()
                    .iter()
                    .map(|t| {
                        let basic = match t.credentials() {
                            ActuatorCredentials::None => None,
                            ActuatorCredentials::Basic(basic) => {
                                Some((basic.username.clone(), basic.password.clone()))
                            }
                        };
                        (
                            t.name().as_str().to_string(),
                            t.base_url().as_url().to_string(),
                            basic,
                        )
                    })
                    .collect(),
                interval_secs: apps.interval().as_duration().as_secs(),
            },
        }
    }

    /// An application as a test expects it: name, base URL, and Basic username and password.
    type Expected<'a> = (&'a str, &'a str, Option<(&'a str, &'a str)>);

    fn on(targets: &[Expected<'_>], interval_secs: u64) -> Summary {
        Summary::On {
            targets: targets
                .iter()
                .map(|(n, u, c)| {
                    (
                        n.to_string(),
                        u.to_string(),
                        c.map(|(user, pw)| (user.to_string(), pw.to_string())),
                    )
                })
                .collect(),
            interval_secs,
        }
    }

    /// `n` distinct applications, `app1=http://127.0.0.1:8001/` and so on.
    fn apps(n: usize) -> String {
        (1..=n)
            .map(|i| format!("app{i}=http://127.0.0.1:{}/", 8000 + i))
            .collect::<Vec<_>>()
            .join(",")
    }

    const ORDERS: &str = "orders=http://127.0.0.1:8081/actuator";
    const LOCAL: &str = "http://127.0.0.1:8081/actuator/";

    type Case<'a> = (
        &'a str,
        Vec<(&'a str, &'a str)>,
        Vec<&'a str>,
        Result<Summary, ApplicationsConfigError>,
    );

    fn off_and_interval_cases() -> Vec<Case<'static>> {
        use ApplicationsConfigError as E;
        let with_interval = |value| {
            vec![
                ("SPRING_BOOT_APPS", ORDERS),
                ("SPRING_BOOT_SCRAPE_INTERVAL", value),
            ]
        };
        vec![
            ("unset is off", vec![], vec![], Ok(Summary::Off)),
            (
                "empty is off",
                vec![("SPRING_BOOT_APPS", "")],
                vec![],
                Ok(Summary::Off),
            ),
            (
                "only whitespace is off",
                vec![("SPRING_BOOT_APPS", "  ")],
                vec![],
                Ok(Summary::Off),
            ),
            (
                "off ignores an interval that isn't UTF-8",
                vec![],
                vec!["SPRING_BOOT_SCRAPE_INTERVAL"],
                Ok(Summary::Off),
            ),
            (
                "off ignores an invalid interval",
                vec![("SPRING_BOOT_SCRAPE_INTERVAL", "1")],
                vec![],
                Ok(Summary::Off),
            ),
            (
                "one application without credentials, default interval",
                vec![("SPRING_BOOT_APPS", ORDERS)],
                vec![],
                Ok(on(&[("orders", LOCAL, None)], 15)),
            ),
            (
                "interval at the minimum",
                with_interval("10"),
                vec![],
                Ok(on(&[("orders", LOCAL, None)], 10)),
            ),
            (
                "interval at the maximum",
                with_interval("3600"),
                vec![],
                Ok(on(&[("orders", LOCAL, None)], 3600)),
            ),
            (
                "an empty interval is the default",
                with_interval(""),
                vec![],
                Ok(on(&[("orders", LOCAL, None)], 15)),
            ),
            (
                "a padded interval is trimmed",
                with_interval(" 20 "),
                vec![],
                Ok(on(&[("orders", LOCAL, None)], 20)),
            ),
            (
                "a whitespace-only interval is the default",
                with_interval("   "),
                vec![],
                Ok(on(&[("orders", LOCAL, None)], 15)),
            ),
            (
                "interval below the minimum",
                with_interval("9"),
                vec![],
                Err(E::InvalidInterval),
            ),
            (
                "interval above the maximum",
                with_interval("3601"),
                vec![],
                Err(E::InvalidInterval),
            ),
            (
                "interval that isn't a number",
                with_interval("15s"),
                vec![],
                Err(E::InvalidInterval),
            ),
            (
                "the interval not UTF-8",
                vec![("SPRING_BOOT_APPS", ORDERS)],
                vec!["SPRING_BOOT_SCRAPE_INTERVAL"],
                Err(E::NotUnicode {
                    variable: "SPRING_BOOT_SCRAPE_INTERVAL".into(),
                }),
            ),
            (
                "SPRING_BOOT_APPS not UTF-8",
                vec![],
                vec!["SPRING_BOOT_APPS"],
                Err(E::NotUnicode {
                    variable: "SPRING_BOOT_APPS".into(),
                }),
            ),
        ]
    }

    fn pair_and_url_cases() -> Vec<Case<'static>> {
        use ApplicationsConfigError as E;
        use UrlProblem as U;
        /// One `SPRING_BOOT_APPS` value and what it parses to.
        struct Row {
            name: &'static str,
            apps: &'static str,
            expected: Result<Summary, E>,
        }
        let url = |name, apps, problem| Row {
            name,
            apps,
            expected: Err(E::InvalidUrl {
                position: 1,
                problem,
            }),
        };
        let row = |name, apps, expected| Row {
            name,
            apps,
            expected,
        };
        let rows = vec![
            url(
                "a URL that isn't http(s)",
                "orders=ftp://127.0.0.1/actuator",
                U::NotHttp,
            ),
            url("a URL that doesn't parse", "orders=not a url", U::NotAUrl),
            url("a URL with an empty host", "orders=http://", U::NotAUrl),
            url(
                "a URL with a username and password",
                "orders=http://monitor:leak-marker@127.0.0.1:8081/actuator",
                U::HasUserinfo,
            ),
            url(
                "a URL with only a username",
                "orders=http://monitor@127.0.0.1:8081/actuator",
                U::HasUserinfo,
            ),
            url(
                "a URL with a query",
                "orders=http://127.0.0.1:8081/actuator?x=1",
                U::HasQuery,
            ),
            url(
                "a URL with a fragment",
                "orders=http://127.0.0.1:8081/actuator#x",
                U::HasFragment,
            ),
            row(
                "a base URL already ending in a slash keeps one slash",
                "orders=http://127.0.0.1:8081/actuator/",
                Ok(on(&[("orders", LOCAL, None)], 15)),
            ),
            row(
                "a custom base path",
                "orders=http://10.0.0.5:9000/manage",
                Ok(on(&[("orders", "http://10.0.0.5:9000/manage/", None)], 15)),
            ),
            row(
                "a bare host gets a root path",
                "orders=https://orders.internal",
                Ok(on(&[("orders", "https://orders.internal/", None)], 15)),
            ),
            row(
                "pairs are trimmed and keep their order",
                " orders=http://127.0.0.1:8081/actuator , billing=http://127.0.0.1:8082/manage ",
                Ok(on(
                    &[
                        ("orders", LOCAL, None),
                        ("billing", "http://127.0.0.1:8082/manage/", None),
                    ],
                    15,
                )),
            ),
            row(
                "a pair without =",
                "orders",
                Err(E::MalformedPair { position: 1 }),
            ),
            row(
                "an empty name",
                "=http://127.0.0.1:8081/actuator",
                Err(E::MalformedPair { position: 1 }),
            ),
            row(
                "an empty URL",
                "orders=",
                Err(E::MalformedPair { position: 1 }),
            ),
            row(
                "a trailing comma is an empty second pair",
                "orders=http://127.0.0.1:8081/actuator,",
                Err(E::MalformedPair { position: 2 }),
            ),
        ];
        rows.into_iter()
            .map(
                |Row {
                     name,
                     apps,
                     expected,
                 }| (name, vec![("SPRING_BOOT_APPS", apps)], vec![], expected),
            )
            .collect()
    }

    fn name_cases() -> Vec<Case<'static>> {
        use ApplicationsConfigError as E;
        let name_64: &'static str = "a".repeat(64).leak();
        let pair_64: &'static str = format!("{name_64}=http://127.0.0.1:8081/actuator").leak();
        let pair_65: &'static str =
            format!("{}=http://127.0.0.1:8081/actuator", "a".repeat(65)).leak();
        let apps_16: &'static str = apps(16).leak();
        let apps_17: &'static str = apps(17).leak();
        let apps_20: &'static str = apps(20).leak();
        let apps_16_comma: &'static str = format!("{},", apps(16)).leak();
        let sixteen = Summary::On {
            targets: (1..=16)
                .map(|i| {
                    (
                        format!("app{i}"),
                        format!("http://127.0.0.1:{}/", 8000 + i),
                        None,
                    )
                })
                .collect(),
            interval_secs: 15,
        };
        let cases: Vec<(&str, &str, Result<Summary, E>)> = vec![
            (
                "upper case, digits and _ are allowed",
                "Orders_1=http://127.0.0.1:8081/actuator",
                Ok(on(&[("Orders_1", LOCAL, None)], 15)),
            ),
            (
                "a name with a / is refused",
                "or/ders=http://127.0.0.1:8081/actuator",
                Err(E::InvalidName { position: 1 }),
            ),
            (
                "a name with a non-ASCII letter is refused",
                "ordérs=http://127.0.0.1:8081/actuator",
                Err(E::InvalidName { position: 1 }),
            ),
            (
                "a name of one dot",
                ".=http://127.0.0.1:8081/actuator",
                Err(E::InvalidName { position: 1 }),
            ),
            (
                "a name of two dots",
                "..=http://127.0.0.1:8081/actuator",
                Err(E::InvalidName { position: 1 }),
            ),
            (
                "a name of 64 bytes",
                pair_64,
                Ok(on(&[(name_64, LOCAL, None)], 15)),
            ),
            (
                "a name of 65 bytes",
                pair_65,
                Err(E::InvalidName { position: 1 }),
            ),
            (
                "a repeated name",
                "orders=http://127.0.0.1:8081/actuator,orders=http://127.0.0.1:8082/actuator",
                Err(E::DuplicateName { position: 2 }),
            ),
            (
                "names equal but for case share a credential key",
                "Orders=http://127.0.0.1:8081/actuator,orders=http://127.0.0.1:8082/actuator",
                Err(E::DuplicateName { position: 2 }),
            ),
            (
                "names equal but for - and . share a credential key",
                "order-service=http://127.0.0.1:8081/actuator,order.service=http://127.0.0.1:8082/actuator",
                Err(E::DuplicateName { position: 2 }),
            ),
            ("sixteen applications", apps_16, Ok(sixteen)),
            (
                "seventeen applications",
                apps_17,
                Err(E::TooMany { count: 17 }),
            ),
            (
                "twenty applications",
                apps_20,
                Err(E::TooMany { count: 20 }),
            ),
            (
                "sixteen applications and a trailing comma are seventeen pairs",
                apps_16_comma,
                Err(E::TooMany { count: 17 }),
            ),
            (
                "a repeat in third position",
                "a=http://127.0.0.1:1/,b=http://127.0.0.1:2/,a=http://127.0.0.1:3/",
                Err(E::DuplicateName { position: 3 }),
            ),
        ];
        cases
            .into_iter()
            .map(|(name, value, expected)| {
                (name, vec![("SPRING_BOOT_APPS", value)], vec![], expected)
            })
            .collect()
    }

    fn credential_cases() -> Vec<Case<'static>> {
        use ApplicationsConfigError as E;
        const USER: &str = "SPRING_BOOT_APP_ORDERS_USERNAME";
        const PASS: &str = "SPRING_BOOT_APP_ORDERS_PASSWORD";
        let orders = |extra: Vec<(&'static str, &'static str)>| {
            let mut vars = vec![("SPRING_BOOT_APPS", ORDERS)];
            vars.extend(extra);
            vars
        };
        vec![
            (
                "Basic credentials from both variables",
                orders(vec![(USER, "monitor"), (PASS, "pw-orders")]),
                vec![],
                Ok(on(&[("orders", LOCAL, Some(("monitor", "pw-orders")))], 15)),
            ),
            (
                "the credential key upper-cases the name and maps - and . to _",
                vec![
                    (
                        "SPRING_BOOT_APPS",
                        "order-service.v2=http://127.0.0.1:8081/actuator",
                    ),
                    ("SPRING_BOOT_APP_ORDER_SERVICE_V2_USERNAME", "monitor"),
                    ("SPRING_BOOT_APP_ORDER_SERVICE_V2_PASSWORD", "pw-v2"),
                ],
                vec![],
                Ok(on(
                    &[("order-service.v2", LOCAL, Some(("monitor", "pw-v2")))],
                    15,
                )),
            ),
            (
                "each application reads its own credentials",
                vec![
                    (
                        "SPRING_BOOT_APPS",
                        "a=http://127.0.0.1:1/,b=http://127.0.0.1:2/",
                    ),
                    ("SPRING_BOOT_APP_B_USERNAME", "user-b"),
                    ("SPRING_BOOT_APP_B_PASSWORD", "pw-b"),
                ],
                vec![],
                Ok(on(
                    &[
                        ("a", "http://127.0.0.1:1/", None),
                        ("b", "http://127.0.0.1:2/", Some(("user-b", "pw-b"))),
                    ],
                    15,
                )),
            ),
            (
                "empty credential variables count as unset",
                orders(vec![(USER, ""), (PASS, "")]),
                vec![],
                Ok(on(&[("orders", LOCAL, None)], 15)),
            ),
            (
                "a username without a password",
                orders(vec![(USER, "monitor")]),
                vec![],
                Err(E::IncompleteCredentials {
                    missing: PASS.into(),
                }),
            ),
            (
                "a username with an empty password",
                orders(vec![(USER, "monitor"), (PASS, "")]),
                vec![],
                Err(E::IncompleteCredentials {
                    missing: PASS.into(),
                }),
            ),
            (
                "a password without a username",
                orders(vec![(PASS, "pw-orders")]),
                vec![],
                Err(E::IncompleteCredentials {
                    missing: USER.into(),
                }),
            ),
            (
                "a password with an empty username",
                orders(vec![(USER, ""), (PASS, "pw-orders")]),
                vec![],
                Err(E::IncompleteCredentials {
                    missing: USER.into(),
                }),
            ),
            (
                "a username with a colon",
                orders(vec![(USER, "ops:team"), (PASS, "pw-orders")]),
                vec![],
                Err(E::InvalidCredential {
                    variable: USER.into(),
                    problem: CredentialProblem::Colon,
                }),
            ),
            (
                "non-ASCII letters and symbols are not control characters",
                orders(vec![(USER, "José"), (PASS, "pässwörd€")]),
                vec![],
                Ok(on(&[("orders", LOCAL, Some(("José", "pässwörd€")))], 15)),
            ),
            (
                "a password ending in a carriage return is refused, not trimmed",
                orders(vec![(USER, "monitor"), (PASS, "pw-orders\r")]),
                vec![],
                Err(E::InvalidCredential {
                    variable: PASS.into(),
                    problem: CredentialProblem::ControlCharacter,
                }),
            ),
            (
                "a username starting with a newline is refused, not trimmed",
                orders(vec![(USER, "\nmonitor"), (PASS, "pw-orders")]),
                vec![],
                Err(E::InvalidCredential {
                    variable: USER.into(),
                    problem: CredentialProblem::ControlCharacter,
                }),
            ),
            (
                "values are kept exactly, surrounding spaces included",
                orders(vec![(USER, " monitor "), (PASS, " pw ")]),
                vec![],
                Ok(on(&[("orders", LOCAL, Some((" monitor ", " pw ")))], 15)),
            ),
            (
                "with control characters in both values, the username is reported",
                orders(vec![(USER, "mon\titor"), (PASS, "pw\t")]),
                vec![],
                Err(E::InvalidCredential {
                    variable: USER.into(),
                    problem: CredentialProblem::ControlCharacter,
                }),
            ),
            (
                "a control character is reported before a colon",
                orders(vec![(USER, "ops:te\u{1b}am"), (PASS, "pw-orders")]),
                vec![],
                Err(E::InvalidCredential {
                    variable: USER.into(),
                    problem: CredentialProblem::ControlCharacter,
                }),
            ),
            (
                "Unicode spaces and separators are not control characters",
                orders(vec![
                    (USER, "ops\u{a0}team"),
                    (PASS, "p\u{a0}w\u{2028}\u{200b}\u{feff}"),
                ]),
                vec![],
                Ok(on(
                    &[(
                        "orders",
                        LOCAL,
                        Some(("ops\u{a0}team", "p\u{a0}w\u{2028}\u{200b}\u{feff}")),
                    )],
                    15,
                )),
            ),
            (
                "a username may contain punctuation other than a colon",
                orders(vec![(USER, "ops;team=x,y@z"), (PASS, "pw-orders")]),
                vec![],
                Ok(on(
                    &[("orders", LOCAL, Some(("ops;team=x,y@z", "pw-orders")))],
                    15,
                )),
            ),
            (
                "a control character in another application's password",
                vec![
                    (
                        "SPRING_BOOT_APPS",
                        "a=http://127.0.0.1:1/,b=http://127.0.0.1:2/",
                    ),
                    ("SPRING_BOOT_APP_B_USERNAME", "user-b"),
                    ("SPRING_BOOT_APP_B_PASSWORD", "pw\u{7}"),
                ],
                vec![],
                Err(E::InvalidCredential {
                    variable: "SPRING_BOOT_APP_B_PASSWORD".into(),
                    problem: CredentialProblem::ControlCharacter,
                }),
            ),
            (
                "a password with a control character and no username is incomplete first",
                orders(vec![(PASS, "pw\n")]),
                vec![],
                Err(E::IncompleteCredentials {
                    missing: USER.into(),
                }),
            ),
            (
                "colon look-alikes and bidi controls are not colons or control characters",
                orders(vec![
                    (USER, "ops\u{ff1a}\u{a789}\u{2236}\u{202e}"),
                    (PASS, "pw\u{ad}\u{200e}"),
                ]),
                vec![],
                Ok(on(
                    &[(
                        "orders",
                        LOCAL,
                        Some(("ops\u{ff1a}\u{a789}\u{2236}\u{202e}", "pw\u{ad}\u{200e}")),
                    )],
                    15,
                )),
            ),
            (
                "a username with a control character and no password is incomplete first",
                orders(vec![(USER, "mon\ntor")]),
                vec![],
                Err(E::IncompleteCredentials {
                    missing: PASS.into(),
                }),
            ),
            (
                "a username may contain a space",
                orders(vec![(USER, "ops team"), (PASS, "pw-orders")]),
                vec![],
                Ok(on(
                    &[("orders", LOCAL, Some(("ops team", "pw-orders")))],
                    15,
                )),
            ),
            (
                "a password may contain a colon and spaces",
                orders(vec![(USER, "monitor"), (PASS, "p: w")]),
                vec![],
                Ok(on(&[("orders", LOCAL, Some(("monitor", "p: w")))], 15)),
            ),
            (
                "a username with a colon and no password is incomplete first",
                orders(vec![(USER, "ops:team")]),
                vec![],
                Err(E::IncompleteCredentials {
                    missing: PASS.into(),
                }),
            ),
            (
                "when both values are unusable, the username is reported",
                orders(vec![(USER, "ops:team"), (PASS, "pw\u{0}")]),
                vec![],
                Err(E::InvalidCredential {
                    variable: USER.into(),
                    problem: CredentialProblem::Colon,
                }),
            ),
            (
                "the second application's credentials are checked too",
                vec![
                    (
                        "SPRING_BOOT_APPS",
                        "a=http://127.0.0.1:1/,b=http://127.0.0.1:2/",
                    ),
                    ("SPRING_BOOT_APP_B_USERNAME", "ops:team"),
                    ("SPRING_BOOT_APP_B_PASSWORD", "pw-b"),
                ],
                vec![],
                Err(E::InvalidCredential {
                    variable: "SPRING_BOOT_APP_B_USERNAME".into(),
                    problem: CredentialProblem::Colon,
                }),
            ),
            (
                "a username not UTF-8",
                orders(vec![(PASS, "pw-orders")]),
                vec![USER],
                Err(E::NotUnicode {
                    variable: USER.into(),
                }),
            ),
            (
                "a password not UTF-8",
                orders(vec![(USER, "monitor")]),
                vec![PASS],
                Err(E::NotUnicode {
                    variable: PASS.into(),
                }),
            ),
        ]
    }

    #[test]
    fn parse_turns_the_environment_into_applications_or_names_what_is_wrong() {
        let cases = [
            off_and_interval_cases(),
            pair_and_url_cases(),
            name_cases(),
            credential_cases(),
        ];
        for (name, vars, not_unicode, expected) in cases.into_iter().flatten() {
            let parsed = ApplicationsConfig::parse(env(&vars, &not_unicode));
            let actual = parsed.as_ref().map(summary).map_err(Clone::clone);
            assert_eq!(actual, expected, "case: {name}");
        }
    }

    #[test]
    fn a_control_character_in_either_credential_is_refused_and_attributed() {
        // Every C0 control, DEL, and every C1 control: what `char::is_control` means.
        let controls = (0x00..=0x1f).chain(0x7f..=0x9f).filter_map(char::from_u32);
        // At the start, in the middle and at the end, so no trimming or stripping passes.
        let placements = controls.flat_map(|c| {
            [
                format!("{c}value"),
                format!("va{c}lue"),
                format!("value{c}"),
                format!("{}{c}", "v".repeat(100)),
            ]
            .map(|bad| (c, bad))
        });
        for (control, bad) in placements {
            let cases = [
                ("SPRING_BOOT_APP_ORDERS_USERNAME", bad.as_str(), "pw-orders"),
                ("SPRING_BOOT_APP_ORDERS_PASSWORD", "monitor", bad.as_str()),
            ];
            for (variable, user, pass) in cases {
                let lookup = env(
                    &[
                        ("SPRING_BOOT_APPS", ORDERS),
                        ("SPRING_BOOT_APP_ORDERS_USERNAME", user),
                        ("SPRING_BOOT_APP_ORDERS_PASSWORD", pass),
                    ],
                    &[],
                );
                let actual = ApplicationsConfig::parse(lookup).map(|config| summary(&config));
                assert_eq!(
                    actual,
                    Err(ApplicationsConfigError::InvalidCredential {
                        variable: variable.into(),
                        problem: CredentialProblem::ControlCharacter,
                    }),
                    "U+{:04X} in {variable} as {bad:?}",
                    u32::from(control)
                );
            }
        }
    }

    #[test]
    fn every_character_that_isnt_a_control_is_kept_except_a_colon_in_a_username() {
        let all: String = (0u32..=0x10FFFF)
            .filter_map(char::from_u32)
            .filter(|c| !c.is_control())
            .collect();
        let username: String = all.chars().filter(|&c| c != ':').collect();
        let lookup = env(
            &[
                ("SPRING_BOOT_APPS", ORDERS),
                ("SPRING_BOOT_APP_ORDERS_USERNAME", username.as_str()),
                ("SPRING_BOOT_APP_ORDERS_PASSWORD", all.as_str()),
            ],
            &[],
        );
        let actual = ApplicationsConfig::parse(lookup).map(|config| summary(&config));
        let expected = Ok(on(
            &[("orders", LOCAL, Some((username.as_str(), all.as_str())))],
            15,
        ));
        // Not assert_eq!: a failure would print megabytes.
        assert!(
            actual == expected,
            "every character that isn't a control must be kept exactly"
        );
    }

    #[test]
    fn a_colon_anywhere_in_a_username_is_refused() {
        for username in [":ops", "ops:", "o:p:s", ":"] {
            let lookup = env(
                &[
                    ("SPRING_BOOT_APPS", ORDERS),
                    ("SPRING_BOOT_APP_ORDERS_USERNAME", username),
                    ("SPRING_BOOT_APP_ORDERS_PASSWORD", "pw"),
                ],
                &[],
            );
            let actual = ApplicationsConfig::parse(lookup).map(|config| summary(&config));
            assert_eq!(
                actual,
                Err(ApplicationsConfigError::InvalidCredential {
                    variable: "SPRING_BOOT_APP_ORDERS_USERNAME".into(),
                    problem: CredentialProblem::Colon,
                }),
                "username {username:?}"
            );
        }
    }

    #[test]
    fn every_config_error_names_its_variable_the_culprit_and_never_a_value() {
        use ApplicationsConfigError as E;
        const APPS: &str = "SPRING_BOOT_APPS";
        const INTERVAL: &str = "SPRING_BOOT_SCRAPE_INTERVAL";
        const PASS: &str = "SPRING_BOOT_APP_ORDERS_PASSWORD";
        let url = |problem| E::InvalidUrl {
            position: 7,
            problem,
        };
        // (error, text it must contain, text it must not contain)
        let cases: Vec<(E, Vec<&str>, Vec<&str>)> = vec![
            (
                E::NotUnicode {
                    variable: PASS.into(),
                },
                vec![PASS],
                vec![INTERVAL],
            ),
            (
                E::NotUnicode {
                    variable: APPS.into(),
                },
                vec![APPS],
                vec![INTERVAL],
            ),
            (
                E::TooMany { count: 17 },
                vec![APPS, "17", "16"],
                vec![INTERVAL],
            ),
            (
                E::TooMany { count: 23 },
                vec![APPS, "23", "16"],
                vec![INTERVAL],
            ),
            (
                E::MalformedPair { position: 7 },
                vec![APPS, "7"],
                vec![INTERVAL],
            ),
            (
                E::MalformedPair { position: 4 },
                vec![APPS, "4"],
                vec![INTERVAL],
            ),
            (
                E::InvalidName { position: 7 },
                vec![APPS, "7"],
                vec![INTERVAL],
            ),
            (
                E::InvalidName { position: 5 },
                vec![APPS, "5"],
                vec![INTERVAL],
            ),
            (
                E::InvalidUrl {
                    position: 9,
                    problem: UrlProblem::NotHttp,
                },
                vec![APPS, "9"],
                vec![INTERVAL],
            ),
            (
                E::DuplicateName { position: 3 },
                vec![APPS, "3"],
                vec![INTERVAL],
            ),
            (url(UrlProblem::NotAUrl), vec![APPS, "7"], vec![INTERVAL]),
            (url(UrlProblem::NotHttp), vec![APPS, "7"], vec![INTERVAL]),
            (
                url(UrlProblem::HasUserinfo),
                vec![APPS, "7"],
                vec![INTERVAL],
            ),
            (url(UrlProblem::HasQuery), vec![APPS, "7"], vec![INTERVAL]),
            (
                url(UrlProblem::HasFragment),
                vec![APPS, "7"],
                vec![INTERVAL],
            ),
            (
                E::DuplicateName { position: 7 },
                vec![APPS, "7"],
                vec![INTERVAL],
            ),
            (
                E::IncompleteCredentials {
                    missing: PASS.into(),
                },
                vec![PASS],
                vec![INTERVAL],
            ),
            (E::InvalidInterval, vec![INTERVAL, "10", "3600"], vec![APPS]),
            (
                E::InvalidCredential {
                    variable: PASS.into(),
                    problem: CredentialProblem::ControlCharacter,
                },
                vec![PASS, "control character"],
                vec![INTERVAL, "colon"],
            ),
            (
                E::InvalidCredential {
                    variable: "SPRING_BOOT_APP_ORDERS_USERNAME".into(),
                    problem: CredentialProblem::ControlCharacter,
                },
                vec!["SPRING_BOOT_APP_ORDERS_USERNAME", "control character"],
                vec![INTERVAL, "colon", PASS],
            ),
            (
                E::InvalidCredential {
                    variable: "SPRING_BOOT_APP_B_USERNAME".into(),
                    problem: CredentialProblem::Colon,
                },
                vec!["SPRING_BOOT_APP_B_USERNAME", "colon"],
                vec![INTERVAL, "control character", "ORDERS"],
            ),
            (
                E::InvalidCredential {
                    variable: "SPRING_BOOT_APP_ORDERS_USERNAME".into(),
                    problem: CredentialProblem::Colon,
                },
                vec!["SPRING_BOOT_APP_ORDERS_USERNAME", "colon"],
                vec![INTERVAL, "control character", PASS],
            ),
        ];
        for (err, required, forbidden) in &cases {
            let text = err.to_string();
            for part in required {
                assert!(
                    text.contains(part),
                    "{err:?} should mention {part:?}, says {text:?}"
                );
            }
            for part in forbidden {
                assert!(
                    !text.contains(part),
                    "{err:?} should not mention {part:?}, says {text:?}"
                );
            }
        }
        let url_texts: HashSet<String> = [
            UrlProblem::NotAUrl,
            UrlProblem::NotHttp,
            UrlProblem::HasUserinfo,
            UrlProblem::HasQuery,
            UrlProblem::HasFragment,
        ]
        .into_iter()
        .map(|problem| url(problem).to_string())
        .collect();
        assert_eq!(
            url_texts.len(),
            5,
            "each URL problem reads differently: {url_texts:?}"
        );

        // Errors from inputs that carry a secret-looking marker never repeat it.
        let leaky = [
            env(
                &[(
                    APPS,
                    "orders=http://monitor:leak-marker@127.0.0.1:8081/actuator",
                )],
                &[],
            ),
            env(&[(APPS, ORDERS), (PASS, "leak-marker-secret")], &[]),
            env(&[(APPS, "leak-marker/x=http://127.0.0.1/")], &[]),
            env(
                &[
                    (APPS, ORDERS),
                    ("SPRING_BOOT_APP_ORDERS_USERNAME", "monitor"),
                ],
                &[PASS],
            ),
            env(
                &[
                    (APPS, ORDERS),
                    ("SPRING_BOOT_APP_ORDERS_USERNAME", "monitor"),
                    (PASS, "leak-marker\u{1}"),
                ],
                &[],
            ),
        ];
        for (i, lookup) in leaky.into_iter().enumerate() {
            match ApplicationsConfig::parse(lookup) {
                Ok(_) => panic!("leaky case {i} should be refused"),
                Err(err) => assert!(
                    !err.to_string().contains("leak-marker"),
                    "leaky case {i}: {err}"
                ),
            }
        }
    }

    #[test]
    fn plaintext_credentials_are_basic_over_http_to_a_host_that_isnt_loopback() {
        // (case, SPRING_BOOT_APPS, applications given credentials, applications warned about)
        let cases: [(&str, &str, &[&str], &[&str]); 10] = [
            (
                "http to 127.0.0.1",
                "o=http://127.0.0.1:8081/actuator",
                &["O"],
                &[],
            ),
            (
                "http to 127.0.0.2, loopback too",
                "o=http://127.0.0.2:8081/actuator",
                &["O"],
                &[],
            ),
            ("http to ::1", "o=http://[::1]:8081/actuator", &["O"], &[]),
            (
                "http to localhost",
                "o=http://localhost:8081/actuator",
                &["O"],
                &[],
            ),
            (
                "http to a bridge address",
                "o=http://172.17.0.2:8081/actuator",
                &["O"],
                &["o"],
            ),
            (
                "http to a private IPv6 address",
                "o=http://[fd00::5]:8081/actuator",
                &["O"],
                &["o"],
            ),
            (
                "http to a host name",
                "o=http://orders.internal/actuator",
                &["O"],
                &["o"],
            ),
            (
                "https to a host name",
                "o=https://orders.internal/actuator",
                &["O"],
                &[],
            ),
            (
                "http without credentials",
                "o=http://172.17.0.2:8081/actuator",
                &[],
                &[],
            ),
            (
                "only the application sending credentials over http",
                "a=https://a.internal/,b=http://172.17.0.2/",
                &["A", "B"],
                &["b"],
            ),
        ];
        for (name, value, with_credentials, warned) in cases {
            let keys: Vec<(String, String)> = with_credentials
                .iter()
                .map(|key| {
                    (
                        format!("SPRING_BOOT_APP_{key}_USERNAME"),
                        format!("SPRING_BOOT_APP_{key}_PASSWORD"),
                    )
                })
                .collect();
            let mut vars = vec![("SPRING_BOOT_APPS", value)];
            for (user, pass) in &keys {
                vars.push((user.as_str(), "monitor"));
                vars.push((pass.as_str(), "secret"));
            }
            let config = ApplicationsConfig::parse(env(&vars, &[]))
                .unwrap_or_else(|err| panic!("case {name}: {err:?}"));
            let named: Vec<&str> = config
                .plaintext_credentials()
                .into_iter()
                .map(ApplicationName::as_str)
                .collect();
            assert_eq!(named, warned, "case: {name}");
        }
    }
}
