//! The only notes write path from the UI: one ordered "notes-writer"
//! thread with latest-wins drafts per meeting.
//!
//! The UI enqueues the newest draft on every debounce, view change, hide and
//! exit. `pending` holds at most one draft per meeting — a newer draft
//! replaces a queued older one — and the single thread takes a draft out
//! before writing it, so writes for one meeting happen in seq order and an
//! in-flight write can never land after a newer one.
//!
//! Durability: `AppCore::flush_notes` (called on every exit path) is the
//! only guarantee that queued drafts reach the database. The thread reaches
//! the core through a `Weak`, and the writer is dropped together with the
//! core, so drafts still queued when the core itself is dropped are logged
//! and discarded, not written.

use super::{AppCore, CoreEvent, CoreResult};
use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

/// Where drafts go. `AppCore` is the real one; tests use a fake.
pub(crate) trait NotesTarget: Send + Sync + 'static {
    fn write_notes(&self, meeting_id: i64, text: &str) -> CoreResult<()>;
    fn emit(&self, event: CoreEvent);
}

impl NotesTarget for AppCore {
    fn write_notes(&self, meeting_id: i64, text: &str) -> CoreResult<()> {
        self.set_meeting_notes(meeting_id, text)
    }
    fn emit(&self, event: CoreEvent) {
        AppCore::emit(self, event);
    }
}

#[derive(Default)]
struct NotesQueue {
    /// meeting_id → (seq, newest draft). The lowest meeting id is served first.
    pending: BTreeMap<i64, (u64, String)>,
    /// A draft has been taken out and is being written.
    busy: bool,
    /// The writer handle is gone: drain `pending`, then exit.
    closed: bool,
    last_seq: u64,
}

struct Shared {
    queue: Mutex<NotesQueue>,
    /// Signalled on every enqueue, completion and close.
    changed: Condvar,
}

pub(crate) struct NotesWriter {
    shared: Arc<Shared>,
}

impl NotesWriter {
    /// Start the "notes-writer" thread. Once the writer is dropped the thread
    /// drains what is queued — writing it only while `target` is still alive
    /// (never the case for `AppCore`, see the module docs) — then exits.
    pub(crate) fn spawn<T: NotesTarget>(target: Weak<T>) -> NotesWriter {
        let shared = Arc::new(Shared {
            queue: Mutex::new(NotesQueue::default()),
            changed: Condvar::new(),
        });
        let worker = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("notes-writer".into())
            .spawn(move || run(&worker, &target));
        if let Err(error) = spawned {
            log::error!("could not start the notes writer: {error}");
        }
        NotesWriter { shared }
    }

    /// Enqueue the newest draft; returns its seq (monotonic across meetings).
    pub(crate) fn queue(&self, meeting_id: i64, text: String) -> u64 {
        let mut queue = self.shared.queue.lock().unwrap();
        queue.last_seq += 1;
        let seq = queue.last_seq;
        queue.pending.insert(meeting_id, (seq, text));
        drop(queue);
        self.shared.changed.notify_all();
        seq
    }

    /// Wait until nothing is queued or being written, up to `timeout`.
    pub(crate) fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut queue = self.shared.queue.lock().unwrap();
        while !queue.pending.is_empty() || queue.busy {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            queue = self
                .shared
                .changed
                .wait_timeout(queue, deadline - now)
                .unwrap()
                .0;
        }
        true
    }
}

impl Drop for NotesWriter {
    fn drop(&mut self) {
        self.shared.queue.lock().unwrap().closed = true;
        self.shared.changed.notify_all();
    }
}

