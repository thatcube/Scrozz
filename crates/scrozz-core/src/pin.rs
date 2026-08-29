//! Pure pinned-capture state and geometry.
//!
//! This module deliberately knows nothing about egui or a native window server.
//! It owns the invariants that must survive both: pins remain recoverable on the
//! current display set, movement lands on physical pixels, window captures keep
//! their native chrome, and a click-through pin cannot be locked without an
//! external way to unlock it.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Display, DisplayId, LogicalPoint, LogicalRect, LogicalSize, ScaleFactor};

/// Smallest permitted window opacity.
///
/// A pin may be faint, but never invisible: an invisible always-on-top,
/// click-through window is effectively trapped.
pub const MIN_OPACITY: f64 = 0.15;
/// Largest permitted window opacity.
pub const MAX_OPACITY: f64 = 1.0;
/// Smallest permitted image scale.
pub const MIN_PIN_SCALE: f64 = 0.1;
/// Largest permitted image scale.
pub const MAX_PIN_SCALE: f64 = 4.0;
/// Maximum edge length of a newly-created pin, in logical points.
pub const DEFAULT_PIN_MAX_EDGE: f64 = 420.0;
/// Largest physical edge a pin viewport may ask the GPU/window server to allocate.
pub const MAX_PIN_PHYSICAL_EDGE: u32 = 4_096;
/// Largest physical backing surface a pin viewport may allocate.
pub const MAX_PIN_PHYSICAL_PIXELS: u64 = 8 * 1_024 * 1_024;

/// Invalid source geometry for a pinned capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidPinSize;

impl fmt::Display for InvalidPinSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("pinned capture dimensions must be finite and greater than zero")
    }
}

impl std::error::Error for InvalidPinSize {}

/// Stable identity of a pinned capture.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PinId(pub String);

impl From<String> for PinId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for PinId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Display for PinId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Native window opacity, clamped to a recoverable range.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(from = "f64", into = "f64")]
pub struct Opacity(f64);

impl Opacity {
    /// Fully opaque.
    pub const OPAQUE: Self = Self(MAX_OPACITY);

    /// Creates an opacity, clamping invalid or out-of-range input.
    #[must_use]
    pub fn new(value: f64) -> Self {
        let value = if value.is_finite() {
            value
        } else {
            MAX_OPACITY
        };
        Self(value.clamp(MIN_OPACITY, MAX_OPACITY))
    }

    /// The normalized alpha value.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl Default for Opacity {
    fn default() -> Self {
        Self::OPAQUE
    }
}

impl From<f64> for Opacity {
    fn from(value: f64) -> Self {
        Self::new(value)
    }
}

impl From<Opacity> for f64 {
    fn from(value: Opacity) -> Self {
        value.get()
    }
}

/// Image scale for a pinned capture.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct PinScale(f64);

impl PinScale {
    /// Original capture size.
    pub const ORIGINAL: Self = Self(1.0);

    /// Creates a scale, clamping invalid or out-of-range input.
    #[must_use]
    pub fn new(value: f64) -> Self {
        let value = if value.is_finite() { value } else { 1.0 };
        Self(value.clamp(MIN_PIN_SCALE, MAX_PIN_SCALE))
    }

    /// Chooses a scale that keeps a new pin compact without enlarging it.
    #[must_use]
    pub fn fit(size: LogicalSize, max_edge: f64) -> Self {
        let longest = size.width.max(size.height);
        if longest <= 0.0 || !longest.is_finite() || max_edge <= 0.0 || !max_edge.is_finite() {
            return Self::ORIGINAL;
        }
        Self((max_edge / longest).clamp(f64::MIN_POSITIVE, 1.0))
    }

    /// The scale multiplier.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }

    fn for_surface(value: f64, minimum: f64, maximum: f64) -> Self {
        let value = if value.is_finite() { value } else { 1.0 };
        let maximum = maximum.clamp(f64::MIN_POSITIVE, MAX_PIN_SCALE);
        Self(value.clamp(minimum.min(maximum), maximum))
    }

    fn from_persisted(value: f64) -> Self {
        let value = if value.is_finite() && value > 0.0 {
            value
        } else {
            1.0
        };
        Self(value.clamp(f64::MIN_POSITIVE, MAX_PIN_SCALE))
    }
}

impl Default for PinScale {
    fn default() -> Self {
        Self::ORIGINAL
    }
}

impl From<f64> for PinScale {
    fn from(value: f64) -> Self {
        Self::new(value)
    }
}

impl From<PinScale> for f64 {
    fn from(value: PinScale) -> Self {
        value.get()
    }
}

impl Serialize for PinScale {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f64(self.0)
    }
}

impl<'de> Deserialize<'de> for PinScale {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        f64::deserialize(deserializer).map(Self::from_persisted)
    }
}

/// Optional border drawn around a pinned capture.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PinBorder {
    /// Border width in logical points.
    pub width: f64,
}

impl PinBorder {
    /// Creates a border with a non-negative, finite width.
    #[must_use]
    pub fn new(width: f64) -> Self {
        Self {
            width: if width.is_finite() {
                width.max(0.0)
            } else {
                0.0
            },
        }
    }
}

/// Synthetic chrome around a pinned image.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PinChrome {
    /// Rounded-corner radius in logical points.
    pub corner_radius: f64,
    /// Whether the OS or UI should draw a shadow.
    pub shadow: bool,
    /// Optional outline.
    pub border: Option<PinBorder>,
}

