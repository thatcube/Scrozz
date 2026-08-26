//! Scrozz UI spike — entry point.
//!
//! THROWAWAY. Proves whether Rust + egui can reach CleanShot-grade polish.
//! No real functionality; every surface is faked.
//!
//! Interactive:  cargo run
//! Screenshot:   cargo run -- --surface quick --theme dark --backdrop on --shot screenshots/quick.png

mod app;
mod icons;
mod paint;
mod surfaces;
mod theme;
mod vibrancy;

use app::Config;
use eframe::egui;
use surfaces::Surface;
use vibrancy::Material;

fn parse_args() -> Config {
    let mut surface = Surface::Quick;
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
                    _ => Surface::Quick,
                };
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
        egui::vec2(960.0, 700.0)
    } else {
        cfg.shot_window_size()
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(inner_size)
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top()
        .with_resizable(cfg.interactive())
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
