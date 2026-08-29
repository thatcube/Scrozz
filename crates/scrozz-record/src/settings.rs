//! Everything the user can configure about a recording, as values.
//!
//! # Why these are values and not a settings store
//!
//! The settings surface (`apps/scrozz/src/settings.rs`) owns keys, defaults,
//! parsing and persistence. This module owns the *meaning*. The seam between
//! them is deliberately a plain value type with no I/O, no strings-as-state and
//! no `Option` soup: the settings layer parses `record.click-style = "outline"`
//! once, hands over a [`ClickStyle`], and from that point nothing downstream can
//! misspell it.
//!
//! That matters more here than elsewhere because recording has roughly twenty
//! knobs across five overlays, and each one is read by two very different
//! consumers — the capture engine and the renderer. A stringly-typed settings
//! blob would let those two disagree silently, and the disagreement would only
//! be visible in a finished video.
//!
//! # Every enum round-trips through a slug
//!
//! [`ClickStyle::slug`] / [`ClickStyle::from_slug`] and their siblings are what
//! the settings schema, the CLI and `--json` all speak. The slugs are stable
//! identifiers, not display strings: renaming one silently invalidates a user's
//! configuration file, so they are treated like the hotkey action ids and never
//! changed.
//!
//! # Defaults are the design
//!
//! [`RecordingSettings::default`] is what the overwhelming majority of users
//! will ever run with, so it is chosen rather than inherited: cursor visible,
//! three-second countdown, clicks and keystrokes **off**. The last of those is
//! not timidity — click and keystroke overlays need input-monitoring grants
//! (D15), and a default that provokes a keylogger-class permission prompt on
//! first recording would tax every user for a feature most never enable.

use scrozz_core::{CursorMode, Error, Result};

// ===========================================================================
// Small shared vocabulary
// ===========================================================================

/// An 8-bit-per-channel colour, straight-alpha.
///
/// Defined here rather than borrowed from the UI crate because settings must be
/// expressible without a renderer: the CLI parses `#ff3b30`, `scrozz-record`
/// stores it, and only `scrozz-ui` ever turns it into an `egui::Color32`. A
/// domain crate that depended on a widget toolkit to describe a colour would be
/// the wrong shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgba8 {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha, where 255 is opaque.
    pub a: u8,
}

impl Rgba8 {
    /// An opaque colour.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// A colour with explicit alpha.
    #[must_use]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// This colour at a new alpha.
    #[must_use]
    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    /// Parses `#rgb`, `#rrggbb` or `#rrggbbaa`, with or without the `#`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] naming the accepted forms. A colour is
    /// something a user types by hand, so the message has to teach rather than
    /// merely refuse.
    pub fn parse(text: &str) -> Result<Self> {
        let hex = text.strip_prefix('#').unwrap_or(text);
        let bad = || {
            Error::InvalidRequest(format!(
                "{text:?} is not a colour; expected #rgb, #rrggbb or #rrggbbaa"
            ))
        };
        let nibble = |c: u8| -> Result<u8> {
            match c {
                b'0'..=b'9' => Ok(c - b'0'),
                b'a'..=b'f' => Ok(c - b'a' + 10),
                b'A'..=b'F' => Ok(c - b'A' + 10),
                _ => Err(bad()),
            }
        };
        let bytes = hex.as_bytes();
        let byte = |i: usize| -> Result<u8> { Ok(nibble(bytes[i])? << 4 | nibble(bytes[i + 1])?) };
        match bytes.len() {
            3 => {
                let dup = |i: usize| -> Result<u8> {
                    let n = nibble(bytes[i])?;
                    Ok(n << 4 | n)
                };
                Ok(Self::rgb(dup(0)?, dup(1)?, dup(2)?))
            }
            6 => Ok(Self::rgb(byte(0)?, byte(2)?, byte(4)?)),
            8 => Ok(Self::rgba(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
            _ => Err(bad()),
        }
    }

    /// `#rrggbb`, or `#rrggbbaa` when not fully opaque.
    #[must_use]
    pub fn to_hex(self) -> String {
        if self.a == 255 {
            format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            format!("#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }
}

/// A corner or edge an overlay can be pinned to.
///
/// Shared by the keystroke display and the camera, because "bottom left" must
/// mean the same offsets for both or the two overlays collide at the same
/// nominal position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OverlayAnchor {
    /// Top-left corner.
    TopLeft,
    /// Top edge, horizontally centred.
    TopCenter,
    /// Top-right corner.
    TopRight,
    /// Bottom-left corner.
    BottomLeft,
    /// Bottom edge, horizontally centred. The default for keystrokes: it is
    /// where a viewer's eye already is during a demo, and it is the one
    /// position that never covers a window's own controls.
    #[default]
    BottomCenter,
    /// Bottom-right corner.
    BottomRight,
}

impl OverlayAnchor {
    /// Every anchor, in a stable order.
    pub const ALL: [Self; 6] = [
        Self::TopLeft,
        Self::TopCenter,
        Self::TopRight,
        Self::BottomLeft,
        Self::BottomCenter,
        Self::BottomRight,
    ];

    /// The stable settings slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::TopLeft => "top-left",
            Self::TopCenter => "top-center",
            Self::TopRight => "top-right",
            Self::BottomLeft => "bottom-left",
            Self::BottomCenter => "bottom-center",
            Self::BottomRight => "bottom-right",
        }
    }

    /// Parses a slug.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] listing every valid slug.
    pub fn from_slug(slug: &str) -> Result<Self> {
        from_slug(&Self::ALL, Self::slug, slug, "overlay position")
    }

    /// Unit position within a rectangle: `(0, 0)` top-left, `(1, 1)`
    /// bottom-right.
    ///
    /// One place decides what an anchor means geometrically, so the keystroke
    /// display and the camera cannot drift apart.
    #[must_use]
    pub const fn unit(self) -> (f32, f32) {
        match self {
            Self::TopLeft => (0.0, 0.0),
            Self::TopCenter => (0.5, 0.0),
            Self::TopRight => (1.0, 0.0),
            Self::BottomLeft => (0.0, 1.0),
            Self::BottomCenter => (0.5, 1.0),
            Self::BottomRight => (1.0, 1.0),
        }
    }

    /// Whether this anchor sits against the top edge.
    #[must_use]
    pub const fn is_top(self) -> bool {
        matches!(self, Self::TopLeft | Self::TopCenter | Self::TopRight)
    }
}

/// Three sizes, because a slider here is a worse control than a choice.
///
/// The overlays are read at a glance in a screen recording that someone else
/// will watch; the meaningful question is "can the viewer read it", and three
/// well-chosen steps answer it better than a continuous value the user has to
/// tune by trial and error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub enum OverlaySize {
    /// Unobtrusive.
    Small,
    /// The default.
    #[default]
    Medium,
    /// Readable on a downscaled or projected video.
    Large,
}

impl OverlaySize {
    /// Every size, smallest first.
    pub const ALL: [Self; 3] = [Self::Small, Self::Medium, Self::Large];

