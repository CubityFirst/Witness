//! Process-wide admission control for ML inference. Live captions get the
//! next available slot ahead of batch transcription so a background meeting
//! cannot continuously starve the active recording.

use std::sync::{Condvar, Mutex, OnceLock};

#[derive(Default)]
struct State {
    active: bool,
    live_waiters: usize,
    live_sessions: usize,
}

pub struct Scheduler {
    state: Mutex<State>,
    available: Condvar,
}

impl Scheduler {
    fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            available: Condvar::new(),
        }
    }

    fn acquire(&self, live: bool) -> Permit<'_> {
        let mut state = self.state.lock().unwrap();
        if live {
            state.live_waiters += 1;
        }
        while state.active || (!live && (state.live_waiters > 0 || state.live_sessions > 0)) {
            state = self.available.wait(state).unwrap();
        }
        if live {
            state.live_waiters -= 1;
        }
        state.active = true;
        Permit { scheduler: self }
    }

    fn begin_live_session(&self) -> LiveSession<'_> {
        let mut state = self.state.lock().unwrap();
        state.live_sessions += 1;
        self.available.notify_all();
        LiveSession { scheduler: self }
    }
}

pub struct Permit<'a> {
    scheduler: &'a Scheduler,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut state = self.scheduler.state.lock().unwrap();
        state.active = false;
        self.scheduler.available.notify_all();
    }
}

pub struct LiveSession<'a> {
    scheduler: &'a Scheduler,
}

impl Drop for LiveSession<'_> {
    fn drop(&mut self) {
        let mut state = self.scheduler.state.lock().unwrap();
        state.live_sessions -= 1;
        self.scheduler.available.notify_all();
    }
}

fn scheduler() -> &'static Scheduler {
    static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();
    SCHEDULER.get_or_init(Scheduler::new)
}

pub fn batch() -> Permit<'static> {
    scheduler().acquire(false)
}

pub fn live() -> Permit<'static> {
    scheduler().acquire(true)
}

/// Hold for the lifetime of a live-caption worker. Batch jobs pause between
/// inference calls until the active recording releases this guard.
pub fn live_session() -> LiveSession<'static> {
    scheduler().begin_live_session()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn live_waiter_gets_the_next_slot_before_batch() {
        let scheduler = Scheduler::new();
        let first = scheduler.acquire(false);
        let (tx, rx) = mpsc::channel();

        std::thread::scope(|scope| {
            let live_tx = tx.clone();
            let live_scheduler = &scheduler;
            scope.spawn(move || {
                let _permit = live_scheduler.acquire(true);
                live_tx.send("live").unwrap();
                std::thread::sleep(Duration::from_millis(20));
            });

            // Wait until the live waiter has registered before adding batch
            // work, making the priority assertion deterministic.
            loop {
                if scheduler.state.lock().unwrap().live_waiters == 1 {
                    break;
                }
                std::thread::yield_now();
            }

            let batch_tx = tx.clone();
            let batch_scheduler = &scheduler;
            scope.spawn(move || {
                let _permit = batch_scheduler.acquire(false);
                batch_tx.send("batch").unwrap();
            });

            drop(first);
            assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "live");
            assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), "batch");
        });
    }

    #[test]
    fn live_session_pauses_new_batch_work() {
        let scheduler = Scheduler::new();
        let session = scheduler.begin_live_session();
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let batch_scheduler = &scheduler;
            scope.spawn(move || {
                let _permit = batch_scheduler.acquire(false);
                tx.send(()).unwrap();
            });
            assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
            drop(session);
            rx.recv_timeout(Duration::from_secs(1)).unwrap();
        });
    }
}
