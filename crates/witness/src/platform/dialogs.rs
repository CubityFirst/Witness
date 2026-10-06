//! Native save / folder dialogs (`rfd`), parented to the main window. They
//! block, so they only ever run inside a worker closure (`Tasks::spawn` /
//! `spawn_op`), never on the UI thread (design §7).
//!
//! 0.2.x showed these unparented (tauri-plugin-dialog), so they could fall
//! behind the window and their focus change dismissed the tray flyout.
//! Parenting to the main HWND makes them modal: the shell disables the
//! owner while the dialog is up (the UI thread keeps pumping, so egui keeps
//! painting), and it stays in front of Witness. Callers get the owner from
//! `UiCtx::dialog_owner`, which pins the flyout first.
//!
//! Each dialog runs on its own short-lived thread. rfd initialises COM as
//! single-threaded and silently reports "cancelled" when the calling thread
//! already joined the multithreaded apartment (any core call that touched
//! WASAPI, e.g. the device list, does that), so the worker closure's thread
//! is never trusted with it.

use super::OwnerHwnd;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle,
    Win32WindowHandle, WindowHandle,
};
use std::num::NonZeroIsize;
use std::path::PathBuf;

/// A save-file dialog request, e.g. `{stem}.opus` / "Ogg Opus audio" / ["opus"].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveFileRequest {
    /// Suggested file name, including the extension.
    pub file_name: String,
    /// Filter label shown in the type drop-down.
    pub filter_name: String,
    /// Extensions without the dot.
    pub extensions: Vec<String>,
}

/// Show a modal save dialog; None when cancelled. The first extension is
/// also the default one, so a typed name without one still gets it.
pub fn save_file_blocking(owner: OwnerHwnd, request: SaveFileRequest) -> Option<PathBuf> {
    on_dialog_thread("save", owner, move || {
        let mut dialog = with_owner(rfd::FileDialog::new(), owner).set_file_name(request.file_name);
        if !request.extensions.is_empty() {
            dialog = dialog.add_filter(request.filter_name, &request.extensions);
        }
        dialog.save_file()
    })
}

/// Show a modal folder picker; None when cancelled. `title` replaces the
/// shell's default caption ("Select Folder").
pub fn pick_folder_blocking(owner: OwnerHwnd, title: Option<&str>) -> Option<PathBuf> {
    let title = title.map(str::to_owned);
    on_dialog_thread("folder", owner, move || {
        let mut dialog = with_owner(rfd::FileDialog::new(), owner);
        if let Some(title) = title {
            dialog = dialog.set_title(title);
        }
        dialog.pick_folder()
    })
}

/// Run `show` on a fresh thread (clean COM apartment) and wait for it. A
/// panic inside the dialog reads as "cancelled" after being logged.
///
/// Refused (logged, read as "cancelled") in every build when the calling
/// thread owns `owner`: the dialog's Show disables the owner through
/// EnableWindow, which waits for the owner's thread to handle messages,
/// while that thread sits in the join below without pumping, so both would
/// wait forever. That is exactly the UI thread misusing this API.
fn on_dialog_thread<F>(kind: &str, owner: OwnerHwnd, show: F) -> Option<PathBuf>
where
    F: FnOnce() -> Option<PathBuf> + Send,
{
    // Debug builds also catch an unparented dialog opened on the UI thread
    // (no deadlock, but it freezes the UI). This relies on Rust naming the
    // process's main thread "main", which is where eframe/winit run.
    // TODO(I1): use a `witness_core::service::is_ui_thread()` next to
    // `mark_ui_thread` instead, so there is one definition of the UI thread
    // (U9 does not own witness-core).
    debug_assert!(
        std::thread::current().name() != Some("main"),
        "the {kind} dialog blocks and must not be opened on the UI thread \
         (open it from a Tasks worker)"
    );
    if owned_by_current_thread(owner) {
        log::error!(
            "the {kind} dialog was opened on its owner window's thread (the UI \
             thread); refused, since waiting for it there would hang Witness"
        );
        return None;
    }
    let picked = std::thread::scope(|scope| {
        let spawned = std::thread::Builder::new()
            .name(format!("dialog-{kind}"))
            .spawn_scoped(scope, show);
        match spawned {
            Ok(handle) => handle.join().unwrap_or_else(|_| {
                log::error!("{kind} dialog panicked");
                None
            }),
            Err(error) => {
                log::error!("could not start the {kind} dialog thread: {error}");
                None
            }
        }
    });
    match &picked {
        Some(path) => log::debug!("{kind} dialog: {}", path.display()),
        None => log::debug!("{kind} dialog cancelled"),
    }
    picked
}