    /// The stable settings slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }

    /// Parses a slug.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] listing every valid slug.
    pub fn from_slug(slug: &str) -> Result<Self> {
        from_slug(&Self::ALL, Self::slug, slug, "overlay size")
    }

    /// A multiplier applied to each overlay's own base dimension.
    ///
    /// Deliberately not a point size: a click ripple and a keystroke chip have
    /// nothing in common except that the user wants both a bit bigger.
    #[must_use]
    pub const fn scale(self) -> f32 {
        match self {
            Self::Small => 0.75,
            Self::Medium => 1.0,
            Self::Large => 1.4,
        }
    }
}

/// Light, dark, or contrast-adaptive chrome for an overlay.
///
/// Independent because the overlay is composited into someone else's video: the
/// right answer depends on what is being recorded, not on what appearance the
/// person recording happens to run their desktop in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OverlayTheme {
    /// Use high-contrast chrome that remains legible over changing video.
    #[default]
    Adaptive,
    /// Dark chrome, light text.
    Dark,
    /// Light chrome, dark text.
    Light,
}

impl OverlayTheme {
    /// Every theme, in a stable order.
    pub const ALL: [Self; 3] = [Self::Adaptive, Self::Dark, Self::Light];

    /// The stable settings slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Adaptive => "adaptive",
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    /// Parses a slug.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] listing every valid slug.
    pub fn from_slug(slug: &str) -> Result<Self> {
        if slug == "system" {
            return Ok(Self::Adaptive);
        }
        from_slug(&Self::ALL, Self::slug, slug, "overlay theme")
    }

    /// Resolves [`Self::Adaptive`] against a sampled background.
    #[must_use]
    pub const fn resolve(self, background_is_dark: bool) -> Self {
        match self {
            Self::Adaptive if background_is_dark => Self::Light,
            Self::Adaptive => Self::Dark,
            other => other,
        }
    }

    /// Whether this resolves to dark chrome.
    #[must_use]
    pub const fn is_dark(self, background_is_dark: bool) -> bool {
        matches!(self.resolve(background_is_dark), Self::Dark)
    }
}

// ===========================================================================
// Countdown and dim screen
// ===========================================================================

/// The countdown shown between "start" and the first recorded frame (NEW-17).
///
/// It exists for two unrelated reasons and both are load-bearing: it gives the
/// user time to move the pointer off the Start button, and it gives the capture
/// engine time to warm up so the first second of the video is not a stutter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CountdownSettings {
    /// Whether to count down at all.
    pub enabled: bool,
    /// Whole seconds to count.
    pub seconds: u8,
}

impl CountdownSettings {
    /// The longest countdown offered.
    ///
    /// Ten seconds is already past the point where the user assumes it hung.
    pub const MAX_SECONDS: u8 = 10;

