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

//! The Maildir reader (RFC 0017 §6): every scan takes at most 256 messages from `new/`,
//! oldest first, ingests each and deletes it, accepted or refused, so a mailbox anyone can
//! write to can't fill the hub's disk. Blocking: runs on the blocking pool.

use std::path::Path;

use super::ingest::{Ingested, MessageRefusal, ingest_message};
use super::seal::MailMasterKey;
use crate::state::AppState;

/// The most messages one scan takes.
pub const MAX_MESSAGES_PER_SCAN: usize = 256;
/// The largest message the hub reads; a larger one is deleted unread.
pub const MAX_MESSAGE_BYTES: u64 = 1024 * 1024;

/// What one scan did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanPass {
    pub stored: usize,
    pub duplicates: usize,
    /// Refused messages, by reason, in the order first seen.
    pub refused: Vec<(Refused, usize)>,
    /// Files that couldn't be read or deleted: left for the next scan.
    pub stuck: usize,
}

/// Why a message was deleted without being stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    TooLarge,
    Message(MessageRefusal),
}

/// Scans `maildir/new` once at `now` (unix seconds).
pub fn scan_once(app: &AppState, key: &MailMasterKey, maildir: &Path, now: u64) -> ScanPass {
    let mut pass = ScanPass::default();
    let names = match oldest_messages(&maildir.join("new")) {
        Ok(names) => names,
        Err(err) => {
            tracing::warn!("The mail intake couldn't list its Maildir: {err}");
            return pass;
        }
    };
    for path in names {
        match handle(app, key, &path, now) {
            Ok(outcome) => pass.note(outcome),
            Err(err) => {
                tracing::warn!(
                    "The mail intake couldn't handle {:?}: {err}",
                    path.file_name()
                );
                pass.stuck += 1;
                continue;
            }
        }
        if let Err(err) = std::fs::remove_file(&path) {
            tracing::warn!(
                "The mail intake couldn't delete {:?}: {err}",
                path.file_name()
            );
            pass.stuck += 1;
        }
    }
    pass
}

/// The oldest `MAX_MESSAGES_PER_SCAN` files in `new`, by name (Maildir names start with the
/// delivery time).
fn oldest_messages(new: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(new)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .collect();
    names.sort();
    names.truncate(MAX_MESSAGES_PER_SCAN);
    Ok(names)
}

/// What became of one message.
enum Outcome {
    Ingested(Ingested),
    Refused(Refused),
}

/// Reads and ingests one message; a file over the cap is refused unread.
fn handle(app: &AppState, key: &MailMasterKey, path: &Path, now: u64) -> std::io::Result<Outcome> {
    if std::fs::metadata(path)?.len() > MAX_MESSAGE_BYTES {
        return Ok(Outcome::Refused(Refused::TooLarge));
    }
    let raw = std::fs::read(path)?;
    Ok(match ingest_message(app, key, &raw, now) {
        Ok(ingested) => Outcome::Ingested(ingested),
        Err(refusal) => Outcome::Refused(Refused::Message(refusal)),
    })
}

impl ScanPass {
    fn note(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Ingested(Ingested::Stored { .. }) => self.stored += 1,
            Outcome::Ingested(Ingested::Duplicate) => self.duplicates += 1,
            Outcome::Refused(reason) => match self.refused.iter_mut().find(|(r, _)| *r == reason) {
                Some((_, count)) => *count += 1,
                None => self.refused.push((reason, 1)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::mail_intake::ingest::tests::{NOW, master, valid_mail};
    use std::sync::Arc;

    fn app() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Arc::new(Database::new(path.to_str().unwrap()).unwrap());
        (AppState::new(db).unwrap(), dir)
    }

    fn maildir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["new", "cur", "tmp"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
        }
        dir
    }

    fn deliver(maildir: &Path, name: &str, bytes: &[u8]) {
        std::fs::write(maildir.join("new").join(name), bytes).unwrap();
    }

    fn left_in_new(maildir: &Path) -> usize {
        std::fs::read_dir(maildir.join("new")).unwrap().count()
    }

    /// RFC 0017 §6: an accepted message is stored and deleted; a refused one is deleted and
    /// counted by reason; one over 1 MiB is deleted unread.
    #[test]
    fn every_message_is_deleted_once_handled() {
        let (app, _db) = app();
        let dir = maildir();
        deliver(dir.path(), "1.a", &valid_mail("web-01", 1));
        deliver(dir.path(), "2.b", b"Subject: hello\r\n\r\nspam\r\n");
        deliver(
            dir.path(),
            "3.c",
            &vec![b'x'; MAX_MESSAGE_BYTES as usize + 1],
        );

        let pass = scan_once(&app, &master(), dir.path(), NOW);

        assert_eq!(pass.stored, 1);
        assert_eq!(
            pass.refused,
            [
                (
                    Refused::Message(MessageRefusal::Open(
                        crate::mail_intake::seal::OpenRefusal::BadArmor
                    )),
                    1
                ),
                (Refused::TooLarge, 1),
            ]
        );
        assert_eq!(left_in_new(dir.path()), 0, "every message deleted");
        assert!(app.db.get_system("web-01").unwrap().is_some());
    }

    /// RFC 0017 §6: a scan takes at most 256 messages, oldest name first.
    #[test]
    fn a_scan_takes_at_most_256_messages_oldest_first() {
        let (app, _db) = app();
        let dir = maildir();
        for n in 0..=MAX_MESSAGES_PER_SCAN {
            deliver(dir.path(), &format!("{n:04}.x"), b"Subject: x\r\n\r\nx\r\n");
        }

        let first = scan_once(&app, &master(), dir.path(), NOW);

        assert_eq!(
            first.refused.iter().map(|(_, n)| n).sum::<usize>(),
            MAX_MESSAGES_PER_SCAN
        );
        let left: Vec<String> = std::fs::read_dir(dir.path().join("new"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            left,
            [format!("{:04}.x", MAX_MESSAGES_PER_SCAN)],
            "the newest is left"
        );
        scan_once(&app, &master(), dir.path(), NOW);
        assert_eq!(left_in_new(dir.path()), 0);
    }

    /// RFC 0017 §6: a backlog delivered at once is stored whole: no report pace.
    #[test]
    fn a_backlog_is_stored_whole() {
        let (app, _db) = app();
        let dir = maildir();
        for seq in 1..=96 {
            deliver(
                dir.path(),
                &format!("{seq:04}.m"),
                &valid_mail("web-01", seq),
            );
        }

        let pass = scan_once(&app, &master(), dir.path(), NOW);

        assert_eq!(pass.stored, 96);
    }
}