impl PinChrome {
    /// No synthetic chrome, as required for window-provenance captures by D9.
    pub const NONE: Self = Self {
        corner_radius: 0.0,
        shadow: false,
        border: None,
    };

    /// Creates chrome, sanitizing the radius.
    #[must_use]
    pub fn new(corner_radius: f64, shadow: bool, border: Option<PinBorder>) -> Self {
        Self {
            corner_radius: if corner_radius.is_finite() {
                corner_radius.max(0.0)
            } else {
                0.0
            },
            shadow,
            border,
        }
    }
}

impl Default for PinChrome {
    fn default() -> Self {
        Self::new(10.0, true, Some(PinBorder::new(1.0)))
    }
}

/// Whether synthetic image chrome is legal for this capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinChromePolicy {
    /// Display, region, all-display, and stitched captures may be decorated.
    Allowed,
    /// Window captures already contain their native shape and shadow (D9).
    Forbidden,
}

/// Durable state of one on-screen pin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PinState {
    /// Window frame in global logical coordinates.
    pub frame: LogicalRect,
    /// Native window opacity.
    #[serde(default)]
    pub opacity: Opacity,
    /// Scale relative to the source image's logical size.
    #[serde(default)]
    pub scale: PinScale,
    /// Synthetic display chrome.
    #[serde(default)]
    pub chrome: PinChrome,
    /// Whether the window is click-through.
    #[serde(default)]
    pub locked: bool,
    /// Last display that owned the pin.
    #[serde(default)]
    pub display: Option<DisplayId>,
}

impl PinState {
    /// Creates a durable pin state.
    #[must_use]
    pub fn new(frame: LogicalRect, scale: PinScale, display: Option<DisplayId>) -> Self {
        Self {
            frame,
            opacity: Opacity::default(),
            scale,
            chrome: PinChrome::default(),
            locked: false,
            display,
        }
    }
}

/// A route that remains usable when the pin itself ignores pointer events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockEscape {
    /// Unlock all pins from the app's tray/status menu.
    TrayMenu,
    /// Unlock or unpin through the command-line interface.
    CommandLine,
    /// A registered system-wide keyboard shortcut.
    GlobalHotkey,
}

/// Refusal to make a pin click-through without a recovery route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockEscapeRequired;

impl fmt::Display for LockEscapeRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a locked pin requires an external non-pointer unlock route")
    }
}

impl std::error::Error for LockEscapeRequired {}

/// Arrow-key movement direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Move toward smaller x.
    Left,
    /// Move toward larger x.
    Right,
    /// Move toward smaller y.
    Up,
    /// Move toward larger y.
    Down,
}

/// Distance used for one keyboard nudge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeStep {
    /// One logical point.
    Fine,
    /// Ten logical points.
    Normal,
    /// Snap to the corresponding display work-area edge.
    Edge,
}

/// A snapshot of the connected display topology.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplaySet {
    displays: Vec<Display>,
}

impl DisplaySet {
    /// Creates a topology snapshot.
    #[must_use]
    pub fn new(displays: Vec<Display>) -> Self {
        Self { displays }
    }

    /// Connected displays.
    #[must_use]
    pub fn displays(&self) -> &[Display] {
        &self.displays
    }

    /// Finds a display by stable ID.
    #[must_use]
    pub fn get(&self, id: &DisplayId) -> Option<&Display> {
        self.displays.iter().find(|display| &display.id == id)
    }

    /// Finds the display whose full bounds contain a logical point.
    #[must_use]
    pub fn containing(&self, point: LogicalPoint) -> Option<&Display> {
        self.displays
            .iter()
            .find(|display| contains(display.bounds, point))
    }

    /// Chooses a display for live movement.
    ///
    /// The frame centre wins first so ownership changes at the intuitive moment
    /// during a cross-display nudge. Intersection handles gaps and oversized
    /// windows, then primary/nearest are deterministic fallbacks.
    #[must_use]
    pub fn display_for_frame(&self, frame: LogicalRect) -> Option<&Display> {
        let center = LogicalPoint::new(
            frame.origin.x + frame.size.width / 2.0,
            frame.origin.y + frame.size.height / 2.0,
        );
        self.containing(center)
            .or_else(|| greatest_intersection(&self.displays, frame))
            .or_else(|| self.displays.iter().find(|display| display.is_primary))
            .or_else(|| nearest(&self.displays, frame))
    }

    /// Chooses a display while restoring persisted geometry.
    ///
    /// A still-connected saved display is authoritative. If it disappeared,
    /// intersection, primary, and nearest fallbacks keep the pin recoverable.
    #[must_use]
    pub fn best_for(&self, frame: LogicalRect, saved: Option<&DisplayId>) -> Option<&Display> {
        saved
            .and_then(|id| self.get(id))
            .or_else(|| greatest_intersection(&self.displays, frame))
            .or_else(|| self.displays.iter().find(|display| display.is_primary))
            .or_else(|| nearest(&self.displays, frame))
    }

    /// Finds the nearest display wholly in `direction` from `current`.
    fn directional_neighbor(&self, current: &Display, direction: Direction) -> Option<&Display> {
        self.displays
            .iter()
            .filter(|candidate| candidate.id != current.id)
            .filter_map(|candidate| {
                directional_score(current.work_area, candidate.work_area, direction)
                    .map(|score| (candidate, score))
            })
            .min_by(|(left, left_score), (right, right_score)| {
                left_score
                    .total_cmp(right_score)
                    .then_with(|| left.id.0.cmp(&right.id.0))
            })
            .map(|(display, _)| display)
    }