    /// Validates the countdown length.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the count is zero while enabled, or
    /// longer than [`Self::MAX_SECONDS`].
    pub fn validate(self) -> Result<Self> {
        if self.enabled && self.seconds == 0 {
            return Err(Error::InvalidRequest(
                "a countdown of zero seconds is not a countdown; disable it instead".to_owned(),
            ));
        }
        if self.seconds > Self::MAX_SECONDS {
            return Err(Error::InvalidRequest(format!(
                "countdown of {} s is longer than the {} s maximum",
                self.seconds,
                Self::MAX_SECONDS
            )));
        }
        Ok(self)
    }

    /// How long the countdown actually delays the recording, in seconds.
    #[must_use]
    pub fn duration_secs(self) -> f64 {
        if self.enabled {
            f64::from(self.seconds)
        } else {
            0.0
        }
    }
}

impl Default for CountdownSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            seconds: 3,
        }
    }
}

/// Dimming everything outside the recorded region while recording (NEW-16).
///
/// The dim is drawn *outside* the recorded rectangle and is never captured, so
/// it is a cue for the person recording rather than an effect in the video.
/// Getting that backwards would ruin every recording made with it on, which is
/// why [`DimSettings::covers_recorded_region`] exists as an explicit `false`
/// rather than as an unstated assumption.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DimSettings {
    /// Whether to dim at all.
    pub enabled: bool,
    /// How dark, from `0.0` (no dim) to `1.0` (black).
    pub strength: f32,
}

impl DimSettings {
    /// The dim never covers what is being recorded. Always `false`; present so
    /// the invariant is greppable and testable rather than tribal knowledge.
    pub const fn covers_recorded_region(self) -> bool {
        false
    }

    /// The effective strength, clamped and zeroed when disabled.
    #[must_use]
    pub fn effective(self) -> f32 {
        if self.enabled {
            self.strength.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Validates the strength.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the strength is outside `0.0..=1.0`
    /// or is not a number.
    pub fn validate(self) -> Result<Self> {
        if !self.strength.is_finite() || !(0.0..=1.0).contains(&self.strength) {
            return Err(Error::InvalidRequest(format!(
                "dim strength {} is outside 0.0..=1.0",
                self.strength
            )));
        }
        Ok(self)
    }
}

impl Default for DimSettings {
    fn default() -> Self {
        // Enough to read as "that part is not being recorded", not so much that
        // the user cannot see what is behind it while they work.
        Self {
            enabled: false,
            strength: 0.45,
        }
    }
}

// ===========================================================================
// Click highlights (REC-20, REC-21)
// ===========================================================================

/// How a click highlight is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ClickStyle {
    /// A ring around the pointer. The default: it shows *where* the click
    /// landed without hiding *what* was clicked.
    #[default]
    Outline,
    /// A filled disc.
    Filled,
}

impl ClickStyle {
    /// Both styles, in a stable order.
    pub const ALL: [Self; 2] = [Self::Outline, Self::Filled];

    /// The stable settings slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Outline => "outline",
            Self::Filled => "filled",
        }
    }

    /// Parses a slug.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] listing every valid slug.
    pub fn from_slug(slug: &str) -> Result<Self> {
        from_slug(&Self::ALL, Self::slug, slug, "click style")
    }
}

/// Visual click highlights burned into the recording.
///
/// Requires global mouse monitoring, which is an Accessibility grant on macOS, a
/// low-level hook on Windows, XInput2 on X11, and **is not available at all**
/// under a Wayland session without a portal. Per D15 the permission is requested
/// when the feature is first used, never at launch, and per D8 the Wayland gap is
/// reported rather than hidden.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClickSettings {
    /// Whether to draw click highlights.
    pub enabled: bool,
    /// Highlight colour.
    pub color: Rgba8,
    /// Highlight size.
    pub size: OverlaySize,
    /// Ring or disc.
    pub style: ClickStyle,
    /// Whether the highlight expands and fades, or simply blinks.
    ///
    /// Off is not merely a preference: an expanding ripple in a video that is
    /// later slowed down or frame-stepped reads as motion blur, and some users
    /// producing documentation want a crisp, single-frame-legible marker.
    pub animate: bool,
}

impl Default for ClickSettings {
    fn default() -> Self {
        Self {
            // Off by default: enabling it prompts for input monitoring (D15).
            enabled: false,
            // Scrozz iris, not a warning red — a click is not an error.
            color: Rgba8::rgb(0x7C, 0x6C, 0xF6),
            size: OverlaySize::Medium,
            style: ClickStyle::Outline,
            animate: true,
        }
    }
}

// ===========================================================================
// Keystroke display (REC-22, REC-23)
// ===========================================================================

/// Which key presses reach the on-screen display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KeystrokeScope {
    /// Every key, including plain letters.
    All,
    /// Only combinations that include a modifier, plus the named navigation and
    /// editing keys.
    ///
    /// The default, and the reason is privacy rather than taste: a recording of
    /// a person typing shows what they typed. Modifiers-only displays the
    /// shortcuts a viewer needs to learn and drops the prose, the search terms
    /// and the passwords.
    #[default]
    ModifiersOnly,
}

