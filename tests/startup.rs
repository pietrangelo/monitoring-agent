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

//! Runs the real `system-agent` binary to check what no unit test can: that a malformed
//! configuration refuses startup before anything starts (RFC 0009 §2, RFC 0015), and that the
//! agent serves where `SYSTEM_AGENT_LISTEN` says.

#![cfg(unix)]

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::net::TcpListener;
use std::os::unix::ffi::OsStrExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// `EX_CONFIG` from `sysexits.h`: the exit code of a refused configuration, and of nothing
/// else, so no other failure (a busy port, a panic) can pass for a refusal.
const EX_CONFIG: i32 = 78;

/// The line `run` logs once the configuration has been accepted, before binding.
const STARTED: &str = "No auth token configured";

/// What the agent logs first once its runtime runs: a refusal must come before it.
const FIRST_RUNTIME_LINE: &str = "execution environment";

/// What the startup warning about plain-text Basic credentials says.
const PLAINTEXT_WARNING: &str = "in plain text";

/// What the agent printed, and how its run ended.
struct Run {
    /// The exit code, or `None` if it was still running and was killed.
    code: Option<i32>,
    output: String,
}

/// Runs the agent with `vars` and nothing else in its environment until it exits,
/// it prints a line containing `stop_at`, or 10 s pass. Then it is killed if still running.
fn run_agent(vars: &[(&str, &OsStr)], stop_at: Option<&str>) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_system-agent"));
    // A clean environment, so nothing inherited from the developer's shell decides a case.
    command
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in vars {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("the agent binary starts");

    // tracing's fmt subscriber writes to stdout; panics go to stderr.
    let (lines, received) = mpsc::channel();
    let stdout = child.stdout.take().expect("stdout is piped");
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                return;
            }
        }
    });
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = String::new();
    let code = loop {
        while let Ok(line) = received.try_recv() {
            output.push_str(&line);
            output.push('\n');
        }
        if let Some(status) = child.try_wait().expect("the agent can be waited on") {
            break status.code();
        }
        let reached = stop_at.is_some_and(|marker| output.contains(marker));
        if reached || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    while let Ok(line) = received.recv_timeout(Duration::from_millis(200)) {
        output.push_str(&line);
        output.push('\n');
    }
    output.push_str(&stderr_reader.join().unwrap_or_default());
    Run { code, output }
}

/// Counts connections to a listener that nothing should reach.
fn connections(listener: &TcpListener) -> usize {
    let mut count = 0;
    loop {
        match listener.accept() {
            Ok(_) => count += 1,
            Err(err) if err.kind() == ErrorKind::WouldBlock => return count,
            Err(err) => panic!("the counting listener failed: {err}"),
        }
    }
}

/// The variables every run in this file shares, so a refusal and its control differ only in
/// the variable under test.
fn common(push_to: &str) -> Vec<(&'static str, String)> {
    vec![
        ("PUSH_TO", push_to.to_string()),
        ("SPRING_BOOT_APP_ORDERS_USERNAME", "monitor".to_string()),
        (
            "SPRING_BOOT_APP_ORDERS_PASSWORD",
            "leak-marker-secret".to_string(),
        ),
    ]
}

/// Runs the agent with `common` plus `extra`, `extra` winning, and no `PUSH_TO` when `push_to`
/// is `None`.
fn run_with(push_to: Option<&str>, extra: &[(&str, &str)], stop_at: Option<&str>) -> Run {
    let mut vars: Vec<(&str, String)> = common(push_to.unwrap_or(""))
        .into_iter()
        .filter(|(key, _)| push_to.is_some() || *key != "PUSH_TO")
        .filter(|(key, _)| extra.iter().all(|(k, _)| k != key))
        .collect();
    vars.extend(extra.iter().map(|(k, v)| (*k, v.to_string())));
    let borrowed: Vec<(&str, &OsStr)> = vars.iter().map(|(k, v)| (*k, OsStr::new(v))).collect();
    run_agent(&borrowed, stop_at)
}