    /// Clamps a frame to a display's work area, never its raw bounds.
    ///
    /// Pins that fit are kept entirely visible. Oversized pins retain at least
    /// `minimum_visible` points on each axis, preserving a reachable edge.
    #[must_use]
    pub fn clamp_visible(
        &self,
        frame: LogicalRect,
        display: &Display,
        minimum_visible: LogicalSize,
    ) -> LogicalRect {
        let work = display.work_area;
        let width = finite_nonnegative(frame.size.width);
        let height = finite_nonnegative(frame.size.height);
        let min_visible_x = minimum_visible.width.max(1.0).min(width.max(1.0));
        let min_visible_y = minimum_visible.height.max(1.0).min(height.max(1.0));

        let x = clamp_axis(
            finite_or(frame.origin.x, work.origin.x),
            width,
            work.origin.x,
            work.size.width,
            min_visible_x,
        );
        let y = clamp_axis(
            finite_or(frame.origin.y, work.origin.y),
            height,
            work.origin.y,
            work.size.height,
            min_visible_y,
        );

        LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(width, height))
    }
}

/// Pure runtime model for one pinned capture.
#[derive(Debug, Clone, PartialEq)]
pub struct PinnedSurface {
    id: PinId,
    natural_size: LogicalSize,
    chrome_policy: PinChromePolicy,
    escapes: Vec<LockEscape>,
    state: PinState,
}

impl PinnedSurface {
    /// Creates a compact pin near the top-right of a display's work area.
    pub fn on_display(
        id: PinId,
        natural_size: LogicalSize,
        display: &Display,
        chrome_policy: PinChromePolicy,
        escapes: Vec<LockEscape>,
    ) -> Result<Self, InvalidPinSize> {
        validate_natural_size(natural_size)?;
        let scale = bounded_scale(
            natural_size,
            PinScale::fit(natural_size, DEFAULT_PIN_MAX_EDGE).get(),
            display,
        );
        let size = scaled_size(natural_size, scale);
        let margin = 24.0;
        let frame = LogicalRect::new(
            LogicalPoint::new(
                display.work_area.origin.x + display.work_area.size.width - size.width - margin,
                display.work_area.origin.y + margin,
            ),
            size,
        );
        let mut surface = Self {
            id,
            natural_size,
            chrome_policy,
            escapes,
            state: PinState::new(frame, scale, Some(display.id.clone())),
        };
        surface.enforce_chrome_policy();
        let _ = surface.reconcile(&DisplaySet::new(vec![display.clone()]));
        Ok(surface)
    }

    /// Restores a persisted pin and reconciles it with the current displays.
    pub fn restore(
        id: PinId,
        state: PinState,
        chrome_policy: PinChromePolicy,
        escapes: Vec<LockEscape>,
        displays: &DisplaySet,
    ) -> Result<Option<Self>, InvalidPinSize> {
        let scale = state.scale.get();
        let natural_size = LogicalSize::new(
            state.frame.size.width / scale,
            state.frame.size.height / scale,
        );
        Self::restore_with_natural_size(id, natural_size, state, chrome_policy, escapes, displays)
    }

    /// Restores persisted state against authoritative source dimensions.
    pub fn restore_with_natural_size(
        id: PinId,
        natural_size: LogicalSize,
        state: PinState,
        chrome_policy: PinChromePolicy,
        escapes: Vec<LockEscape>,
        displays: &DisplaySet,
    ) -> Result<Option<Self>, InvalidPinSize> {
        validate_natural_size(natural_size)?;
        let mut surface = Self {
            id,
            natural_size,
            chrome_policy,
            escapes,
            state,
        };
        surface.enforce_chrome_policy();
        if surface.reconcile(displays).is_none() {
            return Ok(None);
        }
        if surface.state.locked && surface.escapes.is_empty() {
            surface.state.locked = false;
        }
        Ok(Some(surface))
    }

    /// Stable pin identity.
    #[must_use]
    pub fn id(&self) -> &PinId {
        &self.id
    }

    /// Durable state.
    #[must_use]
    pub fn state(&self) -> &PinState {
        &self.state
    }

    /// Whether synthetic radius, shadow, and border controls are legal.
    #[must_use]
    pub const fn allows_synthetic_chrome(&self) -> bool {
        matches!(self.chrome_policy, PinChromePolicy::Allowed)
    }

    /// Whether locking leaves a registered route outside the pin window.
    #[must_use]
    pub fn has_lock_escape(&self) -> bool {
        !self.escapes.is_empty()
    }

    /// Successfully established routes outside this pin window.
    #[must_use]
    pub fn lock_escapes(&self) -> &[LockEscape] {
        &self.escapes
    }

    /// Changes native opacity.
    pub fn set_opacity(&mut self, opacity: f64) {
        self.state.opacity = Opacity::new(opacity);
    }

