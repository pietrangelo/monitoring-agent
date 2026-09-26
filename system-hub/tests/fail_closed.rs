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

//! Runs the real `system-hub` binary to check startup configuration that no router test
//! can reach (RFC 0006 § Fail-closed token).

#![cfg(unix)]

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;

/// What the hub printed, and whether it exited on its own within 10 s.
struct Run {
    exited: bool,
    success: bool,
    stdout: String,
    stderr: String,
}

/// Runs the hub in `dir` with `HUB_PUSH_TOKEN` set to `token` and `RUST_LOG` removed. It is
/// killed if it is still running after 10 s, so a hub that starts serving fails an
/// assertion instead of hanging the suite.
async fn run_hub(dir: &std::path::Path, token: &OsStr) -> Run {
    run_hub_with(dir, &[("HUB_PUSH_TOKEN", token)]).await
}

/// Runs the hub in `dir` with `vars` set and `RUST_LOG` and `HUB_STATIC_DIR` removed, under
/// the same 10 s limit as `run_hub`.
async fn run_hub_with(dir: &std::path::Path, vars: &[(&str, &OsStr)]) -> Run {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_system-hub"));
    command
        .current_dir(dir)
        .env_remove("RUST_LOG")
        .env_remove("HUB_STATIC_DIR");
    for (key, value) in vars {
        command.env(key, value);
    }
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    match tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await {
        Ok(output) => {
            let output = output.unwrap();
            Run {
                exited: true,
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }
        }
        Err(_) => Run {
            exited: false,
            success: false,
            stdout: String::new(),
            stderr: String::new(),
        },
    }
}

#[tokio::test]
async fn a_non_utf8_push_token_refuses_startup_before_touching_the_database() {
    let dir = tempfile::tempdir().unwrap();
    // A recognisable prefix, so a leaked value would show up in the output even though the
    // invalid byte makes the whole value non-UTF-8.
    let run = run_hub(dir.path(), OsStr::from_bytes(b"leak-marker-\xff")).await;

    assert!(
        run.exited,
        "the hub must exit on its own instead of serving with push auth open"
    );
    assert!(!run.success, "a refused start exits non-zero");
    // tracing's fmt subscriber writes to stdout.
    assert!(
        run.stdout.contains("HUB_PUSH_TOKEN") && run.stdout.contains("not valid UTF-8"),
        "stdout: {}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("any client may push"),
        "the hub opened push instead of refusing: {}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("leak-marker"),
        "the value leaked on stdout: {}",
        run.stdout
    );
    assert!(
        !run.stderr.contains("leak-marker"),
        "the value leaked on stderr: {}",
        run.stderr
    );
    let created: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert!(
        created.is_empty(),
        "files were created before the configuration was checked: {created:?}"
    );
}

/// The negative control: a valid token, or an empty one (push open, the README's basic
/// start), gets past the configuration, and a database that won't open is a logged,
/// non-zero exit rather than a panic. A directory where the database file should be makes
/// the open fail before any port is bound.
#[tokio::test]
async fn an_accepted_push_token_reaches_the_database_and_an_unopenable_one_is_a_logged_exit() {
    let cases = [
        ("valid token", "leak-marker-valid"),
        ("empty token leaves push open", ""),
    ];
    for (name, token) in cases {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("system-hub.db")).unwrap();

        let run = run_hub(dir.path(), OsStr::new(token)).await;

        assert!(
            run.exited,
            "{name}: the hub must exit when its database won't open"
        );
        assert!(
            !run.success,
            "{name}: an unopenable database exits non-zero"
        );
        assert!(
            !run.stdout.contains("not valid UTF-8"),
            "{name}: the token was refused: {}",
            run.stdout
        );
        assert!(
            run.stdout.contains("unable to open database file"),
            "{name}: SQLite's open error is logged: stdout: {}",
            run.stdout
        );
        assert!(
            !run.stderr.contains("panicked"),
            "{name}: the hub panicked instead of exiting: {}",
            run.stderr
        );
        assert!(
            !run.stdout.contains("leak-marker"),
            "{name}: the token leaked: {}",
            run.stdout
        );
    }
}

