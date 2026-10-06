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

//! The span handoff (RFC 0010 §6): the writer chooses the closed spans at each sweep and hands
//! them, after the commit that sealed them, to the block writer thread, which reads the span's
//! chunks in a read transaction, writes its file durably and sends the outcome back; the writer
//! then inserts the `blocks` row and deletes the span's chunks in one commit.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::JoinHandle;

use redb::ReadableDatabase;

use super::blocks::{SpanKey, span_range};
use super::writer::Request;
use super::{HandoffFailure, Shared};
use crate::block::file::WriteError;
use crate::block::format::{
    BLOCK_FORMAT_V1, BlockSummary, ChunkRef, HEADER_LEN, TRAILER_LEN, encode_block, read_trailer,
};
use crate::block::name::{BlockName, Rewrite};
use crate::block::record::BlockRecord;
use crate::series::SeriesId;
use crate::tables::CHUNKS;
use crate::tier::Tier;

/// How many spans of one tier may wait on a failing handoff before the store fails stop
/// (§6): `hub.redb` would otherwise grow with no bound while the hub runs.
pub(crate) const BACKLOG_BOUND: usize = 3;

/// What the block writer sends back for one span.
pub(crate) enum Outcome {
    Written {
        name: BlockName,
        record: BlockRecord,
        summary: BlockSummary,
    },
    Failed(BlockName, HandoffFailure),
}

/// More than [`BACKLOG_BOUND`] spans of a tier wait on a failing handoff: the tier, and why
/// its last attempt failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Backlog {
    pub tier: Tier,
    pub last: HandoffFailure,
}

/// The writer's view of the spans to hand off. Pure bookkeeping: the clock is passed in.
#[derive(Debug, Default)]
pub(crate) struct Handoffs {
    /// Spans with chunks in `chunks`.
    waiting: BTreeSet<SpanKey>,
    /// Handed to the block writer, outcome not yet committed.
    in_flight: BTreeSet<SpanKey>,
    /// Waiting after a failed attempt: when (in failure order) and why its last one failed.
    failed: BTreeMap<SpanKey, (u64, HandoffFailure)>,
    /// Failures recorded so far: the order of the next one.
    failures: u64,
    /// Chosen at the last sweep: handed over after the next commit.
    due: BTreeSet<SpanKey>,
}

impl Handoffs {
    pub(crate) fn new(waiting: BTreeSet<SpanKey>) -> Handoffs {
        Handoffs {
            waiting,
            ..Handoffs::default()
        }
    }

    /// A chunk of the span went into `chunks`.
    pub(crate) fn sealed(&mut self, span: SpanKey) {
        self.waiting.insert(span);
    }

    /// At a sweep: the closed spans not yet in flight become due; too many failing spans of a
    /// tier are a [`Backlog`].
    pub(crate) fn choose(&mut self, hub_now: u64) -> Result<(), Backlog> {
        if let Some(backlog) = Tier::ALL.into_iter().find_map(|tier| self.backlog(tier)) {
            return Err(backlog);
        }
        let closed = self
            .waiting
            .iter()
            .filter(|(tier, span)| span.is_closed(*tier, hub_now))
            .filter(|span| !self.in_flight.contains(span));
        self.due.extend(closed);
        Ok(())
    }

    /// After a commit: the due spans, now in flight.
    pub(crate) fn take_due(&mut self) -> Vec<SpanKey> {
        let due = std::mem::take(&mut self.due);
        self.in_flight.extend(due.iter().copied());
        due.into_iter().collect()
    }

    /// The span's file and row committed: its chunks are gone from `chunks`.
    pub(crate) fn landed(&mut self, span: SpanKey) {
        self.waiting.remove(&span);
        self.in_flight.remove(&span);
        self.failed.remove(&span);
    }

    /// The attempt failed: the span waits for the next sweep.
    pub(crate) fn failed(&mut self, span: SpanKey, cause: HandoffFailure) {
        self.in_flight.remove(&span);
        self.failed.insert(span, (self.failures, cause));
        self.failures += 1;
    }

    /// The tier's backlog, if more than [`BACKLOG_BOUND`] of its spans wait on failures: named
    /// by the latest of them. A failed span counts until it lands, retried or not; a span in
    /// flight that never failed doesn't: a block writer that hangs on its file system hangs
    /// redb's commits on the same volume too, which fails the store anyway.
    fn backlog(&self, tier: Tier) -> Option<Backlog> {
        let failing = self.failed.iter().filter(|((t, _), _)| *t == tier);
        let count = failing.clone().count();
        let (_, &(_, last)) = failing.max_by_key(|(_, (order, _))| *order)?;
        (count > BACKLOG_BOUND).then_some(Backlog { tier, last })
    }
}