impl KeystrokeScope {
    /// Both scopes, in a stable order.
    pub const ALL: [Self; 2] = [Self::All, Self::ModifiersOnly];

    /// The stable settings slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::ModifiersOnly => "modifiers-only",
        }
    }

    /// Parses a slug.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] listing every valid slug.
    pub fn from_slug(slug: &str) -> Result<Self> {
        from_slug(&Self::ALL, Self::slug, slug, "keystroke scope")
    }
}

/// The on-screen keystroke display.
///
/// The same permission story as [`ClickSettings`], only more sensitive: this is
/// a keylogger-class API surface, which is exactly why it is off by default and
/// why [`KeystrokeScope::ModifiersOnly`] is the default when it is turned on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeystrokeSettings {
    /// Whether to display keystrokes.
    pub enabled: bool,
    /// Where the display sits.
    pub position: OverlayAnchor,
    /// How large the chips are.
    pub size: OverlaySize,
    /// Light or dark chips.
    pub theme: OverlayTheme,
    /// Everything, or only modifier combinations.
    pub scope: KeystrokeScope,
    /// How long a chip stays on screen after its key is released, in seconds.
    pub hold_secs: f32,
    /// How many chips may be shown at once; older ones retire first.
    pub max_visible: usize,
}

impl KeystrokeSettings {
    /// Hard privacy and layout bound for retained display chips.
    pub const MAX_VISIBLE: usize = 8;

    /// Validates the hold time and chip count.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the hold is not a positive finite
    /// number of seconds, or no chips are allowed while the display is enabled.
    pub fn validate(self) -> Result<Self> {
        if !self.hold_secs.is_finite() || self.hold_secs <= 0.0 {
            return Err(Error::InvalidRequest(format!(
                "keystroke hold of {} s must be a positive number of seconds",
                self.hold_secs
            )));
        }
        if self.enabled && self.max_visible == 0 {
            return Err(Error::InvalidRequest(
                "a keystroke display showing zero keys is not a display; disable it instead"
                    .to_owned(),
            ));
        }
        if self.max_visible > Self::MAX_VISIBLE {
            return Err(Error::InvalidRequest(format!(
                "keystroke display count {} exceeds the hard limit of {}",
                self.max_visible,
                Self::MAX_VISIBLE
            )));
        }
        Ok(self)
    }
}

impl Default for KeystrokeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            position: OverlayAnchor::BottomCenter,
            size: OverlaySize::Medium,
            theme: OverlayTheme::Adaptive,
            scope: KeystrokeScope::ModifiersOnly,
            // Long enough to read a chord, short enough that a fast typist does
            // not build a wall of chips.
            hold_secs: 1.4,
            max_visible: 4,
        }
    }
}

// ===========================================================================
// Camera (REC-24, REC-25, REC-26)
// ===========================================================================

/// The outline the camera image is masked to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CameraShape {
    /// A circle. The default, because a talking head fills a circle better than
    /// it fills a 16:9 box, and a circle reads as "a person" rather than "a
    /// second window".
    #[default]
    Circle,
    /// A rounded rectangle at the camera's own aspect ratio.
    Rounded,
    /// A plain rectangle at the camera's own aspect ratio.
    Rectangle,
}

impl CameraShape {
    /// Every shape, in a stable order.
    pub const ALL: [Self; 3] = [Self::Circle, Self::Rounded, Self::Rectangle];

    /// The stable settings slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Circle => "circle",
            Self::Rounded => "rounded",
            Self::Rectangle => "rectangle",
        }
    }

    /// Parses a slug.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] listing every valid slug.
    pub fn from_slug(slug: &str) -> Result<Self> {
        from_slug(&Self::ALL, Self::slug, slug, "camera shape")
    }

    /// Whether the shape forces a square frame, cropping the camera image.
    #[must_use]
    pub const fn is_square(self) -> bool {
        matches!(self, Self::Circle)
    }
}

/// The webcam picture-in-picture, and its fullscreen presenter mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraSettings {
    /// Whether the camera is composited at all.
    pub enabled: bool,
    /// Which corner the picture sits in.
    pub position: OverlayAnchor,
    /// The picture's height as a fraction of the recorded region's shorter
    /// edge. A fraction rather than pixels so one setting looks right on a
    /// laptop panel and on a 5K display.
    pub size: f32,
    /// The mask applied to the camera image.
    pub shape: CameraShape,
    /// Presenter mode: the camera fills the frame and the screen is inset, or
    /// dropped entirely (REC-26).
    pub presenter: bool,
    /// Mirror the camera image horizontally.
    ///
    /// On by default because an unmirrored self-view reads as wrong to the
    /// person on camera — it is the one place where "what the lens saw" is the
    /// less useful truth.
    pub mirror: bool,
}