/// A `HUB_STATIC_DIR` that names no directory would serve an empty dashboard, so it refuses
/// startup before anything is created. One that names a directory, or a symlink to one, or
/// none at all (unset or empty keep the default `static`, unchecked), gets as far as the
/// database, which a directory in its place makes fail.
#[tokio::test]
async fn a_static_dir_that_is_not_a_directory_refuses_startup_before_touching_the_database() {
    let outside = tempfile::tempdir().unwrap();
    let a_file = outside.path().join("index.html");
    std::fs::write(&a_file, "not a directory").unwrap();
    let missing = outside.path().join("missing");
    // A directory the hub can't look inside: its parent has no permissions at all.
    let locked = outside.path().join("locked");
    let behind_lock = locked.join("static");
    std::fs::create_dir_all(&behind_lock).unwrap();
    std::fs::set_permissions(&locked, PermissionsExt::from_mode(0o000)).unwrap();
    // (case, HUB_STATIC_DIR, what the refusal says)
    let cases = [
        ("a missing path", missing.as_os_str(), "does not exist"),
        ("a file", a_file.as_os_str(), "is not a directory"),
        (
            "a path the hub can't read",
            behind_lock.as_os_str(),
            "cannot be read",
        ),
    ];
    for (name, static_dir, says) in cases {
        let dir = tempfile::tempdir().unwrap();
        let run = run_hub_with(dir.path(), &[("HUB_STATIC_DIR", static_dir)]).await;

        assert!(run.exited, "{name}: the hub must exit instead of serving");
        assert!(!run.success, "{name}: a refused start exits non-zero");
        assert!(
            run.stdout.contains("HUB_STATIC_DIR"),
            "{name}: the refusal names the variable: {}",
            run.stdout
        );
        assert!(
            run.stdout.contains(says),
            "{name}: the refusal says it {says}: {}",
            run.stdout
        );
        assert!(
            !run.stderr.contains("panicked"),
            "{name}: the hub panicked instead of refusing: {}",
            run.stderr
        );
        let created: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(
            created.is_empty(),
            "{name}: files were created before the configuration was checked: {created:?}"
        );
    }
    // Let the temporary directory be removed.
    std::fs::set_permissions(&locked, PermissionsExt::from_mode(0o700)).unwrap();
}

#[tokio::test]
async fn a_directory_or_no_static_dir_at_all_gets_past_the_check() {
    let web = tempfile::tempdir().unwrap();
    let link_parent = tempfile::tempdir().unwrap();
    let link = link_parent.path().join("static-link");
    std::os::unix::fs::symlink(web.path(), &link).unwrap();
    // (case, HUB_STATIC_DIR if set, whether the working directory has a static/ of its own)
    let cases: [(&str, Option<&OsStr>, bool); 6] = [
        (
            "unset, with no static/ in the working directory",
            None,
            false,
        ),
        ("an absolute directory", Some(web.path().as_os_str()), false),
        ("a symlink to a directory", Some(link.as_os_str()), false),
        (
            "a relative directory, against the working directory",
            Some(OsStr::new("web")),
            false,
        ),
        (
            "empty, with no static/ in the working directory",
            Some(OsStr::new("")),
            false,
        ),
        (
            "empty, with a static/ in the working directory",
            Some(OsStr::new("")),
            true,
        ),
    ];
    for (name, static_dir, has_static) in cases {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("web")).unwrap();
        if has_static {
            std::fs::create_dir(dir.path().join("static")).unwrap();
        }
        std::fs::create_dir(dir.path().join("system-hub.db")).unwrap();
        let vars: Vec<(&str, &OsStr)> = static_dir
            .map(|v| ("HUB_STATIC_DIR", v))
            .into_iter()
            .collect();
        let run = run_hub_with(dir.path(), &vars).await;
        assert!(
            run.stdout.contains("unable to open database file")
                && !run.stdout.contains("HUB_STATIC_DIR"),
            "{name}: the check passed and the run reached the database: {}",
            run.stdout
        );
    }
}
