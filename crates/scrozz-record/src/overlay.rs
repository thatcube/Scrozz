//! Timed recording-overlay values and camera composition geometry.
//!
//! There are no input hooks or renderer types here. Platform listeners append
//! values, renderers ask what is visible at a virtual time, and tests can pin
//! the exact same answers without a display server.

use std::{collections::VecDeque, time::Duration};

use scrozz_core::{Error, LogicalPoint, LogicalRect, LogicalSize, Result};

use crate::interaction::SensitiveLabel;
use crate::settings::{
    CameraSettings, CameraShape, ClickSettings, ClickStyle, KeystrokeScope, KeystrokeSettings,
    OverlaySize, OverlayTheme, Rgba8,
};

/// Lifetime of an animated click ripple.
pub const ANIMATED_CLICK_LIFETIME: Duration = Duration::from_millis(650);
/// Lifetime of a non-animated click flash.
pub const STATIC_CLICK_LIFETIME: Duration = Duration::from_millis(180);
/// Maximum click samples retained even when many occur at one timestamp.
pub const MAX_RETAINED_CLICKS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq)]
struct TimedClick {
    at: Duration,
    position: LogicalPoint,
}

/// One click ripple visible at a requested virtual time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VisibleClick {
    /// Click position in recording coordinates.
    pub position: LogicalPoint,
    /// Configured straight-alpha colour.
    pub color: Rgba8,
    /// Ring or filled disc.
    pub style: ClickStyle,
    /// Current diameter in logical points.
    pub diameter: f32,
    /// Current opacity from transparent `0.0` to opaque `1.0`.
    pub opacity: f32,
    /// Normalized age over the click's lifetime.
    pub progress: f32,
}

/// Deterministic click-ripple timeline.
#[derive(Debug, Clone)]
pub struct ClickTrack {
    settings: ClickSettings,
    clicks: VecDeque<TimedClick>,
}

impl ClickTrack {
    /// Creates an empty track.
    #[must_use]
    pub const fn new(settings: ClickSettings) -> Self {
        Self {
            settings,
            clicks: VecDeque::new(),
        }
    }

    /// Adds a click sample.
    ///
    /// Returns `false` when click capture is disabled.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for non-finite coordinates or a sample
    /// earlier than the preceding sample.
    pub fn push(&mut self, at: Duration, position: LogicalPoint) -> Result<bool> {
        if !self.settings.enabled {
            return Ok(false);
        }
        if !position.x.is_finite() || !position.y.is_finite() {
            return Err(Error::InvalidRequest(
                "click position must contain finite coordinates".to_owned(),
            ));
        }
        if self.clicks.back().is_some_and(|click| at < click.at) {
            return Err(Error::InvalidRequest(
                "click samples must be appended in time order".to_owned(),
            ));
        }
        let lifetime = self.lifetime();
        self.clicks
            .retain(|click| at.saturating_sub(click.at) < lifetime);
        self.clicks.push_back(TimedClick { at, position });
        while self.clicks.len() > MAX_RETAINED_CLICKS {
            self.clicks.pop_front();
        }
        Ok(true)
    }

    /// Snapshots the ripples visible at `at`, oldest first.
    #[must_use]
    pub fn visible(&self, at: Duration) -> Vec<VisibleClick> {
        if !self.settings.enabled {
            return Vec::new();
        }
        let lifetime = self.lifetime();
        let lifetime_secs = lifetime.as_secs_f32();
        self.clicks
            .iter()
            .filter_map(|click| {
                let age = at.checked_sub(click.at)?;
                if age >= lifetime {
                    return None;
                }
                let progress = (age.as_secs_f32() / lifetime_secs).clamp(0.0, 1.0);
                let base = 36.0 * self.settings.size.scale();
                let (diameter, opacity) = if self.settings.animate {
                    (base * (0.7 + progress * 0.8), 1.0 - progress)
                } else {
                    (base, 1.0)
                };
                Some(VisibleClick {
                    position: click.position,
                    color: self.settings.color,
                    style: self.settings.style,
                    diameter,
                    opacity,
                    progress,
                })
            })
            .collect()
    }