impl CameraSettings {
    /// Smallest usable fraction: below this a face is unrecognisable.
    pub const MIN_SIZE: f32 = 0.08;
    /// Largest fraction before the camera stops being picture-*in*-picture.
    pub const MAX_SIZE: f32 = 0.5;

    /// Validates the size fraction.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the fraction falls outside
    /// [`Self::MIN_SIZE`]..=[`Self::MAX_SIZE`].
    pub fn validate(self) -> Result<Self> {
        if !self.size.is_finite() || !(Self::MIN_SIZE..=Self::MAX_SIZE).contains(&self.size) {
            return Err(Error::InvalidRequest(format!(
                "camera size {} is outside {}..={}",
                self.size,
                Self::MIN_SIZE,
                Self::MAX_SIZE
            )));
        }
        Ok(self)
    }
}

impl Default for CameraSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            position: OverlayAnchor::BottomRight,
            size: 0.22,
            shape: CameraShape::Circle,
            presenter: false,
            mirror: true,
        }
    }
}

// ===========================================================================
// Audio and video
// ===========================================================================

/// Which audio sources are mixed into the recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioSettings {
    /// Capture the microphone.
    pub microphone: bool,
    /// Capture system output.
    pub system_audio: bool,
}

impl AudioSettings {
    /// Whether any audio at all is being captured.
    #[must_use]
    pub const fn any(self) -> bool {
        self.microphone || self.system_audio
    }
}

/// The encoding quality ladder (REC-04, VID-02).
///
/// Named rungs rather than a bitrate field: a bitrate is meaningless without the
/// resolution and frame rate beside it, and asking a user to reason about all
/// three is asking them to do the encoder's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub enum Quality {
    /// Smallest file. Fine for a UI walkthrough, visibly soft on text.
    Low,
    /// The default. Text stays crisp at ordinary desktop resolutions.
    #[default]
    Balanced,
    /// Large file, for footage that will be re-encoded or edited downstream.
    High,
}

impl Quality {
    /// Every rung, lowest first.
    pub const ALL: [Self; 3] = [Self::Low, Self::Balanced, Self::High];

    /// The stable settings slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Balanced => "balanced",
            Self::High => "high",
        }
    }

    /// Parses a slug.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] listing every valid slug.
    pub fn from_slug(slug: &str) -> Result<Self> {
        from_slug(&Self::ALL, Self::slug, slug, "quality")
    }

    /// Bits per pixel per frame, the encoder-independent way to express this.
    ///
    /// Multiplied by width × height × fps to get a target bitrate, so one rung
    /// means the same visual quality at 720p30 and at 4K60 — which a fixed
    /// bitrate emphatically does not.
    #[must_use]
    pub const fn bits_per_pixel(self) -> f64 {
        match self {
            Self::Low => 0.04,
            Self::Balanced => 0.09,
            Self::High => 0.18,
        }
    }

    /// A target bitrate in bits per second for a given frame size and rate.
    #[must_use]
    pub fn target_bitrate(self, width: u32, height: u32, fps: u32) -> u64 {
        let pixels = f64::from(width) * f64::from(height) * f64::from(fps);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            (pixels * self.bits_per_pixel()).max(64_000.0) as u64
        }
    }
}

/// A ceiling on the recorded or exported frame size (REC-04, VID-03).
///
/// A cap rather than an exact size, because the aspect ratio belongs to the
/// captured region and must never be changed to satisfy a resolution setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ResolutionCap {
    /// Record at the display's own pixel dimensions, HiDPI included.
    #[default]
    Native,
    /// Cap the shorter edge at 2160 px.
    Uhd2160,
    /// Cap the shorter edge at 1440 px.
    Qhd1440,
    /// Cap the shorter edge at 1080 px.
    Fhd1080,
    /// Cap the shorter edge at 720 px.
    Hd720,
    /// Halve the native dimensions. On a HiDPI display this is the
    /// point-for-pixel size, which is usually what someone means by "smaller
    /// file, still sharp".
    Half,
}

impl ResolutionCap {
    /// Every cap, in a stable order.
    pub const ALL: [Self; 6] = [
        Self::Native,
        Self::Uhd2160,
        Self::Qhd1440,
        Self::Fhd1080,
        Self::Hd720,
        Self::Half,
    ];