    /// Changes the image scale and clamps the resized window.
    pub fn set_scale(&mut self, scale: f64, displays: &DisplaySet) {
        // Exceptionally large captures may need an initial fit below the normal
        // 10% interactive floor. Keep that fitted size reachable instead of
        // making the first resize jump to a window larger than the display.
        let minimum = PinScale::fit(self.natural_size, DEFAULT_PIN_MAX_EDGE)
            .get()
            .min(MIN_PIN_SCALE);
        let maximum = displays
            .display_for_frame(self.state.frame)
            .or_else(|| self.state.display.as_ref().and_then(|id| displays.get(id)))
            .map_or(MAX_PIN_SCALE, |display| {
                maximum_scale(self.natural_size, display)
            });
        self.state.scale = PinScale::for_surface(scale, minimum, maximum);
        self.state.frame.size = scaled_size(self.natural_size, self.state.scale);
        let _ = self.reconcile(displays);
    }

    /// Changes synthetic chrome when the capture permits it.
    pub fn set_chrome(&mut self, chrome: PinChrome) {
        if self.allows_synthetic_chrome() {
            self.state.chrome = chrome;
        } else {
            self.state.chrome = PinChrome::NONE;
        }
    }

    /// Locks or unlocks pointer interaction.
    ///
    /// Locking is rejected unless at least one route remains available outside
    /// the click-through window itself.
    pub fn set_locked(&mut self, locked: bool) -> Result<(), LockEscapeRequired> {
        if locked && self.escapes.is_empty() {
            return Err(LockEscapeRequired);
        }
        self.state.locked = locked;
        Ok(())
    }

    /// Reconcile geometry reported by the native child window after a drag.
    ///
    /// The window manager is the source of truth while the pointer owns the
    /// drag. The resulting state remains clamped to a work area and snapped to
    /// a whole physical pixel so restores do not accumulate fractional-DPI
    /// drift.
    pub fn sync_native_frame(&mut self, frame: LogicalRect, displays: &DisplaySet) -> bool {
        let Some(display) = displays
            .display_for_frame(frame)
            .or_else(|| self.state.display.as_ref().and_then(|id| displays.get(id)))
            .or_else(|| displays.best_for(frame, self.state.display.as_ref()))
        else {
            return false;
        };
        self.state.scale = bounded_scale(self.natural_size, self.state.scale.get(), display);
        let mut frame = frame;
        frame.size = scaled_size(self.natural_size, self.state.scale);
        frame = displays.clamp_visible(frame, display, LogicalSize::new(48.0, 32.0));
        frame.origin = snap_origin(frame.origin, display.scale);
        frame = displays.clamp_visible(frame, display, LogicalSize::new(48.0, 32.0));
        let changed = self.state.frame != frame || self.state.display.as_ref() != Some(&display.id);
        self.state.frame = frame;
        self.state.display = Some(display.id.clone());
        changed
    }

    /// Moves by one keyboard step, snapping the final origin to whole physical
    /// pixels on the destination display.
    pub fn nudge(&mut self, direction: Direction, step: NudgeStep, displays: &DisplaySet) -> bool {
        let Some(current) = displays
            .display_for_frame(self.state.frame)
            .or_else(|| self.state.display.as_ref().and_then(|id| displays.get(id)))
        else {
            return false;
        };

        let mut frame = self.state.frame;
        if !matches!(step, NudgeStep::Edge)
            && frame_is_at_edge(frame, current.work_area, direction)
            && let Some(destination) = displays.directional_neighbor(current, direction)
        {
            place_at_near_edge(&mut frame, destination.work_area, direction);
            self.state.scale =
                bounded_scale(self.natural_size, self.state.scale.get(), destination);
            frame.size = scaled_size(self.natural_size, self.state.scale);
            frame = displays.clamp_visible(frame, destination, LogicalSize::new(48.0, 32.0));
            frame.origin = snap_origin(frame.origin, destination.scale);
            frame = displays.clamp_visible(frame, destination, LogicalSize::new(48.0, 32.0));
            self.state.frame = frame;
            self.state.display = Some(destination.id.clone());
            return true;
        }

        match step {
            NudgeStep::Fine | NudgeStep::Normal => {
                let amount = if matches!(step, NudgeStep::Fine) {
                    1.0
                } else {
                    10.0
                };
                move_by_physical_step(&mut frame.origin, direction, amount, current.scale);
            }
            NudgeStep::Edge => {
                let work = current.work_area;
                match direction {
                    Direction::Left => frame.origin.x = work.origin.x,
                    Direction::Right => {
                        frame.origin.x = work.origin.x + work.size.width - frame.size.width;
                    }
                    Direction::Up => frame.origin.y = work.origin.y,
                    Direction::Down => {
                        frame.origin.y = work.origin.y + work.size.height - frame.size.height;
                    }
                }
            }
        }

        let destination = if matches!(step, NudgeStep::Edge) {
            current
        } else {
            let Some(destination) = displays.display_for_frame(frame) else {
                return false;
            };
            destination
        };
        self.state.scale = bounded_scale(self.natural_size, self.state.scale.get(), destination);
        frame.size = scaled_size(self.natural_size, self.state.scale);
        frame = displays.clamp_visible(frame, destination, LogicalSize::new(48.0, 32.0));
        frame.origin = snap_origin(frame.origin, destination.scale);
        frame = displays.clamp_visible(frame, destination, LogicalSize::new(48.0, 32.0));

        self.state.frame = frame;
        self.state.display = Some(destination.id.clone());
        true
    }