fn run<T: NotesTarget>(shared: &Shared, target: &Weak<T>) {
    loop {
        let (meeting_id, seq, text) = {
            let mut queue = shared.queue.lock().unwrap();
            loop {
                if let Some((meeting_id, (seq, text))) = queue.pending.pop_first() {
                    queue.busy = true;
                    break (meeting_id, seq, text);
                }
                if queue.closed {
                    return;
                }
                queue = shared.changed.wait(queue).unwrap();
            }
        };

        match target.upgrade() {
            Some(target) => {
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    target.write_notes(meeting_id, &text)
                }))
                .unwrap_or_else(|panic| {
                    Err(format!(
                        "internal error: {}",
                        super::rec_control::panic_message(&panic)
                    ))
                });
                // Emit before clearing `busy`: a successful flush() means
                // every outcome has been reported too.
                target.emit(match result {
                    Ok(()) => CoreEvent::NotesSaved { meeting_id, seq },
                    Err(error) => {
                        log::warn!("saving notes for meeting {meeting_id} failed: {error}");
                        CoreEvent::NotesSaveFailed {
                            meeting_id,
                            seq,
                            error,
                        }
                    }
                });
            }
            None => log::warn!("notes for meeting {meeting_id} dropped: the core is gone"),
        }

        shared.queue.lock().unwrap().busy = false;
        shared.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::testing::CollectSink;
    use crate::service::EventSink;
    use crossbeam_channel::{Receiver, Sender};

    /// Records writes; every write first announces itself on `entered`, then
    /// waits for `hold` so a test can keep a write in flight.
    struct FakeDb {
        writes: Mutex<Vec<(i64, String)>>,
        hold: Mutex<()>,
        entered: Sender<(i64, String)>,
        fail_meeting: Option<i64>,
        sink: CollectSink,
    }

    fn fake(fail_meeting: Option<i64>) -> (Arc<FakeDb>, Receiver<(i64, String)>) {
        let (entered, entered_rx) = crossbeam_channel::unbounded();
        let db = Arc::new(FakeDb {
            writes: Mutex::new(Vec::new()),
            hold: Mutex::new(()),
            entered,
            fail_meeting,
            sink: CollectSink::default(),
        });
        (db, entered_rx)
    }

    impl NotesTarget for FakeDb {
        fn write_notes(&self, meeting_id: i64, text: &str) -> CoreResult<()> {
            let _ = self.entered.send((meeting_id, text.to_string()));
            let _hold = self.hold.lock().unwrap();
            if self.fail_meeting == Some(meeting_id) {
                return Err("disk full".into());
            }
            self.writes
                .lock()
                .unwrap()
                .push((meeting_id, text.to_string()));
            Ok(())
        }
        fn emit(&self, event: CoreEvent) {
            self.sink.emit(event);
        }
    }

    fn acks(sink: &CollectSink) -> Vec<(i64, u64, bool)> {
        sink.events()
            .into_iter()
            .filter_map(|event| match event {
                CoreEvent::NotesSaved { meeting_id, seq } => Some((meeting_id, seq, true)),
                CoreEvent::NotesSaveFailed {
                    meeting_id, seq, ..
                } => Some((meeting_id, seq, false)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn latest_draft_wins_and_never_lands_before_an_older_one() {
        let (db, entered) = fake(None);
        let writer = NotesWriter::spawn(Arc::downgrade(&db));

        let hold = db.hold.lock().unwrap();
        let first = writer.queue(1, "a".into());
        // The first draft is in flight (taken out of the queue, blocked).
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(5)).unwrap(),
            (1, "a".into())
        );
        let skipped = writer.queue(1, "ab".into());
        let newest = writer.queue(1, "abc".into());
        assert!(first < skipped && skipped < newest);
        drop(hold);

        assert!(writer.flush(Duration::from_secs(5)));
        assert_eq!(
            *db.writes.lock().unwrap(),
            [(1, "a".to_string()), (1, "abc".to_string())]
        );
        assert_eq!(acks(&db.sink), [(1, first, true), (1, newest, true)]);
    }

    #[test]
    fn meetings_are_served_lowest_id_first_one_draft_each() {
        let (db, entered) = fake(None);
        let writer = NotesWriter::spawn(Arc::downgrade(&db));

        let hold = db.hold.lock().unwrap();
        writer.queue(9, "blocker".into());
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        writer.queue(5, "five-old".into());
        writer.queue(3, "three".into());
        writer.queue(5, "five-new".into());
        drop(hold);

        assert!(writer.flush(Duration::from_secs(5)));
        let writes: Vec<_> = db.writes.lock().unwrap().clone();
        assert_eq!(
            writes,
            [
                (9, "blocker".to_string()),
                (3, "three".to_string()),
                (5, "five-new".to_string())
            ]
        );
    }

    #[test]
    fn flush_waits_for_the_in_flight_write_and_times_out_while_blocked() {
        let (db, entered) = fake(None);
        let writer = NotesWriter::spawn(Arc::downgrade(&db));

        let hold = db.hold.lock().unwrap();
        writer.queue(1, "draft".into());
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        // Blocked write: flush must report the timeout, not pretend.
        assert!(!writer.flush(Duration::from_millis(50)));
        assert!(db.writes.lock().unwrap().is_empty());

        std::thread::scope(|scope| {
            let flushing = scope.spawn(|| writer.flush(Duration::from_secs(5)));
            std::thread::sleep(Duration::from_millis(100));
            assert!(!flushing.is_finished(), "flush returned during a write");
            drop(hold);
            assert!(flushing.join().unwrap());
        });
        // flush returned only after the write and its acknowledgement.
        assert_eq!(db.writes.lock().unwrap().len(), 1);
        assert_eq!(acks(&db.sink).len(), 1);
        // Idle writer: flush returns at once.
        assert!(writer.flush(Duration::ZERO));
    }

    #[test]
    fn failures_are_reported_with_their_seq() {
        let (db, _entered) = fake(Some(2));
        let writer = NotesWriter::spawn(Arc::downgrade(&db));
        let seq = writer.queue(2, "lost".into());
        assert!(writer.flush(Duration::from_secs(5)));
        assert_eq!(acks(&db.sink), [(2, seq, false)]);
        assert!(db.sink.events().iter().any(|event| matches!(
            event,
            CoreEvent::NotesSaveFailed { error, .. } if error == "disk full"
        )));
    }

    /// Writer semantics only: the fake target outlives the writer here. With
    /// the real core both go together, so `flush_notes` is what exits rely on.
    #[test]
    fn dropped_writer_drains_queue_while_target_lives() {
        let (db, _entered) = fake(None);
        let writer = NotesWriter::spawn(Arc::downgrade(&db));
        writer.queue(4, "last words".into());
        drop(writer);
        let deadline = Instant::now() + Duration::from_secs(5);
        while db.writes.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*db.writes.lock().unwrap(), [(4, "last words".to_string())]);
    }
}
