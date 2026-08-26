//! Design-token layer for the Scrozz spike.
//!
//! This is the thing the maintainer worried about: default egui looks like a
//! debug tool. So we define a real token system — a color ramp, a spacing
//! scale, radii, elevation and a type ramp — and drive a custom `egui::Style`
//! from it. Almost nothing in the surfaces uses a stock egui widget; the tokens
//! here are the single source of truth for the hand-drawn ones.

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Spacing scale (4pt grid). Using a scale instead of magic numbers is a big
// part of why premium UIs feel coherent.
// ---------------------------------------------------------------------------
pub const SP_1: f32 = 4.0;
pub const SP_2: f32 = 8.0;
pub const SP_3: f32 = 12.0;
pub const SP_4: f32 = 16.0;
pub const SP_5: f32 = 20.0;
pub const SP_6: f32 = 24.0;

// ---------------------------------------------------------------------------
// Corner radii — one consistent family, the way CleanShot keeps every corner
// on the same rhythm.
// ---------------------------------------------------------------------------
pub const R_CARD: f32 = 20.0;
pub const R_BAR: f32 = 17.0;
pub const R_THUMB: f32 = 13.0;
pub const R_BTN: f32 = 10.0;
pub const R_CHIP: f32 = 6.0;

// ---------------------------------------------------------------------------
// Type ramp. Weight is expressed through separate Inter cuts (egui selects a
// family, not a weight axis), so we register four named families.
// ---------------------------------------------------------------------------
pub fn font_regular() -> FontFamily {
    FontFamily::Proportional
}
pub fn font_medium() -> FontFamily {
    FontFamily::Name("medium".into())
}
pub fn font_semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}
pub fn font_bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

pub fn ts_title() -> FontId {
    FontId::new(15.0, font_semibold())
}
pub fn ts_label() -> FontId {
    FontId::new(13.5, font_medium())
}
pub fn ts_body() -> FontId {
    FontId::new(13.0, font_regular())
}
pub fn ts_caption() -> FontId {
    FontId::new(11.5, font_regular())
}
pub fn ts_shortcut() -> FontId {
    FontId::new(12.5, font_medium())
}

/// A resolved set of semantic colors for one appearance (dark or light).
#[derive(Clone, Copy)]
pub struct Palette {
    pub is_dark: bool,
    /// When true the card sits over a *real* OS glass material, so its fill is
    /// dialled way down and the external shadow suppressed — the material does
    /// the work.
    pub over_material: bool,

    /// Signature accent (Scrozz iris/indigo — deliberately not CleanShot blue).
    pub accent: Color32,
    pub accent_hi: Color32,
    pub accent_press: Color32,
    pub on_accent: Color32,

    pub text: Color32,
    pub text_muted: Color32,
    pub text_faint: Color32,

    pub card_fill: Color32,
    pub card_fill_raised: Color32,
    pub hairline: Color32,
    pub top_highlight: Color32,
    pub bottom_shade: Color32,

    pub hover: Color32,
    pub active: Color32,

    pub key_shadow: Color32,
    pub ambient_shadow: Color32,

    pub thumb_border: Color32,
    pub chip_fill: Color32,
    pub divider: Color32,
}

