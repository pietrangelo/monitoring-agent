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

/// Runs the hub in `dir` with `vars` set and `RUST_LOG`, `HUB_PUSH_TOKEN` and
/// `HUB_STATIC_DIR` removed otherwise, under
/// the same 10 s limit as `run_hub`.
async fn run_hub_with(dir: &std::path::Path, vars: &[(&str, &OsStr)]) -> Run {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_system-hub"));
    command
        .current_dir(dir)
        .env_remove("RUST_LOG")
        .env_remove("HUB_PUSH_TOKEN")
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
/// Sets a directory's permissions, and gives them back on drop, so the temporary directory
/// can be removed even when an assertion fails first.
struct Locked<'a>(&'a std::path::Path);

impl<'a> Locked<'a> {
    fn new(path: &'a std::path::Path, mode: u32) -> Self {
        std::fs::set_permissions(path, PermissionsExt::from_mode(mode)).unwrap();
        Self(path)
    }

    /// Whether the lock binds this process: it doesn't for root, who can look inside anyway.
    fn holds(&self) -> bool {
        std::fs::metadata(self.0.join(".")).is_err()
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(self.0, PermissionsExt::from_mode(0o700));
    }
}

#[tokio::test]
async fn a_static_dir_that_is_not_a_directory_refuses_startup_before_touching_the_database() {
    let outside = tempfile::tempdir().unwrap();
    let a_file = outside.path().join("index.html");
    std::fs::write(&a_file, "not a directory").unwrap();
    let missing = outside.path().join("missing");
    // A missing path that isn't UTF-8 must still be read as configured, not as unset.
    let missing_not_utf8 = outside.path().join(OsStr::from_bytes(b"miss\xffing"));
    // A directory the hub can't look inside: its parent has no permissions at all.
    let locked = outside.path().join("locked");
    let behind_lock = locked.join("static");
    std::fs::create_dir_all(&behind_lock).unwrap();
    let locked = Locked::new(&locked, 0o000);
    // A directory the hub may list but not look inside, so it would serve nothing: `ServeDir`
    // needs search permission, not read.
    let unsearchable = outside.path().join("unsearchable");
    std::fs::create_dir(&unsearchable).unwrap();
    std::fs::write(unsearchable.join("index.html"), "unreachable").unwrap();
    let unsearchable = Locked::new(&unsearchable, 0o400);
    // No permissions at all on the directory itself, and none for its owner while everyone
    // else may search it: what counts is whether the hub can look inside, not the mode bits.
    let closed = outside.path().join("closed");
    std::fs::create_dir(&closed).unwrap();
    std::fs::write(closed.join("index.html"), "unreachable").unwrap();
    let closed = Locked::new(&closed, 0o000);
    let owner_shut_out = outside.path().join("owner-shut-out");
    std::fs::create_dir(&owner_shut_out).unwrap();
    let owner_shut_out = Locked::new(&owner_shut_out, 0o077);
    // (case, HUB_STATIC_DIR, what the refusal says)
    let mut cases = vec![
        ("a missing path", missing.as_os_str(), "does not exist"),
        (
            "a missing path that is not UTF-8",
            missing_not_utf8.as_os_str(),
            "does not exist",
        ),
        ("a file", a_file.as_os_str(), "is not a directory"),
    ];
    let locks = [&locked, &unsearchable, &closed, &owner_shut_out];
    if locks.iter().all(|lock| lock.holds()) {
        cases.push((
            "a path the hub can't read",
            behind_lock.as_os_str(),
            "cannot be read",
        ));
        cases.push((
            "a directory the hub can list but not search",
            unsearchable.0.as_os_str(),
            "cannot be read",
        ));
        cases.push((
            "a directory with no permissions",
            closed.0.as_os_str(),
            "cannot be read",
        ));
        cases.push((
            "a directory its owner may not search",
            owner_shut_out.0.as_os_str(),
            "cannot be read",
        ));
    } else {
        eprintln!("permissions don't bind this user (root?): the unreadable rows are skipped");
    }
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
}

