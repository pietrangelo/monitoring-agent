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
//! applications configuration refuses startup before anything starts (RFC 0009 §2).

#![cfg(unix)]

use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// `EX_CONFIG` from `sysexits.h`: the exit code of a refused configuration, and of nothing
/// else, so no other failure (a busy port, a panic) can pass for a refusal.
const EX_CONFIG: i32 = 78;

/// The line `run` logs once the configuration has been accepted, before binding.
const STARTED: &str = "No auth token configured";

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
fn run_agent(vars: &[(&str, &str)], stop_at: Option<&str>) -> Run {
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
    let borrowed: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
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