    /// The stable settings slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Uhd2160 => "2160p",
            Self::Qhd1440 => "1440p",
            Self::Fhd1080 => "1080p",
            Self::Hd720 => "720p",
            Self::Half => "half",
        }
    }

    /// Parses a slug.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] listing every valid slug.
    pub fn from_slug(slug: &str) -> Result<Self> {
        from_slug(&Self::ALL, Self::slug, slug, "resolution")
    }

    /// Applies the cap to a frame size, preserving aspect ratio.
    ///
    /// The named rungs cap the **shorter** edge, because that is what "1080p"
    /// has always meant: 3840×2160 capped at 1080p is 1920×1080, not 1080×608.
    /// Capping the longest edge instead would silently shrink every landscape
    /// recording to roughly half the size the user asked for.
    ///
    /// Never scales *up*: a cap is a ceiling, and enlarging a recording to meet
    /// one would add pixels that carry no information and cost bitrate.
    /// Dimensions are rounded to even numbers because every hardware H.264
    /// encoder on all three platforms requires it, and a 1081-pixel-tall
    /// recording is a failure that surfaces minutes later at export time.
    #[must_use]
    pub fn apply(self, width: u32, height: u32) -> (u32, u32) {
        let (w, h) = (width.max(1), height.max(1));
        let scaled = match self {
            Self::Native => (w, h),
            Self::Half => (w.div_ceil(2), h.div_ceil(2)),
            _ => {
                let Some(limit) = self.shortest_edge() else {
                    return even(w, h);
                };
                let shortest = w.min(h);
                if shortest <= limit {
                    (w, h)
                } else {
                    let factor = f64::from(limit) / f64::from(shortest);
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    {
                        (
                            ((f64::from(w) * factor).round() as u32).max(1),
                            ((f64::from(h) * factor).round() as u32).max(1),
                        )
                    }
                }
            }
        };
        even(scaled.0, scaled.1)
    }

    /// The pixel ceiling on the shorter edge, where this cap has one.
    #[must_use]
    pub const fn shortest_edge(self) -> Option<u32> {
        match self {
            Self::Uhd2160 => Some(2160),
            Self::Qhd1440 => Some(1440),
            Self::Fhd1080 => Some(1080),
            Self::Hd720 => Some(720),
            Self::Native | Self::Half => None,
        }
    }
}

/// Rounds a frame size down to even dimensions, never below 2×2.
fn even(w: u32, h: u32) -> (u32, u32) {
    ((w & !1).max(2), (h & !1).max(2))
}

/// Frame rate, quality and resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSettings {
    /// Frames per second.
    pub fps: u32,
    /// The quality rung.
    pub quality: Quality,
    /// The frame-size ceiling.
    pub resolution: ResolutionCap,
}

impl VideoSettings {
    /// The slowest frame rate that still reads as motion rather than a slideshow.
    pub const MIN_FPS: u32 = 1;
    /// The fastest frame rate offered.
    pub const MAX_FPS: u32 = 240;

    /// Validates the frame rate.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the frame rate is outside
    /// [`Self::MIN_FPS`]..=[`Self::MAX_FPS`].
    pub fn validate(self) -> Result<Self> {
        if !(Self::MIN_FPS..=Self::MAX_FPS).contains(&self.fps) {
            return Err(Error::InvalidRequest(format!(
                "{} fps is outside {}..={}",
                self.fps,
                Self::MIN_FPS,
                Self::MAX_FPS
            )));
        }
        Ok(self)
    }
}

impl Default for VideoSettings {
    fn default() -> Self {
        Self {
            fps: 30,
            quality: Quality::Balanced,
            resolution: ResolutionCap::Native,
        }
    }
}

// ===========================================================================
// The whole thing
// ===========================================================================

/// Independent actions performed after a recording finalizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AfterCaptureSettings {
    /// Add completed video to the Recent Captures Overlay.
    pub recent_captures_overlay: bool,
    /// Open the Video Editor as a normal foreground window.
    pub open_editor: bool,
}

impl Default for AfterCaptureSettings {
    fn default() -> Self {
        Self {
            recent_captures_overlay: true,
            open_editor: false,
        }
    }
}

/// Every recording preference, in one value.
///
/// This is what the settings session hands to the recording session: one struct,
/// fully validated, with no strings left to parse. Everything downstream —
/// the state machine, the overlays, the HUD, the encoder — reads from here and
/// from nowhere else, so a preference cannot be honoured in one place and
/// ignored in another.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RecordingSettings {
    /// Independent actions performed after finalization.
    pub after_capture: AfterCaptureSettings,
    /// Countdown before the first frame (NEW-17).
    pub countdown: CountdownSettings,
    /// Dimming outside the recorded region (NEW-16).
    pub dim: DimSettings,
    /// Whether the pointer is drawn into the video (REC-08).
    pub cursor: CursorMode,
    /// Apply deterministic bounded smoothing to rendered cursor motion.
    pub cursor_smoothing: bool,
    /// Click highlights (REC-20/21).
    pub clicks: ClickSettings,
    /// Keystroke display (REC-22/23).
    pub keystrokes: KeystrokeSettings,
    /// Webcam picture-in-picture (REC-24/25/26).
    pub camera: CameraSettings,
    /// Microphone and system audio (REC-05/06).
    pub audio: AudioSettings,
    /// Frame rate, quality, resolution (REC-04).
    pub video: VideoSettings,
    /// Whether the next recording starts from the last selection (NEW-18,
    /// AIO-04).
    pub remember_last_selection: bool,
}