    /// Reconciles persisted geometry against the current display topology.
    pub fn reconcile(&mut self, displays: &DisplaySet) -> Option<()> {
        let display = displays.best_for(self.state.frame, self.state.display.as_ref())?;
        self.state.scale = bounded_scale(self.natural_size, self.state.scale.get(), display);
        self.state.frame.size = scaled_size(self.natural_size, self.state.scale);
        self.state.frame =
            displays.clamp_visible(self.state.frame, display, LogicalSize::new(48.0, 32.0));
        self.state.frame.origin = snap_origin(self.state.frame.origin, display.scale);
        self.state.display = Some(display.id.clone());
        Some(())
    }

    fn enforce_chrome_policy(&mut self) {
        if !self.allows_synthetic_chrome() {
            self.state.chrome = PinChrome::NONE;
        } else {
            self.state.chrome = PinChrome::new(
                self.state.chrome.corner_radius,
                self.state.chrome.shadow,
                self.state
                    .chrome
                    .border
                    .map(|border| PinBorder::new(border.width)),
            );
        }
    }
}

fn scaled_size(size: LogicalSize, scale: PinScale) -> LogicalSize {
    LogicalSize::new(size.width * scale.get(), size.height * scale.get())
}

fn validate_natural_size(size: LogicalSize) -> Result<(), InvalidPinSize> {
    if size.width.is_finite() && size.height.is_finite() && size.width > 0.0 && size.height > 0.0 {
        Ok(())
    } else {
        Err(InvalidPinSize)
    }
}

fn bounded_scale(size: LogicalSize, requested: f64, display: &Display) -> PinScale {
    let maximum = maximum_scale(size, display);
    PinScale::for_surface(requested, f64::MIN_POSITIVE, maximum)
}

fn maximum_scale(size: LogicalSize, display: &Display) -> f64 {
    let display_scale = display.scale.get();
    let physical_width = size.width * display_scale;
    let physical_height = size.height * display_scale;
    let edge_limit = f64::from(MAX_PIN_PHYSICAL_EDGE)
        / physical_width.max(physical_height).max(f64::MIN_POSITIVE);
    let physical_area = physical_width * physical_height;
    let area_limit = if physical_area.is_finite() && physical_area > 0.0 {
        (MAX_PIN_PHYSICAL_PIXELS as f64 / physical_area).sqrt()
    } else {
        f64::MIN_POSITIVE
    };
    edge_limit
        .min(area_limit)
        .clamp(f64::MIN_POSITIVE, MAX_PIN_SCALE)
}

fn snap_origin(origin: LogicalPoint, scale: ScaleFactor) -> LogicalPoint {
    let scale = scale.get();
    LogicalPoint::new(
        (origin.x * scale).round() / scale,
        (origin.y * scale).round() / scale,
    )
}

fn move_by_physical_step(
    origin: &mut LogicalPoint,
    direction: Direction,
    logical_amount: f64,
    scale: ScaleFactor,
) {
    let scale = scale.get();
    let physical_step = (logical_amount * scale).round().max(1.0);
    match direction {
        Direction::Left => origin.x = ((origin.x * scale).round() - physical_step) / scale,
        Direction::Right => origin.x = ((origin.x * scale).round() + physical_step) / scale,
        Direction::Up => origin.y = ((origin.y * scale).round() - physical_step) / scale,
        Direction::Down => origin.y = ((origin.y * scale).round() + physical_step) / scale,
    }
}

fn contains(rect: LogicalRect, point: LogicalPoint) -> bool {
    point.x >= rect.origin.x
        && point.y >= rect.origin.y
        && point.x < rect.origin.x + rect.size.width
        && point.y < rect.origin.y + rect.size.height
}

fn intersection_area(a: LogicalRect, b: LogicalRect) -> f64 {
    let left = a.origin.x.max(b.origin.x);
    let top = a.origin.y.max(b.origin.y);
    let right = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let bottom = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    (right - left).max(0.0) * (bottom - top).max(0.0)
}

fn greatest_intersection(displays: &[Display], frame: LogicalRect) -> Option<&Display> {
    displays
        .iter()
        .map(|display| (display, intersection_area(display.work_area, frame)))
        .filter(|(_, area)| *area > 0.0)
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .map(|(display, _)| display)
}

fn nearest(displays: &[Display], frame: LogicalRect) -> Option<&Display> {
    let center = LogicalPoint::new(
        frame.origin.x + frame.size.width / 2.0,
        frame.origin.y + frame.size.height / 2.0,
    );
    displays.iter().min_by(|a, b| {
        distance_squared(center, a.work_area).total_cmp(&distance_squared(center, b.work_area))
    })
}

fn distance_squared(point: LogicalPoint, rect: LogicalRect) -> f64 {
    let x = point
        .x
        .clamp(rect.origin.x, rect.origin.x + rect.size.width);
    let y = point
        .y
        .clamp(rect.origin.y, rect.origin.y + rect.size.height);
    (point.x - x).powi(2) + (point.y - y).powi(2)
}