const VALID_APPS: &str = "orders=http://127.0.0.1:8081/actuator";

#[test]
fn a_malformed_applications_config_refuses_startup_before_anything_starts() {
    struct Refusal {
        name: &'static str,
        pushing: bool,
        breaking: &'static [(&'static str, &'static str)],
        names: &'static str,
    }
    let cases = [
        Refusal {
            name: "a repeated name, pushing",
            pushing: true,
            breaking: &[(
                "SPRING_BOOT_APPS",
                "orders=http://127.0.0.1:8081/actuator,orders=http://127.0.0.1:8082/actuator",
            )],
            names: "repeats an earlier name",
        },
        Refusal {
            name: "a repeated name, not pushing",
            pushing: false,
            breaking: &[(
                "SPRING_BOOT_APPS",
                "orders=http://127.0.0.1:8081/actuator,orders=http://127.0.0.1:8082/actuator",
            )],
            names: "repeats an earlier name",
        },
        Refusal {
            name: "a password without its username",
            pushing: false,
            breaking: &[
                ("SPRING_BOOT_APPS", VALID_APPS),
                ("SPRING_BOOT_APP_ORDERS_USERNAME", ""),
            ],
            names: "SPRING_BOOT_APP_ORDERS_USERNAME",
        },
        Refusal {
            name: "a listen address that isn't one, pushing",
            pushing: true,
            breaking: &[("SYSTEM_AGENT_LISTEN", "leak-marker-listen:9090")],
            names: "SYSTEM_AGENT_LISTEN",
        },
        Refusal {
            name: "a listen address that isn't one, not pushing",
            pushing: false,
            breaking: &[
                ("SPRING_BOOT_APPS", VALID_APPS),
                ("SYSTEM_AGENT_LISTEN", "leak-marker-listen:9090"),
            ],
            names: "SYSTEM_AGENT_LISTEN",
        },
        Refusal {
            name: "an interval below the minimum",
            pushing: true,
            breaking: &[
                ("SPRING_BOOT_APPS", VALID_APPS),
                ("SPRING_BOOT_SCRAPE_INTERVAL", "9"),
            ],
            names: "SPRING_BOOT_SCRAPE_INTERVAL",
        },
    ];
    for Refusal {
        name,
        pushing,
        breaking: extra,
        names: variable,
    } in cases
    {
        let hub = TcpListener::bind("127.0.0.1:0").unwrap();
        hub.set_nonblocking(true).unwrap();
        let push_to = format!("ws://{}", hub.local_addr().unwrap());
        let run = run_with(pushing.then_some(push_to.as_str()), extra, None);
        // Any connection attempt the agent made has reached the backlog by the time it exited.
        let hub_connections = connections(&hub);
        let output = &run.output;

        assert_eq!(
            run.code,
            Some(EX_CONFIG),
            "case {name}: a refused configuration exits with EX_CONFIG: {output}"
        );
        assert!(
            output.contains(variable),
            "case {name}: the refusal names {variable}: {output}"
        );
        assert!(
            !output.contains("leak-marker"),
            "case {name}: the refusal never prints a credential: {output}"
        );
        assert!(
            !output.contains(STARTED) && !output.contains("listening"),
            "case {name}: the agent refused before starting anything: {output}"
        );
        assert!(
            !output.contains(FIRST_RUNTIME_LINE),
            "case {name}: refused before the runtime, whose first task logs this: {output}"
        );
        assert_eq!(
            hub_connections, 0,
            "case {name}: the agent refused before starting the push client"
        );
    }
}