impl RecordingSettings {
    /// Validates every sub-setting, reporting the first problem found.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] from whichever sub-setting is invalid.
    pub fn validate(self) -> Result<Self> {
        self.countdown.validate()?;
        self.dim.validate()?;
        self.keystrokes.validate()?;
        self.camera.validate()?;
        self.video.validate()?;
        if self.cursor_smoothing && !self.shows_cursor() {
            return Err(Error::InvalidRequest(
                "cursor smoothing requires the recording cursor to be visible".to_owned(),
            ));
        }
        Ok(self)
    }

    /// Whether this configuration needs global input monitoring.
    ///
    /// The single predicate D15's deferred permission prompt keys off: exactly
    /// one grant covers both overlays on every platform, so it is asked for once
    /// and only when one of them is actually switched on.
    #[must_use]
    pub const fn needs_input_monitoring(&self) -> bool {
        self.clicks.enabled || self.keystrokes.enabled
    }

    /// Whether this configuration needs camera access.
    #[must_use]
    pub const fn needs_camera(&self) -> bool {
        self.camera.enabled
    }

    /// Whether this configuration needs microphone access.
    #[must_use]
    pub const fn needs_microphone(&self) -> bool {
        self.audio.microphone
    }

    /// Whether the pointer is drawn into the video.
    #[must_use]
    pub const fn shows_cursor(&self) -> bool {
        matches!(self.cursor, CursorMode::Visible)
    }
}

impl Default for CursorModeDefaults {
    fn default() -> Self {
        Self
    }
}

/// Marker documenting why the cursor default is *visible* while
/// [`CursorMode`]'s own default is hidden.
///
/// A still capture that includes the pointer is usually a mistake; a screen
/// recording without one is unwatchable, because the viewer cannot follow what
/// is being demonstrated. [`RecordingSettings::default`] therefore overrides it,
/// and this type exists so the override is documented at the place a reader
/// would look for it rather than being mistaken for a bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorModeDefaults;

// `RecordingSettings` derives `Default`, which would take `CursorMode::Hidden`.
// Recording wants the opposite, so the derive is overridden here rather than in
// core: still captures are right to default to a hidden pointer.
impl RecordingSettings {
    /// The shipped defaults.
    ///
    /// Identical to [`Default::default`] except that the pointer is visible, for
    /// the reason [`CursorModeDefaults`] explains.
    #[must_use]
    pub fn shipped() -> Self {
        Self {
            cursor: CursorMode::Visible,
            cursor_smoothing: false,
            remember_last_selection: true,
            ..Self::default()
        }
    }
}

// ===========================================================================
// Slug parsing
// ===========================================================================