fn directional_score(
    current: LogicalRect,
    candidate: LogicalRect,
    direction: Direction,
) -> Option<f64> {
    let current_right = current.origin.x + current.size.width;
    let current_bottom = current.origin.y + current.size.height;
    let candidate_right = candidate.origin.x + candidate.size.width;
    let candidate_bottom = candidate.origin.y + candidate.size.height;
    let (primary_gap, orthogonal_gap) = match direction {
        Direction::Left if candidate_right <= current.origin.x => (
            current.origin.x - candidate_right,
            interval_gap(
                current.origin.y,
                current_bottom,
                candidate.origin.y,
                candidate_bottom,
            ),
        ),
        Direction::Right if candidate.origin.x >= current_right => (
            candidate.origin.x - current_right,
            interval_gap(
                current.origin.y,
                current_bottom,
                candidate.origin.y,
                candidate_bottom,
            ),
        ),
        Direction::Up if candidate_bottom <= current.origin.y => (
            current.origin.y - candidate_bottom,
            interval_gap(
                current.origin.x,
                current_right,
                candidate.origin.x,
                candidate_right,
            ),
        ),
        Direction::Down if candidate.origin.y >= current_bottom => (
            candidate.origin.y - current_bottom,
            interval_gap(
                current.origin.x,
                current_right,
                candidate.origin.x,
                candidate_right,
            ),
        ),
        _ => return None,
    };
    Some(primary_gap.mul_add(primary_gap, orthogonal_gap * orthogonal_gap))
}

fn interval_gap(a_start: f64, a_end: f64, b_start: f64, b_end: f64) -> f64 {
    if a_end < b_start {
        b_start - a_end
    } else if b_end < a_start {
        a_start - b_end
    } else {
        0.0
    }
}

fn frame_is_at_edge(frame: LogicalRect, work: LogicalRect, direction: Direction) -> bool {
    const EPSILON: f64 = 1.0e-6;
    match direction {
        Direction::Left => {
            frame.size.width <= work.size.width + EPSILON
                && frame.origin.x <= work.origin.x + EPSILON
        }
        Direction::Right => {
            frame.size.width <= work.size.width + EPSILON
                && frame.origin.x + frame.size.width >= work.origin.x + work.size.width - EPSILON
        }
        Direction::Up => {
            frame.size.height <= work.size.height + EPSILON
                && frame.origin.y <= work.origin.y + EPSILON
        }
        Direction::Down => {
            frame.size.height <= work.size.height + EPSILON
                && frame.origin.y + frame.size.height >= work.origin.y + work.size.height - EPSILON
        }
    }
}

fn place_at_near_edge(frame: &mut LogicalRect, work: LogicalRect, direction: Direction) {
    match direction {
        Direction::Left => {
            frame.origin.x = work.origin.x + work.size.width - frame.size.width;
        }
        Direction::Right => frame.origin.x = work.origin.x,
        Direction::Up => {
            frame.origin.y = work.origin.y + work.size.height - frame.size.height;
        }
        Direction::Down => frame.origin.y = work.origin.y,
    }
}

fn clamp_axis(
    value: f64,
    extent: f64,
    work_origin: f64,
    work_extent: f64,
    minimum_visible: f64,
) -> f64 {
    if extent <= work_extent {
        value.clamp(work_origin, work_origin + work_extent - extent)
    } else {
        value.clamp(
            work_origin + minimum_visible - extent,
            work_origin + work_extent - minimum_visible,
        )
    }
}

