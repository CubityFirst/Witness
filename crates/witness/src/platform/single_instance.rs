//! One Witness per user. The names are identical to
//! tauri-plugin-single-instance's, so the native app and a lingering 0.2.x
//! process recognise each other (design §4.4): mutex
//! `com.github.cubityfirst.witness-sim`, window class `…-sic`, title
//! `…-siw`, WM_COPYDATA `dwData = 1542`, payload `"{cwd}|{arg0}|…\0"`.
//!
//! Two fixes over the plugin:
//! - A second launch that finds the mutex but not (yet) the window retries
//!   for 2 s instead of falling through into a second full instance.
//! - If the mutex owner lets go meanwhile (a restart: the old process
//!   releases it at its point of no return), the new launch takes it over
//!   and becomes the primary instance.
//! - An exiting instance refuses requests (`begin_exit`), so a launch that
//!   races the exit keeps waiting and takes over instead of being told
//!   "shown" by a process that is about to disappear.

use crate::bridge::UiSink;
use crate::platform::ShellEvent;
use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// The kernel/window names one instance is known by.
#[derive(Debug, Clone, Copy)]
struct Names {
    mutex: &'static str,
    class: &'static str,
    title: &'static str,
}

/// `{identifier}-sim/-sic/-siw` (the plugin's `semver` feature was off).
const NAMES: Names = Names {
    mutex: "com.github.cubityfirst.witness-sim",
    class: "com.github.cubityfirst.witness-sic",
    title: "com.github.cubityfirst.witness-siw",
};

/// `COPYDATASTRUCT::dwData` of a second-instance request.
const COPYDATA_SINGLE_INSTANCE: usize = 1542;
/// How long a second launch waits for the first instance's window (or for
/// the mutex to be released, on a restart).
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(2);
const ACQUIRE_POLL: Duration = Duration::from_millis(100);
/// How long the WM_COPYDATA send may block on the first instance.
const SEND_TIMEOUT_MS: u32 = 2000;

/// Where second-instance requests go once the UI exists.
static SINK: OnceLock<UiSink> = OnceLock::new();
/// A request arrived before `attach`: replay it then.
static PENDING_SHOW: AtomicBool = AtomicBool::new(false);
/// Log lines from before logging was up (`acquire` runs first thing in
/// `main`); `attach` writes them to the log.
static EARLY_LOG: Mutex<Vec<(log::Level, String)>> = Mutex::new(Vec::new());

/// Result of [`acquire`].
pub enum Acquire {
    /// This is the only instance; keep the guard alive until exit.
    Primary(InstanceGuard),
    /// Another instance runs and has been asked to show itself: exit(0).
    Secondary,
}

/// Claim the instance mutex, or signal the running instance. Runs on the
/// main thread before the event loop (whose message pump then serves the
/// hidden window). Logging is not up yet at this point: see [`early_log`].
pub fn acquire(args: &[OsString]) -> Acquire {
    imp::acquire(&NAMES, args)
}

/// Owns the instance mutex and the hidden WM_COPYDATA window.
pub struct InstanceGuard {
    mutex: isize,
    hwnd: isize,
}

impl InstanceGuard {
    /// Route second-instance requests to `sink` (and replay one that
    /// arrived before the UI existed).
    pub fn attach(&self, sink: UiSink) {
        // Logging is up by now: write what `acquire` noted.
        let early = std::mem::take(&mut *EARLY_LOG.lock().unwrap_or_else(|e| e.into_inner()));
        for (level, line) in early {
            log::log!(level, "{line}");
        }
        let sink = SINK.get_or_init(|| sink);
        if PENDING_SHOW.swap(false, Ordering::SeqCst) {
            sink.shell(ShellEvent::SecondInstance);
        }
    }

    /// Stop answering second launches: from now on a request is refused
    /// (answered 0), so the new launch keeps waiting for the mutex and takes
    /// over once [`release`](Self::release) lets go. Call it first in an
    /// exit's teardown: removing the tray icon (`NIM_DELETE`) waits on
    /// Explorer, a thread waiting in a send still dispatches incoming
    /// WM_COPYDATA, and without this a racing launch would be answered
    /// "shown" by a process that is about to exit (leaving no Witness).
    // TODO(I1): need `teardown_and_exit` to call
    // `shell.instance_guard.begin_exit()` before dropping the tray/hotkeys.
    pub fn begin_exit(&self) {
        imp::begin_exit(self.hwnd);
    }