/// Parent the dialog to `owner` when it is still a live window; otherwise
/// show it unparented rather than not at all.
fn with_owner(dialog: rfd::FileDialog, owner: OwnerHwnd) -> rfd::FileDialog {
    match Owner::new(owner) {
        Some(owner) => dialog.set_parent(&owner),
        None => dialog,
    }
}

/// A raw HWND presented through raw-window-handle, which is what rfd's
/// `set_parent` takes.
struct Owner(NonZeroIsize);

impl Owner {
    fn new(owner: OwnerHwnd) -> Option<Owner> {
        let hwnd = NonZeroIsize::new(owner.0)?;
        is_window(hwnd.get()).then_some(Owner(hwnd))
    }
}

impl HasWindowHandle for Owner {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = RawWindowHandle::Win32(Win32WindowHandle::new(self.0));
        // SAFETY: the HWND was checked with IsWindow and belongs to the main
        // window, which lives for the whole process; rfd only reads the
        // value to pass it to IFileDialog::Show.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

impl HasDisplayHandle for Owner {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::windows())
    }
}

#[cfg(windows)]
fn is_window(hwnd: isize) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::IsWindow;
    // SAFETY: IsWindow accepts any value and only reports validity.
    unsafe { IsWindow(Some(HWND(hwnd as *mut core::ffi::c_void))).as_bool() }
}

#[cfg(not(windows))]
fn is_window(_hwnd: isize) -> bool {
    false
}

/// Whether `owner` is a live window created by the calling thread.
#[cfg(windows)]
fn owned_by_current_thread(owner: OwnerHwnd) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    if owner.0 == 0 || !is_window(owner.0) {
        return false;
    }
    // SAFETY: plain queries. GetWindowThreadProcessId returns 0 for a window
    // that vanished in between, and 0 is never a thread id.
    unsafe {
        GetWindowThreadProcessId(HWND(owner.0 as *mut core::ffi::c_void), None)
            == GetCurrentThreadId()
    }
}

