//! Headless snapshot test — the CI-story proof.
//!
//! This renders a real Scrozz surface with **no display server**: `egui_kittest`
//! drives an offscreen `wgpu` device, rasterises the frame, and diffs it against
//! a committed PNG baseline under `tests/snapshots/`. That is exactly what an
//! agent (or CI on Linux/Windows/macOS) needs to verify UI without a screen.
//!
//! First run writes the baseline:
//!     UPDATE_SNAPSHOTS=1 cargo test --test snapshot
//! Thereafter a plain `cargo test` fails if the pixels drift.
//!
//! The UI layer (theme / icons / paint / surfaces) has zero window/app coupling,
//! so we include those four modules directly. This is the *same* drawing code
//! the real binary runs — not a reimplementation.

#[path = "../src/theme.rs"]
mod theme;
#[path = "../src/icons.rs"]
mod icons;
#[path = "../src/motion.rs"]
mod motion;
#[path = "../src/paint.rs"]
mod paint;
#[path = "../src/surfaces.rs"]
mod surfaces;

use egui_kittest::Harness;
use icons::IconStore;
use surfaces::Surface;

/// Renders the Quick Access Overlay (the primary surface) over the in-egui
/// wallpaper backdrop and snapshots it deterministically.
#[test]
fn quick_access_headless_snapshot() {
    let pad = 32.0_f32;
    let card = Surface::Quick.size();
    // egui_kittest wraps the UI closure in a CentralPanel with an 8px *outer*
    // margin, so `ui.max_rect()` is inset 8px per side. Enlarge the harness by
    // that margin on every edge so the drawable content area equals the real
    // window's inner size (card + pad on all sides) and the layout matches the
    // windowed capture exactly.
    let kittest_margin = 8.0_f32;
    let content = egui::vec2(card.x + pad * 2.0, card.y + pad * 2.0);
    let size = content + egui::vec2(kittest_margin * 2.0, kittest_margin * 2.0);

    let mut harness = Harness::builder()
        .with_size(size)
        .with_pixels_per_point(2.0)
        .with_theme(egui::Theme::Dark)
        .with_max_steps(16)
        .wgpu()
        .build_ui_state(
            move |ui, store: &mut Option<IconStore>| {
                let ctx = ui.ctx().clone();
                // First pass: install the custom fonts + Style and rasterise the
                // SVG icon textures, then bail out WITHOUT drawing. egui applies
                // `set_fonts` at the *next* begin-pass, so the custom families
                // ("medium"/"semibold"/"bold") aren't bound yet on this pass —
                // drawing surface text now would panic. In the real binary this
                // happens in `CreationContext` before frame 1; headless we fake
                // that by skipping one frame and forcing a repaint.
                if store.is_none() {
                    paint::set_screenshot(true);
                    theme::install_fonts(&ctx);
                    theme::install_style(&ctx, &theme::Palette::dark());
                    *store = Some(IconStore::new(&ctx));
                    ctx.request_repaint();
                    return;
                }

                let pal = theme::Palette::dark();
                let screen = ui.max_rect();
                paint::wallpaper(ui.painter(), screen, pal.is_dark);
                let card = screen.shrink(pad);
                Surface::Quick.show(ui, store.as_ref().unwrap(), &pal, card);
            },
            Option::<IconStore>::None,
        );

    harness.run();
    harness.snapshot("quick_access");
}
