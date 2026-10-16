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

//! Where the store is in its life (RFC 0010 §6): running, closed (a clean stop) or failed (a
//! fail-stop). One state, so a clean stop is never reported as a failure, nor a failure as a
//! clean stop.

use std::sync::atomic::{AtomicU8, Ordering};

use super::StoreError;

/// The store's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Running,
    /// `close` flushed and committed: every later call answers `Closed`.
    Closed,
    /// The writer failed: every later call answers `Failed`.
    Failed,
}

/// The state, shared by the writer and every caller.
pub(crate) struct Lifecycle(AtomicU8);

const RUNNING: u8 = 0;
const CLOSED: u8 = 1;
const FAILED: u8 = 2;

impl Lifecycle {
    pub(crate) fn new() -> Lifecycle {
        Lifecycle(AtomicU8::new(RUNNING))
    }

    pub(crate) fn state(&self) -> State {
        match self.0.load(Ordering::SeqCst) {
            RUNNING => State::Running,
            CLOSED => State::Closed,
            // Only the three constants are ever stored.
            _ => State::Failed,
        }
    }

    /// `Ok` while running, else the error every call answers.
    pub(crate) fn check(&self) -> Result<(), StoreError> {
        match self.state() {
            State::Running => Ok(()),
            State::Closed => Err(StoreError::Closed),
            State::Failed => Err(StoreError::Failed),
        }
    }

    /// The writer failed: from any state, since a failure is never hidden.
    pub(crate) fn fail(&self) {
        self.0.store(FAILED, Ordering::SeqCst);
    }

    /// The writer's close committed: a running store is closed. (A compare-and-swap, so a
    /// failure latched meanwhile is never overwritten.)
    pub(crate) fn close(&self) {
        let _ = self
            .0
            .compare_exchange(RUNNING, CLOSED, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// A caller's reply was dropped unanswered: the writer is gone. Unless it stopped
    /// cleanly, that is a failure, latched here so every later call sees it at once.
    pub(crate) fn writer_gone(&self) -> StoreError {
        let _ = self
            .0
            .compare_exchange(RUNNING, FAILED, Ordering::SeqCst, Ordering::SeqCst);
        self.check().err().unwrap_or(StoreError::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug)]
    enum Event {
        Fail,
        Close,
        WriterGone,
    }

    fn at(state: State) -> Lifecycle {
        let lifecycle = Lifecycle::new();
        match state {
            State::Running => {}
            State::Closed => lifecycle.close(),
            State::Failed => lifecycle.fail(),
        }
        lifecycle
    }

    fn answer(state: State) -> Result<(), StoreError> {
        match state {
            State::Running => Ok(()),
            State::Closed => Err(StoreError::Closed),
            State::Failed => Err(StoreError::Failed),
        }
    }

    #[test]
    fn a_new_store_runs() {
        let lifecycle = Lifecycle::new();
        assert_eq!(lifecycle.state(), State::Running);
        assert_eq!(lifecycle.check(), Ok(()));
    }

    #[test]
    fn a_clean_stop_and_a_failure_each_keep_their_own_answer() {
        // (start, event, state after, what writer_gone answers when it is the event)
        let cases = [
            (State::Running, Event::Fail, State::Failed, None),
            (State::Running, Event::Close, State::Closed, None),
            (
                State::Running,
                Event::WriterGone,
                State::Failed,
                Some(StoreError::Failed),
            ),
            (State::Closed, Event::Fail, State::Failed, None),
            (State::Closed, Event::Close, State::Closed, None),
            (
                State::Closed,
                Event::WriterGone,
                State::Closed,
                Some(StoreError::Closed),
            ),
            (State::Failed, Event::Fail, State::Failed, None),
            (State::Failed, Event::Close, State::Failed, None),
            (
                State::Failed,
                Event::WriterGone,
                State::Failed,
                Some(StoreError::Failed),
            ),
        ];
        for (start, event, end, gone) in cases {
            let lifecycle = at(start);
            assert_eq!(lifecycle.state(), start, "{start:?} set up");
            let answered = match event {
                Event::Fail => {
                    lifecycle.fail();
                    None
                }
                Event::Close => {
                    lifecycle.close();
                    None
                }
                Event::WriterGone => Some(lifecycle.writer_gone()),
            };
            assert_eq!(answered, gone, "{start:?} then {event:?}: the answer");
            assert_eq!(lifecycle.state(), end, "{start:?} then {event:?}");
            assert_eq!(
                lifecycle.check(),
                answer(end),
                "{start:?} then {event:?}: check"
            );
        }
    }
}
