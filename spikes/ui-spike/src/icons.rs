//! SVG icon pipeline.
//!
//! The spec is explicit that emoji/unicode glyphs would invalidate the spike,
//! so we rasterize real Tabler SVGs with `resvg` into egui textures. Each icon
//! is rendered once as a white alpha-mask at high resolution; egui then
//! downscales it (LINEAR) and we tint it per state via the painter, so one
//! texture serves every color/hover/selected variant.

use egui::{Color32, Rect, TextureHandle, TextureId, TextureOptions};
use std::collections::HashMap;

/// Rasterize at a good bit above display size so downscaling stays crisp.
const RASTER_PX: u32 = 80;

/// Every icon the surfaces reference. Preloaded up-front so drawing is a cheap
/// immutable lookup.
pub const USED: &[&str] = &[
    // quick-access action bar
    "grip-vertical",
    "copy",
    "device-floppy",
    "pencil",
    "pin",
    "cloud-upload",
    "x",
    "check",
    // menu
    "layout-grid",
    "viewfinder",
    "app-window",
    "device-desktop",
    "arrow-bar-to-down",
    "video",
    "scan",
    "history",
    "settings",
    "power",
    "chevron-right",
    // annotation tools
    "crop",
    "arrow-up-right",
    "square",
    "circle",
    "line",
    "letter-t",
    "highlight",
    "droplet",
    "grid-dots",
    "list-numbers",
    "palette",
    "arrow-back-up",
    "arrow-forward-up",
];

pub struct IconStore {
    map: HashMap<&'static str, TextureHandle>,
}

impl IconStore {
    pub fn new(ctx: &egui::Context) -> Self {
        let mut map = HashMap::new();
        let opt = resvg::usvg::Options::default();
        for &name in USED {
            if let Some(img) = rasterize(name, &opt) {
                let tex = ctx.load_texture(format!("icon:{name}"), img, TextureOptions::LINEAR);
                map.insert(name, tex);
            } else {
                eprintln!("warning: failed to rasterize icon `{name}`");
            }
        }
        Self { map }
    }

    pub fn id(&self, name: &str) -> Option<TextureId> {
        self.map.get(name).map(|h| h.id())
    }

    /// Draw an icon centered in `rect`, tinted `color`, at a square size of
    /// `px` logical points (keeps the icon's aspect and optical size uniform
    /// regardless of the hit-rect around it).
    pub fn draw(&self, painter: &egui::Painter, name: &str, center: egui::Pos2, px: f32, color: Color32) {
        if let Some(id) = self.id(name) {
            let r = Rect::from_center_size(center, egui::vec2(px, px));
            painter.image(
                id,
                r,
                Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                color,
            );
        }
    }
}

fn rasterize(name: &str, opt: &resvg::usvg::Options) -> Option<egui::ColorImage> {
    let path = format!(
        "{}/assets/icons/{}.svg",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    let raw = std::fs::read_to_string(&path).ok()?;
    // Tabler icons use `currentColor`; force white so we own the tint in-egui.
    let svg = raw.replace("currentColor", "#FFFFFF");

    let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), opt).ok()?;
    let size = tree.size();
    let max_dim = size.width().max(size.height());
    let scale = RASTER_PX as f32 / max_dim;
    let w = (size.width() * scale).ceil() as u32;
    let h = (size.height() * scale).ceil() as u32;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(w.max(1), h.max(1))?;
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // tiny_skia is premultiplied; for a pure-white glyph rgb == alpha, so we
    // keep only alpha as a coverage mask and hand egui an unmultiplied white
    // image. That avoids the classic double-darkened edges.
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for px in pixmap.pixels() {
        let a = px.alpha();
        rgba.extend_from_slice(&[255, 255, 255, a]);
    }
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        &rgba,
    ))
}