    /// Configured click lifetime.
    #[must_use]
    pub const fn lifetime(&self) -> Duration {
        if self.settings.animate {
            ANIMATED_CLICK_LIFETIME
        } else {
            STATIC_CLICK_LIFETIME
        }
    }

    /// Number of retained source samples.
    #[must_use]
    pub fn len(&self) -> usize {
        self.clicks.len()
    }

    /// Whether no samples are retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clicks.is_empty()
    }
}

/// Semantic kind of key display input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeystrokeKind {
    /// Plain text with no modifier.
    Text,
    /// A modifier key or chord, such as `⌘K`.
    Modifier,
    /// Named navigation/editing key, such as `Escape` or `Page Down`.
    NavigationOrEditing,
}

/// A key label supplied by an input listener.
#[derive(Clone, PartialEq, Eq)]
pub struct Keystroke {
    /// Human-readable chord or key.
    pub label: SensitiveLabel,
    /// Privacy-relevant key classification.
    pub kind: KeystrokeKind,
}

impl Keystroke {
    /// Creates a key display value.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when the label cannot be retained safely.
    pub fn new(label: &str, kind: KeystrokeKind) -> Result<Self> {
        Ok(Self {
            label: SensitiveLabel::new(label)?,
            kind,
        })
    }
}

impl std::fmt::Debug for Keystroke {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Keystroke")
            .field("label", &self.label)
            .field("kind", &self.kind)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TimedKeystroke {
    at: Duration,
    key: Keystroke,
}

/// One visible keystroke chip.
#[derive(Clone, PartialEq)]
pub struct VisibleKeystroke {
    /// Display label.
    pub label: SensitiveLabel,
    /// Chip size.
    pub size: OverlaySize,
    /// Chip theme.
    pub theme: OverlayTheme,
    /// Age since the key was observed.
    pub age: Duration,
    /// Remaining display time.
    pub remaining: Duration,
}

impl VisibleKeystroke {
    /// Display-only key label.
    #[must_use]
    pub fn label(&self) -> &str {
        self.label.as_str()
    }
}

impl std::fmt::Debug for VisibleKeystroke {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VisibleKeystroke")
            .field("label", &self.label)
            .field("size", &self.size)
            .field("theme", &self.theme)
            .field("age", &self.age)
            .field("remaining", &self.remaining)
            .finish()
    }
}

/// Deterministic keystroke-chip timeline.
#[derive(Debug, Clone)]
pub struct KeystrokeTrack {
    settings: KeystrokeSettings,
    hold: Duration,
    keys: VecDeque<TimedKeystroke>,
}

impl KeystrokeTrack {
    /// Creates an empty validated track.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for invalid hold time or chip count.
    pub fn new(settings: KeystrokeSettings) -> Result<Self> {
        settings.validate()?;
        let hold = Duration::try_from_secs_f64(f64::from(settings.hold_secs)).map_err(|_| {
            Error::InvalidRequest(format!(
                "keystroke hold of {} s cannot be represented",
                settings.hold_secs
            ))
        })?;
        Ok(Self {
            settings,
            hold,
            keys: VecDeque::new(),
        })
    }

    /// Adds a key when it passes the configured privacy filter.
    ///
    /// Returns `false` when disabled or filtered by
    /// [`KeystrokeScope::ModifiersOnly`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for an empty label or out-of-order
    /// timestamp.
    pub fn push(&mut self, at: Duration, key: Keystroke) -> Result<bool> {
        if !self.settings.enabled {
            return Ok(false);
        }
        if key.label.is_empty() {
            return Err(Error::InvalidRequest(
                "a keystroke chip needs a non-empty label".to_owned(),
            ));
        }
        if self.keys.back().is_some_and(|sample| at < sample.at) {
            return Err(Error::InvalidRequest(
                "keystrokes must be appended in time order".to_owned(),
            ));
        }
        if self.settings.scope == KeystrokeScope::ModifiersOnly && key.kind == KeystrokeKind::Text {
            return Ok(false);
        }

        self.keys
            .retain(|sample| at.saturating_sub(sample.at) < self.hold);
        self.keys.push_back(TimedKeystroke { at, key });
        while self.keys.len() > self.settings.max_visible {
            self.keys.pop_front();
        }
        Ok(true)
    }

