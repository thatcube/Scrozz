//! Icons: SVG in, tintable texture out.
//!
//! The icons are [Tabler Icons](https://tabler.io/icons) (MIT, © Paweł Kuna;
//! the licence travels with them in `assets/icons/LICENSE`). Emoji and Unicode
//! symbol glyphs are not an option — they carry another vendor's design
//! language, vary by platform, and cannot be tinted or optically sized — so the
//! real vector artwork is rasterised with `resvg` and uploaded as a texture.
//!
//! # How a tint works
//!
//! Each icon is rendered **once**, at [`RASTER_PX`], as a pure white glyph, and
//! then reduced to a *coverage mask*: RGB is forced to white and only alpha is
//! kept. egui multiplies an image by the tint passed to
//! [`egui::Painter::image`], so one texture serves every colour, every state
//! and every size. A hover tint is a different multiply, not a different
//! upload.
//!
//! Keeping only alpha also sidesteps the classic double-darkened edge: tiny-skia
//! hands back premultiplied pixels, and feeding those to egui as if they were
//! unmultiplied darkens every antialiased edge.
//!
//! # Icons are a closed set
//!
//! [`Icon`] is an enum rather than a string key. A typo is then a compile error
//! instead of a silently missing glyph, the full set is enumerable for specimen
//! renders, and lookup is an array index rather than a hash. The SVG source is
//! embedded with `include_str!`, so an icon resolves identically from a test
//! binary, from an installed app, and from any working directory.

use crate::motion::fade;
use egui::{Color32, ColorImage, Painter, Pos2, Rect, TextureHandle, TextureId, TextureOptions};
use scrozz_core::{Error, Result};

/// The pixel size icons are rasterised at.
///
/// Comfortably above the largest on-screen size (24 pt at 2× = 48 px) so that
/// egui's linear downscale stays crisp on any display scale. Upscaling a mask
/// is what makes an icon look soft, and it is the one thing this avoids.
pub const RASTER_PX: u32 = 80;

/// The default on-screen size for an icon inside a control, in points.
pub const SIZE: f32 = 18.0;

/// The on-screen size for an icon that is the sole content of a large target.
pub const SIZE_LARGE: f32 = 22.0;

macro_rules! icons {
    ($( $variant:ident => $slug:literal , )+) => {
        /// Every icon the product can draw.
        ///
        /// Closed on purpose: adding one means adding an SVG *and* a variant,
        /// which is the point at which someone notices the set is growing.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(usize)]
        pub enum Icon {
            $(
                #[doc = concat!("`", $slug, ".svg`")]
                $variant,
            )+
        }

        impl Icon {
            /// Every icon, in declaration order.
            pub const ALL: &'static [Self] = &[ $( Self::$variant, )+ ];

            /// The number of icons.
            pub const COUNT: usize = Self::ALL.len();

            /// The Tabler name this icon came from.
            #[must_use]
            pub const fn slug(self) -> &'static str {
                match self { $( Self::$variant => $slug, )+ }
            }

            /// The embedded SVG source.
            #[must_use]
            pub const fn svg(self) -> &'static str {
                match self {
                    $( Self::$variant => include_str!(
                        concat!("../assets/icons/", $slug, ".svg")
                    ), )+
                }
            }

            /// This icon's index into an [`IconStore`]'s table.
            ///
            /// The enum is fieldless and `repr(usize)`, so the discriminant is
            /// declaration order, which is exactly [`Icon::ALL`]'s order.
            #[must_use]
            pub const fn index(self) -> usize {
                self as usize
            }
        }
    };
}

icons! {
    AppWindow => "app-window",
    ArrowBackUp => "arrow-back-up",
    ArrowBarToDown => "arrow-bar-to-down",
    ArrowForwardUp => "arrow-forward-up",
    ArrowUpRight => "arrow-up-right",
    Check => "check",
    ChevronRight => "chevron-right",
    Circle => "circle",
    CloudUpload => "cloud-upload",
    Copy => "copy",
    Crop => "crop",
    DeviceDesktop => "device-desktop",
    DeviceFloppy => "device-floppy",
    Folder => "folder",
    Droplet => "droplet",
    GridDots => "grid-dots",
    GripVertical => "grip-vertical",
    Highlight => "highlight",
    History => "history",
    LayoutGrid => "layout-grid",
    LetterT => "letter-t",
    Line => "line",
    ListNumbers => "list-numbers",
    Palette => "palette",
    Pencil => "pencil",
    Pin => "pin",
    Pointer => "pointer",
    Power => "power",
    Scan => "scan",
    Settings => "settings",
    Square => "square",
    Video => "video",
    Viewfinder => "viewfinder",
    X => "x",
}

impl std::fmt::Display for Icon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug())
    }
}

