//! Working out a display's scale factor on X11.
//!
//! X11 has no concept of a per-display scale factor. It has one screen-wide
//! resolution in dots per inch, and everything else is convention layered on top
//! by toolkits. So this module does what GTK and Qt do — read `Xft.dpi` out of
//! the X resource database, honour the environment overrides, and fall back to
//! 1× — and it does it as pure string arithmetic so the parsing is tested.
//!
//! Two things are deliberately *not* done here:
//!
//! - **Physical size is not used.** Deriving DPI from the monitor's millimetre
//!   dimensions is tempting and wrong: a large fraction of monitors report
//!   fabricated or zero physical sizes over EDID, and a projector reports
//!   whatever it feels like. The result is a screenshot scaled by 1.37.
//! - **Per-monitor scale is not invented.** Under plain X11 every monitor shares
//!   one scale, and pretending otherwise would produce
//!   [`scrozz_core::Display`] values that disagree with what the toolkits
//!   actually did. Genuine per-monitor scaling on Linux is a Wayland feature.

use scrozz_core::ScaleFactor;

/// Scale factors outside this range are treated as garbage.
///
/// A malformed `Xft.dpi` of `0` or `96000` otherwise propagates into a capture
/// size, where it appears as an out-of-memory kill rather than as a bad setting.
const MIN_SCALE: f64 = 0.5;
/// The upper end of the plausible range; 8K at 4× is already beyond any shipping
/// hardware.
const MAX_SCALE: f64 = 6.0;

/// The reference resolution a 1× display is defined to have.
pub const BASE_DPI: f64 = 96.0;

/// Extracts `Xft.dpi` from the X resource database.
///
/// `RESOURCE_MANAGER` on the root window is a newline-separated list of
/// `Name: value` lines, as written by `xrdb`. Lines may carry leading
/// whitespace, comments start with `!`, and the file is Latin-1 rather than
/// UTF-8.
///
/// This is where a HiDPI X11 desktop records its scale, so getting it right is
/// the difference between a 3840×2160 screenshot correctly labelled 2× and the
/// same screenshot labelled 1× and then downscaled by half somewhere downstream.
#[must_use]
pub fn parse_xft_dpi(resource_manager: &str) -> Option<f64> {
    for line in resource_manager.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('!') {
            continue;
        }
        // A line with no colon is not a resource; `xrdb` emits them for
        // continuation lines and stray directives. Skipping is right — bailing
        // out would let one malformed line hide a perfectly good `Xft.dpi`
        // further down the database.
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("Xft.dpi") {
            continue;
        }
        let dpi = value.trim().parse::<f64>().ok()?;
        if dpi.is_finite() && dpi > 0.0 {
            return Some(dpi);
        }
    }
    None
}

/// Converts a resource-database DPI into a scale factor.
#[must_use]
pub fn scale_from_dpi(dpi: f64) -> Option<f64> {
    let scale = dpi / BASE_DPI;
    is_plausible(scale).then_some(scale)
}

/// Reads a toolkit scale override out of its environment string.
///
/// `GDK_SCALE` is an integer, `QT_SCALE_FACTOR` may be fractional, and both are
/// set by users who have already decided what their desktop's scale is. When one
/// is present it beats anything derived from the resource database, because it
/// is what the rest of their session is actually using.
#[must_use]
pub fn parse_scale_override(value: &str) -> Option<f64> {
    let scale = value.trim().parse::<f64>().ok()?;
    is_plausible(scale).then_some(scale)
}

/// Resolves the session's scale factor from every available signal.
///
/// Precedence, strongest first:
///
/// 1. `GDK_SCALE` / `QT_SCALE_FACTOR` — an explicit user decision.
/// 2. `Xft.dpi` — what the desktop environment wrote when the user chose a
///    scale in its settings panel.
/// 3. 1×, which is right for the overwhelming majority of X11 desktops.
#[must_use]
pub fn resolve_scale(
    gdk_scale: Option<&str>,
    qt_scale: Option<&str>,
    resource_manager: Option<&str>,
) -> ScaleFactor {
    let from_env = gdk_scale
        .and_then(parse_scale_override)
        .or_else(|| qt_scale.and_then(parse_scale_override));

    let from_xft = resource_manager
        .and_then(parse_xft_dpi)
        .and_then(scale_from_dpi);

    ScaleFactor::new(from_env.or(from_xft).unwrap_or(1.0))
}

fn is_plausible(scale: f64) -> bool {
    scale.is_finite() && (MIN_SCALE..=MAX_SCALE).contains(&scale)
}
