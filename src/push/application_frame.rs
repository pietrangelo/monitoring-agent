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

//! The application frame (RFC 0009 §7): one scrape round, as a second kind of binary push
//! message beside the snapshot frame.

use serde::Serialize;

use crate::applications::round::ScrapeRound;
use crate::applications::wire::ApplicationReportDto;

/// The only frame kind this agent sends. An incompatible later shape gets a new kind, which a
/// v1 hub drops.
const KIND: &str = "applications.v1";

/// An application frame, encoded positionally: the field *order* is the contract, pinned by
/// `testdata/application-frame-v1.msgpack`.
#[derive(Debug, Serialize)]
pub(crate) struct ApplicationFrame {
    kind: &'static str,
    run: String,
    seq: u64,
    interval_secs: u64,
    applications: Vec<ApplicationReportDto>,
}

impl From<&ScrapeRound> for ApplicationFrame {
    fn from(round: &ScrapeRound) -> Self {
        Self {
            kind: KIND,
            run: round.id.run.as_uuid().hyphenated().to_string(),
            seq: round.id.seq,
            interval_secs: round.interval.as_duration().as_secs(),
            applications: round
                .applications
                .iter()
                .map(ApplicationReportDto::from)
                .collect(),
        }
    }
}

/// Encodes a scrape round as a v1 application frame.
pub fn encode(round: &ScrapeRound) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    rmp_serde::to_vec(&ApplicationFrame::from(round))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::AgentRun;
    use crate::applications::wire::{sample_round, sparse_round};
    use std::collections::BTreeMap;

    /// The contract both crates read: written by an encoder independent of rmp-serde.
    const GOLDEN: &[u8] = include_bytes!("../../testdata/application-frame-v1.msgpack");

    #[test]
    fn a_round_encodes_to_the_golden_application_frame() {
        let run = uuid::Uuid::parse_str("6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b").unwrap();
        let round = sample_round(AgentRun::new(run), 42);
        let frame = encode(&round).expect("a round encodes");
        assert_eq!(frame, GOLDEN, "the encoding moved off the v1 contract");
    }

    #[test]
    fn every_part_of_a_round_reaches_the_frame() {
        type Report = (String, String, Option<String>, BTreeMap<String, f64>);
        type Frame = (String, String, u64, u64, Vec<Report>);
        let run = uuid::Uuid::from_u128(0xabc);
        let sparse = sparse_round(AgentRun::new(run), 7);
        let mut empty = sparse_round(AgentRun::new(run), 8);
        empty.applications.clear();
        let solo: Report = (
            "solo".into(),
            "down".into(),
            None,
            BTreeMap::from([("live_threads".into(), 12.0)]),
        );
        let run = run.hyphenated().to_string();
        let cases: [(&str, ScrapeRound, Frame); 2] = [
            (
                "one sparse application",
                sparse,
                ("applications.v1".into(), run.clone(), 7, 3600, vec![solo]),
            ),
            (
                "no applications",
                empty,
                ("applications.v1".into(), run, 8, 3600, vec![]),
            ),
        ];
        for (case, round, expected) in cases {
            let frame = encode(&round).expect("a round encodes");
            let decoded: Frame = rmp_serde::from_slice(&frame).expect("a 5-element frame");
            assert_eq!(decoded, expected, "case: {case}");
        }
    }
}