/// Rasterise an icon to a white coverage mask.
///
/// Pure: no [`egui::Context`], no GPU, no filesystem. That is deliberate —
/// it makes the whole pipeline unit-testable with no display, which is the
/// only way a headless golden test can assert an icon actually rendered rather
/// than silently falling back to nothing.
///
/// `px` is the longest edge in pixels; the other is derived from the SVG's
/// aspect so non-square artwork is never stretched.
///
/// # Errors
///
/// [`Error::Codec`] if the SVG cannot be parsed, if the requested size is
/// degenerate, or if the pixel buffer could not be allocated.
pub fn rasterize(icon: Icon, px: u32) -> Result<ColorImage> {
    let px = px.clamp(1, 4096);

    // Tabler strokes are `currentColor`, which usvg resolves to black. Force
    // white so the mask is pure coverage and the tint is entirely ours.
    let svg = icon.svg().replace("currentColor", "#FFFFFF");

    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &options)
        .map_err(|e| Error::Codec(format!("icon `{icon}`: {e}")))?;

    let size = tree.size();
    let longest = size.width().max(size.height());
    if !longest.is_finite() || longest <= 0.0 {
        return Err(Error::Codec(format!("icon `{icon}`: zero-sized artwork")));
    }

    #[allow(clippy::cast_precision_loss)]
    let scale = px as f32 / longest;
    let dim = |v: f32| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let d = (v * scale).ceil().max(1.0) as u32;
        d
    };
    let (w, h) = (dim(size.width()), dim(size.height()));

    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h)
        .ok_or_else(|| Error::Codec(format!("icon `{icon}`: could not allocate {w}×{h}")))?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    let mut rgba = Vec::with_capacity(pixmap.pixels().len() * 4);
    for pixel in pixmap.pixels() {
        rgba.extend_from_slice(&[255, 255, 255, pixel.alpha()]);
    }

    Ok(ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        &rgba,
    ))
}

/// Every icon, uploaded once and looked up by index.
///
/// Build one per [`egui::Context`] at startup and pass it down with the rest of
/// the drawing context. Rasterising is milliseconds of CPU per icon, so it must
/// not happen inside a frame.
pub struct IconStore {
    table: Vec<Option<TextureHandle>>,
}

impl IconStore {
    /// Rasterise and upload every icon.
    ///
    /// Never fails: an icon that cannot be rasterised is logged and left empty,
    /// and [`IconStore::draw`] then draws nothing. A broken icon must not take
    /// the app down with it — the surrounding control is still operable, still
    /// hit-testable, and still labelled for assistive technology (D13).
    ///
    /// Use [`IconStore::try_new`] where a missing icon should be fatal, such as
    /// in a golden-image test.
    #[must_use]
    pub fn new(ctx: &egui::Context) -> Self {
        let mut table = Vec::with_capacity(Icon::COUNT);
        for &icon in Icon::ALL {
            table.push(match rasterize(icon, RASTER_PX) {
                Ok(image) => {
                    Some(ctx.load_texture(format!("icon:{icon}"), image, TextureOptions::LINEAR))
                }
                Err(error) => {
                    tracing::warn!(%icon, %error, "icon failed to rasterize; it will not draw");
                    None
                }
            });
        }
        Self { table }
    }

    /// Rasterise and upload every icon, failing on the first that cannot be
    /// rasterised.
    ///
    /// # Errors
    ///
    /// Propagates the first [`rasterize`] failure.
    pub fn try_new(ctx: &egui::Context) -> Result<Self> {
        let mut table = Vec::with_capacity(Icon::COUNT);
        for &icon in Icon::ALL {
            let image = rasterize(icon, RASTER_PX)?;
            table.push(Some(ctx.load_texture(
                format!("icon:{icon}"),
                image,
                TextureOptions::LINEAR,
            )));
        }
        Ok(Self { table })
    }

    /// A store with no textures at all.
    ///
    /// Lets a surface be constructed, laid out and hit-tested without a live
    /// graphics context. Icons simply do not draw.
    #[must_use]
    pub fn empty() -> Self {
        Self { table: Vec::new() }
    }

    /// Whether every icon uploaded successfully.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.table.len() == Icon::COUNT && self.table.iter().all(Option::is_some)
    }

    /// The texture backing an icon, if it uploaded.
    #[must_use]
    pub fn texture(&self, icon: Icon) -> Option<TextureId> {
        self.table
            .get(icon.index())?
            .as_ref()
            .map(TextureHandle::id)
    }

    /// Draw an icon centred on `center`, at a square `size` in points.
    ///
    /// The size is independent of whatever hit rectangle surrounds it, so an
    /// icon in a 34 pt button and the same icon in a 44 pt row are optically
    /// identical.
    pub fn draw(&self, painter: &Painter, icon: Icon, center: Pos2, size: f32, tint: Color32) {
        self.draw_in(
            painter,
            icon,
            Rect::from_center_size(center, egui::vec2(size, size)),
            tint,
        );
    }

    /// Draw an icon to fill `rect` exactly.
    pub fn draw_in(&self, painter: &Painter, icon: Icon, rect: Rect, tint: Color32) {
        let Some(id) = self.texture(icon) else {
            return;
        };
        painter.image(id, rect, FULL_UV, tint);
    }

    /// Draw an icon at a fractional opacity.
    ///
    /// A cross-fade between two icons is two calls to this, which is how an
    /// icon swap is done: egui cannot rotate or morph a texture, so a fade is
    /// the only transition available (and per D19 it is used for *content*,
    /// never for a control's own state).
    pub fn draw_faded(
        &self,
        painter: &Painter,
        icon: Icon,
        center: Pos2,
        size: f32,
        tint: Color32,
        opacity: f32,
    ) {
        if opacity <= 0.0 {
            return;
        }
        self.draw(painter, icon, center, size, fade(tint, opacity));
    }
}

impl Default for IconStore {
    fn default() -> Self {
        Self::empty()
    }
}

impl std::fmt::Debug for IconStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let loaded = self.table.iter().filter(|slot| slot.is_some()).count();
        f.debug_struct("IconStore")
            .field("loaded", &loaded)
            .field("of", &Icon::COUNT)
            .finish()
    }
}

/// The whole of a texture, in UV space.
const FULL_UV: Rect = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
