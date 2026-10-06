//! Panic policy (design §4.6).
//!
//! Under Tauri a frontend exception could not touch the backend: WebView2
//! ran in another process. Now the UI runs on the thread that owns the event
//! loop and the tray, and winit re-raises a callback panic when the loop
//! exits, so the process dies. The hook makes that death clean:
//!
//! 1. Every panic is logged (thread, location, message, forced backtrace).
//! 2. Worker-thread panics stop there — `Tasks` already contains them with
//!    `catch_unwind`.
//! 3. A main-thread panic (first time only) stops the recording on a helper
//!    thread (WAVs finalised, meeting queued for transcription on the next
//!    launch), removes the tray icon, flushes the log, tells the user, and
//!    exits with code 101.
//!
//! Nothing here takes a lock on the panicking thread: the UI thread holds no
//! core locks while drawing, and `AppCore::emergency_stop` does its work on
//! a fresh thread with a timeout. The panicking thread may still hold the
//! egui `Context` lock, though (the hook runs before unwinding), so repaint
//! requests from other threads are switched off first
//! ([`crate::bridge::mark_ui_dead`]) — the recorder's writer thread emits
//! through the UI sink while it finalises.
//!
//! The single-instance mutex stays held until the process exits: the
//! pipeline worker, meeting watcher and rec-control threads keep running
//! behind the message box, so a relaunch must not start a second core on
//! the same database meanwhile. The hidden instance window refuses requests
//! from the start ([`crate::platform::single_instance::begin_exit_raw`]),
//! so a relaunch waits and takes the mutex over once this process is gone.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::ThreadId;
use std::time::Duration;
use witness_core::AppCore;

static CORE: OnceLock<Arc<AppCore>> = OnceLock::new();
/// (owner hwnd, uID) of the notification-area icon.
static TRAY: OnceLock<(isize, u32)> = OnceLock::new();
/// The single-instance (mutex, hidden window) handles.
static INSTANCE: OnceLock<(isize, isize)> = OnceLock::new();
static MAIN_THREAD: OnceLock<ThreadId> = OnceLock::new();
static ONCE: AtomicBool = AtomicBool::new(false);

/// How long the emergency stop may take before the process exits anyway.
const EMERGENCY_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Install the hook. Call first thing in `main()`, on the main thread.
pub fn install() {
    let _ = MAIN_THREAD.set(std::thread::current().id());
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>").to_owned();
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        let message = panic_message(info.payload());
        let backtrace = std::backtrace::Backtrace::force_capture();
        log::error!("panic in thread '{name}' at {location}: {message}\n{backtrace}");
        // Also on stderr, for `cargo run` and test output.
        eprintln!("thread '{name}' panicked at {location}:\n{message}");

        if MAIN_THREAD.get() != Some(&thread.id()) {
            return;
        }
        if ONCE.swap(true, Ordering::SeqCst) {
            return;
        }
        main_thread_teardown();
    }));
}

/// Register the core, so a main-thread panic can save the recording.
pub fn register_core(core: Arc<AppCore>) {
    let _ = CORE.set(core);
}

/// Register the tray icon's identity for `NIM_DELETE`.
pub fn register_tray(ids: (isize, u32)) {
    if ids.0 != 0 {
        let _ = TRAY.set(ids);
    }
}

/// Register the single-instance mutex (released before exiting) and its
/// hidden window (told to stop answering second launches first).
pub fn register_instance(mutex: isize, hwnd: isize) {
    if mutex != 0 || hwnd != 0 {
        let _ = INSTANCE.set((mutex, hwnd));
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// The irreversible main-thread path. Never returns.
fn main_thread_teardown() -> ! {
    crate::bridge::mark_ui_dead();
    // Refuse second launches from here on (the message box below pumps the
    // hidden window's messages); they wait for the mutex instead.
    if let Some(&(_, hwnd)) = INSTANCE.get() {
        crate::platform::single_instance::begin_exit_raw(hwnd);
    }
    let mut stopped = false;
    let mut log_path = None;
    if let Some(core) = CORE.get() {
        stopped = core.emergency_stop(EMERGENCY_STOP_TIMEOUT);
        log_path = Some(core.runtime_data_dir().join("witness.log"));
    }
    if let Some(&(hwnd, uid)) = TRAY.get() {
        remove_tray_icon(hwnd, uid);
    }
    log::logger().flush();
    let details = log_path
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "witness.log".into());
    crate::platform::system::error_box(&teardown_message(stopped, &details));
    // Only now: the core's threads die with the process a moment later.
    if let Some(&(mutex, _)) = INSTANCE.get() {
        if mutex != 0 {
            release_mutex(mutex);
        }
    }
    std::process::exit(101)
}

fn teardown_message(stopped: bool, details: &str) -> String {
    format!(
        "Witness hit an internal error and has to close.{} Details: {details}",
        if stopped {
            " The recording in progress was saved."
        } else {
            ""
        }
    )
}

#[cfg(windows)]
fn remove_tray_icon(hwnd: isize, uid: u32) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{Shell_NotifyIconW, NIM_DELETE, NOTIFYICONDATAW};
    let data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: HWND(hwnd as *mut _),
        uID: uid,
        ..Default::default()
    };
    // SAFETY: NIM_DELETE only reads hWnd + uID from a correctly sized struct.
    let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
}

#[cfg(not(windows))]
fn remove_tray_icon(_hwnd: isize, _uid: u32) {}

#[cfg(windows)]
fn release_mutex(mutex: isize) {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Threading::ReleaseMutex;
    let handle = HANDLE(mutex as *mut _);
    // SAFETY: the handle was registered by the single-instance guard and is
    // owned by this (main) thread; failures are harmless at this point.
    unsafe {
        let _ = ReleaseMutex(handle);
        let _ = CloseHandle(handle);
    }
}

#[cfg(not(windows))]
fn release_mutex(_mutex: isize) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teardown_message_mentions_the_saved_recording_only_when_stopped() {
        assert_eq!(
            teardown_message(false, "C:\\data\\witness.log"),
            "Witness hit an internal error and has to close. Details: C:\\data\\witness.log"
        );
        assert!(teardown_message(true, "x")
            .contains("has to close. The recording in progress was saved. Details: x"));
    }

    #[test]
    fn panic_messages_from_str_and_string_payloads() {
        assert_eq!(panic_message(&"boom"), "boom");
        assert_eq!(panic_message(&String::from("bang")), "bang");
        assert_eq!(panic_message(&42u8), "unknown panic");
    }
}