/// Resolves a slug against a fixed set, naming every alternative on failure.
///
/// Shared so every enum in this module fails the same way: an agent or a user
/// who mistypes a value is told the answer, not merely told they are wrong.
fn from_slug<T: Copy>(
    all: &[T],
    slug_of: fn(T) -> &'static str,
    slug: &str,
    what: &str,
) -> Result<T> {
    all.iter()
        .copied()
        .find(|v| slug_of(*v) == slug)
        .ok_or_else(|| {
            let known: Vec<&str> = all.iter().copied().map(slug_of).collect();
            Error::InvalidRequest(format!(
                "unknown {what} {slug:?}; expected one of: {}",
                known.join(", ")
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_slug_round_trips() {
        for a in OverlayAnchor::ALL {
            assert_eq!(OverlayAnchor::from_slug(a.slug()).unwrap(), a);
        }
        for s in OverlaySize::ALL {
            assert_eq!(OverlaySize::from_slug(s.slug()).unwrap(), s);
        }
        for t in OverlayTheme::ALL {
            assert_eq!(OverlayTheme::from_slug(t.slug()).unwrap(), t);
        }
        for s in ClickStyle::ALL {
            assert_eq!(ClickStyle::from_slug(s.slug()).unwrap(), s);
        }
        for s in KeystrokeScope::ALL {
            assert_eq!(KeystrokeScope::from_slug(s.slug()).unwrap(), s);
        }
        for s in CameraShape::ALL {
            assert_eq!(CameraShape::from_slug(s.slug()).unwrap(), s);
        }
        for q in Quality::ALL {
            assert_eq!(Quality::from_slug(q.slug()).unwrap(), q);
        }
        for r in ResolutionCap::ALL {
            assert_eq!(ResolutionCap::from_slug(r.slug()).unwrap(), r);
        }
    }

    #[test]
    fn an_unknown_slug_names_the_alternatives() {
        let err = ClickStyle::from_slug("dotted").unwrap_err().to_string();
        assert!(err.contains("outline"), "{err}");
        assert!(err.contains("filled"), "{err}");
    }

    #[test]
    fn colours_parse_in_every_accepted_form() {
        assert_eq!(Rgba8::parse("#f00").unwrap(), Rgba8::rgb(255, 0, 0));
        assert_eq!(Rgba8::parse("ff0000").unwrap(), Rgba8::rgb(255, 0, 0));
        assert_eq!(
            Rgba8::parse("#ff000080").unwrap(),
            Rgba8::rgba(255, 0, 0, 128)
        );
        assert!(Rgba8::parse("#ff00").is_err());
        assert!(Rgba8::parse("#gg0000").is_err());
    }

    #[test]
    fn colours_round_trip_through_hex() {
        let opaque = Rgba8::rgb(0x7c, 0x6c, 0xf6);
        assert_eq!(opaque.to_hex(), "#7c6cf6");
        assert_eq!(Rgba8::parse(&opaque.to_hex()).unwrap(), opaque);
        let translucent = Rgba8::rgba(1, 2, 3, 4);
        assert_eq!(Rgba8::parse(&translucent.to_hex()).unwrap(), translucent);
    }

    #[test]
    fn a_resolution_cap_never_scales_up() {
        assert_eq!(ResolutionCap::Uhd2160.apply(1280, 720), (1280, 720));
        assert_eq!(ResolutionCap::Native.apply(1280, 720), (1280, 720));
    }

    #[test]
    fn a_resolution_cap_preserves_aspect_and_evenness() {
        // "1080p" caps the shorter edge, so 4K lands on exactly 1920x1080.
        let (w, h) = ResolutionCap::Fhd1080.apply(3840, 2160);
        assert_eq!((w, h), (1920, 1080));
        let (w, h) = ResolutionCap::Hd720.apply(1512, 945);
        assert_eq!(h, 720);
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
        // Aspect preserved to within the even-rounding.
        let before = 1512.0 / 945.0;
        let after = f64::from(w) / f64::from(h);
        assert!((before - after).abs() < 0.01, "{before} vs {after}");
    }

    #[test]
    fn half_resolution_halves_and_stays_even() {
        assert_eq!(ResolutionCap::Half.apply(2560, 1600), (1280, 800));
        assert_eq!(ResolutionCap::Half.apply(1, 1), (2, 2));
    }

    #[test]
    fn quality_bitrate_scales_with_pixels_and_rate() {
        let low = Quality::Low.target_bitrate(1920, 1080, 30);
        let high = Quality::High.target_bitrate(1920, 1080, 30);
        assert!(high > low * 3, "{high} vs {low}");
        let sixty = Quality::Balanced.target_bitrate(1920, 1080, 60);
        let thirty = Quality::Balanced.target_bitrate(1920, 1080, 30);
        assert_eq!(sixty, thirty * 2);
    }

    #[test]
    fn a_zero_second_countdown_is_rejected_rather_than_silently_skipped() {
        let cd = CountdownSettings {
            enabled: true,
            seconds: 0,
        };
        assert!(cd.validate().is_err());
        // Disabled, zero is simply the absence of a countdown.
        assert!(
            CountdownSettings {
                enabled: false,
                seconds: 0
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn the_dim_never_covers_what_is_being_recorded() {
        assert!(!DimSettings::default().covers_recorded_region());
    }

    #[test]
    fn shipped_defaults_show_the_pointer_and_ask_for_no_permissions() {
        let s = RecordingSettings::shipped();
        assert!(
            s.shows_cursor(),
            "a recording without a pointer is unusable"
        );
        assert!(!s.needs_input_monitoring());
        assert!(!s.needs_camera());
        assert!(!s.needs_microphone());
        assert!(s.remember_last_selection);
        assert!(s.after_capture.recent_captures_overlay);
        assert!(!s.after_capture.open_editor);
        s.validate().expect("shipped defaults must be valid");
    }

    #[test]
    fn invalid_sub_settings_are_reported_by_the_whole() {
        let mut s = RecordingSettings::shipped();
        s.video.fps = 0;
        assert!(s.validate().is_err());
        let mut s = RecordingSettings::shipped();
        s.camera.size = 0.9;
        assert!(s.validate().is_err());
        let mut s = RecordingSettings::shipped();
        s.cursor = CursorMode::Hidden;
        s.cursor_smoothing = true;
        assert!(s.validate().is_err());
        let mut s = RecordingSettings::shipped();
        s.keystrokes.max_visible = KeystrokeSettings::MAX_VISIBLE + 1;
        assert!(s.validate().is_err());
    }

    #[test]
    fn overlay_anchors_agree_on_what_a_corner_means() {
        assert_eq!(OverlayAnchor::TopLeft.unit(), (0.0, 0.0));
        assert_eq!(OverlayAnchor::BottomRight.unit(), (1.0, 1.0));
        assert!(OverlayAnchor::TopCenter.is_top());
        assert!(!OverlayAnchor::BottomCenter.is_top());
    }

    #[test]
    fn an_adaptive_theme_resolves_against_the_background() {
        assert_eq!(OverlayTheme::Adaptive.resolve(true), OverlayTheme::Light);
        assert_eq!(OverlayTheme::Adaptive.resolve(false), OverlayTheme::Dark);
        assert_eq!(OverlayTheme::Dark.resolve(false), OverlayTheme::Dark);
    }
}
