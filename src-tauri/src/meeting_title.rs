//! Best-effort meeting names from the Teams window title. During a call the
//! meeting window's title usually carries the meeting subject, ending in
//! "| Microsoft Teams". Reading window titles is far more robust than UI
//! scraping — worst case we find nothing and the timestamp name stays.
//!
//! The main Teams window and the call window belong to the same process, and
//! the main window's title is whatever chat/tab happens to be open — which
//! can look meeting-ish. The watcher snapshots Teams window handles while no
//! meeting is active (`teams_window_handles`), and naming prefers windows
//! that appeared since: those are the ones the call spawned.

/// Titles that are Teams app tabs, not meeting subjects (after stripping
/// the "| Microsoft Teams" suffix).
const GENERIC_TITLES: &[&str] = &[
    "",
    "microsoft teams",
    "chat",
    "activity",
    "calendar",
    "communities",
    "teams",
    "teams and channels",
    "onedrive",
    "apps",
    "calls",
];

/// Visible top-level windows of any Teams process: (hwnd, raw title).
#[cfg(windows)]
fn teams_windows() -> Vec<(isize, String)> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
        IsWindowVisible,
    };

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let windows = unsafe { &mut *(lparam.0 as *mut Vec<(isize, String)>) };
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return BOOL(1);
            }
            let len = GetWindowTextLengthW(hwnd);
            if len == 0 {
                return BOOL(1);
            }
            // Only windows belonging to the Teams process.
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 {
                return BOOL(1);
            }
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return BOOL(1);
            };
            let mut exe_buf = [0u16; 512];
            let mut exe_len = exe_buf.len() as u32;
            let is_teams = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(exe_buf.as_mut_ptr()),
                &mut exe_len,
            )
            .is_ok()
                && String::from_utf16_lossy(&exe_buf[..exe_len as usize])
                    .to_lowercase()
                    .contains("teams");
            let _ = windows::Win32::Foundation::CloseHandle(handle);
            if !is_teams {
                return BOOL(1);
            }

            let mut buf = vec![0u16; len as usize + 1];
            let read = GetWindowTextW(hwnd, &mut buf);
            if read > 0 {
                windows.push((hwnd.0 as isize, String::from_utf16_lossy(&buf[..read as usize])));
            }
        }
        BOOL(1)
    }

    let mut windows: Vec<(isize, String)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut windows as *mut _ as isize));
    }
    windows
}

#[cfg(not(windows))]
fn teams_windows() -> Vec<(isize, String)> {
    Vec::new()
}

/// Handles of the Teams windows that exist right now. The watcher calls this
/// while the mic is idle so `find_teams_meeting_title` can tell call windows
/// (spawned with the call) from the long-lived main window.
pub fn teams_window_handles() -> Vec<isize> {
    teams_windows().into_iter().map(|(hwnd, _)| hwnd).collect()
}

/// `pre_call` = window handles snapshotted before the call started. Windows
/// not in it were spawned by the call and are preferred; if none survive
/// (stale/empty baseline, or Teams reuses the main window), fall back to all.
pub fn find_teams_meeting_title(pre_call: &[isize]) -> Option<String> {
    let windows = teams_windows();
    let fresh: Vec<&String> = windows
        .iter()
        .filter(|(hwnd, _)| !pre_call.contains(hwnd))
        .map(|(_, title)| title)
        .collect();
    let pool: Vec<&String> = if fresh.is_empty() {
        windows.iter().map(|(_, title)| title).collect()
    } else {
        fresh
    };

    // Strip the app suffix, drop generic tab names, prefer meeting-ish ones.
    let mut candidates: Vec<String> = pool
        .into_iter()
        .map(|t| {
            t.trim_end_matches("| Microsoft Teams")
                .trim_end_matches('|')
                .trim()
                .to_string()
        })
        .filter(|t| !GENERIC_TITLES.contains(&t.to_lowercase().as_str()))
        .filter(|t| t.len() >= 3 && t.len() <= 120)
        .collect();
    candidates.sort_by_key(|t| {
        // Meeting-ish titles first, then longer ones.
        let meetingish = t.to_lowercase().contains("meeting") || t.contains(',');
        (std::cmp::Reverse(meetingish), std::cmp::Reverse(t.len()))
    });
    candidates.into_iter().next()
}
