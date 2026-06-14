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
    {
        if out.status.success() {
            return parse_dpkg(&String::from_utf8_lossy(&out.stdout));
        }
    }

    // Try rpm (RHEL/Fedora)
    if let Ok(out) = Command::new("rpm")
        .args(["-qa", "--queryformat", "%{NAME}\t%{VERSION}-%{RELEASE}\n"])
        .output()
    {
        if out.status.success() {
            return parse_rpm(&String::from_utf8_lossy(&out.stdout));
        }
    }

    // Try pacman (Arch)
    if let Ok(out) = Command::new("pacman").args(["-Q"]).output() {
        if out.status.success() {
            return parse_pacman(&String::from_utf8_lossy(&out.stdout));
        }
    }

    // Try apk (Alpine)
    if let Ok(out) = Command::new("apk").args(["info", "-v"]).output() {
        if out.status.success() {
            return parse_apk(&String::from_utf8_lossy(&out.stdout));
        }
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