#[test]
fn a_valid_or_empty_applications_config_starts_the_agent() {
    // The same variables as the refusals, differing only in SPRING_BOOT_APPS.
    let hub = TcpListener::bind("127.0.0.1:0").unwrap();
    let push_to = format!("ws://{}", hub.local_addr().unwrap());
    // (case, whether PUSH_TO is set, SPRING_BOOT_APPS). Both with and without PUSH_TO, like the
    // refusals, so no variable but SPRING_BOOT_APPS can decide the outcome.
    let cases = [
        ("a valid configuration, pushing", true, VALID_APPS),
        ("an empty configuration, pushing", true, ""),
        ("a valid configuration, not pushing", false, VALID_APPS),
        ("an empty configuration, not pushing", false, ""),
    ];
    for (name, pushing, apps) in cases {
        let run = run_with(
            pushing.then_some(push_to.as_str()),
            &[("SPRING_BOOT_APPS", apps)],
            Some(STARTED),
        );
        let output = &run.output;
        assert!(
            output.contains(STARTED),
            "case {name}: the agent got past its configuration: {output}"
        );
        assert!(
            !output.contains("refusing to start"),
            "case {name}: nothing was refused: {output}"
        );
        assert!(
            !output.contains("leak-marker"),
            "case {name}: no credential is ever printed: {output}"
        );
    }
}

#[test]
fn basic_credentials_over_plain_http_to_another_host_are_warned_about_at_startup() {
    let hub = TcpListener::bind("127.0.0.1:0").unwrap();
    let push_to = format!("ws://{}", hub.local_addr().unwrap());
    // (case, SPRING_BOOT_APPS, whether the warning appears). Each runs with and without
    // PUSH_TO: applications are scraped, and credentials sent, either way.
    let cases = [
        (
            "http to a bridge address",
            "orders=http://172.17.0.2:8081/actuator",
            true,
        ),
        ("http to 127.0.0.1", VALID_APPS, false),
        (
            "http to localhost",
            "orders=http://localhost:8081/actuator",
            false,
        ),
    ];
    for (case, apps, warned) in cases {
        for pushing in [true, false] {
            let name = format!(
                "{case}, {}",
                if pushing { "pushing" } else { "not pushing" }
            );
            let run = run_with(
                pushing.then_some(push_to.as_str()),
                &[("SPRING_BOOT_APPS", apps)],
                Some(STARTED),
            );
            let output = &run.output;
            let lines: Vec<&str> = output.lines().collect();
            let started = lines.iter().position(|line| line.contains(STARTED));
            let warning = lines
                .iter()
                .position(|line| line.contains(PLAINTEXT_WARNING) && line.contains("orders"));
            assert!(
                started.is_some(),
                "case {name}: the agent started: {output}"
            );
            assert_eq!(
                warning.is_some(),
                warned,
                "case {name}: the plain-text warning naming the application: {output}"
            );
            if let (Some(warning), Some(started)) = (warning, started) {
                assert!(
                    warning < started,
                    "case {name}: the warning comes with the configuration, before startup: {output}"
                );
                assert!(
                    !lines[warning].contains("172.17.0.2") && !lines[warning].contains("http"),
                    "case {name}: the warning names the application, never its URL: {output}"
                );
            }
        }
    }
}

/// The rest of the first line of `lines` after `marker`, up to whitespace, read until the agent
/// logs one, it exits, or 10 s pass; else what it logged instead.
fn logged_after(
    lines: &mpsc::Receiver<String>,
    child: &mut std::process::Child,
    marker: &str,
) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = String::new();
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("the agent can be waited on") {
            return Err(format!(
                "exited ({status}) before logging {marker:?}: {seen}"
            ));
        }
        let Ok(line) = lines.recv_timeout(Duration::from_millis(100)) else {
            continue;
        };
        if let Some((_, rest)) = line.split_once(marker) {
            return Ok(rest.chars().take_while(|c| !c.is_whitespace()).collect());
        }
        seen.push_str(&line);
        seen.push('\n');
    }
    Err(format!("never logged {marker:?}: {seen}"))
}

/// A port nothing listens on at `ip` right now.
fn free_port(ip: &str) -> u16 {
    TcpListener::bind((ip, 0))
        .and_then(|listener| listener.local_addr())
        .expect("a free port")
        .port()
}

