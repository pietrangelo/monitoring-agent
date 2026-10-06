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

//! Block files (RFC 0010 §5, §6): each closed span of a tier, once handed off out of redb,
//! lives in one immutable file. Its format, its name, its `blocks` row and the reconciliation
//! of files with rows at open are pure; reading and writing files is the store's.

pub mod format;
pub mod name;
pub mod reconcile;
pub mod record;
