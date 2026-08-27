//! Scrozz UI spike — entry point.
//!
//! THROWAWAY. Proves whether Rust + egui can reach CleanShot-grade polish.
//! No real functionality; every surface is faked.
//!
//! Interactive:  cargo run
//! Screenshot:   cargo run -- --surface quick --theme dark --backdrop on --shot screenshots/quick.png

mod app;
mod icons;
mod motion;
mod paint;
mod stack;
mod surfaces;
mod theme;
mod tuner;
mod vibrancy;

use app::Config;
use eframe::egui;
use surfaces::{QuickVariant, Surface};
use vibrancy::Material;

fn parse_args() -> Config {
    let mut surface = Surface::Quick;
    let mut quick_variant = QuickVariant::Stack;
    let mut theme_dark = true;
    let mut backdrop = true;
    let mut material_override: Option<Material> = None;
    let mut shot = None;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--surface" => {
                i += 1;
                surface = match args.get(i).map(String::as_str) {
                    Some("menu") => Surface::Menu,
                    Some("annotate") => Surface::Annotate,
                    Some("onboard") => Surface::Onboard,
                    _ => Surface::Quick,
                };
            }
            "--variant" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    quick_variant = QuickVariant::parse(v);
                }
            }
            "--theme" => {
                i += 1;
                theme_dark = args.get(i).map(|s| s != "light").unwrap_or(true);
            }
            "--backdrop" => {
                i += 1;
                backdrop = args.get(i).map(|s| s != "off").unwrap_or(true);
            }
            "--material" => {
                i += 1;
                material_override = args.get(i).map(|s| Material::parse(s));
            }
            "--shot" => {
                i += 1;
                shot = args.get(i).map(std::path::PathBuf::from);
            }
            _ => {}
        }
        i += 1;
    }

    // With our own opaque backdrop there is nothing to frost, so default to no
    // native material. On a transparent window (backdrop off) default to the
    // HUD vibrancy that frosts the real desktop behind the card.
    let material = material_override.unwrap_or(if backdrop {
        Material::None
    } else {
        Material::Vibrancy
    });

    Config {
        surface,
        quick_variant,
        theme_dark,
        backdrop,
        material,
        shot,
        window_pos: (160.0, 160.0),
    }
}

fn main() -> eframe::Result<()> {
    let cfg = parse_args();

    let inner_size = if cfg.interactive() {
        egui::vec2(960.0, 1040.0)
    } else {
        cfg.shot_window_size()
    };

    // Window behaviour follows decision D27's development rule: a spike must not
    // ambush the person running it. Always-on-top and borderless together
    // produced a window that sat over the user's work with no way to move it,
    // which was disruptive enough to halt a session. So:
    //
    //   * normal window level, never always-on-top
    //   * real decorations in interactive mode, so it has a titlebar to drag
    //   * borderless only for screenshot capture, where nobody is interacting
    //
    // The screenshot path keeps `decorations(false)` because chrome would appear
    // in the captured image, and it exits on its own.
    let interactive = cfg.interactive();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(inner_size)
        .with_decorations(interactive)
        .with_transparent(true)
        .with_resizable(interactive)
        // Dragging anywhere on the background moves the window, so it is movable
        // even where the titlebar is hidden.
        .with_movable_by_background(true)
        .with_position([cfg.window_pos.0, cfg.window_pos.1]);
    // Borderless windows still need a title for the taskbar/window list.
    viewport = viewport.with_title("Scrozz UI Spike");

    let options = eframe::NativeOptions {
        viewport,
        // Glow renderer (eframe default) — proven to composite over the macOS
        // material with a transparent clear color.
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native(
        "Scrozz UI Spike",
        options,
        Box::new(move |cc| Ok(Box::new(app::SpikeApp::new(cc, cfg)))),
    )
}