/// Starts the block writer: it serves jobs until the writer drops their sender.
pub(crate) fn spawn<C: Send + 'static>(
    shared: Arc<Shared<C>>,
    requests: SyncSender<Request<C>>,
) -> std::io::Result<(Sender<BlockName>, JoinHandle<()>)> {
    let (jobs, inbox) = mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("hub-store-blocks".into())
        .spawn(move || run(&shared, &inbox, &requests))?;
    Ok((jobs, handle))
}

fn run<C>(shared: &Shared<C>, inbox: &Receiver<BlockName>, requests: &SyncSender<Request<C>>) {
    for name in inbox {
        let outcome = match write_span(shared, name) {
            Ok((record, summary)) => Outcome::Written {
                name,
                record,
                summary,
            },
            Err(cause) => Outcome::Failed(name, cause),
        };
        if requests.send(Request::HandOff(outcome)).is_err() {
            return;
        }
    }
}

/// The span's file, written from a read transaction taken after the commit that sealed it.
/// Everything that can fail before the file exists is done first, so a failure leaves no file.
fn write_span<C>(
    shared: &Shared<C>,
    name: BlockName,
) -> Result<(BlockRecord, BlockSummary), HandoffFailure> {
    let held = read_span(shared, name).map_err(|_| HandoffFailure::Read)?;
    let refs: Vec<ChunkRef<'_>> = held
        .iter()
        .map(|(id, seq, bytes)| ChunkRef {
            id: *id,
            seq: *seq,
            bytes,
        })
        .collect();
    let bytes = encode_block(name.tier, name.span, &refs).map_err(|_| HandoffFailure::Encode)?;
    let summary = summary_of(name, &bytes).ok_or(HandoffFailure::Encode)?;
    shared
        .blocks
        .dir()
        .write(&name, &bytes)
        .map_err(|e| match e {
            WriteError::Exists => HandoffFailure::Exists,
            WriteError::Io(kind) => HandoffFailure::Write(kind),
        })?;
    let record = BlockRecord {
        format: BLOCK_FORMAT_V1,
        rewrite: name.rewrite,
        length: bytes.len() as u64,
        chunk_count: summary.chunk_count(),
        tombstoned: 0,
    };
    Ok((record, summary))
}

fn read_span<C>(
    shared: &Shared<C>,
    name: BlockName,
) -> Result<Vec<(SeriesId, u16, Vec<u8>)>, redb::Error> {
    let read = shared.db.begin_read()?;
    let table = read.open_table(CHUNKS)?;
    let mut held = Vec::new();
    for entry in table.range(span_range(name.tier, name.span))? {
        let (key, bytes) = entry?;
        let (_, _, id, seq) = key.value();
        held.push((SeriesId(id), seq, bytes.value().to_vec()));
    }
    Ok(held)
}

/// The summary of a file about to be written, from its bytes.
fn summary_of(name: BlockName, bytes: &[u8]) -> Option<BlockSummary> {
    let len = bytes.len();
    let trailer = bytes.get(len.checked_sub(TRAILER_LEN)?..)?;
    let offset = usize::try_from(read_trailer(trailer).ok()?).ok()?;
    BlockSummary::parse(
        name.tier,
        name.span,
        bytes.get(..HEADER_LEN)?,
        bytes.get(offset..)?,
        len as u64,
    )
    .ok()
}

