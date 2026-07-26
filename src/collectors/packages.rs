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

use crate::models::PackageInfo;
use std::process::Command;

pub fn collect() -> Vec<PackageInfo> {
    // Try dpkg (Debian/Ubuntu)
    if let Ok(out) = Command::new("dpkg-query")
        .args(["-W", "-f=${Package}\t${Version}\n"])
        .output()
        && out.status.success()
    {
        return parse_dpkg(&String::from_utf8_lossy(&out.stdout));
    }

    // Try rpm (RHEL/Fedora)
    if let Ok(out) = Command::new("rpm")
        .args(["-qa", "--queryformat", "%{NAME}\t%{VERSION}-%{RELEASE}\n"])
        .output()
        && out.status.success()
    {
        return parse_rpm(&String::from_utf8_lossy(&out.stdout));
    }

    // Try pacman (Arch)
    if let Ok(out) = Command::new("pacman").args(["-Q"]).output()
        && out.status.success()
    {
        return parse_pacman(&String::from_utf8_lossy(&out.stdout));
    }

    // Try apk (Alpine)
    if let Ok(out) = Command::new("apk").args(["info", "-v"]).output()
        && out.status.success()
    {
        return parse_apk(&String::from_utf8_lossy(&out.stdout));
    }

    vec![]
}

fn parse_dpkg(output: &str) -> Vec<PackageInfo> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '\t');
            Some(PackageInfo {
                name: parts.next()?.to_string(),
                version: parts.next().unwrap_or("").to_string(),
                manager: "dpkg".into(),
            })
        })
        .collect()
}

fn parse_rpm(output: &str) -> Vec<PackageInfo> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '\t');
            Some(PackageInfo {
                name: parts.next()?.to_string(),
                version: parts.next().unwrap_or("").to_string(),
                manager: "rpm".into(),
            })
        })
        .collect()
}

fn parse_pacman(output: &str) -> Vec<PackageInfo> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, ' ');
            Some(PackageInfo {
                name: parts.next()?.to_string(),
                version: parts.next().unwrap_or("").to_string(),
                manager: "pacman".into(),
            })
        })
        .collect()
}

fn parse_apk(output: &str) -> Vec<PackageInfo> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '-');
            let name = parts.next()?.to_string();
            let ver = parts.next().unwrap_or("").to_string();
            if name.is_empty() {
                return None;
            }
            Some(PackageInfo {
                name,
                version: ver,
                manager: "apk".into(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dpkg_multiple_lines() {
        let out = "bash\t5.2.15-2\ncurl\t8.4.0-1\n";
        let pkgs = parse_dpkg(out);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "bash");
        assert_eq!(pkgs[0].version, "5.2.15-2");
        assert_eq!(pkgs[0].manager, "dpkg");
    }

    #[test]
    fn parse_dpkg_missing_version_defaults_empty() {
        let pkgs = parse_dpkg("onlyname\n");
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].version, "");
    }

    #[test]
    fn parse_dpkg_empty_input() {
        assert!(parse_dpkg("").is_empty());
    }

    #[test]
    fn parse_rpm_basic() {
        let out = "glibc\t2.38-6\n";
        let pkgs = parse_rpm(out);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "glibc");
        assert_eq!(pkgs[0].version, "2.38-6");
        assert_eq!(pkgs[0].manager, "rpm");
    }

    #[test]
    fn parse_pacman_space_separated() {
        let out = "linux 6.6.1.arch1-1\nbash 5.2.021-1\n";
        let pkgs = parse_pacman(out);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "linux");
        assert_eq!(pkgs[0].version, "6.6.1.arch1-1");
        assert_eq!(pkgs[0].manager, "pacman");
    }

    #[test]
    fn parse_apk_dash_separated() {
        let out = "musl-1.2.4-r2\nbusybox-1.36.1-r15\n";
        let pkgs = parse_apk(out);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "musl");
        assert_eq!(pkgs[0].version, "1.2.4-r2");
        assert_eq!(pkgs[0].manager, "apk");
    }

    #[test]
    fn parse_apk_skips_lines_with_empty_name() {
        let out = "-1.0\nvalid-2.0\n";
        let pkgs = parse_apk(out);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "valid");
    }

    #[test]
    fn parse_apk_no_dash_keeps_name_empty_version() {
        let pkgs = parse_apk("noversionatall\n");
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "noversionatall");
        assert_eq!(pkgs[0].version, "");
    }
}
