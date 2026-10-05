//! Tiny `EventSink` / `Notifier` implementations for unit tests (here and
//! in the bin crate). Always compiled; they cost nothing.

use super::{CoreEvent, EventSink, Notifier};
use std::sync::Mutex;

/// Collects every emitted event in order.
#[derive(Debug, Default)]
pub struct CollectSink(pub Mutex<Vec<CoreEvent>>);

impl CollectSink {
    /// A snapshot of everything emitted so far.
    pub fn events(&self) -> Vec<CoreEvent> {
        self.0.lock().unwrap().clone()
    }
}

impl EventSink for CollectSink {
    fn emit(&self, event: CoreEvent) {
        self.0.lock().unwrap().push(event);
    }
}

/// Discards toasts.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullNotifier;

impl Notifier for NullNotifier {
    fn toast(&self, _body: &str) {}
}