    /// DestroyWindow + ReleaseMutex + CloseHandle. Called only at the point
    /// of no return of an exit path, on the main thread (the mutex's owner).
    /// The window goes first, so a launch racing this exit does not find a
    /// window that would answer: it waits for the mutex instead and takes
    /// over (see [`begin_exit`](Self::begin_exit) for the teardown before).
    pub fn release(self) {
        imp::release(self.mutex, self.hwnd);
    }

    /// The raw mutex handle (0 = none), for the panic hook.
    pub fn raw_mutex(&self) -> isize {
        self.mutex
    }
}

/// Note a line for the log before logging exists; `attach` writes it out.
/// It also goes to the debugger output (DebugView), the only trace of a
/// second launch that exits before any log is opened (that case appends to
/// the log file itself as well, [`append_to_log_file`]).
fn early_log(level: log::Level, line: String) {
    imp::debug_output(&format!("Witness: {line}\n"));
    EARLY_LOG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((level, line));
}

/// Best effort: append one line in simplelog's format to the configured
/// `witness.log`, for a second launch that exits without ever starting
/// logging. The running instance holds the file open for appending too;
/// std opens files shared, so both can append.
fn append_to_log_file(level: log::Level, line: &str) {
    use std::io::Write;
    let Ok(settings) = witness_core::settings::Settings::load_result() else {
        return;
    };
    let path = settings.data_dir().join("witness.log");
    let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(path) else {
        return;
    };
    // simplelog's default config stamps UTC.
    let time = chrono::Utc::now().format("%H:%M:%S");
    let _ = writeln!(file, "{time} [{}] {line}", level.as_str().to_uppercase());
}

/// A second-instance request reached the hidden window (any thread that
/// owns it; in the app, the main thread inside winit's message pump).
fn on_second_instance(payload: &[u8]) {
    let (cwd, args) = decode_payload(payload);
    log::info!("second launch (cwd {cwd:?}, args {args:?}): showing the window");
    match SINK.get() {
        Some(sink) => sink.shell(ShellEvent::SecondInstance),
        None => PENDING_SHOW.store(true, Ordering::SeqCst),
    }
}

/// The plugin's payload: UTF-8 `"{cwd}|{arg0}|{arg1}…\0"`.
fn encode_payload(cwd: &Path, args: &[OsString]) -> Vec<u8> {
    let args: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    format!("{}|{}\0", cwd.to_string_lossy(), args.join("|")).into_bytes()
}

/// Inverse of [`encode_payload`] (stops at the first NUL; lossy UTF-8).
fn decode_payload(bytes: &[u8]) -> (String, Vec<String>) {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let text = String::from_utf8_lossy(&bytes[..end]);
    let mut parts = text.split('|');
    let cwd = parts.next().unwrap_or_default().to_string();
    (cwd, parts.map(str::to_string).collect())
}

