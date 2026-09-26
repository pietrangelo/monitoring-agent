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

//! Application Telemetry: the Spring Boot applications the operator asked this agent to
//! watch, and what it saw of them (RFC 0009).

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "wired to the scrape loop in RFC 0009 commit 2")
)]
pub mod actuator;
pub mod config;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "wired to the actuator adapter and scrape loop in RFC 0009 commit 2"
    )
)]
pub mod report;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "wired to the scrape loop in RFC 0009 commit 2 step 3"
    )
)]
pub mod scraper;