#[test]
fn the_agent_serves_at_its_configured_listen_address() {
    let fixed = format!("127.0.0.2:{}", free_port("127.0.0.2"));
    // (name, SYSTEM_AGENT_LISTEN, the address it must bind (`:0` when the OS picks the port),
    // the host the dashboard is suggested at, `None` for the bound IP)
    let cases = [
        (
            "any port on loopback",
            "127.0.0.1:0".to_owned(),
            "127.0.0.1:0".to_owned(),
            None,
        ),
        (
            "a fixed port on another address",
            fixed.clone(),
            fixed,
            None,
        ),
        (
            "any port on every interface",
            "0.0.0.0:0".to_owned(),
            "0.0.0.0:0".to_owned(),
            Some("localhost"),
        ),
        (
            "any port on every ipv6 interface",
            "[::]:0".to_owned(),
            "[::]:0".to_owned(),
            Some("localhost"),
        ),
    ];
    for (name, listen, expected, suggested_host) in cases {
        let expected: std::net::SocketAddr = expected.parse().expect("a test address");
        let mut child = Command::new(env!("CARGO_BIN_EXE_system-agent"))
            .env_clear()
            .env("SYSTEM_AGENT_LISTEN", &listen)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the agent binary starts");
        let (lines, received) = mpsc::channel();
        let stdout = child.stdout.take().expect("stdout is piped");
        // Drains stdout for the agent's whole life, so a full pipe never blocks its logging.
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = lines.send(line);
            }
        });

        let listening = logged_after(&received, &mut child, "listening on http://");
        let dashboard = logged_after(&received, &mut child, "Dashboard: ");
        let bound: Option<std::net::SocketAddr> =
            listening.as_ref().ok().and_then(|a| a.parse().ok());
        let reach = bound.map(reachable);
        let answer = reach.map(health);
        let _ = child.kill();
        let _ = child.wait();
        assert!(bound.is_some(), "{name}: the agent serves: {listening:?}");
        let (bound, answer) = (bound.unwrap(), answer.unwrap_or_else(|| Err(String::new())));

        assert_eq!(bound.ip(), expected.ip(), "{name}: the configured address");
        match expected.port() {
            0 => assert_ne!(bound.port(), 0, "{name}: the port the OS chose, not 0"),
            port => assert_eq!(bound.port(), port, "{name}: the configured port"),
        }
        let host = suggested_host.map_or_else(|| bound.ip().to_string(), str::to_owned);
        assert_eq!(
            dashboard,
            Ok(format!("http://{host}:{}/", bound.port())),
            "{name}: the dashboard is suggested where it can be reached"
        );
        assert!(
            answer.as_ref().is_ok_and(|a| a.starts_with("HTTP/1.1 200")),
            "{name}: the agent answers there: {answer:?}"
        );
    }
}

#[test]
fn a_listen_address_that_isnt_utf8_refuses_startup_before_anything_starts() {
    let hub = TcpListener::bind("127.0.0.1:0").unwrap();
    hub.set_nonblocking(true).unwrap();
    let push_to = format!("ws://{}", hub.local_addr().unwrap());
    let run = run_agent(
        &[
            ("PUSH_TO", OsStr::new(&push_to)),
            (
                "SYSTEM_AGENT_LISTEN",
                OsStr::from_bytes(b"leak-marker-\xff:9090"),
            ),
        ],
        None,
    );
    let output = &run.output;

    assert_eq!(
        run.code,
        Some(EX_CONFIG),
        "a refused configuration: {output}"
    );
    assert!(
        output.contains("SYSTEM_AGENT_LISTEN") && output.contains("not valid UTF-8"),
        "the refusal names the variable and the problem: {output}"
    );
    assert!(
        !output.contains("leak-marker"),
        "the value leaked: {output}"
    );
    assert!(
        !output.contains(FIRST_RUNTIME_LINE) && !output.contains("listening"),
        "refused before the runtime, never on the default address: {output}"
    );
    assert_eq!(connections(&hub), 0, "refused before the push client");
}

