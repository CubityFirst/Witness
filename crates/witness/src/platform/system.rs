//! Small, stateless Win32 queries the shell and the UI need: monitor work
//! areas and DPI, our own top-level windows, the accessibility settings
//! WebView2 used to honour for free, and an error MessageBox for the paths
//! that run before (or after) there is a window to show anything in.

use winit::dpi::PhysicalPosition;

/// A rectangle in physical pixels (x/y = top-left).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RectI {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl RectI {
    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    /// True when `p` lies inside the rectangle grown by `margin` on every side.
    pub fn contains_with_margin(&self, p: PhysicalPosition<i32>, margin: i32) -> bool {
        p.x >= self.x - margin
            && p.x < self.right() + margin
            && p.y >= self.y - margin
            && p.y < self.bottom() + margin
    }
}

#[cfg(windows)]
mod imp {
    use super::RectI;
    use windows::core::{BOOL, PCWSTR};
    use windows::Win32::Foundation::{HWND, POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowExW, GetCursorPos, GetWindowThreadProcessId, MessageBoxW, SetWindowPos,
        SystemParametersInfoW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, SPI_GETCLIENTAREAANIMATION,
        SPI_GETHIGHCONTRAST, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOZORDER,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    use winit::dpi::PhysicalPosition;

    pub(crate) fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn rect_from(r: RECT) -> RectI {
        RectI {
            x: r.left,
            y: r.top,
            w: r.right - r.left,
            h: r.bottom - r.top,
        }
    }

    pub fn work_area_at(p: PhysicalPosition<i32>) -> RectI {
        // SAFETY: plain Win32 queries on stack-owned out-parameters.
        unsafe {
            let monitor = MonitorFromPoint(POINT { x: p.x, y: p.y }, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(monitor, &mut info).as_bool() {
                return rect_from(info.rcWork);
            }
            // The primary monitor's work area is the best fallback there is.
            let mut work = RECT::default();
            if SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some(&mut work as *mut RECT as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .is_ok()
            {
                return rect_from(work);
            }
        }
        RectI {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        }
    }

    pub fn dpi_at(p: PhysicalPosition<i32>) -> u32 {
        // SAFETY: plain Win32 queries on stack-owned out-parameters.
        unsafe {
            let monitor = MonitorFromPoint(POINT { x: p.x, y: p.y }, MONITOR_DEFAULTTONEAREST);
            let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
            match GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) {
                Ok(()) if dpi_x > 0 => dpi_x,
                _ => 96,
            }
        }
    }

    pub fn find_own_window(title: &str) -> Option<isize> {
        let title = wide(title);
        // SAFETY: FindWindowExW walks top-level windows; the title buffer
        // outlives every call.
        unsafe {
            let own_pid = GetCurrentProcessId();
            let mut after: Option<HWND> = None;
            // Another process may own a window with the same title; keep
            // looking past it.
            for _ in 0..64 {
                let hwnd =
                    FindWindowExW(None, after, PCWSTR::null(), PCWSTR(title.as_ptr())).ok()?;
                if hwnd.is_invalid() {
                    return None;
                }
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                if pid == own_pid {
                    return Some(hwnd.0 as isize);
                }
                after = Some(hwnd);
            }
        }
        None
    }

    pub fn set_window_rect(hwnd: isize, r: RectI) {
        // SAFETY: SetWindowPos tolerates a stale handle (it just fails).
        let result = unsafe {
            SetWindowPos(
                HWND(hwnd as *mut _),
                None,
                r.x,
                r.y,
                r.w,
                r.h,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
        if let Err(error) = result {
            log::debug!("SetWindowPos failed: {error}");
        }
    }

    pub fn high_contrast_on() -> bool {
        let mut hc = HIGHCONTRASTW {
            cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
            ..Default::default()
        };
        // SAFETY: the out-parameter is a correctly sized HIGHCONTRASTW.
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETHIGHCONTRAST,
                hc.cbSize,
                Some(&mut hc as *mut HIGHCONTRASTW as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        }
        .is_ok();
        ok && hc.dwFlags.contains(HCF_HIGHCONTRASTON)
    }

    pub fn client_area_animations() -> bool {
        let mut enabled = BOOL(1);
        // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL.
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETCLIENTAREAANIMATION,
                0,
                Some(&mut enabled as *mut BOOL as *mut _),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        }
        .is_ok();
        !ok || enabled.as_bool()
    }

    pub fn cursor_pos() -> PhysicalPosition<i32> {
        let mut p = POINT::default();
        // SAFETY: GetCursorPos writes one POINT.
        if unsafe { GetCursorPos(&mut p) }.is_err() {
            return PhysicalPosition::new(i32::MIN / 2, i32::MIN / 2);
        }
        PhysicalPosition::new(p.x, p.y)
    }

    pub fn error_box(text: &str) {
        let text = wide(text);
        let caption = wide("Witness");
        // SAFETY: both buffers outlive the (modal) call.
        unsafe {
            MessageBoxW(
                None,
                PCWSTR(text.as_ptr()),
                PCWSTR(caption.as_ptr()),
                MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
            );
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::RectI;
    use winit::dpi::PhysicalPosition;

    pub fn work_area_at(_p: PhysicalPosition<i32>) -> RectI {
        RectI {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        }
    }
    pub fn dpi_at(_p: PhysicalPosition<i32>) -> u32 {
        96
    }
    pub fn find_own_window(_title: &str) -> Option<isize> {
        None
    }
    pub fn set_window_rect(_hwnd: isize, _r: RectI) {}
    pub fn high_contrast_on() -> bool {
        false
    }
    pub fn client_area_animations() -> bool {
        true
    }
    pub fn cursor_pos() -> PhysicalPosition<i32> {
        PhysicalPosition::new(0, 0)
    }
    pub fn error_box(text: &str) {
        eprintln!("{text}");
    }
}

/// Work area (screen minus taskbar) of the monitor nearest to `p`.
/// `MonitorFromPoint(MONITOR_DEFAULTTONEAREST)` → `rcWork`.
pub fn work_area_at(p: PhysicalPosition<i32>) -> RectI {
    imp::work_area_at(p)
}

/// Effective DPI of the monitor nearest to `p` (96 = 100 %), 96 on failure.
pub fn dpi_at(p: PhysicalPosition<i32>) -> u32 {
    imp::dpi_at(p)
}

/// A top-level window of *this* process with exactly this title (raw HWND).
/// Used to reach eframe's child viewports, which expose no handle.
pub fn find_own_window(title: &str) -> Option<isize> {
    imp::find_own_window(title)
}

/// Move/resize a window in physical pixels without activating it or
/// changing its z-order (`SWP_NOZORDER | SWP_NOACTIVATE`).
pub fn set_window_rect(hwnd: isize, r: RectI) {
    imp::set_window_rect(hwnd, r)
}

/// The Windows "Text size" accessibility setting as a factor (1.0–2.25),
/// from `HKCU\Software\Microsoft\Accessibility\TextScaleFactor` (100–225).
/// WebView2 applied it automatically; the native UI multiplies it into the
/// zoom factor and re-reads it on every focus gain.
pub fn text_scale_factor() -> f32 {
    text_scale_from_registry().unwrap_or(1.0)
}

#[cfg(windows)]
fn text_scale_from_registry() -> Option<f32> {
    use winreg::enums::HKEY_CURRENT_USER;
    let key = winreg::RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Accessibility")
        .ok()?;
    let percent: u32 = key.get_value("TextScaleFactor").ok()?;
    Some(text_scale_from_percent(percent))
}

#[cfg(not(windows))]
fn text_scale_from_registry() -> Option<f32> {
    None
}

/// Clamp the registry value to the range Settings can produce.
fn text_scale_from_percent(percent: u32) -> f32 {
    percent.clamp(100, 225) as f32 / 100.0
}

#[cfg(windows)]
const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";

/// Windows' "Choose your app mode": `Some(true)` for Light, `Some(false)`
/// for Dark (`Personalize\AppsUseLightTheme`), `None` when unknown.
pub fn apps_use_light_theme() -> Option<bool> {
    personalize_flag("AppsUseLightTheme")
}

/// Windows' "Choose your default Windows mode", which the taskbar and its
/// notification area follow: `Some(true)` for Light
/// (`Personalize\SystemUsesLightTheme`), `None` when unknown.
pub fn system_uses_light_theme() -> Option<bool> {
    personalize_flag("SystemUsesLightTheme")
}

fn personalize_flag(name: &str) -> Option<bool> {
    #[cfg(windows)]
    {
        use winreg::enums::HKEY_CURRENT_USER;
        let key = winreg::RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(PERSONALIZE)
            .ok()?;
        let value: u32 = key.get_value(name).ok()?;
        Some(value != 0)
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        None
    }
}

/// Blocks the calling thread for good, calling `changed` after every write
/// under `Personalize` (both theme modes, transparency). Returns only if the
/// key cannot be watched. Event-driven, so a hidden app costs nothing.
pub fn watch_personalize(mut changed: impl FnMut()) {
    #[cfg(windows)]
    {
        use windows::Win32::System::Registry::{
            RegNotifyChangeKeyValue, HKEY, REG_NOTIFY_CHANGE_LAST_SET,
        };
        use winreg::enums::{HKEY_CURRENT_USER, KEY_NOTIFY};
        let key = match winreg::RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(PERSONALIZE, KEY_NOTIFY)
        {
            Ok(key) => key,
            Err(error) => {
                log::warn!("cannot watch the Windows theme: {error}");
                return;
            }
        };
        let hkey = HKEY(key.raw_handle() as *mut _);
        loop {
            // SAFETY: `key` stays open for the whole loop; a synchronous
            // watch takes no event handle and returns after one change.
            let status = unsafe {
                RegNotifyChangeKeyValue(hkey, false, REG_NOTIFY_CHANGE_LAST_SET, None, false)
            };
            if status.is_err() {
                log::warn!("watching the Windows theme stopped: {status:?}");
                return;
            }
            changed();
        }
    }
    #[cfg(not(windows))]
    {
        let _ = &mut changed;
    }
}

/// Windows' accent colour ramp from `Explorer\Accent\AccentPalette`: seven
/// RGB shades, light3, light2, light1, base, dark1, dark2, dark3 (the
/// value holds eight RGBA entries; the last is unused).
pub fn accent_palette() -> Option<[[u8; 3]; 7]> {
    #[cfg(windows)]
    {
        use winreg::enums::HKEY_CURRENT_USER;
        let key = winreg::RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent")
            .ok()?;
        let raw = key.get_raw_value("AccentPalette").ok()?;
        accent_palette_from_bytes(&raw.bytes)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn accent_palette_from_bytes(bytes: &[u8]) -> Option<[[u8; 3]; 7]> {
    if bytes.len() < 28 {
        return None;
    }
    let mut out = [[0u8; 3]; 7];
    for (i, shade) in out.iter_mut().enumerate() {
        shade.copy_from_slice(&bytes[i * 4..i * 4 + 3]);
    }
    Some(out)
}

/// Whether a Windows high-contrast theme is active (`SPI_GETHIGHCONTRAST`).
pub fn high_contrast_on() -> bool {
    imp::high_contrast_on()
}

/// "Show animations in Windows" (`SPI_GETCLIENTAREAANIMATION`); spinners and
/// the recording pulse are static when it is off. True if the query fails.
pub fn client_area_animations() -> bool {
    imp::client_area_animations()
}

/// The mouse cursor in physical screen pixels (`GetCursorPos`).
pub fn cursor_pos() -> PhysicalPosition<i32> {
    imp::cursor_pos()
}

/// A modal error MessageBox with no owner window: for startup failures and
/// the panic hook, where there is no (usable) UI to report through.
pub fn error_box(text: &str) {
    imp::error_box(text)
}

#[cfg(test)]
mod tests {
    #[test]
    fn accent_palette_bytes_are_rgba_entries() {
        let mut raw = Vec::new();
        for i in 0..8u8 {
            raw.extend_from_slice(&[i, i + 10, i + 20, 0xff]);
        }
        let shades = super::accent_palette_from_bytes(&raw).unwrap();
        assert_eq!(shades[0], [0, 10, 20]);
        assert_eq!(shades[6], [6, 16, 26]);
        assert!(super::accent_palette_from_bytes(&raw[..20]).is_none());
    }

    use super::*;

    #[test]
    fn text_scale_is_clamped_to_the_settings_range() {
        assert_eq!(text_scale_from_percent(100), 1.0);
        assert_eq!(text_scale_from_percent(150), 1.5);
        assert_eq!(text_scale_from_percent(50), 1.0);
        assert_eq!(text_scale_from_percent(400), 2.25);
    }

    #[test]
    fn rect_margin_containment() {
        let r = RectI {
            x: 100,
            y: 100,
            w: 50,
            h: 40,
        };
        assert!(r.contains_with_margin(PhysicalPosition::new(100, 100), 0));
        assert!(!r.contains_with_margin(PhysicalPosition::new(150, 100), 0));
        assert!(r.contains_with_margin(PhysicalPosition::new(165, 155), 16));
        assert!(!r.contains_with_margin(PhysicalPosition::new(83, 120), 16));
    }

    #[test]
    fn system_queries_do_not_panic() {
        let p = PhysicalPosition::new(0, 0);
        let work = work_area_at(p);
        assert!(work.w > 0 && work.h > 0);
        assert!(dpi_at(p) >= 96);
        let _ = high_contrast_on();
        let _ = client_area_animations();
        let _ = cursor_pos();
        assert!(find_own_window("no such witness window title ✓").is_none());
    }
}
