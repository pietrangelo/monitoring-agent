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

//! Retention (RFC 0010 §5), the pure core: the policies (the global one, per-system
//! overrides and their pending shortenings, RFC 0012 §2), which chunks are live, and what
//! becomes of each block file. The retention pass that acts on them runs on the writer thread.

mod files;
mod liveness;
mod policy;

pub use files::{
    FileCondition, FileFacts, FileFate, FileKey, FilePlan, Holder, RewriteCause, file_fate,
    plan_files,
};
pub use liveness::{Death, Liveness, Owner, Tombstones, chunk_liveness, expired};
pub use policy::{
    DuplicateTier, OutOfBounds, Override, PendingShortening, PerTier, Policies, RetentionChange,
    RetentionOutcome, RetentionPolicy, TierMismatch, TierOverride, TierPeriod, TierSetting,
};