/// A directory of its own under the system's temp directory, removed on drop.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("agent-startup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn an_unusable_id_file_refuses_startup_before_anything_starts_when_pushing() {
    // (case, whether PUSH_TO is set, SYSTEM_AGENT_ID_FILE's value, the file's content if any,
    // the exit code, or `None` for an agent that starts)
    let scratch = Scratch::new();
    let invalid = scratch.0.join("leak-marker-invalid");
    std::fs::write(&invalid, "..\n").unwrap();
    let invalid = invalid.to_str().unwrap().to_owned();
    let cases = [
        (
            "a relative path, pushing",
            true,
            "leak-marker-relative/id",
            Some(EX_CONFIG),
        ),
        (
            "a file breaking the rule, pushing",
            true,
            invalid.as_str(),
            Some(EX_CONFIG),
        ),
        (
            "a relative path, not pushing",
            false,
            "leak-marker-relative/id",
            None,
        ),
    ];
    for (name, pushing, value, code) in cases {
        let hub = TcpListener::bind("127.0.0.1:0").unwrap();
        hub.set_nonblocking(true).unwrap();
        let push_to = format!("ws://{}", hub.local_addr().unwrap());
        let run = run_with(
            pushing.then_some(push_to.as_str()),
            &[("SYSTEM_AGENT_ID_FILE", value)],
            Some(STARTED),
        );
        let hub_connections = connections(&hub);
        let output = &run.output;

        assert_eq!(run.code, code, "case {name}: {output}");
        if code.is_none() {
            assert!(output.contains(STARTED), "case {name}: started: {output}");
            continue;
        }
        assert!(
            output.contains("SYSTEM_AGENT_ID_FILE"),
            "case {name}: the refusal names the variable: {output}"
        );
        assert!(
            !output.contains("leak-marker"),
            "case {name}: never the path: {output}"
        );
        assert!(
            !output.contains(FIRST_RUNTIME_LINE) && !output.contains(STARTED),
            "case {name}: refused before the runtime: {output}"
        );
        assert_eq!(
            hub_connections, 0,
            "case {name}: refused before the push client"
        );
    }
}

#[test]
fn a_missing_id_file_is_written_at_startup() {
    let scratch = Scratch::new();
    let file = scratch.0.join("id");
    let hub = TcpListener::bind("127.0.0.1:0").unwrap();
    let push_to = format!("ws://{}", hub.local_addr().unwrap());

    let run = run_with(
        Some(&push_to),
        &[("SYSTEM_AGENT_ID_FILE", file.to_str().unwrap())],
        Some(STARTED),
    );

    let output = &run.output;
    assert!(output.contains(STARTED), "the agent started: {output}");
    let id = std::fs::read_to_string(&file).unwrap_or_default();
    let id = id.trim();
    assert!(
        !id.is_empty() && id.len() <= 255 && id != "." && id != "..",
        "the file holds an id the hub accepts: {id:?}; {output}"
    );
}

/// Where a client reaches a service bound at `bound`: an unspecified IP can't be connected
/// to, so its family's loopback stands in.
fn reachable(bound: std::net::SocketAddr) -> std::net::SocketAddr {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    match bound.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => (Ipv4Addr::LOCALHOST, bound.port()).into(),
        IpAddr::V6(ip) if ip.is_unspecified() => (Ipv6Addr::LOCALHOST, bound.port()).into(),
        IpAddr::V4(_) | IpAddr::V6(_) => bound,
    }
}

/// The raw answer to `GET /api/health` at `address`, or why there was none. Never panics, so
/// a failing case still kills its agent.
fn health(address: std::net::SocketAddr) -> Result<String, String> {
    use std::io::Write;
    let mut stream = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .map_err(|err| format!("no connection at {address}: {err}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| err.to_string())?;
    stream
        .write_all(b"GET /api/health HTTP/1.1\r\nHost: agent\r\nConnection: close\r\n\r\n")
        .map_err(|err| err.to_string())?;
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    Ok(answer)
}
