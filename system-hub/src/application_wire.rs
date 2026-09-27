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

//! Scrape rounds as agents send them, pushed as an application frame (RFC 0009 §7) or polled
//! from `/api/applications` (§6), and their conversion into a `ScrapeRound`: the
//! anti-corruption layer between what an agent sends and Fleet History.

use serde::Deserialize;
use std::collections::BTreeMap;

use crate::applications::{
    ApplicationHealth, ApplicationName, ApplicationReport, ApplicationVersion, Gauges, RoundId,
    ScrapeInterval, ScrapeRound, ScrapeRoundError,
};

/// The only frame kind this hub reads. A later, incompatible shape gets a new kind.
const APPLICATION_FRAME_KIND: &str = "applications.v1";

/// An application frame, decoded positionally: the field *order* is the contract, pinned by
/// `testdata/application-frame-v1.msgpack`.
#[derive(Debug, PartialEq, Deserialize)]
pub struct ApplicationFrameDto {
    kind: String,
    run: String,
    seq: u64,
    interval_secs: u64,
    applications: Vec<ApplicationReportDto>,
}

/// One application report as an agent sends it.
#[derive(Debug, PartialEq, Deserialize)]
pub struct ApplicationReportDto {
    name: String,
    health: String,
    version: Option<String>,
    gauges: BTreeMap<String, f64>,
}

/// The applications poll response as an agent serves it. `scraped_at` (the agent's clock) is
/// ignored: the hub ages rounds by its own clock.
#[derive(Debug, PartialEq, Deserialize)]
pub struct ApplicationsResponseDto {
    round: Option<RoundIdDto>,
    interval_secs: Option<u64>,
    #[serde(default)]
    applications: Vec<ApplicationReportDto>,
}

#[derive(Debug, PartialEq, Deserialize)]
struct RoundIdDto {
    run: String,
    seq: u64,
}

/// What a poll of `/api/applications` answered: no round (applications off, or none yet), or
/// one.
#[derive(Debug, PartialEq)]
pub enum PolledRound {
    NoRound,
    Round(ScrapeRound),
}

impl TryFrom<ApplicationsResponseDto> for PolledRound {
    type Error = ScrapeRoundError;

    fn try_from(response: ApplicationsResponseDto) -> Result<Self, Self::Error> {
        let Some(round) = response.round else {
            return Ok(Self::NoRound);
        };
        let interval_secs = response
            .interval_secs
            .ok_or(ScrapeRoundError::InvalidInterval)?;
        build_round(&round.run, round.seq, interval_secs, response.applications).map(Self::Round)
    }
}

/// Why an application frame was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRefusal {
    /// A frame kind this hub doesn't read.
    UnknownKind,
    Round(ScrapeRoundError),
}

/// Decodes an application frame; `None` for anything else.
pub fn decode(data: &[u8]) -> Option<ApplicationFrameDto> {
    rmp_serde::from_slice(data).ok()
}

impl TryFrom<ApplicationFrameDto> for ScrapeRound {
    type Error = FrameRefusal;

    fn try_from(frame: ApplicationFrameDto) -> Result<Self, Self::Error> {
        if frame.kind != APPLICATION_FRAME_KIND {
            return Err(FrameRefusal::UnknownKind);
        }
        build_round(
            &frame.run,
            frame.seq,
            frame.interval_secs,
            frame.applications,
        )
        .map_err(FrameRefusal::Round)
    }
}

/// Builds a round from the fields both wire shapes carry, refusing it as a whole on the first
/// broken rule.
fn build_round(
    run: &str,
    seq: u64,
    interval_secs: u64,
    applications: Vec<ApplicationReportDto>,
) -> Result<ScrapeRound, ScrapeRoundError> {
    let id = RoundId::parse(run, seq)?;
    let interval = ScrapeInterval::from_secs(interval_secs)?;
    let applications = applications
        .into_iter()
        .map(ApplicationReport::try_from)
        .collect::<Result<_, _>>()?;
    ScrapeRound::new(id, interval, applications)
}

