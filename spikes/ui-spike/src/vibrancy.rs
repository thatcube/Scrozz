//! macOS window material.
//!
//! Finding baked into the code: NSGlassEffectView (Liquid Glass) styles a
//! view's *content* as glass, which blurs our foreground UI — unusable as a
//! plain window backdrop for crisp controls. The classic NSVisualEffectView
//! vibrancy sits *behind* the content and frosts the desktop, which is what an
//! overlay actually wants. We support both so FINDINGS can show the difference.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Material {
    /// No native material (opaque backdrop drawn in-egui, or fully transparent).
    None,
    /// NSVisualEffectView HudWindow — frosts the desktop behind, content crisp.
    Vibrancy,
    /// NSGlassEffectView Liquid Glass — macOS 26; blurs foreground (documented).
    Glass,
}

impl Material {
    pub fn parse(s: &str) -> Self {
        match s {
            "vibrancy" => Material::Vibrancy,
            "glass" => Material::Glass,
            _ => Material::None,
        }
    }
}

#[cfg(target_os = "macos")]
pub fn apply(cc: &eframe::CreationContext<'_>, material: Material, radius: f64) -> String {
    use window_vibrancy::{apply_liquid_glass, LiquidGlassOptions, NSGlassEffectViewStyle};
    match material {
        Material::None => "none".to_owned(),
        Material::Glass => {
            let opts = LiquidGlassOptions::new(NSGlassEffectViewStyle::Regular).radius(radius);
            match apply_liquid_glass(cc, opts) {
                Ok(()) => "liquid_glass (NSGlassEffectView, macOS 26)".to_owned(),
                Err(e) => {
                    eprintln!("apply_liquid_glass failed, falling back to vibrancy: {e}");
                    apply_hud(cc, radius)
                }
            }
        }
        Material::Vibrancy => apply_hud(cc, radius),
    }
}

#[cfg(target_os = "macos")]
fn apply_hud(cc: &eframe::CreationContext<'_>, radius: f64) -> String {
    use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial, NSVisualEffectState};
    match apply_vibrancy(
        cc,
        NSVisualEffectMaterial::HudWindow,
        Some(NSVisualEffectState::Active),
        Some(radius),
    ) {
        Ok(()) => "hud_vibrancy (NSVisualEffectMaterial::HudWindow)".to_owned(),
        Err(e) => {
            eprintln!("apply_vibrancy failed: {e}");
            "none (material failed)".to_owned()
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn apply(_cc: &eframe::CreationContext<'_>, _material: Material, _radius: f64) -> String {
    "none (non-macOS)".to_owned()
}
