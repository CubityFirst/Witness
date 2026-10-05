//! One Witness per user. The names are identical to
//! tauri-plugin-single-instance's, so the native app and a lingering 0.2.x
//! process recognise each other (design §4.4): mutex
//! `com.github.cubityfirst.witness-sim`, window class `…-sic`, title
//! `…-siw`, WM_COPYDATA `dwData = 1542`, payload `"{cwd}|{arg0}|…\0"`.
//!
//! STUB (F0.2): the signatures are final; the shell unit (U1) implements the
//! mutex, the hidden message window and the hand-off to the first instance.

use crate::bridge::UiSink;
use std::ffi::OsString;

/// Result of [`acquire`].
pub enum Acquire {
    /// This is the only instance; keep the guard alive until exit.
    Primary(InstanceGuard),
    /// Another instance runs and has been asked to show itself: exit(0).
    Secondary,
}

/// Claim the instance mutex, or signal the running instance. Runs on the
/// main thread before the event loop. TODO(U1)
pub fn acquire(_args: &[OsString]) -> Acquire {
    Acquire::Primary(InstanceGuard { _private: () })
}

/// Owns the instance mutex and the hidden WM_COPYDATA window.
pub struct InstanceGuard {
    _private: (),
}

impl InstanceGuard {
    /// Route second-instance requests to `sink` (and replay one that
    /// arrived before the UI existed). TODO(U1)
    pub fn attach(&self, _sink: UiSink) {}

    /// ReleaseMutex + CloseHandle + DestroyWindow. Called only at the point
    /// of no return of an exit path. TODO(U1)
    pub fn release(self) {}

    /// The raw mutex handle (0 = none), for the panic hook.
    pub fn raw_mutex(&self) -> isize {
        0
    }
}
