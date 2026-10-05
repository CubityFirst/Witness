//! The notification-area icon: menu (Open Witness / Record now ⇄ Stop
//! recording / status line / Quit), tooltip and the recording icon swap.
//! Created exactly once per process on the main thread, as the process's
//! first `TrayIcon` (uID 1, like 0.2.x), and never with a GUID (design §7).
//!
//! STUB (F0.2): the signatures are final; the shell unit (U1) builds the
//! real icon with `tray-icon`. Handlers only call `sink.shell(..)`.

use crate::bridge::UiSink;

pub struct Tray {
    _sink: UiSink,
    recording: bool,
    warning: Option<String>,
}

impl Tray {
    /// Create the icon and menu and route their events to `sink`.
    /// TODO(U1)
    pub fn build(sink: UiSink) -> anyhow::Result<Tray> {
        Ok(Tray {
            _sink: sink,
            recording: false,
            warning: None,
        })
    }

    /// Icon, tooltip and menu text for the recording state.
    pub fn set_recording(&mut self, recording: bool) {
        self.recording = recording;
        if !recording {
            self.warning = None;
        }
    }

    /// Capture warning appended to the recording tooltip (None clears it).
    pub fn set_warning(&mut self, warning: Option<String>) {
        self.warning = warning;
    }

    /// (owner HWND, uID) of the shell icon, for the panic hook's NIM_DELETE.
    /// (0, 0) while no icon exists.
    pub fn notify_ids(&self) -> (isize, u32) {
        (0, 0)
    }
}