/// The handoff's file name for a span: rewrite 0.
pub(crate) fn handoff_name((tier, span): SpanKey) -> BlockName {
    BlockName {
        tier,
        span,
        rewrite: Rewrite(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::SpanStart;

    const T0: u64 = 1_800_057_600;
    const HOUR: u64 = 3_600;
    const DAY: u64 = 86_400;

    fn span(tier: Tier, at: u64) -> SpanKey {
        (tier, SpanStart::new(tier, at).expect("grid"))
    }

    fn raw(i: u64) -> SpanKey {
        span(Tier::Raw, T0 + i * HOUR)
    }

    fn minute(i: u64) -> SpanKey {
        span(Tier::Minute, T0 + i * HOUR)
    }

    /// One step of a script, with what it must answer.
    enum Step {
        Seal(SpanKey),
        Choose(u64, Result<(), Backlog>),
        Take(Vec<SpanKey>),
        Fail(SpanKey, HandoffFailure),
        Land(SpanKey),
    }
    use Step::{Choose, Fail, Land, Seal, Take};

    /// Long after every span below has closed.
    const LATE: u64 = T0 + 30 * DAY;

    fn backlog(tier: Tier, last: HandoffFailure) -> Result<(), Backlog> {
        Err(Backlog { tier, last })
    }

    /// Seals, chooses and takes the given spans: they are in flight.
    fn in_flight(spans: &[SpanKey]) -> Vec<Step> {
        let mut steps: Vec<Step> = spans.iter().map(|s| Seal(*s)).collect();
        steps.push(Choose(LATE, Ok(())));
        steps.push(Take(spans.to_vec()));
        steps
    }

    fn script(mut head: Vec<Step>, tail: Vec<Step>) -> Vec<Step> {
        head.extend(tail);
        head
    }

    #[test]
    fn handoffs_choose_closed_spans_once_and_name_a_tiers_backlog_by_its_last_failure() {
        let write = HandoffFailure::Write(std::io::ErrorKind::Other);
        let cases: Vec<(&str, Vec<Step>)> = vec![
            (
                "a raw span is due only past its end plus its grace",
                vec![
                    Seal(raw(0)),
                    Choose(T0 + HOUR + 120, Ok(())),
                    Take(vec![]),
                    Choose(T0 + HOUR + 121, Ok(())),
                    Take(vec![raw(0)]),
                ],
            ),
            (
                "an hour span is due a day, an hour bucket and a sweep after its start",
                vec![
                    Seal(span(Tier::Hour, T0)),
                    Choose(T0 + DAY + HOUR + 60, Ok(())),
                    Take(vec![]),
                    Choose(T0 + DAY + HOUR + 61, Ok(())),
                    Take(vec![span(Tier::Hour, T0)]),
                ],
            ),
            (
                "a span in flight isn't chosen again; a failed one is; a landed one is done",
                script(
                    in_flight(&[raw(0)]),
                    vec![
                        Choose(LATE, Ok(())),
                        Take(vec![]),
                        Fail(raw(0), HandoffFailure::Read),
                        Choose(LATE, Ok(())),
                        Take(vec![raw(0)]),
                        Land(raw(0)),
                        Choose(LATE, Ok(())),
                        Take(vec![]),
                    ],
                ),
            ),
            (
                "three failing spans are within the bound; a fourth names the last cause",
                script(
                    in_flight(&[raw(0), raw(1), raw(2), raw(3)]),
                    vec![
                        Fail(raw(0), HandoffFailure::Read),
                        Fail(raw(1), HandoffFailure::Read),
                        Fail(raw(2), HandoffFailure::Exists),
                        Choose(LATE, Ok(())),
                        Fail(raw(3), write),
                        Choose(LATE, backlog(Tier::Raw, write)),
                    ],
                ),
            ),
            (
                "the last failure, not the first or the latest span's",
                script(
                    in_flight(&[raw(0), raw(1), raw(2), raw(3)]),
                    vec![
                        Fail(raw(3), HandoffFailure::Exists),
                        Fail(raw(1), HandoffFailure::Exists),
                        Fail(raw(2), HandoffFailure::Exists),
                        Fail(raw(0), HandoffFailure::Encode),
                        Choose(LATE, backlog(Tier::Raw, HandoffFailure::Encode)),
                    ],
                ),
            ),
            (
                "a span that failed then landed no longer counts",
                script(
                    in_flight(&[raw(0), raw(1), raw(2), raw(3)]),
                    vec![
                        Fail(raw(0), HandoffFailure::Read),
                        Fail(raw(1), HandoffFailure::Read),
                        Fail(raw(2), HandoffFailure::Read),
                        Fail(raw(3), HandoffFailure::Read),
                        Land(raw(0)),
                        Choose(LATE, Ok(())),
                    ],
                ),
            ),
            (
                "tiers are counted apart, and a minute backlog names the minute tier",
                script(
                    in_flight(&[raw(0), raw(1), raw(2), minute(0), minute(1), minute(2)]),
                    vec![
                        Fail(raw(0), HandoffFailure::Read),
                        Fail(raw(1), HandoffFailure::Read),
                        Fail(raw(2), HandoffFailure::Read),
                        Fail(minute(0), write),
                        Fail(minute(1), write),
                        Fail(minute(2), write),
                        Choose(LATE, Ok(())),
                        Seal(minute(3)),
                        Choose(LATE, Ok(())),
                        Take(vec![
                            raw(0),
                            raw(1),
                            raw(2),
                            minute(0),
                            minute(1),
                            minute(2),
                            minute(3),
                        ]),
                        Fail(minute(3), HandoffFailure::Exists),
                        Choose(LATE, backlog(Tier::Minute, HandoffFailure::Exists)),
                    ],
                ),
            ),
        ];
        for (name, steps) in cases {
            let mut h = Handoffs::default();
            for (i, step) in steps.into_iter().enumerate() {
                match step {
                    Seal(s) => h.sealed(s),
                    Choose(now, expected) => {
                        assert_eq!(h.choose(now), expected, "{name}: step {i}")
                    }
                    Take(expected) => assert_eq!(h.take_due(), expected, "{name}: step {i}"),
                    Fail(s, cause) => h.failed(s, cause),
                    Land(s) => h.landed(s),
                }
            }
        }
    }
}
