//! Phosphor (regular) icons as font glyphs — they tint with the text colour
//! like `fill=currentColor` did — plus the two animations: the spinning
//! `CircleNotch` and the 1.2 s recording pulse. Both are static when Windows
//! "Show animations" is off (`SPI_GETCLIENTAREAANIMATION`).
//!
//! Animations never request repaints themselves: a hidden root still runs
//! full passes on Windows, so an unconditional repaint would burn CPU in the
//! tray. They flag the pass instead, and `app.rs` schedules the next frame
//! only while the window is shown and not minimized (design §4.2).

use eframe::egui::{self, Color32, RichText};
use std::sync::atomic::{AtomicBool, Ordering};

pub use egui_phosphor::regular as ph;

static ANIMATIONS: AtomicBool = AtomicBool::new(true);

/// Set at theme install from `platform::system::client_area_animations()`.
pub fn set_animations(enabled: bool) {
    ANIMATIONS.store(enabled, Ordering::Relaxed);
}

/// Whether spinners spin and the recording dot pulses.
pub fn animations_enabled() -> bool {
    ANIMATIONS.load(Ordering::Relaxed)
}

fn animation_flag_id() -> egui::Id {
    egui::Id::new("witness-animation-requested")
}

/// Record that this pass drew something animated.
pub fn request_animation_frame(ctx: &egui::Context) {
    if animations_enabled() {
        ctx.data_mut(|d| d.insert_temp(animation_flag_id(), true));
    }
}

/// Whether anything animated was drawn since the last call (clears the flag).
/// `app.rs` turns this into a 33 ms repaint while the window is visible.
pub fn take_animation_request(ctx: &egui::Context) -> bool {
    ctx.data_mut(|d| d.remove_temp::<bool>(animation_flag_id()))
        .unwrap_or(false)
}

/// A Phosphor glyph as rich text at `size` points.
pub fn icon(glyph: &str, size: f32) -> RichText {
    RichText::new(glyph).size(size)
}

/// Paint the spinner glyph centred in `rect` (for custom widgets, e.g. a
/// spinner inside a button).
pub fn paint_spinner(ui: &egui::Ui, rect: egui::Rect, size: f32, color: Color32) {
    let angle = if animations_enabled() {
        request_animation_frame(ui.ctx());
        let t = ui.input(|i| i.time);
        (t.fract() as f32) * std::f32::consts::TAU
    } else {
        0.0
    };
    paint_rotated_glyph(
        ui.painter(),
        rect.center(),
        ph::CIRCLE_NOTCH,
        size,
        color,
        angle,
    );
}

/// Draw `glyph` rotated by `angle` (clockwise radians) about its ink centre,
/// which sits at `center`.
pub fn paint_rotated_glyph(
    painter: &egui::Painter,
    center: egui::Pos2,
    glyph: &str,
    size: f32,
    color: Color32,
    angle: f32,
) {
    let galley = painter.layout_no_wrap(glyph.to_string(), egui::FontId::proportional(size), color);
    // Rotate about the glyph's ink box, not the line box, so it doesn't wobble.
    let pivot = if galley.mesh_bounds.is_positive() {
        galley.mesh_bounds.center().to_vec2()
    } else {
        galley.rect.center().to_vec2()
    };
    let pos = center - egui::emath::Rot2::from_angle(angle) * pivot;
    painter.add(egui::epaint::TextShape::new(pos, galley, color).with_angle(angle));
}

/// Opacity of the pulsing recording dot/icon: 1 → 0.3 → 1 over 1.2 s
/// (ease-in-out). 1.0 when animations are off.
pub fn pulse_alpha(ctx: &egui::Context) -> f32 {
    if !animations_enabled() {
        return 1.0;
    }
    request_animation_frame(ctx);
    let t = ctx.input(|i| i.time);
    pulse_at(t)
}

fn pulse_at(t: f64) -> f32 {
    let phase = (t % 1.2) / 1.2;
    let wave = 0.5 + 0.5 * (phase * std::f64::consts::TAU).cos();
    (0.3 + 0.7 * wave) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_goes_from_one_to_point_three_and_back() {
        assert!((pulse_at(0.0) - 1.0).abs() < 1e-6);
        assert!((pulse_at(0.6) - 0.3).abs() < 1e-6);
        assert!((pulse_at(1.2) - 1.0).abs() < 1e-4);
        assert!(pulse_at(0.3) > 0.3 && pulse_at(0.3) < 1.0);
    }
}