#[cfg(not(windows))]
fn owned_by_current_thread(_owner: OwnerHwnd) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal COM imports for the tests (the bin's `windows` features do not
    /// include Win32_System_Com, and the tests should not widen them).
    #[cfg(windows)]
    mod com {
        pub const COINIT_MULTITHREADED: u32 = 0x0;
        pub const COINIT_APARTMENTTHREADED: u32 = 0x2;
        /// What rfd's STA init gets on a thread already in the MTA.
        pub const RPC_E_CHANGED_MODE: i32 = 0x8001_0106_u32 as i32;
        #[link(name = "ole32")]
        extern "system" {
            pub fn CoInitializeEx(reserved: *const core::ffi::c_void, coinit: u32) -> i32;
            pub fn CoUninitialize();
        }
    }

    /// The reason for `on_dialog_thread`: a worker that already joined the
    /// multithreaded apartment (as WASAPI device listing does) cannot host
    /// rfd's single-threaded COM, but the dialog thread it hops to can.
    #[cfg(windows)]
    #[test]
    fn dialog_thread_gets_a_fresh_apartment_from_an_mta_worker() {
        std::thread::Builder::new()
            .name("tasks-worker".into())
            .spawn(|| {
                // SAFETY: plain COM init/uninit pairs on threads we own.
                unsafe {
                    assert!(com::CoInitializeEx(std::ptr::null(), com::COINIT_MULTITHREADED) >= 0);
                    // The worker itself is stuck in the MTA...
                    let here = com::CoInitializeEx(std::ptr::null(), com::COINIT_APARTMENTTHREADED);
                    assert_eq!(here, com::RPC_E_CHANGED_MODE);
                    let worker = std::thread::current().id();
                    // ...but the dialog thread starts clean.
                    let picked = on_dialog_thread("probe", OwnerHwnd(0), move || {
                        assert_ne!(std::thread::current().id(), worker);
                        let hr =
                            com::CoInitializeEx(std::ptr::null(), com::COINIT_APARTMENTTHREADED);
                        assert!(hr >= 0, "STA init failed on the dialog thread: {hr:#x}");
                        com::CoUninitialize();
                        Some(PathBuf::from("ok"))
                    });
                    assert_eq!(picked, Some(PathBuf::from("ok")));
                    com::CoUninitialize();
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// A dialog opened on its owner's own thread (the UI thread misusing the
    /// API) must come back as "cancelled" at once in every build instead of
    /// deadlocking in EnableWindow. It runs on a helper thread so that a
    /// regression fails the test rather than hanging it.
    #[cfg(windows)]
    #[test]
    fn dialog_on_the_owner_thread_is_refused_not_deadlocked() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::Duration;
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
        };

        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("owner-thread".into())
            .spawn(move || {
                // SAFETY: a message-only STATIC window owned by this thread,
                // destroyed below.
                let hwnd = unsafe {
                    CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        w!("STATIC"),
                        w!("Witness dialog owner (same-thread test)"),
                        WINDOW_STYLE(0),
                        0,
                        0,
                        0,
                        0,
                        Some(HWND_MESSAGE),
                        None,
                        None,
                        None,
                    )
                }
                .unwrap();
                let owner = OwnerHwnd(hwnd.0 as isize);
                assert!(owned_by_current_thread(owner));
                let shown = AtomicBool::new(false);
                let picked = on_dialog_thread("probe", owner, || {
                    shown.store(true, Ordering::SeqCst);
                    Some(PathBuf::from("shown"))
                });
                let shown = shown.into_inner();
                // SAFETY: the window was created on this thread.
                unsafe { DestroyWindow(hwnd).unwrap() };
                tx.send((picked, shown)).unwrap();
            })
            .unwrap();
        let (picked, shown) = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("a dialog on its owner's thread hung");
        assert_eq!(picked, None);
        assert!(!shown, "the dialog must not be shown at all");
        assert!(!owned_by_current_thread(OwnerHwnd(0)));
    }

    #[test]
    fn owner_requires_a_live_window() {
        assert!(Owner::new(OwnerHwnd(0)).is_none());
        // A value that is not a window handle: shown unparented instead.
        assert!(Owner::new(OwnerHwnd(0x7fff_fff0)).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn owner_exposes_the_hwnd_to_rfd() {
        use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;
        // SAFETY: plain query.
        let desktop = unsafe { GetDesktopWindow() }.0 as isize;
        let owner = Owner::new(OwnerHwnd(desktop)).expect("the desktop is a window");
        match owner.window_handle().unwrap().as_raw() {
            RawWindowHandle::Win32(handle) => assert_eq!(handle.hwnd.get(), desktop),
            other => panic!("unexpected handle {other:?}"),
        }
        assert!(owner.display_handle().is_ok());
    }

    /// The real thing, in process: an owner window pumped by its own "UI"
    /// thread, both dialogs opened from a worker thread — plain, and one that
    /// already joined the MTA like a Tasks worker after a WASAPI call. Each
    /// must be owned by that window and disable it while up (= modal), and
    /// re-enable it on cancel. Shows windows, so it needs a desktop session.
    /// `WITNESS_TEST_DIALOG_HOLD_MS` keeps each dialog up for a screenshot.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn dialogs_are_modal_over_the_owner_from_a_worker() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        use windows::core::w;
        use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::System::Threading::GetCurrentThreadId;
        use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
        use windows::Win32::UI::WindowsAndMessaging::*;

        unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
            // SAFETY: default handling for a plain test window.
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }

        // The owner, created and pumped on its own thread like eframe's.
        let (tx, rx) = mpsc::channel();
        let ui = std::thread::spawn(move || {
            // SAFETY: standard window creation and message loop on this thread.
            unsafe {
                let module = GetModuleHandleW(None).unwrap();
                let class = WNDCLASSW {
                    lpfnWndProc: Some(wndproc),
                    hInstance: module.into(),
                    lpszClassName: w!("WitnessDialogTestOwner"),
                    ..Default::default()
                };
                RegisterClassW(&class);
                let hwnd = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("WitnessDialogTestOwner"),
                    w!("Witness dialog owner (test)"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    200,
                    200,
                    460,
                    620,
                    None,
                    None,
                    Some(module.into()),
                    None,
                )
                .unwrap();
                tx.send((hwnd.0 as isize, GetCurrentThreadId())).unwrap();
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        });
        let (owner, ui_tid) = rx.recv().unwrap();
        let owner_hwnd = HWND(owner as *mut core::ffi::c_void);
        let hold = std::env::var("WITNESS_TEST_DIALOG_HOLD_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0u64);

        /// The visible top-level window owned by `owner`, if any.
        fn owned_by(owner: isize) -> Option<isize> {
            struct Search {
                owner: isize,
                found: Option<isize>,
            }
            unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> windows::core::BOOL {
                // SAFETY: lp points at the Search below for the whole enumeration.
                unsafe {
                    let search = &mut *(lp.0 as *mut Search);
                    let owned =
                        GetWindow(hwnd, GW_OWNER).is_ok_and(|o| o.0 as isize == search.owner);
                    if owned && IsWindowVisible(hwnd).as_bool() {
                        search.found = Some(hwnd.0 as isize);
                        return false.into();
                    }
                    true.into()
                }
            }
            let mut search = Search { owner, found: None };
            // SAFETY: the callback only runs during this call.
            let _ = unsafe { EnumWindows(Some(each), LPARAM(&mut search as *mut Search as isize)) };
            search.found
        }

        let check = |kind: &str,
                     mta: bool,
                     title: Option<&str>,
                     open: Box<dyn FnOnce() -> Option<PathBuf> + Send>| {
            let worker = std::thread::spawn(move || {
                if mta {
                    // SAFETY: COM init on this worker, undone below.
                    let hr =
                        unsafe { com::CoInitializeEx(std::ptr::null(), com::COINIT_MULTITHREADED) };
                    assert!(hr >= 0, "MTA init failed: {hr:#x}");
                }
                let picked = open();
                if mta {
                    // SAFETY: pairs the CoInitializeEx above.
                    unsafe { com::CoUninitialize() };
                }
                picked
            });
            let deadline = Instant::now() + Duration::from_secs(15);
            let dialog = loop {
                if let Some(dialog) = owned_by(owner) {
                    break dialog;
                }
                assert!(
                    Instant::now() < deadline,
                    "{kind} dialog never appeared owned by the window"
                );
                std::thread::sleep(Duration::from_millis(50));
            };
            let dialog_hwnd = HWND(dialog as *mut core::ffi::c_void);
            // SAFETY: plain queries / posts on live windows.
            unsafe {
                assert!(
                    !IsWindowEnabled(owner_hwnd).as_bool(),
                    "{kind} dialog is not modal"
                );
                if let Some(title) = title {
                    let mut buf = [0u16; 256];
                    let n = GetWindowTextW(dialog_hwnd, &mut buf) as usize;
                    assert_eq!(String::from_utf16_lossy(&buf[..n]), title);
                }
                println!(
                        "{kind} dialog (mta worker: {mta}) {dialog:#x} owned by {owner:#x}, owner disabled"
                    );
                std::thread::sleep(Duration::from_millis(hold));
                PostMessageW(
                    Some(dialog_hwnd),
                    WM_COMMAND,
                    WPARAM(2 /* IDCANCEL */),
                    LPARAM(0),
                )
                .unwrap();
            }
            assert_eq!(worker.join().unwrap(), None, "cancel reads as None");
            // SAFETY: plain query.
            assert!(
                unsafe { IsWindowEnabled(owner_hwnd) }.as_bool(),
                "owner re-enabled"
            );
        };

        let save = move || {
            save_file_blocking(
                OwnerHwnd(owner),
                SaveFileRequest {
                    file_name: "Weekly sync.opus".into(),
                    filter_name: "Ogg Opus audio".into(),
                    extensions: vec!["opus".into()],
                },
            )
        };
        let title = "Choose where to create the Witness backup";
        let folder = move || pick_folder_blocking(OwnerHwnd(owner), Some(title));
        for mta in [false, true] {
            check("save", mta, None, Box::new(save));
            check("folder", mta, Some(title), Box::new(folder));
        }

        // SAFETY: ends the owner thread's message loop.
        unsafe { PostThreadMessageW(ui_tid, WM_QUIT, WPARAM(0), LPARAM(0)).unwrap() };
        ui.join().unwrap();
    }
}