    /// Snapshots visible chips, oldest first.
    #[must_use]
    pub fn visible(&self, at: Duration) -> Vec<VisibleKeystroke> {
        if !self.settings.enabled {
            return Vec::new();
        }
        self.keys
            .iter()
            .filter_map(|sample| {
                let age = at.checked_sub(sample.at)?;
                let remaining = self.hold.checked_sub(age)?;
                if remaining.is_zero() {
                    return None;
                }
                Some(VisibleKeystroke {
                    label: sample.key.label.clone(),
                    size: self.settings.size,
                    theme: self.settings.theme,
                    age,
                    remaining,
                })
            })
            .collect()
    }

    /// Number of retained chips, including any awaiting a later visibility
    /// query.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether no chips are retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Camera composition mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraLayoutMode {
    /// The screen fills the frame and the camera sits above it.
    PictureInPicture,
    /// The camera fills the frame and the screen is an inset above it.
    Presenter,
}

/// How source camera pixels fit the camera rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraCrop {
    /// Crop the source around its center to a square.
    CenterSquare,
    /// Preserve the source camera aspect without cropping.
    PreserveSourceAspect,
    /// Center-crop the camera to fill the output frame.
    FillOutput,
}

/// Fully resolved screen/camera composition in output coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraLayout {
    /// PiP or presenter composition.
    pub mode: CameraLayoutMode,
    /// Rectangle occupied by screen content.
    pub screen: LogicalRect,
    /// Rectangle occupied by camera content.
    pub camera: LogicalRect,
    /// Camera source crop behavior.
    pub crop: CameraCrop,
    /// Mask used for the camera layer.
    pub shape: CameraShape,
    /// Whether the camera pixels are horizontally mirrored.
    pub mirror: bool,
}

/// Resolves camera composition geometry.
///
/// Returns `Ok(None)` when the camera overlay is disabled. In presenter mode
/// the camera is a rectangular full-frame background and the captured screen is
/// a positioned inset, making layer intent explicit.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] for invalid frame geometry, camera aspect,
/// margin, or camera settings.
pub fn layout_camera(
    output: LogicalRect,
    source_aspect: f64,
    margin: f64,
    settings: CameraSettings,
) -> Result<Option<CameraLayout>> {
    if !settings.enabled {
        return Ok(None);
    }
    settings.validate()?;
    validate_rect(output, "camera output")?;
    if !source_aspect.is_finite() || source_aspect <= 0.0 {
        return Err(Error::InvalidRequest(format!(
            "camera source aspect {source_aspect} must be positive and finite"
        )));
    }
    if !margin.is_finite() || margin < 0.0 {
        return Err(Error::InvalidRequest(format!(
            "camera margin {margin} must be a finite non-negative value"
        )));
    }
    let available_width = output.size.width - margin * 2.0;
    let available_height = output.size.height - margin * 2.0;
    if available_width <= 0.0 || available_height <= 0.0 {
        return Err(Error::InvalidRequest(format!(
            "camera margin {margin} leaves no room inside {}x{} output",
            output.size.width, output.size.height
        )));
    }

    if settings.presenter {
        let scale = (1.0 - f64::from(settings.size)).clamp(0.5, 0.85);
        let screen_size = LogicalSize::new(available_width * scale, available_height * scale);
        let screen = anchored_rect(output, screen_size, margin, settings.position);
        return Ok(Some(CameraLayout {
            mode: CameraLayoutMode::Presenter,
            screen,
            camera: output,
            crop: CameraCrop::FillOutput,
            shape: CameraShape::Rectangle,
            mirror: settings.mirror,
        }));
    }

    let requested_height = output.size.width.min(output.size.height) * f64::from(settings.size);
    let (mut width, mut height, crop) = if settings.shape.is_square() {
        (requested_height, requested_height, CameraCrop::CenterSquare)
    } else {
        (
            requested_height * source_aspect,
            requested_height,
            CameraCrop::PreserveSourceAspect,
        )
    };
    let fit = (available_width / width)
        .min(available_height / height)
        .min(1.0);
    width *= fit;
    height *= fit;
    let camera = anchored_rect(
        output,
        LogicalSize::new(width, height),
        margin,
        settings.position,
    );
    Ok(Some(CameraLayout {
        mode: CameraLayoutMode::PictureInPicture,
        screen: output,
        camera,
        crop,
        shape: settings.shape,
        mirror: settings.mirror,
    }))
}

