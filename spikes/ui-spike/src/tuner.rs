//! The live motion-tuning overlay (`M`).
//!
//! Deliberately built from stock egui widgets rather than the hand-drawn
//! primitives the rest of the spike uses: this is a dev tool, and its only job
//! is to let the feel be dialled in without an edit-recompile loop. It writes
//! straight into `motion`'s globals, so every animation in the app — including
//! ones already in flight — picks the change up on the next frame.

use crate::motion;
use crate::theme::Palette;
use egui::{vec2, Color32, Context, Pos2, Rect, Sense, Stroke};

#[derive(Default)]
pub struct Actions {
    pub replay: bool,
    pub spawn: bool,
    pub dismiss: bool,
}

pub fn show(ctx: &Context, open: &mut bool, pal: &Palette) -> Actions {
    let mut act = Actions::default();
    let mut win_open = *open;

    egui::Window::new("Motion")
        .open(&mut win_open)
        .resizable(false)
        .default_pos([24.0, 24.0])
        .show(ctx, |ui| {
            ui.spacing_mut().slider_width = 190.0;

            // ---- global duration multiplier --------------------------------
            let mut s = motion::scale();
            ui.horizontal(|ui| {
                ui.label("Duration");
                if ui.add(egui::Slider::new(&mut s, 0.25..=3.0).suffix("×").fixed_decimals(2)).changed() {
                    motion::set_scale(s);
                }
            });
            ui.horizontal(|ui| {
                for preset in [0.25_f32, 0.5, 1.0, 2.0, 3.0] {
                    if ui.small_button(format!("{preset}×")).clicked() {
                        motion::set_scale(preset);
                    }
                }
            });
            ui.label(
                egui::RichText::new(format!(
                    "fast {:.0}ms · base {:.0}ms · slow {:.0}ms",
                    motion::dur(motion::FAST) * 1000.0,
                    motion::dur(motion::BASE) * 1000.0,
                    motion::dur(motion::SLOW) * 1000.0,
                ))
                .small()
                .color(pal.text_faint),
            );

            ui.separator();

            // ---- easing override -------------------------------------------
            ui.label("Easing");
            let cur = motion::curve_override();
            egui::ComboBox::from_id_salt("motion-curve")
                .selected_text(motion::curve_override_name())
                .width(210.0)
                .show_ui(ui, |ui| {
                    let mut pick = cur;
                    ui.selectable_value(&mut pick, 0, "as designed (per call site)");
                    for (i, (name, _)) in motion::CURVES.iter().enumerate() {
                        ui.selectable_value(&mut pick, i + 1, *name);
                    }
                    if pick != cur {
                        motion::set_curve_override(pick);
                    }
                });
            curve_preview(ui, pal);

            ui.separator();

            // ---- accessibility ----------------------------------------------
            let mut reduce = motion::reduced();
            if ui
                .checkbox(&mut reduce, "Reduce motion (D13) — collapse every duration to 0")
                .changed()
            {
                motion::set_reduce(reduce);
            }

            ui.separator();

            ui.horizontal(|ui| {
                act.replay = ui.button("Replay entry  (R)").clicked();
                act.spawn = ui.button("New capture  (N)").clicked();
                act.dismiss = ui.button("Dismiss  (⌫)").clicked();
            });
            ui.label(
                egui::RichText::new("Drag the top card to swipe it away. Flick fast to throw it.")
                    .small()
                    .color(pal.text_faint),
            );
        });

    *open = win_open;
    act
}

/// A small graph of the currently selected curve, with a dot sweeping it in
/// real time — the quickest way to see what "spring overshoot" actually means
/// before hunting for it on a card.
fn curve_preview(ui: &mut egui::Ui, pal: &Palette) {
    let (rect, _) = ui.allocate_exact_size(vec2(210.0, 72.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, crate::theme::cr(6.0), Color32::from_black_alpha(40));

    let idx = motion::curve_override();
    let ease: motion::Ease = if idx == 0 {
        motion::ease_out_cubic
    } else {
        motion::CURVES[(idx - 1).min(motion::CURVES.len() - 1)].1
    };

    // Overshooting curves leave the 0..1 band, so the plot is padded.
    let pad = 12.0;
    let plot = Rect::from_min_max(
        rect.min + vec2(6.0, pad),
        rect.max - vec2(6.0, pad),
    );
    let at = |t: f32, v: f32| -> Pos2 {
        egui::pos2(plot.left() + t * plot.width(), plot.bottom() - v * plot.height())
    };

    p.line_segment([at(0.0, 0.0), at(1.0, 0.0)], Stroke::new(1.0, pal.hairline));
    p.line_segment([at(0.0, 1.0), at(1.0, 1.0)], Stroke::new(1.0, pal.hairline));

    let pts: Vec<Pos2> = (0..=64).map(|i| { let t = i as f32 / 64.0; at(t, ease(t)) }).collect();
    p.add(egui::Shape::line(pts, Stroke::new(1.6, pal.accent)));

    // The sweeping dot uses the same clock the real animations do, so a change
    // to the duration multiplier is visible here too.
    let period = motion::dur(motion::SLOW).max(0.05) * 3.0;
    let t = ((ui.input(|i| i.time) as f32 / period).fract()).clamp(0.0, 1.0);
    p.circle_filled(at(t, ease(t)), 3.5, pal.accent_hi);
    ui.ctx().request_repaint(); // the preview is the one thing that animates forever
}