impl TryFrom<ApplicationReportDto> for ApplicationReport {
    type Error = ScrapeRoundError;

    fn try_from(dto: ApplicationReportDto) -> Result<Self, Self::Error> {
        Ok(Self {
            name: ApplicationName::parse(&dto.name)
                .ok_or(ScrapeRoundError::InvalidApplicationName)?,
            health: ApplicationHealth::from_wire(&dto.health),
            version: dto.version.as_deref().and_then(ApplicationVersion::parse),
            gauges: Gauges::from_wire(
                dto.gauges
                    .iter()
                    .map(|(name, value)| (name.as_str(), *value)),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Written by `testdata/generate_application_frame_v1.py`, an encoder independent of
    /// rmp-serde; the agent's test pins the same bytes.
    const GOLDEN: &[u8] = include_bytes!("../../testdata/application-frame-v1.msgpack");
    const RUN: &str = "6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b";

    fn golden_dto() -> ApplicationFrameDto {
        ApplicationFrameDto {
            kind: "applications.v1".into(),
            run: RUN.into(),
            seq: 42,
            interval_secs: 20,
            applications: vec![
                ApplicationReportDto {
                    name: "orders".into(),
                    health: "up".into(),
                    version: Some("2.4.1".into()),
                    gauges: BTreeMap::from([
                        ("heap_used_bytes".into(), 300.0),
                        ("uptime_seconds".into(), 600.5),
                    ]),
                },
                ApplicationReportDto {
                    name: "billing".into(),
                    health: "unreachable".into(),
                    version: None,
                    gauges: BTreeMap::new(),
                },
            ],
        }
    }

    fn app(name: &str) -> ApplicationReportDto {
        ApplicationReportDto {
            name: name.into(),
            health: "up".into(),
            version: None,
            gauges: BTreeMap::new(),
        }
    }

    #[test]
    fn the_golden_frame_decodes_to_exactly_the_agents_round() {
        assert_eq!(decode(GOLDEN), Some(golden_dto()));
    }

    #[test]
    fn any_five_element_frame_decodes_and_other_arities_dont() {
        type Report = (
            &'static str,
            &'static str,
            Option<&'static str>,
            BTreeMap<&'static str, f64>,
        );
        let solo: Report = (
            "solo",
            "down",
            None,
            BTreeMap::from([("live_threads", 12.0)]),
        );
        let other =
            rmp_serde::to_vec(&("applications.v1", "run-x", 7u64, 3600u64, vec![solo])).unwrap();
        assert_eq!(
            decode(&other),
            Some(ApplicationFrameDto {
                kind: "applications.v1".into(),
                run: "run-x".into(),
                seq: 7,
                interval_secs: 3600,
                applications: vec![ApplicationReportDto {
                    name: "solo".into(),
                    health: "down".into(),
                    version: None,
                    gauges: BTreeMap::from([("live_threads".into(), 12.0)]),
                }],
            })
        );
        let four = rmp_serde::to_vec(&("applications.v1", "run-x", 7u64, 3600u64)).unwrap();
        let six = rmp_serde::to_vec(&(
            "applications.v1",
            "run-x",
            7u64,
            3600u64,
            Vec::<Report>::new(),
            1u8,
        ))
        .unwrap();
        assert_eq!(decode(&four), None, "four elements");
        assert_eq!(decode(&six), None, "six elements");
    }

    #[test]
    fn a_decoded_frame_converts_into_the_round_it_describes() {
        let expected = ScrapeRound::new(
            RoundId::parse(RUN, 42).unwrap(),
            ScrapeInterval::from_secs(20).unwrap(),
            vec![
                ApplicationReport {
                    name: ApplicationName::parse("orders").unwrap(),
                    health: ApplicationHealth::Up,
                    version: ApplicationVersion::parse("2.4.1"),
                    gauges: Gauges::from_wire([
                        ("heap_used_bytes", 300.0),
                        ("uptime_seconds", 600.5),
                    ]),
                },
                ApplicationReport {
                    name: ApplicationName::parse("billing").unwrap(),
                    health: ApplicationHealth::Unreachable,
                    version: None,
                    gauges: Gauges::default(),
                },
            ],
        )
        .unwrap();
        assert_eq!(ScrapeRound::try_from(golden_dto()), Ok(expected));
    }

    #[test]
    fn a_frame_is_refused_as_a_whole_for_each_broken_rule() {
        use ScrapeRoundError::*;
        type Edit = fn(&mut ApplicationFrameDto);
        let cases: [(&str, Edit, FrameRefusal); 12] = [
            (
                "a later kind",
                |f| f.kind = "applications.v2".into(),
                FrameRefusal::UnknownKind,
            ),
            (
                "the kind in capitals",
                |f| f.kind = "APPLICATIONS.V1".into(),
                FrameRefusal::UnknownKind,
            ),
            (
                "a kind with a suffix",
                |f| f.kind = "applications.v10".into(),
                FrameRefusal::UnknownKind,
            ),
            (
                "a 3601 s interval",
                |f| f.interval_secs = 3601,
                FrameRefusal::Round(InvalidInterval),
            ),
            (
                "a u64::MAX interval",
                |f| f.interval_secs = u64::MAX,
                FrameRefusal::Round(InvalidInterval),
            ),
            (
                "a 2^32 + 15 s interval",
                |f| f.interval_secs = (1 << 32) + 15,
                FrameRefusal::Round(InvalidInterval),
            ),
            (
                "no kind",
                |f| f.kind = String::new(),
                FrameRefusal::UnknownKind,
            ),
            (
                "a run that isn't a UUID",
                |f| f.run = "run".into(),
                FrameRefusal::Round(InvalidRun),
            ),
            (
                "a 9 s interval",
                |f| f.interval_secs = 9,
                FrameRefusal::Round(InvalidInterval),
            ),
            (
                "17 applications",
                |f| f.applications = (0..17).map(|i| app(&format!("a{i}"))).collect(),
                FrameRefusal::Round(TooManyApplications),
            ),
            (
                "an invalid name",
                |f| f.applications[0].name = "or ders".into(),
                FrameRefusal::Round(InvalidApplicationName),
            ),
            (
                "a repeated name",
                |f| f.applications[1].name = "orders".into(),
                FrameRefusal::Round(DuplicateApplicationName),
            ),
        ];
        for (case, edit, refusal) in cases {
            let mut frame = golden_dto();
            edit(&mut frame);
            assert_eq!(ScrapeRound::try_from(frame), Err(refusal), "case: {case}");
        }
    }

    #[test]
    fn a_frame_at_each_accepted_bound_converts() {
        type Edit = fn(&mut ApplicationFrameDto);
        let cases: [(&str, Edit); 3] = [
            ("a 10 s interval", |f| f.interval_secs = 10),
            ("a 3600 s interval", |f| f.interval_secs = 3600),
            ("16 applications", |f| {
                f.applications = (0..16).map(|i| app(&format!("a{i}"))).collect()
            }),
        ];
        for (case, edit) in cases {
            let mut frame = golden_dto();
            edit(&mut frame);
            let converted = ScrapeRound::try_from(frame);
            assert!(converted.is_ok(), "case: {case}: {converted:?}");
        }
    }

    #[test]
    fn a_blank_version_on_the_wire_is_no_version() {
        let mut frame = golden_dto();
        frame.applications[0].version = Some("  ".into());
        let converted = ScrapeRound::try_from(frame);
        assert!(converted.is_ok(), "{converted:?}");
        assert_eq!(converted.unwrap().applications()[0].version, None);
    }

    #[test]
    fn what_a_later_agent_adds_degrades_instead_of_refusing_the_round() {
        let mut frame = golden_dto();
        let orders = &mut frame.applications[0];
        orders.health = "degraded".into();
        orders.version = Some(format!("{}-SNAPSHOT", "9".repeat(70)));
        orders.gauges.insert("a_later_gauge".into(), 1.0);
        orders.gauges.insert("live_threads".into(), f64::NAN);
        let converted = ScrapeRound::try_from(frame);
        assert!(converted.is_ok(), "the round is kept: {converted:?}");
        let round = converted.unwrap();
        let orders = &round.applications()[0];
        assert_eq!(orders.health, ApplicationHealth::Unknown);
        assert_eq!(orders.version.as_ref().map(|v| v.as_str().len()), Some(64));
        let gauges: Vec<_> = orders.gauges.iter().map(|(g, _)| g.wire_name()).collect();
        assert_eq!(gauges, ["heap_used_bytes", "uptime_seconds"]);
    }

    const GOLDEN_JSON: &str = include_str!("../../testdata/applications-v1.json");

    #[test]
    fn the_golden_poll_body_decodes_to_exactly_the_agents_round() {
        let decoded: ApplicationsResponseDto = serde_json::from_str(GOLDEN_JSON).unwrap();
        let golden = golden_dto();
        assert_eq!(
            decoded,
            ApplicationsResponseDto {
                round: Some(RoundIdDto {
                    run: golden.run.clone(),
                    seq: golden.seq,
                }),
                interval_secs: Some(golden.interval_secs),
                applications: golden.applications,
            }
        );
    }

    #[test]
    fn a_polled_body_converts_like_the_frame_it_mirrors() {
        let decoded: ApplicationsResponseDto = serde_json::from_str(GOLDEN_JSON).unwrap();
        let from_frame = ScrapeRound::try_from(golden_dto()).unwrap();
        assert_eq!(
            PolledRound::try_from(decoded),
            Ok(PolledRound::Round(from_frame))
        );
    }

    #[test]
    fn a_polled_body_without_a_round_is_no_round_and_a_broken_one_is_refused() {
        use ScrapeRoundError::*;
        let body = |json: &str| serde_json::from_str::<ApplicationsResponseDto>(json).unwrap();
        let seventeen = format!(
            r#"{{"round":{{"run":"{RUN}","seq":1}},"interval_secs":15,"applications":[{}]}}"#,
            (0..17)
                .map(|i| format!(r#"{{"name":"a{i}","health":"up","version":null,"gauges":{{}}}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let cases = [
            (
                "no round",
                r#"{"round":null,"interval_secs":null,"scraped_at":null,"applications":[]}"#,
                Ok(PolledRound::NoRound),
            ),
            (
                "no round, fields missing",
                r#"{"round":null}"#,
                Ok(PolledRound::NoRound),
            ),
            (
                "a round without an interval",
                r#"{"round":{"run":"6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b","seq":1},"applications":[]}"#,
                Err(InvalidInterval),
            ),
            (
                "a run that isn't a UUID",
                r#"{"round":{"run":"x","seq":1},"interval_secs":15,"applications":[]}"#,
                Err(InvalidRun),
            ),
            (
                "an invalid application name",
                r#"{"round":{"run":"6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b","seq":1},"interval_secs":15,"applications":[{"name":"or ders","health":"up","version":null,"gauges":{}}]}"#,
                Err(InvalidApplicationName),
            ),
            (
                "a repeated application name",
                r#"{"round":{"run":"6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b","seq":1},"interval_secs":15,"applications":[{"name":"a","health":"up","version":null,"gauges":{}},{"name":"a","health":"down","version":null,"gauges":{}}]}"#,
                Err(DuplicateApplicationName),
            ),
            (
                "17 applications",
                seventeen.as_str(),
                Err(TooManyApplications),
            ),
            (
                "a far-future scraped_at is ignored",
                r#"{"round":{"run":"6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b","seq":1},"interval_secs":15,"scraped_at":18446744073709551615,"applications":[]}"#,
                Ok(PolledRound::Round(
                    ScrapeRound::new(
                        RoundId::parse("6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b", 1).unwrap(),
                        ScrapeInterval::from_secs(15).unwrap(),
                        vec![],
                    )
                    .unwrap(),
                )),
            ),
        ];
        for (case, json, expected) in cases {
            assert_eq!(PolledRound::try_from(body(json)), expected, "case: {case}");
        }
    }
}