fn anchored_rect(
    output: LogicalRect,
    size: LogicalSize,
    margin: f64,
    anchor: crate::settings::OverlayAnchor,
) -> LogicalRect {
    let (unit_x, unit_y) = anchor.unit();
    let available_width = output.size.width - margin * 2.0;
    let available_height = output.size.height - margin * 2.0;
    let x = output.origin.x + margin + (available_width - size.width) * f64::from(unit_x);
    let y = output.origin.y + margin + (available_height - size.height) * f64::from(unit_y);
    LogicalRect::new(LogicalPoint::new(x, y), size)
}

fn validate_rect(rect: LogicalRect, name: &str) -> Result<()> {
    let values = [
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    ];
    if values.iter().any(|value| !value.is_finite()) || rect.is_empty() {
        return Err(Error::InvalidRequest(format!(
            "{name} must be finite and have non-zero area"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::settings::{ClickStyle, OverlayAnchor};

    use super::*;

    fn output() -> LogicalRect {
        LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(1920.0, 1080.0),
        )
    }

    #[test]
    fn disabled_tracks_ignore_input_and_render_nothing() {
        let mut clicks = ClickTrack::new(ClickSettings::default());
        assert!(
            !clicks
                .push(Duration::ZERO, LogicalPoint::new(10.0, 10.0))
                .unwrap()
        );
        assert!(clicks.visible(Duration::ZERO).is_empty());

        let mut keys = KeystrokeTrack::new(KeystrokeSettings::default()).unwrap();
        assert!(
            !keys
                .push(
                    Duration::ZERO,
                    Keystroke::new("⌘K", KeystrokeKind::Modifier).unwrap()
                )
                .unwrap()
        );
        assert!(keys.visible(Duration::ZERO).is_empty());
        assert!(
            layout_camera(output(), 16.0 / 9.0, 24.0, CameraSettings::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn animated_clicks_expand_fade_and_expire_at_a_bounded_lifetime() {
        let mut settings = ClickSettings {
            enabled: true,
            ..ClickSettings::default()
        };
        settings.style = ClickStyle::Filled;
        let mut track = ClickTrack::new(settings);
        track
            .push(Duration::from_millis(100), LogicalPoint::new(5.0, 8.0))
            .unwrap();
        let first = track.visible(Duration::from_millis(100))[0];
        let later = track.visible(Duration::from_millis(500))[0];
        assert_eq!(first.style, ClickStyle::Filled);
        assert!(later.diameter > first.diameter);
        assert!(later.opacity < first.opacity);
        assert!(
            track
                .visible(Duration::from_millis(100) + ANIMATED_CLICK_LIFETIME)
                .is_empty()
        );
    }

    #[test]
    fn click_history_is_count_bounded() {
        let settings = ClickSettings {
            enabled: true,
            ..ClickSettings::default()
        };
        let mut track = ClickTrack::new(settings);
        for index in 0..(MAX_RETAINED_CLICKS + 10) {
            track
                .push(
                    Duration::ZERO,
                    LogicalPoint::new(index as f64, index as f64),
                )
                .unwrap();
        }
        assert_eq!(track.len(), MAX_RETAINED_CLICKS);
    }

    #[test]
    fn modifiers_only_filters_text_but_keeps_shortcuts_and_named_keys() {
        let settings = KeystrokeSettings {
            enabled: true,
            scope: KeystrokeScope::ModifiersOnly,
            ..KeystrokeSettings::default()
        };
        let mut track = KeystrokeTrack::new(settings).unwrap();
        assert!(
            !track
                .push(
                    Duration::ZERO,
                    Keystroke::new("p", KeystrokeKind::Text).unwrap(),
                )
                .unwrap()
        );
        assert!(
            track
                .push(
                    Duration::ZERO,
                    Keystroke::new("⌘P", KeystrokeKind::Modifier).unwrap()
                )
                .unwrap()
        );
        assert!(
            track
                .push(
                    Duration::from_millis(1),
                    Keystroke::new("Escape", KeystrokeKind::NavigationOrEditing).unwrap()
                )
                .unwrap()
        );
        let labels: Vec<_> = track
            .visible(Duration::from_millis(2))
            .into_iter()
            .map(|chip| chip.label)
            .collect();
        assert_eq!(labels, ["⌘P", "Escape"]);
    }

    #[test]
    fn keystrokes_expire_and_the_oldest_retires_at_the_cap() {
        let settings = KeystrokeSettings {
            enabled: true,
            scope: KeystrokeScope::All,
            hold_secs: 1.0,
            max_visible: 2,
            ..KeystrokeSettings::default()
        };
        let mut track = KeystrokeTrack::new(settings).unwrap();
        for (millis, label) in [(0, "A"), (10, "B"), (20, "C")] {
            track
                .push(
                    Duration::from_millis(millis),
                    Keystroke::new(label, KeystrokeKind::Text).unwrap(),
                )
                .unwrap();
        }
        let labels: Vec<_> = track
            .visible(Duration::from_millis(30))
            .into_iter()
            .map(|chip| chip.label)
            .collect();
        assert_eq!(labels, ["B", "C"]);
        assert!(track.visible(Duration::from_millis(1_020)).is_empty());
    }

    #[test]
    fn circle_pip_is_square_anchored_and_mirrored() {
        let settings = CameraSettings {
            enabled: true,
            position: OverlayAnchor::BottomRight,
            shape: CameraShape::Circle,
            mirror: true,
            ..CameraSettings::default()
        };
        let layout = layout_camera(output(), 16.0 / 9.0, 24.0, settings)
            .unwrap()
            .unwrap();
        assert_eq!(layout.mode, CameraLayoutMode::PictureInPicture);
        assert_eq!(layout.crop, CameraCrop::CenterSquare);
        assert_eq!(layout.camera.size.width, layout.camera.size.height);
        assert_eq!(
            layout.camera.origin.x + layout.camera.size.width,
            1920.0 - 24.0
        );
        assert_eq!(
            layout.camera.origin.y + layout.camera.size.height,
            1080.0 - 24.0
        );
        assert!(layout.mirror);
    }

    #[test]
    fn rectangular_pip_preserves_camera_source_aspect() {
        let settings = CameraSettings {
            enabled: true,
            position: OverlayAnchor::TopLeft,
            shape: CameraShape::Rounded,
            ..CameraSettings::default()
        };
        let layout = layout_camera(output(), 4.0 / 3.0, 20.0, settings)
            .unwrap()
            .unwrap();
        assert_eq!(layout.crop, CameraCrop::PreserveSourceAspect);
        assert!((layout.camera.size.width / layout.camera.size.height - 4.0 / 3.0).abs() < 1e-9);
        assert_eq!(layout.camera.origin, LogicalPoint::new(20.0, 20.0));
    }

    #[test]
    fn presenter_mode_has_full_frame_camera_and_explicit_screen_inset() {
        let settings = CameraSettings {
            enabled: true,
            presenter: true,
            position: OverlayAnchor::TopRight,
            shape: CameraShape::Circle,
            ..CameraSettings::default()
        };
        let layout = layout_camera(output(), 16.0 / 9.0, 30.0, settings)
            .unwrap()
            .unwrap();
        assert_eq!(layout.mode, CameraLayoutMode::Presenter);
        assert_eq!(layout.camera, output());
        assert_eq!(layout.crop, CameraCrop::FillOutput);
        assert_eq!(layout.shape, CameraShape::Rectangle);
        assert!(layout.screen.size.width < layout.camera.size.width);
        assert_eq!(
            layout.screen.origin.x + layout.screen.size.width,
            1920.0 - 30.0
        );
    }

    #[test]
    fn invalid_camera_geometry_is_an_error_not_a_clamp() {
        let settings = CameraSettings {
            enabled: true,
            ..CameraSettings::default()
        };
        assert!(layout_camera(output(), f64::NAN, 20.0, settings).is_err());
        assert!(layout_camera(output(), 1.0, 600.0, settings).is_err());
    }
}
