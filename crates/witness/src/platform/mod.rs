//! Windows shell integration: the main window, tray, global hotkeys, single
//! instance, autostart, file dialogs and a few system queries. Everything
//! here runs on (or is created on) the main winit thread; handlers that fire
//! on other threads only forward a [`ShellEvent`] through the `UiSink`.

// `system` (F0) is complete but consumed by U1/U7; the others with an
// allow are still F0 stubs. TODO(I1): drop these allows after the merge.
#[allow(dead_code)]
pub mod autostart;
#[allow(dead_code)]
pub mod dialogs;
pub mod hotkeys;
#[allow(dead_code)]
pub mod single_instance;
#[allow(dead_code)]
pub mod system;
pub mod tray;
#[allow(dead_code)]
pub mod window;

/// Something the OS shell asked the app to do. Sent by the tray, hotkey and
/// single-instance handlers and handled in `WitnessApp::logic()`, so they
/// work while the window is hidden.
#[allow(dead_code)] // TODO(I1): consumers land with the units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShellEvent {
    /// Left click (up) or double click on the tray icon, in physical pixels.
    TrayClick { x: f64, y: f64 },
    /// Tray menu "Open Witness".
    TrayMenuOpen,
    /// Tray menu "Record now" / "Stop recording".
    TrayMenuToggleRecord,
    /// Tray menu "Quit".
    TrayMenuQuit,
    /// The record global hotkey (toggle).
    HotkeyRecord,
    /// The bookmark global hotkey.
    HotkeyBookmark,
    /// A second launch asked this instance to show itself.
    SecondInstance,
}

/// The window a native dialog is parented to (raw HWND; 0 = none). Plain
/// data so it can cross into the worker closure that runs the dialog.
#[allow(dead_code)] // TODO(I1): consumers land with the units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnerHwnd(pub isize);