#[tokio::test]
async fn a_directory_or_no_static_dir_at_all_gets_past_the_check() {
    let web = tempfile::tempdir().unwrap();
    let link_parent = tempfile::tempdir().unwrap();
    let link = link_parent.path().join("static-link");
    std::os::unix::fs::symlink(web.path(), &link).unwrap();
    let not_utf8 = link_parent.path().join(OsStr::from_bytes(b"we\xffb"));
    std::fs::create_dir(&not_utf8).unwrap();
    // Search without read is all `ServeDir` needs, so it is served.
    let search_only = link_parent.path().join("search-only");
    std::fs::create_dir(&search_only).unwrap();
    let _search_only = Locked::new(&search_only, 0o100);
    let no_read = link_parent.path().join("no-read");
    std::fs::create_dir(&no_read).unwrap();
    let _no_read = Locked::new(&no_read, 0o300);
    // (case, HUB_STATIC_DIR if set, whether the working directory has a static/ of its own)
    let cases: [(&str, Option<&OsStr>, bool); 9] = [
        (
            "unset, with no static/ in the working directory",
            None,
            false,
        ),
        ("an absolute directory", Some(web.path().as_os_str()), false),
        ("a symlink to a directory", Some(link.as_os_str()), false),
        (
            "a directory whose path is not UTF-8",
            Some(not_utf8.as_os_str()),
            false,
        ),
        (
            "a directory the hub may search but not list",
            Some(search_only.as_os_str()),
            false,
        ),
        (
            "a directory the hub may search and write but not list",
            Some(no_read.as_os_str()),
            false,
        ),
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

/// What `run` serves, end to end: the dashboard from `HUB_STATIC_DIR`, never the working
/// directory's own `static/`, behind the full router (API, applications, SSE, push) with
/// CORS. The hub binds its fixed port,
/// so this fails, saying why, while another hub holds 9091.
#[tokio::test]
async fn the_running_hub_serves_the_configured_dashboard_and_the_full_router() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("static")).unwrap();
    std::fs::write(dir.path().join("static/index.html"), "default-marker").unwrap();
    let web = tempfile::tempdir().unwrap();
    std::fs::write(web.path().join("index.html"), "configured-marker").unwrap();

    let mut hub = tokio::process::Command::new(env!("CARGO_BIN_EXE_system-hub"))
        .current_dir(dir.path())
        .env_remove("RUST_LOG")
        .env_remove("HUB_PUSH_TOKEN")
        .env("HUB_STATIC_DIR", web.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let client = reqwest::Client::new();
    let base = "http://127.0.0.1:9091";
    let dashboard = wait_for(&client, &format!("{base}/"), &mut hub).await;

    assert!(
        dashboard.contains("configured-marker"),
        "the dashboard comes from HUB_STATIC_DIR: {dashboard}"
    );
    let applications = client
        .get(format!("{base}/api/systems/sys-1/applications"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        applications.status(),
        200,
        "the applications route is served"
    );
    assert!(
        applications
            .headers()
            .contains_key("access-control-allow-origin"),
        "CORS covers the whole router, not only the API"
    );
    let body = applications.text().await.unwrap();
    assert!(
        body.contains("\"system_id\":\"sys-1\""),
        "the applications route answers with its own JSON: {body}"
    );
    // A plain GET is not a WebSocket upgrade; the push route answers it, not the files.
    let push = client.get(format!("{base}/api/push")).send().await.unwrap();
    assert_ne!(push.status(), 404, "the push endpoint is routed");
    let stream = client
        .get(format!("{base}/api/stream/summary"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        stream.headers().get("content-type").map(|v| v.as_bytes()),
        Some(&b"text/event-stream"[..]),
        "the SSE stream is served"
    );
    drop(stream);
    let health = client
        .get(format!("{base}/api/health"))
        .header("origin", "http://example.test")
        .send()
        .await
        .unwrap();
    // CORS also marks a 404 from the files, so the status proves the API itself answered.
    assert_eq!(health.status(), 200, "the API is served");
    assert!(
        health.headers().contains_key("access-control-allow-origin"),
        "the CORS layer is applied"
    );
}

/// The body of `url` once the hub answers it, within 10 s. Fails if the hub exits first,
/// which is what a port already in use looks like.
async fn wait_for(client: &reqwest::Client, url: &str, hub: &mut tokio::process::Child) -> String {
    for _ in 0..100 {
        if let Some(status) = hub.try_wait().unwrap() {
            panic!("the hub exited ({status}) before serving; is port 9091 already in use?");
        }
        if let Ok(res) = client.get(url).send().await {
            return res.text().await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the hub did not answer {url} within 10 s");
}
