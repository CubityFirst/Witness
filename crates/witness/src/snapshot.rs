//! Debug builds only: render one view off screen and save it as a BMP.
//!
//! `WITNESS_DEBUG_SNAPSHOT=<out.bmp>` turns a launch into a passive visual
//! check: the window is DWM-cloaked and never activated (nothing appears on
//! screen, the focus stays where it is), no tray icon or hotkeys are taken,
//! and a separate single-instance name lets it run beside a real instance.
//! After `WITNESS_DEBUG_SNAPSHOT_DELAY_MS` (default 2500, enough for the
//! view's first loads) the app screenshots its own framebuffer, writes it
//! and exits. `WITNESS_DEBUG_SIZE=<w>x<h>` (logical points, default 460x620)
//! sets the window size, below the normal minimum if asked, so narrow
//! layouts can be checked. Combine with `WITNESS_DEBUG_START_VIEW` and a
//! scratch `WITNESS_SETTINGS` (auto-record off): the core runs for real.

use eframe::egui;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Config {
    pub out: PathBuf,
    /// Logical window size.
    pub size: [f32; 2],
    pub delay: Duration,
}

/// The snapshot request, if this is a debug build launched with one.
pub fn config() -> Option<Config> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let out = PathBuf::from(std::env::var_os("WITNESS_DEBUG_SNAPSHOT")?);
    let size = std::env::var("WITNESS_DEBUG_SIZE")
        .ok()
        .and_then(|s| parse_size(&s))
        .unwrap_or([460.0, 620.0]);
    let delay = std::env::var("WITNESS_DEBUG_SNAPSHOT_DELAY_MS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(2500));
    Some(Config { out, size, delay })
}

fn parse_size(spec: &str) -> Option<[f32; 2]> {
    let (w, h) = spec.trim().split_once(['x', 'X'])?;
    let (w, h): (f32, f32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w >= 100.0 && h >= 100.0).then_some([w, h])
}

/// Drives one snapshot: wait, request the screenshot, save it, exit.
pub struct Snapshot {
    config: Config,
    started: Instant,
    requested: bool,
}

impl Snapshot {
    pub fn new(config: Config) -> Snapshot {
        Snapshot {
            config,
            started: Instant::now(),
            requested: false,
        }
    }

    /// Call from `ui()` every pass.
    pub fn tick(&mut self, ctx: &egui::Context) {
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = shot {
            let code = match write_bmp(&self.config.out, &image) {
                Ok(()) => {
                    log::info!(
                        "snapshot written: {} ({}x{})",
                        self.config.out.display(),
                        image.size[0],
                        image.size[1]
                    );
                    0
                }
                Err(error) => {
                    log::error!("snapshot {}: {error}", self.config.out.display());
                    1
                }
            };
            log::logger().flush();
            std::process::exit(code);
        }
        let waited = self.started.elapsed();
        if !self.requested && waited >= self.config.delay {
            self.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        if waited > self.config.delay + Duration::from_secs(10) {
            log::error!("snapshot: no screenshot arrived; giving up");
            log::logger().flush();
            std::process::exit(2);
        }
        ctx.request_repaint_after(Duration::from_millis(50));
    }
}

/// 32-bit top-down BMP: no image crate needed.
fn write_bmp(path: &Path, image: &egui::ColorImage) -> std::io::Result<()> {
    let [w, h] = image.size;
    let pixels = (w * h * 4) as u32;
    let mut out = Vec::with_capacity(54 + pixels as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54 + pixels).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(-(h as i32)).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&pixels.to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    for p in &image.pixels {
        let [r, g, b, _] = p.to_array();
        out.extend_from_slice(&[b, g, r, 255]);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("380x700"), Some([380.0, 700.0]));
        assert_eq!(parse_size(" 1200 X 800 "), Some([1200.0, 800.0]));
        assert_eq!(parse_size("50x50"), None);
        assert_eq!(parse_size("wide"), None);
    }
}