#[cfg(windows)]
mod imp {
    use super::{
        append_to_log_file, early_log, encode_payload, on_second_instance, Acquire, InstanceGuard,
        Names, ACQUIRE_POLL, ACQUIRE_TIMEOUT, COPYDATA_SINGLE_INSTANCE, SEND_TIMEOUT_MS,
    };
    use log::Level;
    use std::ffi::OsString;
    use std::time::Instant;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT,
        WAIT_ABANDONED, WAIT_OBJECT_0, WPARAM,
    };
    use windows::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
    use windows::Win32::UI::WindowsAndMessaging::{
        AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowW,
        GetWindowLongPtrW, GetWindowThreadProcessId, RegisterClassExW, SendMessageTimeoutW,
        SetWindowLongPtrW, GWLP_USERDATA, SMTO_ABORTIFHUNG, WM_COPYDATA, WNDCLASSEXW,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
    };

    /// The hidden window's `GWLP_USERDATA` once its instance is exiting. Per
    /// window rather than a static, so tests can run several "instances" in
    /// one process.
    const EXITING: isize = 1;

    // `Win32_System_Diagnostics_Debug` is not among the crate's `windows`
    // features (no unit edits Cargo.toml), so declare this one import here.
    #[link(name = "kernel32")]
    extern "system" {
        fn OutputDebugStringW(text: *const u16);
    }

    /// Write to the debugger output (DebugView shows it without a debugger).
    pub fn debug_output(text: &str) {
        let text = wide(text);
        // SAFETY: a NUL-terminated buffer that outlives the call.
        unsafe { OutputDebugStringW(text.as_ptr()) };
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn acquire(names: &Names, args: &[OsString]) -> Acquire {
        let mutex_name = wide(names.mutex);
        // SAFETY: the name buffer outlives the call; no security attributes.
        let created = unsafe { CreateMutexW(None, true, PCWSTR(mutex_name.as_ptr())) };
        // Read right away: the next Win32 call would overwrite it.
        // SAFETY: no arguments.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let mutex = match created {
            Ok(handle) => handle,
            Err(error) => {
                // Without a mutex there is no way to tell; run rather than
                // refuse to start.
                early_log(
                    Level::Error,
                    format!("single-instance mutex unavailable: {error}"),
                );
                return Acquire::Primary(primary(names, HANDLE::default()));
            }
        };
        if !existed {
            return Acquire::Primary(primary(names, mutex));
        }

        let payload = encode_payload(&std::env::current_dir().unwrap_or_default(), args);
        let deadline = Instant::now() + ACQUIRE_TIMEOUT;
        loop {
            // The owner let go (it is exiting or restarting): take over.
            // SAFETY: a valid mutex handle, zero timeout.
            let wait = unsafe { WaitForSingleObject(mutex, 0) };
            if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
                early_log(
                    Level::Info,
                    "took over the instance mutex from an exiting instance".into(),
                );
                return Acquire::Primary(primary(names, mutex));
            }
            if let Some(target) = find_window(names) {
                if signal(target, &payload) {
                    close(mutex);
                    return Acquire::Secondary;
                }
                // The instance is exiting (it refused, or its window is
                // going away): wait for the mutex instead.
            }
            if Instant::now() >= deadline {
                // This process exits before logging ever starts: leave the
                // line in the running instance's log (and debugger output).
                let line = format!(
                    "second launch: another Witness holds the instance mutex but did not \
                     answer within {ACQUIRE_TIMEOUT:?}; exiting"
                );
                append_to_log_file(Level::Warn, &line);
                early_log(Level::Warn, line);
                close(mutex);
                return Acquire::Secondary;
            }
            std::thread::sleep(ACQUIRE_POLL);
        }
    }

    fn close(mutex: HANDLE) {
        // SAFETY: our own handle, closed once.
        let _ = unsafe { CloseHandle(mutex) };
    }

    fn find_window(names: &Names) -> Option<HWND> {
        let class = wide(names.class);
        let title = wide(names.title);
        // SAFETY: both buffers outlive the call.
        unsafe { FindWindowW(PCWSTR(class.as_ptr()), PCWSTR(title.as_ptr())) }
            .ok()
            .filter(|hwnd| !hwnd.is_invalid())
    }

    /// Let the first instance take the foreground, then send it the
    /// request. True once it accepted (answered non-zero: 0.2.x always
    /// answers TRUE; an exiting native instance answers 0).
    fn signal(target: HWND, payload: &[u8]) -> bool {
        let mut pid = 0u32;
        // SAFETY: `target` came from FindWindowW; a stale handle just fails.
        unsafe {
            GetWindowThreadProcessId(target, Some(&mut pid));
            if pid != 0 {
                let _ = AllowSetForegroundWindow(pid);
            }
        }
        let data = COPYDATASTRUCT {
            dwData: COPYDATA_SINGLE_INSTANCE,
            cbData: payload.len() as u32,
            lpData: payload.as_ptr() as *mut _,
        };
        let mut answer = 0usize;
        // SAFETY: WM_COPYDATA marshals `data` (and the payload it points
        // to) into the target process for the duration of the call.
        let sent = unsafe {
            SendMessageTimeoutW(
                target,
                WM_COPYDATA,
                WPARAM(0),
                LPARAM(&data as *const COPYDATASTRUCT as isize),
                SMTO_ABORTIFHUNG,
                SEND_TIMEOUT_MS,
                Some(&mut answer),
            )
        };
        sent.0 != 0 && answer != 0
    }

    /// Create the hidden target window for this (primary) instance.
    fn primary(names: &Names, mutex: HANDLE) -> InstanceGuard {
        let hwnd = match create_window(names) {
            Ok(hwnd) => hwnd.0 as isize,
            Err(error) => {
                early_log(
                    Level::Error,
                    format!("single-instance window could not be created: {error}"),
                );
                0
            }
        };
        InstanceGuard {
            mutex: mutex.0 as isize,
            hwnd,
        }
    }

    fn create_window(names: &Names) -> windows::core::Result<HWND> {
        let class = wide(names.class);
        let title = wide(names.title);
        // SAFETY: plain window-class registration and creation on this
        // thread; every buffer outlives its call. A repeated registration
        // (tests) fails harmlessly and the class from the first one is used.
        unsafe {
            let module = GetModuleHandleW(None)?;
            let instance = HINSTANCE(module.0);
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: instance,
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            };
            RegisterClassExW(&wc);
            // Hidden, never activated, never on the taskbar. FindWindowW
            // finds hidden windows, so 0.2.x launches still reach it.
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                PCWSTR(class.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_COPYDATA {
            // SAFETY: reading our own window's user data.
            if unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } == EXITING {
                // Refused: the sender waits for the mutex and takes over.
                return LRESULT(0);
            }
            let data = lparam.0 as *const COPYDATASTRUCT;
            // SAFETY: for WM_COPYDATA the system guarantees a valid
            // COPYDATASTRUCT (and cbData readable bytes) during the call.
            unsafe {
                if !data.is_null() && (*data).dwData == COPYDATA_SINGLE_INSTANCE {
                    let len = (*data).cbData as usize;
                    let bytes = if len == 0 || (*data).lpData.is_null() {
                        &[][..]
                    } else {
                        std::slice::from_raw_parts((*data).lpData as *const u8, len)
                    };
                    on_second_instance(bytes);
                }
            }
            // The plugin answered TRUE to every WM_COPYDATA.
            return LRESULT(1);
        }
        // SAFETY: default handling for everything else.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    pub fn begin_exit(hwnd: isize) {
        if hwnd != 0 {
            // SAFETY: our own window; any thread of the process may set it.
            unsafe { SetWindowLongPtrW(HWND(hwnd as *mut _), GWLP_USERDATA, EXITING) };
        }
    }

    pub fn release(mutex: isize, hwnd: isize) {
        // SAFETY: our own window and mutex, released once on the thread
        // that created / owns them.
        unsafe {
            if hwnd != 0 {
                let _ = DestroyWindow(HWND(hwnd as *mut _));
            }
            if mutex != 0 {
                let handle = HANDLE(mutex as *mut _);
                let _ = ReleaseMutex(handle);
                let _ = CloseHandle(handle);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{Acquire, InstanceGuard, Names};
    use std::ffi::OsString;

    pub fn acquire(_names: &Names, _args: &[OsString]) -> Acquire {
        Acquire::Primary(InstanceGuard { mutex: 0, hwnd: 0 })
    }
    pub fn begin_exit(_hwnd: isize) {}
    pub fn release(_mutex: isize, _hwnd: isize) {}
    pub fn debug_output(_text: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn payload_matches_the_tauri_plugin() {
        let args: Vec<OsString> = vec![
            r"C:\Users\X\AppData\Local\Witness\witness.exe".into(),
            "--minimized".into(),
        ];
        let bytes = encode_payload(Path::new(r"C:\Windows\System32"), &args);
        assert_eq!(
            bytes,
            b"C:\\Windows\\System32|C:\\Users\\X\\AppData\\Local\\Witness\\witness.exe|--minimized\0"
        );
        let (cwd, decoded) = decode_payload(&bytes);
        assert_eq!(cwd, r"C:\Windows\System32");
        assert_eq!(
            decoded,
            [
                "C:\\Users\\X\\AppData\\Local\\Witness\\witness.exe",
                "--minimized"
            ]
        );
    }

    #[test]
    fn payload_edge_cases() {
        // Only arg0 (the usual second launch from a shortcut).
        let bytes = encode_payload(&PathBuf::from("C:\\"), &["witness.exe".into()]);
        assert_eq!(bytes, b"C:\\|witness.exe\0");
        // No NUL, empty, and trailing garbage after the NUL.
        assert_eq!(decode_payload(b"").0, "");
        assert_eq!(
            decode_payload(b"D:\\x|a.exe\0junk|more"),
            ("D:\\x".to_string(), vec!["a.exe".to_string()])
        );
        assert_eq!(
            decode_payload(b"D:\\x"),
            ("D:\\x".to_string(), Vec::<String>::new())
        );
    }

    /// The full protocol between two "instances" (threads) under private
    /// names, so a running Witness is never touched: the first owns the
    /// mutex and pumps messages; the second signals it and backs off; once
    /// every handle is closed, a third launch is primary again (a fresh
    /// mutex; the takeover branch has its own tests below).
    #[cfg(windows)]
    #[test]
    fn second_launch_signals_the_first_and_backs_off() {
        use crate::bridge::UiMsg;
        use std::sync::mpsc;
        use std::time::Instant;
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
        };

        let names = Names {
            mutex: "com.github.cubityfirst.witness-test-sim-u1",
            class: "com.github.cubityfirst.witness-test-sic-u1",
            title: "com.github.cubityfirst.witness-test-siw-u1",
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let first = std::thread::spawn(move || {
            let Acquire::Primary(guard) = imp::acquire(&names, &["first.exe".into()]) else {
                panic!("first launch must be primary");
            };
            let (sink, rx) = UiSink::new();
            guard.attach(sink);
            ready_tx.send(()).unwrap();
            // Pump until the second launch's request arrives.
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut shown = false;
            while Instant::now() < deadline && !shown {
                let mut msg = MSG::default();
                // SAFETY: a standard message pump on this thread.
                unsafe {
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                shown = matches!(rx.try_recv(), Ok(UiMsg::Shell(ShellEvent::SecondInstance)));
                std::thread::sleep(Duration::from_millis(5));
            }
            release_rx.recv().unwrap();
            guard.release();
            shown
        });

        ready_rx.recv().unwrap();
        let second = imp::acquire(&names, &["second.exe".into(), "--x".into()]);
        assert!(matches!(second, Acquire::Secondary));
        release_tx.send(()).unwrap();
        assert!(
            first.join().unwrap(),
            "the first instance was not signalled"
        );

        // After the release a new launch becomes the primary instance.
        let third = std::thread::spawn(move || match imp::acquire(&names, &[]) {
            Acquire::Primary(guard) => {
                guard.release();
                true
            }
            Acquire::Secondary => false,
        });
        assert!(third.join().unwrap());
    }

    /// A launch on a thread of its own; true if it became primary (its guard
    /// is released on that thread, which owns the mutex).
    #[cfg(windows)]
    fn launch(names: Names) -> std::thread::JoinHandle<bool> {
        std::thread::spawn(move || match imp::acquire(&names, &["new.exe".into()]) {
            Acquire::Primary(guard) => {
                guard.release();
                true
            }
            Acquire::Secondary => false,
        })
    }

    /// A restart: the old instance is exiting (`begin_exit`) but still pumps
    /// messages, e.g. inside the tray's NIM_DELETE. Its refusal keeps the new
    /// launch waiting, and the new launch takes the mutex over
    /// (`WAIT_OBJECT_0`) as soon as the old one releases it.
    #[cfg(windows)]
    #[test]
    fn a_launch_racing_an_exit_takes_over_the_mutex() {
        use std::sync::mpsc;
        use std::time::Instant;
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
        };

        let names = Names {
            mutex: "com.github.cubityfirst.witness-test-sim-u1-exit",
            class: "com.github.cubityfirst.witness-test-sic-u1-exit",
            title: "com.github.cubityfirst.witness-test-siw-u1-exit",
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        let old = std::thread::spawn(move || {
            let Acquire::Primary(guard) = imp::acquire(&names, &["old.exe".into()]) else {
                panic!("the old instance must be primary");
            };
            guard.begin_exit();
            ready_tx.send(()).unwrap();
            // Still pumping (and refusing) for a while, then let go.
            let until = Instant::now() + Duration::from_millis(600);
            while Instant::now() < until {
                let mut msg = MSG::default();
                // SAFETY: a standard message pump on this thread.
                unsafe {
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            guard.release();
        });

        ready_rx.recv().unwrap();
        let started = Instant::now();
        let new = launch(names);
        old.join().unwrap();
        assert!(new.join().unwrap(), "the new launch must take over");
        // It waited for the release instead of exiting on the refusal.
        assert!(started.elapsed() >= Duration::from_millis(500));
    }

    /// The old instance died holding the mutex (its thread ended without
    /// releasing it): the new launch takes it over (`WAIT_ABANDONED`).
    #[cfg(windows)]
    #[test]
    fn a_launch_takes_over_an_abandoned_mutex() {
        let names = Names {
            mutex: "com.github.cubityfirst.witness-test-sim-u1-abandoned",
            class: "com.github.cubityfirst.witness-test-sic-u1-abandoned",
            title: "com.github.cubityfirst.witness-test-siw-u1-abandoned",
        };
        let old = std::thread::spawn(move || {
            let Acquire::Primary(guard) = imp::acquire(&names, &[]) else {
                panic!("the old instance must be primary");
            };
            // Its window is gone and its thread ends while owning the mutex;
            // the handle stays open, so the mutex object survives.
            imp::release(0, guard.hwnd);
            guard.mutex
        });
        let mutex = old.join().unwrap();
        assert!(
            launch(names).join().unwrap(),
            "the new launch must take over"
        );
        // SAFETY: the old instance's handle, closed once (from any thread).
        let _ = unsafe {
            windows::Win32::Foundation::CloseHandle(windows::Win32::Foundation::HANDLE(
                mutex as *mut _,
            ))
        };
    }
}