fn finite_nonnegative(value: f64) -> f64 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn finite_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() { value } else { fallback }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(
        id: &str,
        x: f64,
        width: f64,
        work_top: f64,
        work_height: f64,
        scale: f64,
        primary: bool,
    ) -> Display {
        Display {
            id: DisplayId(id.to_owned()),
            name: id.to_owned(),
            bounds: LogicalRect::new(LogicalPoint::new(x, 0.0), LogicalSize::new(width, 900.0)),
            work_area: LogicalRect::new(
                LogicalPoint::new(x, work_top),
                LogicalSize::new(width, work_height),
            ),
            scale: ScaleFactor::new(scale),
            is_primary: primary,
        }
    }

    #[test]
    fn opacity_and_scale_are_never_trapping_values() {
        assert_eq!(Opacity::new(-4.0).get(), MIN_OPACITY);
        assert_eq!(Opacity::new(4.0).get(), MAX_OPACITY);
        assert_eq!(Opacity::new(f64::NAN), Opacity::OPAQUE);
        assert_eq!(PinScale::new(0.0).get(), MIN_PIN_SCALE);
        assert_eq!(PinScale::new(99.0).get(), MAX_PIN_SCALE);
        assert_eq!(PinScale::new(f64::INFINITY), PinScale::ORIGINAL);
    }

    #[test]
    fn exceptionally_large_new_pins_still_fit_the_declared_maximum() {
        let scale = PinScale::fit(LogicalSize::new(10_000.0, 4_000.0), DEFAULT_PIN_MAX_EDGE);

        assert!(scale.get() < MIN_PIN_SCALE);
        assert_eq!(10_000.0 * scale.get(), DEFAULT_PIN_MAX_EDGE);
    }

    #[test]
    fn restore_prefers_saved_display_then_recovers_when_it_is_gone() {
        let left = display("left", 0.0, 800.0, 24.0, 840.0, 2.0, true);
        let right = display("right", 800.0, 1_200.0, 0.0, 860.0, 1.0, false);
        let frame = LogicalRect::new(
            LogicalPoint::new(900.0, 100.0),
            LogicalSize::new(200.0, 120.0),
        );
        let set = DisplaySet::new(vec![left.clone(), right.clone()]);
        assert_eq!(
            set.best_for(frame, Some(&left.id)).map(|it| &it.id),
            Some(&left.id)
        );

        let set = DisplaySet::new(vec![right.clone()]);
        assert_eq!(
            set.best_for(frame, Some(&left.id)).map(|it| &it.id),
            Some(&right.id)
        );
    }

    #[test]
    fn clamping_uses_work_area_and_preserves_an_edge_of_oversized_pins() {
        let display = display("main", 0.0, 800.0, 24.0, 840.0, 2.0, true);
        let set = DisplaySet::new(vec![display.clone()]);
        let offscreen = LogicalRect::new(
            LogicalPoint::new(-1_000.0, -1_000.0),
            LogicalSize::new(200.0, 100.0),
        );
        let clamped = set.clamp_visible(offscreen, &display, LogicalSize::new(48.0, 32.0));
        assert_eq!(clamped.origin, LogicalPoint::new(0.0, 24.0));

        let huge = LogicalRect::new(
            LogicalPoint::new(-2_000.0, 3_000.0),
            LogicalSize::new(1_000.0, 1_000.0),
        );
        let clamped = set.clamp_visible(huge, &display, LogicalSize::new(48.0, 32.0));
        assert_eq!(clamped.origin.x, -952.0);
        assert_eq!(clamped.origin.y, 832.0);
    }

    #[test]
    fn nudge_is_logical_then_snaps_to_destination_physical_pixels() {
        let left = display("left", 0.0, 100.0, 0.0, 100.0, 2.0, true);
        let right = display("right", 100.0, 100.0, 0.0, 100.0, 1.5, false);
        let set = DisplaySet::new(vec![left.clone(), right.clone()]);
        let state = PinState::new(
            LogicalRect::new(LogicalPoint::new(94.5, 10.0), LogicalSize::new(10.0, 10.0)),
            PinScale::ORIGINAL,
            Some(left.id),
        );
        let mut pin = PinnedSurface::restore(
            PinId::from("pin"),
            state,
            PinChromePolicy::Allowed,
            vec![LockEscape::TrayMenu],
            &set,
        )
        .unwrap()
        .unwrap();
        pin.state.frame.origin.x = 94.5;

        assert!(pin.nudge(Direction::Right, NudgeStep::Fine, &set));
        assert_eq!(pin.state.display.as_ref(), Some(&right.id));
        assert_eq!(pin.state.frame.origin.x * 1.5 % 1.0, 0.0);
    }

    #[test]
    fn opposite_fractional_dpi_nudges_are_reversible() {
        let display = display("fractional", 0.0, 400.0, 0.0, 300.0, 1.5, true);
        let set = DisplaySet::new(vec![display.clone()]);
        let state = PinState::new(
            LogicalRect::new(
                LogicalPoint::new(2.0 / 3.0, 10.0),
                LogicalSize::new(100.0, 60.0),
            ),
            PinScale::ORIGINAL,
            Some(display.id),
        );
        let mut pin = PinnedSurface::restore(
            PinId::from("fractional"),
            state,
            PinChromePolicy::Allowed,
            vec![LockEscape::TrayMenu],
            &set,
        )
        .unwrap()
        .unwrap();
        let start = pin.state.frame.origin;

        assert!(pin.nudge(Direction::Right, NudgeStep::Fine, &set));
        assert!(pin.nudge(Direction::Left, NudgeStep::Fine, &set));

        assert_eq!(pin.state.frame.origin, start);
    }

    #[test]
    fn repeated_nudges_cross_a_display_boundary_instead_of_reclamping_forever() {
        let left = display("left", 0.0, 100.0, 0.0, 100.0, 2.0, true);
        let right = display("right", 100.0, 120.0, 0.0, 100.0, 1.5, false);
        let set = DisplaySet::new(vec![left.clone(), right.clone()]);
        let state = PinState::new(
            LogicalRect::new(LogicalPoint::new(70.0, 20.0), LogicalSize::new(20.0, 20.0)),
            PinScale::ORIGINAL,
            Some(left.id.clone()),
        );
        let mut pin = PinnedSurface::restore(
            PinId::from("crossing"),
            state,
            PinChromePolicy::Allowed,
            vec![LockEscape::TrayMenu],
            &set,
        )
        .unwrap()
        .unwrap();

        assert!(pin.nudge(Direction::Right, NudgeStep::Normal, &set));
        assert_eq!(pin.state.frame.origin.x, 80.0);
        assert_eq!(pin.state.display.as_ref(), Some(&left.id));

        assert!(pin.nudge(Direction::Right, NudgeStep::Normal, &set));
        assert_eq!(pin.state.frame.origin.x, 100.0);
        assert_eq!(pin.state.display.as_ref(), Some(&right.id));

        assert!(pin.nudge(Direction::Left, NudgeStep::Fine, &set));
        assert_eq!(pin.state.frame.origin.x, 80.0);
        assert_eq!(pin.state.display.as_ref(), Some(&left.id));
    }

    #[test]
    fn edge_nudge_stays_on_the_current_display() {
        let left = display("left", 0.0, 100.0, 0.0, 100.0, 2.0, true);
        let right = display("right", 100.0, 120.0, 0.0, 100.0, 1.5, false);
        let set = DisplaySet::new(vec![left.clone(), right]);
        let state = PinState::new(
            LogicalRect::new(LogicalPoint::new(80.0, 20.0), LogicalSize::new(20.0, 20.0)),
            PinScale::ORIGINAL,
            Some(left.id.clone()),
        );
        let mut pin = PinnedSurface::restore(
            PinId::from("edge-snap"),
            state,
            PinChromePolicy::Allowed,
            vec![LockEscape::TrayMenu],
            &set,
        )
        .unwrap()
        .unwrap();

        assert!(pin.nudge(Direction::Right, NudgeStep::Edge, &set));
        assert_eq!(pin.state.frame.origin.x, 80.0);
        assert_eq!(pin.state.display.as_ref(), Some(&left.id));
    }

    #[test]
    fn window_policy_strips_and_refuses_synthetic_chrome() {
        let display = display("main", 0.0, 800.0, 24.0, 840.0, 2.0, true);
        let mut pin = PinnedSurface::on_display(
            PinId::from("window"),
            LogicalSize::new(400.0, 300.0),
            &display,
            PinChromePolicy::Forbidden,
            vec![LockEscape::TrayMenu],
        )
        .unwrap();

        assert_eq!(pin.state.chrome, PinChrome::NONE);
        pin.set_chrome(PinChrome::default());
        assert_eq!(pin.state.chrome, PinChrome::NONE);
        assert!(!pin.allows_synthetic_chrome());
    }

    #[test]
    fn click_through_lock_requires_an_external_escape() {
        let display = display("main", 0.0, 800.0, 24.0, 840.0, 2.0, true);
        let mut trapped = PinnedSurface::on_display(
            PinId::from("trapped"),
            LogicalSize::new(400.0, 300.0),
            &display,
            PinChromePolicy::Allowed,
            vec![],
        )
        .unwrap();
        assert_eq!(trapped.set_locked(true), Err(LockEscapeRequired));
        assert!(!trapped.state.locked);

        let mut recoverable = PinnedSurface::on_display(
            PinId::from("recoverable"),
            LogicalSize::new(400.0, 300.0),
            &display,
            PinChromePolicy::Allowed,
            vec![LockEscape::TrayMenu],
        )
        .unwrap();
        assert!(recoverable.set_locked(true).is_ok());
        assert!(recoverable.state.locked);
    }

    #[test]
    fn a_new_pin_is_clamped_even_when_the_work_area_is_smaller_than_its_default() {
        let display = display("tiny", 0.0, 120.0, 20.0, 80.0, 2.0, true);
        let pin = PinnedSurface::on_display(
            PinId::from("large"),
            LogicalSize::new(1_600.0, 900.0),
            &display,
            PinChromePolicy::Allowed,
            vec![LockEscape::TrayMenu],
        )
        .unwrap();

        let frame = pin.state().frame;
        assert!(frame.origin.x + frame.size.width >= 48.0);
        assert!(frame.origin.y + frame.size.height >= display.work_area.origin.y + 32.0);
        assert_eq!(frame.origin.x * display.scale.get() % 1.0, 0.0);
    }

    #[test]
    fn viewport_backing_allocations_are_bounded_from_one_to_four_x() {
        for display_scale in [1.0, 1.25, 2.0, 4.0] {
            let display = display("bounded", 0.0, 8_000.0, 0.0, 8_000.0, display_scale, true);
            let set = DisplaySet::new(vec![display.clone()]);
            let mut pin = PinnedSurface::on_display(
                PinId::from("huge"),
                LogicalSize::new(24_000.0, 16_000.0),
                &display,
                PinChromePolicy::Allowed,
                vec![LockEscape::TrayMenu],
            )
            .unwrap();

            pin.set_scale(MAX_PIN_SCALE, &set);
            let width = pin.state.frame.size.width * display_scale;
            let height = pin.state.frame.size.height * display_scale;
            assert!(width <= f64::from(MAX_PIN_PHYSICAL_EDGE) + 1.0e-6);
            assert!(height <= f64::from(MAX_PIN_PHYSICAL_EDGE) + 1.0e-6);
            assert!(width * height <= MAX_PIN_PHYSICAL_PIXELS as f64 + 1.0e-6);
        }
    }

    #[test]
    fn moving_to_a_higher_density_display_shrinks_before_the_next_allocation() {
        let one_x = display("one", 0.0, 5_000.0, 0.0, 5_000.0, 1.0, true);
        let four_x = display("four", 5_000.0, 2_000.0, 0.0, 2_000.0, 4.0, false);
        let set = DisplaySet::new(vec![one_x.clone(), four_x.clone()]);
        let mut pin = PinnedSurface::on_display(
            PinId::from("mixed"),
            LogicalSize::new(2_000.0, 1_000.0),
            &one_x,
            PinChromePolicy::Allowed,
            vec![LockEscape::TrayMenu],
        )
        .unwrap();
        pin.set_scale(MAX_PIN_SCALE, &set);

        let moved = LogicalRect::new(LogicalPoint::new(5_100.0, 100.0), pin.state.frame.size);
        assert!(pin.sync_native_frame(moved, &set));
        assert_eq!(pin.state.display.as_ref(), Some(&four_x.id));
        assert!(
            pin.state.frame.size.width * four_x.scale.get()
                <= f64::from(MAX_PIN_PHYSICAL_EDGE) + 1.0e-6
        );
    }

    #[test]
    fn invalid_source_geometry_is_rejected_before_window_creation() {
        let display = display("main", 0.0, 800.0, 0.0, 800.0, 2.0, true);
        assert_eq!(
            PinnedSurface::on_display(
                PinId::from("empty"),
                LogicalSize::new(0.0, 100.0),
                &display,
                PinChromePolicy::Allowed,
                vec![],
            ),
            Err(InvalidPinSize)
        );
    }
}
