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

/// A real [`AppCore`](super::AppCore) on a fresh scratch data dir under
/// %TEMP% (auto-record and the update check off, so nothing touches real
/// devices or the network). Returns the core, its sink and the data dir.
/// The dir is left behind: the core keeps its database open for the life
/// of the test process.
#[cfg(test)]
pub(crate) fn scratch_core(
    tag: &str,
) -> (
    std::sync::Arc<super::AppCore>,
    std::sync::Arc<CollectSink>,
    std::path::PathBuf,
) {
    use std::sync::Arc;
    let dir = std::env::temp_dir().join(format!(
        "witness-core-{tag}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let settings = crate::settings::Settings {
        data_dir: Some(dir.display().to_string()),
        auto_record: false,
        check_for_updates: false,
        ..crate::settings::Settings::default()
    };
    let sink = Arc::new(CollectSink::default());
    let core =
        super::AppCore::start(settings, sink.clone(), Arc::new(NullNotifier)).expect("core starts");
    (core, sink, dir)
}