impl Palette {
    pub fn dark() -> Self {
        Self {
            is_dark: true,
            over_material: false,
            accent: Color32::from_rgb(0x7C, 0x7A, 0xFF),
            accent_hi: Color32::from_rgb(0x97, 0x8C, 0xFF),
            accent_press: Color32::from_rgb(0x66, 0x63, 0xEC),
            on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),

            text: Color32::from_rgb(0xF3, 0xF4, 0xFB),
            text_muted: Color32::from_rgba_unmultiplied(0xEB, 0xEE, 0xF8, 160),
            text_faint: Color32::from_rgba_unmultiplied(0xEB, 0xEE, 0xF8, 96),

            // Semi-transparent so the glass material / wallpaper shows through.
            card_fill: Color32::from_rgba_unmultiplied(0x1B, 0x1D, 0x27, 180),
            card_fill_raised: Color32::from_rgba_unmultiplied(0x24, 0x27, 0x33, 205),
            hairline: Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 26),
            top_highlight: Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 40),
            bottom_shade: Color32::from_rgba_unmultiplied(0x00, 0x00, 0x00, 46),

            hover: Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 20),
            active: Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 36),

            key_shadow: Color32::from_rgba_unmultiplied(0, 0, 0, 120),
            ambient_shadow: Color32::from_rgba_unmultiplied(0, 0, 0, 66),

            thumb_border: Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 30),
            chip_fill: Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 22),
            divider: Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 24),
        }
    }

    pub fn light() -> Self {
        Self {
            is_dark: false,
            over_material: false,
            accent: Color32::from_rgb(0x5B, 0x57, 0xF0),
            accent_hi: Color32::from_rgb(0x74, 0x6F, 0xFF),
            accent_press: Color32::from_rgb(0x49, 0x45, 0xD6),
            on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),

            text: Color32::from_rgb(0x1A, 0x1C, 0x24),
            text_muted: Color32::from_rgba_unmultiplied(0x1A, 0x1C, 0x24, 150),
            text_faint: Color32::from_rgba_unmultiplied(0x1A, 0x1C, 0x24, 96),

            card_fill: Color32::from_rgba_unmultiplied(0xF7, 0xF7, 0xFB, 205),
            card_fill_raised: Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 220),
            hairline: Color32::from_rgba_unmultiplied(0x1A, 0x1C, 0x24, 26),
            top_highlight: Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 235),
            bottom_shade: Color32::from_rgba_unmultiplied(0x1A, 0x1C, 0x24, 14),

            hover: Color32::from_rgba_unmultiplied(0x1A, 0x1C, 0x24, 16),
            active: Color32::from_rgba_unmultiplied(0x1A, 0x1C, 0x24, 30),

            key_shadow: Color32::from_rgba_unmultiplied(0x1A, 0x1C, 0x28, 60),
            ambient_shadow: Color32::from_rgba_unmultiplied(0x1A, 0x1C, 0x28, 30),

            thumb_border: Color32::from_rgba_unmultiplied(0x1A, 0x1C, 0x24, 28),
            chip_fill: Color32::from_rgba_unmultiplied(0x1A, 0x1C, 0x24, 16),
            divider: Color32::from_rgba_unmultiplied(0x1A, 0x1C, 0x24, 22),
        }
    }
}

pub fn cr(v: f32) -> CornerRadius {
    CornerRadius::same(v.round().clamp(0.0, 255.0) as u8)
}

/// Register the vendored Inter cuts as named families.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();

    macro_rules! load {
        ($key:literal, $file:literal) => {{
            fonts.font_data.insert(
                $key.to_owned(),
                Arc::new(FontData::from_static(include_bytes!(concat!(
                    "../assets/fonts/",
                    $file
                )))),
            );
        }};
    }
    load!("Inter-Regular", "Inter-Regular.ttf");
    load!("Inter-Medium", "Inter-Medium.ttf");
    load!("Inter-SemiBold", "Inter-SemiBold.ttf");
    load!("Inter-Bold", "Inter-Bold.ttf");

    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "Inter-Regular".to_owned());
    fonts
        .families
        .insert(FontFamily::Name("medium".into()), vec!["Inter-Medium".to_owned()]);
    fonts.families.insert(
        FontFamily::Name("semibold".into()),
        vec!["Inter-SemiBold".to_owned()],
    );
    fonts
        .families
        .insert(FontFamily::Name("bold".into()), vec!["Inter-Bold".to_owned()]);

    // Give the single-weight custom families the same fallback tail as the
    // proportional family, so symbol glyphs (⌘⇧⌥⌃) resolve even if a given
    // Inter weight is missing one.
    let fallback = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    for name in ["medium", "semibold", "bold"] {
        if let Some(fam) = fonts.families.get_mut(&FontFamily::Name(name.into())) {
            for f in &fallback {
                if !fam.contains(f) {
                    fam.push(f.clone());
                }
            }
        }
    }

    ctx.set_fonts(fonts);
}

/// Install a custom `Style`/`Visuals`. We keep panels transparent (the window
/// is transparent so the OS material shows) and disable animations/blink so
/// snapshots are deterministic.
pub fn install_style(ctx: &egui::Context, pal: &Palette) {
    let pal = *pal;
    ctx.all_styles_mut(move |style| {
        let v = &mut style.visuals;
        v.dark_mode = pal.is_dark;
        v.override_text_color = Some(pal.text);
        v.panel_fill = Color32::TRANSPARENT;
        v.window_fill = Color32::TRANSPARENT;
        v.window_stroke = egui::Stroke::NONE;
        v.window_shadow = egui::epaint::Shadow::NONE;
        v.popup_shadow = egui::epaint::Shadow::NONE;
        v.selection.bg_fill = pal.accent.linear_multiply(0.55);
        v.selection.stroke = egui::Stroke::new(1.0, pal.on_accent);
        v.text_cursor.blink = false;

        style.animation_time = 0.0;
        style.spacing.item_spacing = egui::vec2(SP_2, SP_2);
    });
}
