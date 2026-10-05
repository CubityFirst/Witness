//! Global hotkeys with candidate chains, because other apps can own a combo
//! system-wide: record ctrl+alt+r → ctrl+alt+w → ctrl+alt+shift+r, bookmark
//! ctrl+alt+b → ctrl+alt+m. Registered on the main thread (winit pumps the
//! hotkey window); `Pressed` events only call `sink.shell(..)`.
//!
//! STUB (F0.2): the signatures are final; the shell unit (U1) registers the
//! real hotkeys with `global-hotkey`.

use crate::bridge::UiSink;

pub struct Hotkeys {
    _sink: UiSink,
    record: Option<String>,
    bookmark: Option<String>,
}

impl Hotkeys {
    /// Bind the first free combo of each chain. TODO(U1)
    pub fn register(sink: UiSink) -> Hotkeys {
        Hotkeys {
            _sink: sink,
            record: None,
            bookmark: None,
        }
    }

    /// The record combo that bound (e.g. "ctrl+alt+r"), None if all taken.
    pub fn record_combo(&self) -> Option<String> {
        self.record.clone()
    }

    /// The bookmark combo that bound, None if all taken.
    pub fn bookmark_combo(&self) -> Option<String> {
        self.bookmark.clone()
    }
}
