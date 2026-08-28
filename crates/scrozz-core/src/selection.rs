//! The contract every on-screen selector implements.
//!
//! # Why this is a trait and not a function
//!
//! Choosing a region is the one part of a screenshot tool that cannot be written
//! once. Three genuinely different things happen depending on where Scrozz is
//! running, and decision D31 fixes which is which:
//!
//! - **A client overlay.** The application puts a full-screen, keyboard-owning
//!   window over the desktop and draws the selection itself. macOS and Windows
//!   work this way, and so does X11.
//! - **A compositor-owned selector.** The application asks the desktop to do the
//!   choosing and receives the result. GNOME's Mutter deliberately does not
//!   implement `wlr-layer-shell`, so a Wayland client there cannot position a
//!   window over the desktop at all; `org.freedesktop.portal.Screenshot` with
//!   `interactive: true` is the *only* correct route, and GNOME Shell draws the
//!   selector.
//! - **A layer-shell surface.** KWin and wlroots do implement layer-shell, so a
//!   client overlay is possible there — through a protocol neither winit nor
//!   `eframe` speaks today. That adapter is future work, and it plugs in here.
//!
//! One trait, three implementations, and the caller never branches on the
//! platform. What the caller *does* branch on is [`SelectionCapabilities`],
//! which is a query and never an assumption (D8): a compositor-owned selector
//! has no magnifier and cannot honour an exact pixel size, and saying so up
//! front is how that becomes an explained limitation instead of a control that
//! silently does nothing.
//!
//! # Cancelling
//!
//! Escape always cancels (D27). A cancelled selection is
//! [`Error::Cancelled`], which exists precisely so it can travel back through
//! `?` without any layer mistaking a deliberate dismissal for a fault.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{
    CaptureTarget, DisplayId, Error, LogicalPoint, LogicalRect, LogicalSize, Result, ScaleFactor,
};

/// The static minimum applied before display scale is known.
///
/// A moved drag is normalized to at least one physical pixel by the client
/// selector. Keeping the schema minimum at zero lets a 2× or 3× display express
/// that one-pixel strip without imposing an incorrect app-wide scale.
pub const MIN_SELECTION: f64 = 0.0;

/// The default magnifier zoom, in screen pixels per magnified pixel.
pub const DEFAULT_MAGNIFIER_ZOOM: u32 = 5;

/// Which dimensions the selector reports while a region is being dragged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DimensionLabelMode {
    /// Logical desktop dimensions, independent of Retina/HiDPI scale.
    #[default]
    Logical,
    /// Final physical pixels when the region belongs to one display.
    OutputPixels,
    /// Logical dimensions and physical output pixels together.
    Both,
}

impl DimensionLabelMode {
    /// The stable value used by settings and structured output.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Logical => "logical",
            Self::OutputPixels => "output-pixels",
            Self::Both => "both",
        }
    }
}

/// When the advanced crosshair guides and pixel loupe are active.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CrosshairMode {
    /// Use the native crosshair pointer without additional guides or a loupe.
    #[default]
    Off,
    /// Show the additional aids while the platform-primary modifier is held.
    Modifier,
    /// Always show the additional aids during region selection.
    Always,
}

impl CrosshairMode {
    /// The stable value used by settings and structured CLI output.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Modifier => "modifier",
            Self::Always => "always",
        }
    }

    /// Whether the mode is active for the current modifier state.
    #[must_use]
    pub const fn is_active(self, primary_modifier: bool) -> bool {
        match self {
            Self::Off => false,
            Self::Modifier => primary_modifier,
            Self::Always => true,
        }
    }
}

/// What the user is choosing.
///
/// This is also the menu of the single heads-up display: one surface exposes
/// every capture mode, so the user picks the mode after the overlay is up
/// rather than having to have picked it from a menu beforehand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SelectionMode {
    /// A rectangle dragged out by hand.
    Region,
    /// A window, picked by pointing at it.
    Window,
    /// One whole display.
    Display,
    /// Every display at once.
    AllDisplays,
}

impl SelectionMode {
    /// Every mode, in the order the heads-up display shows them.
    pub const ALL: [Self; 4] = [Self::Region, Self::Window, Self::Display, Self::AllDisplays];

    /// The stable identifier used by the CLI, settings and hotkeys.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Region => "region",
            Self::Window => "window",
            Self::Display => "display",
            Self::AllDisplays => "all-displays",
        }
    }

    /// The human label shown in the heads-up display.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Region => "Region",
            Self::Window => "Window",
            Self::Display => "Display",
            Self::AllDisplays => "All displays",
        }
    }

    /// One line describing what picking this mode will do.
    ///
    /// Read aloud by assistive technology, so it says what happens rather than
    /// naming the control (D13).
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Region => "Drag to choose a rectangle",
            Self::Window => "Point at a window to choose it",
            Self::Display => "Capture the display under the pointer",
            Self::AllDisplays => "Capture every display side by side",
        }
    }

    /// Parses a [`slug`](Self::slug).
    #[must_use]
    pub fn from_slug(slug: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.slug() == slug)
    }

    /// Whether this mode produces a rectangle the user dragged out.
    ///
    /// Only [`Region`](Self::Region) does. The others resolve to a target whose
    /// bounds the system already knows, which is why their outcome carries a
    /// rectangle for information but never asks the user to draw one.
    #[must_use]
    pub const fn is_freehand(self) -> bool {
        matches!(self, Self::Region)
    }
}

/// A constraint on the shape of a dragged selection.
///
/// Free by default. Locking is a deliberate act — it makes some drags
/// impossible to express — so it is never inferred from a modifier held during
/// an unrelated gesture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum AspectLock {
    /// Any shape.
    #[default]
    Free,
    /// Width divided by height is held at this value.
    Ratio {
        /// Numerator, in the same unit as `height`.
        width: f64,
        /// Denominator.
        height: f64,
    },
}

impl AspectLock {
    /// A locked ratio.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] if either term is not finite and positive. A
    /// zero or negative term describes no shape at all, and letting it through
    /// produces a selection that collapses the first time the user drags.
    pub fn ratio(width: f64, height: f64) -> Result<Self> {
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Err(Error::InvalidRequest(format!(
                "an aspect ratio needs two positive numbers, not {width}:{height}"
            )));
        }
        let ratio = width / height;
        if !ratio.is_finite() || ratio <= 0.0 {
            return Err(Error::InvalidRequest(format!(
                "an aspect ratio must resolve to a finite positive value, not {width}:{height}"
            )));
        }
        Ok(Self::Ratio { width, height })
    }

    /// Width divided by height, or `None` when free.
    #[must_use]
    pub fn value(self) -> Option<f64> {
        match self {
            Self::Free => None,
            Self::Ratio { width, height } => Some(width / height),
        }
    }

    /// Whether a ratio is being held.
    #[must_use]
    pub const fn is_locked(self) -> bool {
        matches!(self, Self::Ratio { .. })
    }

    /// Reshapes `rect` to satisfy the lock, keeping `anchor` fixed.
    ///
    /// `anchor` is the corner the user is *not* dragging — the one that must not
    /// move. The dragged corner is pulled to whichever of the two candidate
    /// shapes is closer to what the user asked for, so the selection tracks the
    /// pointer rather than snapping to a corner they were not near.
    ///
    /// Returns `rect` unchanged when free, or when the rectangle has no area to
    /// reshape.
    #[must_use]
    pub fn reshape(self, anchor: LogicalPoint, rect: LogicalRect) -> LogicalRect {
        let Some(target) = self.value() else {
            return rect;
        };
        if rect.size.width <= 0.0 && rect.size.height <= 0.0 {
            return rect;
        }

        // Which way the dragged corner lies from the anchor. Preserving the sign
        // is what lets a drag up-and-left stay up-and-left.
        let sign_x = if rect.origin.x + rect.size.width > anchor.x + f64::EPSILON {
            1.0
        } else if rect.origin.x + f64::EPSILON < anchor.x {
            -1.0
        } else {
            1.0
        };
        let sign_y = if rect.origin.y + rect.size.height > anchor.y + f64::EPSILON {
            1.0
        } else if rect.origin.y + f64::EPSILON < anchor.y {
            -1.0
        } else {
            1.0
        };

        // Take the larger of the two candidate extents so the shape follows the
        // pointer outwards rather than shrinking to the smaller axis.
        let from_width = rect.size.width;
        let from_height = rect.size.height * target;
        let width = from_width.max(from_height);
        let height = width / target;

        let corner = LogicalPoint::new(anchor.x + sign_x * width, anchor.y + sign_y * height);
        LogicalRect::from_corners(anchor, corner)
    }
}

/// How large the selection is allowed, or required, to be.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SizeConstraint {
    /// An exact size the user typed in, in logical points.
    ///
    /// When set the drag positions the rectangle and does not resize it, which
    /// is what "I need exactly 1200×630" means in practice.
    pub exact: Option<LogicalSize>,
    /// The shape lock.
    pub aspect: AspectLock,
    /// The smallest selection that will be accepted.
    pub minimum: LogicalSize,
}

impl Default for SizeConstraint {
    fn default() -> Self {
        Self {
            exact: None,
            aspect: AspectLock::Free,
            minimum: LogicalSize::new(MIN_SELECTION, MIN_SELECTION),
        }
    }
}

impl SizeConstraint {
    /// A constraint that only enforces the minimum.
    #[must_use]
    pub fn free() -> Self {
        Self::default()
    }

    /// The same constraint with an exact size demanded.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] if the size has no area.
    pub fn with_exact(mut self, size: LogicalSize) -> Result<Self> {
        if size.is_empty() {
            return Err(Error::InvalidRequest(format!(
                "an exact size needs positive width and height, not {}x{}",
                size.width, size.height
            )));
        }
        self.minimum = LogicalSize::new(
            self.minimum.width.min(size.width),
            self.minimum.height.min(size.height),
        );
        self.exact = Some(size);
        Ok(self)
    }

    /// The same constraint with a shape lock.
    #[must_use]
    pub const fn with_aspect(mut self, aspect: AspectLock) -> Self {
        self.aspect = aspect;
        self
    }

    /// Whether `rect` satisfies the minimum, exact-size and aspect requirements.
    #[must_use]
    pub fn is_satisfied_by(&self, rect: LogicalRect) -> bool {
        let approximately = |actual: f64, expected: f64| {
            (actual - expected).abs() <= 1e-9 * actual.abs().max(expected.abs()).max(1.0)
        };
        if rect.size.width < self.minimum.width || rect.size.height < self.minimum.height {
            return false;
        }
        if let Some(exact) = self.exact
            && (!approximately(rect.size.width, exact.width)
                || !approximately(rect.size.height, exact.height))
        {
            return false;
        }
        if let Some(ratio) = self.aspect.value()
            && !approximately(rect.size.width / rect.size.height, ratio)
        {
            return false;
        }
        true
    }
}

/// Everything the caller wants from one selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionOptions {
    /// Which mode the overlay opens in.
    pub mode: SelectionMode,
    /// A rectangle to start from, usually the last one the user chose.
    ///
    /// Presented as an adjustable selection rather than committed silently: the
    /// user asked to *retake*, not to re-run blind.
    pub remembered: Option<LogicalRect>,
    /// Display that owned `remembered`, when known.
    ///
    /// Mixed-DPI Windows desktops can have overlapping global logical
    /// rectangles because each monitor's device origin is divided by its own
    /// scale. The rectangle alone is therefore not always enough to recover its
    /// owner.
    #[serde(default)]
    pub remembered_display: Option<DisplayId>,
    /// Commit `remembered` immediately, without showing the overlay at all.
    pub reuse_immediately: bool,
    /// Size and shape limits.
    pub constraint: SizeConstraint,
    /// Freeze pixel-addressed region and display choices behind the overlay.
    ///
    /// A frozen screen is what makes a menu, a tooltip or a drag state possible
    /// to capture, and it is what makes the magnifier honest — a live desktop
    /// under a loupe shows pixels that will not be in the final image. Semantic
    /// window capture and all-display composition stay live because their native
    /// output cannot be reconstructed faithfully from display snapshots.
    pub freeze: bool,
    /// When the advanced region-selection aids are active.
    #[serde(default)]
    pub crosshair_mode: CrosshairMode,
    /// Draw full-width crosshair guides through the pointer.
    pub crosshair: bool,
    /// Show the pixel loupe.
    pub magnifier: bool,
    /// Screen pixels per magnified pixel.
    pub magnifier_zoom: u32,
    /// Dimensions shown beside a dragged region.
    #[serde(default)]
    pub dimension_label: DimensionLabelMode,
    /// Wait this long before the overlay appears.
    pub delay: Option<Duration>,
    /// Show the mode heads-up display.
    pub hud: bool,
}

impl Default for SelectionOptions {
    fn default() -> Self {
        Self {
            mode: SelectionMode::Region,
            remembered: None,
            remembered_display: None,
            reuse_immediately: false,
            constraint: SizeConstraint::default(),
            freeze: false,
            crosshair_mode: CrosshairMode::Off,
            crosshair: false,
            magnifier: false,
            magnifier_zoom: DEFAULT_MAGNIFIER_ZOOM,
            dimension_label: DimensionLabelMode::Logical,
            delay: None,
            hud: true,
        }
    }
}

impl SelectionOptions {
    /// Options for a plain region drag.
    #[must_use]
    pub fn region() -> Self {
        Self::for_mode(SelectionMode::Region)
    }

    /// Enables the complete advanced crosshair experience with one activation policy.
    #[must_use]
    pub const fn with_crosshair_mode(mut self, mode: CrosshairMode) -> Self {
        let enabled = !matches!(mode, CrosshairMode::Off);
        self.crosshair_mode = mode;
        self.crosshair = enabled;
        self.magnifier = enabled;
        self
    }

    /// Whether full-width pointer guides should be drawn right now.
    #[must_use]
    pub const fn draws_crosshair(&self, primary_modifier: bool) -> bool {
        self.crosshair && self.crosshair_mode.is_active(primary_modifier)
    }

    /// Whether the pixel loupe should be drawn right now.
    #[must_use]
    pub const fn shows_magnifier(&self, primary_modifier: bool) -> bool {
        self.magnifier && self.crosshair_mode.is_active(primary_modifier)
    }

    /// Whether selection preparation needs pixels for a potentially visible loupe.
    #[must_use]
    pub const fn needs_magnifier_frame(&self) -> bool {
        self.magnifier && !matches!(self.crosshair_mode, CrosshairMode::Off)
    }

    /// Options for a given mode, otherwise default.
    #[must_use]
    pub fn for_mode(mode: SelectionMode) -> Self {
        Self {
            mode,
            freeze: false,
            hud: false,
            ..Self::default()
        }
    }

    /// Whether a region drag is a one-gesture capture rather than an adjustable
    /// All-in-One selection.
    #[must_use]
    pub fn commits_region_on_release(&self) -> bool {
        self.mode == SelectionMode::Region && !self.hud
    }
}

/// Which of the three routes produced a selection.
///
/// Recorded on the outcome because the routes differ in what they can promise,
/// and a bug report that says "the magnifier was off" is answerable in one step
/// when the answer is "the compositor drew the selector".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SelectionSource {
    /// Scrozz drew a full-screen overlay and the user dragged in it.
    ClientOverlay,
    /// The desktop drew the selector and handed back a result.
    CompositorOwned,
    /// A `wlr-layer-shell` surface drew the overlay.
    LayerShell,
    /// No overlay appeared; a remembered rectangle was reused.
    Remembered,
    /// The rectangle came from arguments, not from a person.
    Scripted,
}

impl SelectionSource {
    /// A short phrase for logs and `--json`.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::ClientOverlay => "client-overlay",
            Self::CompositorOwned => "compositor-owned",
            Self::LayerShell => "layer-shell",
            Self::Remembered => "remembered",
            Self::Scripted => "scripted",
        }
    }

    /// Whether the user could see Scrozz's own overlay.
    #[must_use]
    pub const fn is_scrozz_drawn(self) -> bool {
        matches!(self, Self::ClientOverlay | Self::LayerShell)
    }
}

/// What the user chose.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectionOutcome {
    /// The mode in force when the selection was committed, which may differ
    /// from the mode the overlay opened in if the user switched.
    pub mode: SelectionMode,
    /// The capture target to hand to a backend.
    pub target: CaptureTarget,
    /// The chosen rectangle in logical desktop coordinates.
    ///
    /// Present for every mode, including the ones the user did not drag: a
    /// window pick still has bounds, and the caller frequently wants them for
    /// the remembered-selection slot.
    pub rect: Option<LogicalRect>,
    /// The display the selection sits on, when it sits on exactly one.
    pub display: Option<DisplayId>,
    /// The scale factor of that display.
    ///
    /// Carried separately because a selection is chosen in logical points and
    /// captured in physical pixels, and on a mixed-DPI desktop the conversion
    /// is only correct with the *owning* display's scale — not the primary
    /// display's and not the largest.
    pub scale: ScaleFactor,
    /// Which route produced this.
    pub source: SelectionSource,
}

impl SelectionOutcome {
    /// A freehand region outcome.
    #[must_use]
    pub fn region(
        rect: LogicalRect,
        display: Option<DisplayId>,
        scale: ScaleFactor,
        source: SelectionSource,
    ) -> Self {
        Self {
            mode: SelectionMode::Region,
            target: CaptureTarget::Region(rect),
            rect: Some(rect),
            display,
            scale,
            source,
        }
    }

    /// The selection in physical pixels, rounded outward.
    ///
    /// Outward is the only safe direction: rounding in loses the edge pixel the
    /// user deliberately included, and there is no way to get it back.
    #[must_use]
    pub fn physical_rect(&self) -> Option<crate::PhysicalRect> {
        self.rect.map(|rect| rect.to_physical(self.scale))
    }
}

/// What a selector can actually do.
///
/// D8 in one struct: a caller asks before it offers a feature, and a `false`
/// here becomes a truthful explanation rather than a control that does nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionCapabilities {
    /// A rectangle can be dragged out interactively at all.
    pub interactive_region: bool,
    /// Crosshair guides can be drawn.
    pub crosshair: bool,
    /// A pixel loupe can be shown.
    pub magnifier: bool,
    /// The screen can be frozen behind the selector.
    pub frozen_screen: bool,
    /// An exact pixel size can be demanded.
    pub exact_size: bool,
    /// An aspect ratio can be held.
    pub aspect_lock: bool,
    /// A previous rectangle can be restored for adjustment.
    pub remembered_region: bool,
    /// The selection can be nudged and resized from the keyboard.
    pub keyboard_adjustment: bool,
    /// The mode heads-up display can be shown.
    pub hud: bool,
    /// A window can be picked by pointing at it.
    pub window_picking: bool,
}

impl SelectionCapabilities {
    /// Nothing is possible.
    pub const NONE: Self = Self {
        interactive_region: false,
        crosshair: false,
        magnifier: false,
        frozen_screen: false,
        exact_size: false,
        aspect_lock: false,
        remembered_region: false,
        keyboard_adjustment: false,
        hud: false,
        window_picking: false,
    };

    /// Everything is possible, which is what drawing our own overlay buys.
    pub const CLIENT_OVERLAY: Self = Self {
        interactive_region: true,
        crosshair: true,
        magnifier: true,
        frozen_screen: true,
        exact_size: true,
        aspect_lock: true,
        remembered_region: true,
        keyboard_adjustment: true,
        hud: true,
        window_picking: true,
    };

    /// What survives when the desktop owns the selector.
    ///
    /// The compositor draws its own rubber band with its own affordances. Scrozz
    /// gets a rectangle back and nothing else: no loupe, no ratio lock, no
    /// restored previous selection, and no heads-up display, because there is no
    /// surface of ours on screen to put one on.
    pub const COMPOSITOR_OWNED: Self = Self {
        interactive_region: true,
        crosshair: false,
        magnifier: false,
        frozen_screen: false,
        exact_size: false,
        aspect_lock: false,
        remembered_region: false,
        keyboard_adjustment: false,
        hud: false,
        window_picking: true,
    };

    /// Trims `options` to what is actually available.
    ///
    /// Silently, and on purpose: the alternative is refusing a capture because a
    /// decoration is unavailable. The caller that wants to *tell* the user
    /// compares the returned options with what it passed in, which
    /// [`downgrades`](Self::downgrades) does for it.
    #[must_use]
    pub fn honour(&self, options: &SelectionOptions) -> SelectionOptions {
        let mut out = options.clone();
        if !self.crosshair {
            out.crosshair = false;
        }
        if !self.magnifier {
            out.magnifier = false;
        }
        if !out.crosshair && !out.magnifier {
            out.crosshair_mode = CrosshairMode::Off;
        }
        if !self.frozen_screen {
            out.freeze = false;
        }
        if !self.exact_size {
            out.constraint.exact = None;
        }
        if !self.aspect_lock {
            out.constraint.aspect = AspectLock::Free;
        }
        if !self.remembered_region {
            out.remembered = None;
            out.remembered_display = None;
            out.reuse_immediately = false;
        }
        if !self.hud {
            out.hud = false;
        }
        out
    }

    /// The names of the requested features this selector cannot provide.
    ///
    /// Ordered and stable so it can be asserted on and shown to a user.
    #[must_use]
    pub fn downgrades(&self, options: &SelectionOptions) -> Vec<&'static str> {
        let mut lost = Vec::new();
        if options.crosshair
            && !matches!(options.crosshair_mode, CrosshairMode::Off)
            && !self.crosshair
        {
            lost.push("crosshair");
        }
        if options.needs_magnifier_frame() && !self.magnifier {
            lost.push("magnifier");
        }
        if options.freeze && !self.frozen_screen {
            lost.push("frozen screen");
        }
        if options.constraint.exact.is_some() && !self.exact_size {
            lost.push("exact size");
        }
        if options.constraint.aspect.is_locked() && !self.aspect_lock {
            lost.push("aspect lock");
        }
        if options.remembered.is_some() && !self.remembered_region {
            lost.push("remembered selection");
        }
        if options.hud && !self.hud {
            lost.push("capture-mode display");
        }
        lost
    }

    /// Whether `mode` can be offered at all.
    #[must_use]
    pub const fn supports(&self, mode: SelectionMode) -> bool {
        match mode {
            SelectionMode::Region => self.interactive_region,
            SelectionMode::Window => self.window_picking,
            // Whole-display and all-display captures need no selector at all;
            // any implementation can resolve them from enumeration.
            SelectionMode::Display | SelectionMode::AllDisplays => true,
        }
    }
}

/// An on-screen selector.
///
/// Implementations live outside this crate — nothing here touches the operating
/// system. What lives here is the shape they agree on, so the CLI, the menu and
/// the future layer-shell adapter all call the same three methods.
pub trait RegionSelector: Send + Sync {
    /// A short name for logs and diagnostics, e.g. `"client-overlay"`.
    fn name(&self) -> &'static str;

    /// What this selector can do, asked rather than assumed.
    fn capabilities(&self) -> SelectionCapabilities;

    /// Runs the selection to completion.
    ///
    /// Blocks until the user commits or cancels. Implementations must apply
    /// [`SelectionOptions::delay`] themselves, before anything appears on
    /// screen, so a self-timer hides the overlay too.
    ///
    /// # Errors
    ///
    /// - [`Error::Cancelled`] when the user pressed Escape or dismissed the
    ///   selector. This is not a fault and must not be reported as one.
    /// - [`Error::Unsupported`] when this platform has no route to interactive
    ///   selection, carrying the reason.
    /// - [`Error::PermissionDenied`] when the desktop refused screen access.
    fn select(&self, options: &SelectionOptions) -> Result<SelectionOutcome>;
}

/// The display server and desktop a selector has to work with.
///
/// Deliberately not `cfg!`-derived: the CLI resolves this at runtime from the
/// session it finds itself in, and the same binary can face a Wayland session
/// on one login and X11 on the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionHost {
    /// Scrozz may put its own full-screen window over the desktop.
    ClientOverlay,
    /// A `wlr-layer-shell` surface is available for the overlay.
    LayerShell,
    /// The desktop owns selection and Scrozz asks it to choose.
    CompositorOwned,
    /// There is no display to select on.
    Headless,
}

/// The facts about a session that decide which host applies.
///
/// Every field is something a caller can *measure* — a socket, a portal
/// interface, a protocol in the registry. None of it is inferred from a desktop
/// name, which is how the same code ends up wrong on the next release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionFacts {
    /// A display server was found.
    pub has_display: bool,
    /// The session is Wayland, so a client cannot position its own windows.
    pub is_wayland: bool,
    /// `zwlr_layer_shell_v1` is advertised.
    pub has_layer_shell: bool,
    /// The `Screenshot` portal is reachable and offers interactive mode.
    pub has_interactive_portal: bool,
}

impl SessionFacts {
    /// A session where the application owns its own windows outright.
    ///
    /// macOS, Windows and X11 all look like this.
    pub const NATIVE: Self = Self {
        has_display: true,
        is_wayland: false,
        has_layer_shell: false,
        has_interactive_portal: false,
    };

    /// No display server at all.
    pub const HEADLESS: Self = Self {
        has_display: false,
        is_wayland: false,
        has_layer_shell: false,
        has_interactive_portal: false,
    };
}

/// Decides how selection has to happen in this session.
///
/// The Wayland rule is the whole point, and it is D31: a Wayland client cannot
/// place a surface at an absolute position. With `wlr-layer-shell` it can ask
/// the compositor to do it — KWin and wlroots implement that protocol. Mutter
/// does not, and has said it will not, so on GNOME the *only* correct route is
/// the `Screenshot` portal's interactive mode, where GNOME Shell draws the
/// selector. Reaching for a client overlay there produces a window the
/// compositor places wherever it likes, which is the classic Wayland
/// screenshot bug: an overlay that is not over anything.
#[must_use]
pub const fn host_for(facts: SessionFacts) -> SelectionHost {
    if !facts.has_display {
        return SelectionHost::Headless;
    }
    if !facts.is_wayland {
        return SelectionHost::ClientOverlay;
    }
    if facts.has_layer_shell {
        return SelectionHost::LayerShell;
    }
    if facts.has_interactive_portal {
        return SelectionHost::CompositorOwned;
    }
    SelectionHost::Headless
}

impl SelectionHost {
    /// The capabilities this host can offer before any implementation detail.
    #[must_use]
    pub const fn capabilities(self) -> SelectionCapabilities {
        match self {
            Self::ClientOverlay | Self::LayerShell => SelectionCapabilities::CLIENT_OVERLAY,
            Self::CompositorOwned => SelectionCapabilities::COMPOSITOR_OWNED,
            Self::Headless => SelectionCapabilities::NONE,
        }
    }

    /// Which source an outcome from this host carries.
    #[must_use]
    pub const fn source(self) -> SelectionSource {
        match self {
            Self::ClientOverlay | Self::Headless => SelectionSource::ClientOverlay,
            Self::LayerShell => SelectionSource::LayerShell,
            Self::CompositorOwned => SelectionSource::CompositorOwned,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> LogicalRect {
        LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(w, h))
    }

    #[test]
    fn every_mode_has_a_unique_round_tripping_slug() {
        let mut seen = Vec::new();
        for mode in SelectionMode::ALL {
            assert_eq!(SelectionMode::from_slug(mode.slug()), Some(mode));
            assert!(
                !seen.contains(&mode.slug()),
                "duplicate slug {}",
                mode.slug()
            );
            seen.push(mode.slug());
        }
        assert_eq!(SelectionMode::from_slug("nonsense"), None);
    }

    #[test]
    fn only_region_is_freehand() {
        assert!(SelectionMode::Region.is_freehand());
        for mode in [
            SelectionMode::Window,
            SelectionMode::Display,
            SelectionMode::AllDisplays,
        ] {
            assert!(!mode.is_freehand(), "{mode:?}");
        }
    }

    #[test]
    fn an_aspect_ratio_refuses_nonsense() {
        assert!(AspectLock::ratio(16.0, 9.0).is_ok());
        for (w, h) in [(0.0, 9.0), (16.0, 0.0), (-1.0, 2.0), (f64::NAN, 1.0)] {
            let err = AspectLock::ratio(w, h).unwrap_err();
            assert!(matches!(err, Error::InvalidRequest(_)), "{w}:{h} -> {err}");
        }
    }

    #[test]
    fn a_free_lock_leaves_a_rectangle_alone() {
        let r = rect(10.0, 20.0, 300.0, 17.0);
        assert_eq!(
            AspectLock::Free.reshape(LogicalPoint::new(10.0, 20.0), r),
            r
        );
    }

    #[test]
    fn reshaping_holds_the_ratio_and_the_anchor() {
        let lock = AspectLock::ratio(16.0, 9.0).unwrap();
        let anchor = LogicalPoint::new(100.0, 100.0);
        // Dragged down-right to a shape that is far too tall.
        let dragged = LogicalRect::from_corners(anchor, LogicalPoint::new(260.0, 400.0));
        let fixed = lock.reshape(anchor, dragged);

        assert_eq!(fixed.origin.x, 100.0);
        assert_eq!(fixed.origin.y, 100.0);
        let ratio = fixed.size.width / fixed.size.height;
        assert!((ratio - 16.0 / 9.0).abs() < 1e-9, "ratio was {ratio}");
    }

    #[test]
    fn reshaping_keeps_the_direction_the_user_dragged() {
        let lock = AspectLock::ratio(1.0, 1.0).unwrap();
        let anchor = LogicalPoint::new(500.0, 500.0);
        // Up and to the left: the reshaped rectangle must stay above and left
        // of the anchor, not flip across it.
        let dragged = LogicalRect::from_corners(anchor, LogicalPoint::new(300.0, 420.0));
        let fixed = lock.reshape(anchor, dragged);

        assert!(fixed.origin.x < anchor.x, "{fixed:?}");
        assert!(fixed.origin.y < anchor.y, "{fixed:?}");
        assert!(
            (fixed.size.width - fixed.size.height).abs() < 1e-9,
            "{fixed:?}"
        );
        assert!((fixed.origin.x + fixed.size.width - anchor.x).abs() < 1e-9);
        assert!((fixed.origin.y + fixed.size.height - anchor.y).abs() < 1e-9);
    }

    #[test]
    fn reshaping_a_zero_rectangle_is_a_no_op_rather_than_a_division() {
        let lock = AspectLock::ratio(4.0, 3.0).unwrap();
        let anchor = LogicalPoint::new(0.0, 0.0);
        let r = rect(0.0, 0.0, 0.0, 0.0);
        assert_eq!(lock.reshape(anchor, r), r);
    }

    #[test]
    fn an_exact_size_needs_area() {
        assert!(
            SizeConstraint::free()
                .with_exact(LogicalSize::new(0.0, 100.0))
                .is_err()
        );
        assert!(
            SizeConstraint::free()
                .with_exact(LogicalSize::new(1200.0, 630.0))
                .is_ok()
        );
        let one_pixel = SizeConstraint::free()
            .with_exact(LogicalSize::new(1.0, 1.0))
            .unwrap();
        assert!(one_pixel.is_satisfied_by(rect(0.0, 0.0, 1.0, 1.0)));
    }

    #[test]
    fn the_static_minimum_defers_to_the_displays_one_pixel_size() {
        let c = SizeConstraint::free();
        assert!(c.is_satisfied_by(rect(0.0, 0.0, 0.25, 0.25)));
        assert!(c.is_satisfied_by(rect(0.0, 0.0, MIN_SELECTION, MIN_SELECTION)));
    }

    #[test]
    fn ordinary_region_selection_uses_only_the_native_crosshair_pointer() {
        let options = SelectionOptions::region();

        assert_eq!(options.crosshair_mode, CrosshairMode::Off);
        assert!(!options.crosshair);
        assert!(!options.magnifier);
        assert!(!options.freeze);
        assert!(!options.hud);
        assert_eq!(options.dimension_label, DimensionLabelMode::Logical);
    }

    #[test]
    fn crosshair_mode_supports_off_modifier_and_always_activation() {
        let off = SelectionOptions::region();
        assert!(!off.draws_crosshair(false));
        assert!(!off.shows_magnifier(true));

        let modifier = SelectionOptions::region().with_crosshair_mode(CrosshairMode::Modifier);
        assert!(!modifier.draws_crosshair(false));
        assert!(modifier.draws_crosshair(true));
        assert!(modifier.shows_magnifier(true));
        assert!(modifier.needs_magnifier_frame());

        let always = SelectionOptions::region().with_crosshair_mode(CrosshairMode::Always);
        assert!(always.draws_crosshair(false));
        assert!(always.shows_magnifier(false));
    }

    #[test]
    fn compositor_owned_selection_downgrades_and_says_which_parts() {
        let wanted = SelectionOptions {
            crosshair_mode: CrosshairMode::Always,
            magnifier: true,
            crosshair: true,
            freeze: true,
            hud: true,
            remembered: Some(rect(0.0, 0.0, 10.0, 10.0)),
            constraint: SizeConstraint::free()
                .with_aspect(AspectLock::ratio(1.0, 1.0).unwrap())
                .with_exact(LogicalSize::new(100.0, 100.0))
                .unwrap(),
            ..SelectionOptions::default()
        };

        let caps = SelectionCapabilities::COMPOSITOR_OWNED;
        let lost = caps.downgrades(&wanted);
        assert_eq!(
            lost,
            [
                "crosshair",
                "magnifier",
                "frozen screen",
                "exact size",
                "aspect lock",
                "remembered selection",
                "capture-mode display",
            ]
        );

        let honoured = caps.honour(&wanted);
        assert!(!honoured.magnifier);
        assert!(!honoured.crosshair);
        assert_eq!(honoured.crosshair_mode, CrosshairMode::Off);
        assert!(!honoured.freeze);
        assert!(!honoured.hud);
        assert!(honoured.remembered.is_none());
        assert!(honoured.constraint.exact.is_none());
        assert!(!honoured.constraint.aspect.is_locked());
    }

    #[test]
    fn a_client_overlay_downgrades_nothing() {
        let wanted = SelectionOptions {
            remembered: Some(rect(0.0, 0.0, 10.0, 10.0)),
            constraint: SizeConstraint::free().with_aspect(AspectLock::ratio(3.0, 2.0).unwrap()),
            ..SelectionOptions::default().with_crosshair_mode(CrosshairMode::Modifier)
        };
        let caps = SelectionCapabilities::CLIENT_OVERLAY;
        assert!(caps.downgrades(&wanted).is_empty());
        assert_eq!(caps.honour(&wanted), wanted);
    }

    #[test]
    fn gnome_wayland_must_hand_selection_to_the_compositor() {
        // D31. Mutter implements no layer shell, so the portal is the only
        // route; reaching for a client overlay here is the bug this test exists
        // to prevent from ever being reintroduced.
        let gnome = SessionFacts {
            has_display: true,
            is_wayland: true,
            has_layer_shell: false,
            has_interactive_portal: true,
        };
        assert_eq!(host_for(gnome), SelectionHost::CompositorOwned);
        assert_eq!(
            host_for(gnome).capabilities(),
            SelectionCapabilities::COMPOSITOR_OWNED
        );
    }

    #[test]
    fn kwin_and_wlroots_get_a_layer_shell_overlay() {
        let plasma = SessionFacts {
            has_display: true,
            is_wayland: true,
            has_layer_shell: true,
            has_interactive_portal: true,
        };
        assert_eq!(host_for(plasma), SelectionHost::LayerShell);
        assert_eq!(
            host_for(plasma).capabilities(),
            SelectionCapabilities::CLIENT_OVERLAY
        );
        assert_eq!(host_for(plasma).source(), SelectionSource::LayerShell);
    }

    #[test]
    fn native_sessions_draw_their_own_overlay() {
        assert_eq!(host_for(SessionFacts::NATIVE), SelectionHost::ClientOverlay);
    }

    #[test]
    fn a_wayland_session_with_neither_route_is_honest_about_it() {
        let stuck = SessionFacts {
            has_display: true,
            is_wayland: true,
            has_layer_shell: false,
            has_interactive_portal: false,
        };
        assert_eq!(host_for(stuck), SelectionHost::Headless);
        assert_eq!(host_for(stuck).capabilities(), SelectionCapabilities::NONE);
    }

    #[test]
    fn no_display_means_no_selector() {
        assert_eq!(host_for(SessionFacts::HEADLESS), SelectionHost::Headless);
    }

    #[test]
    fn an_outcome_converts_with_the_owning_displays_scale() {
        // The mixed-DPI trap: a selection on a 2x display must not be converted
        // with the primary display's 1x.
        let outcome = SelectionOutcome::region(
            rect(100.0, 100.0, 50.5, 20.25),
            Some(DisplayId("hidpi".into())),
            ScaleFactor::new(2.0),
            SelectionSource::ClientOverlay,
        );
        let physical = outcome.physical_rect().expect("a region has a rectangle");
        assert_eq!(physical.origin.x, 200.0);
        // Outward rounding: 100 + 50.5 = 150.5 logical -> 301 physical, so the
        // width is 101 rather than 101 truncated to 100.
        assert_eq!(physical.pixel_width(), 101);
        assert_eq!(physical.pixel_height(), 41);
    }

    #[test]
    fn size_constraints_validate_exact_size_and_aspect_at_commit() {
        let constraint = SizeConstraint::free()
            .with_exact(LogicalSize::new(160.0, 90.0))
            .unwrap()
            .with_aspect(AspectLock::ratio(16.0, 9.0).unwrap());
        assert!(constraint.is_satisfied_by(rect(0.0, 0.0, 160.0, 90.0)));
        assert!(!constraint.is_satisfied_by(rect(0.0, 0.0, 159.0, 90.0)));
        assert!(!constraint.is_satisfied_by(rect(0.0, 0.0, 160.0, 91.0)));
    }

    #[test]
    fn aspect_terms_must_not_overflow_or_underflow_the_resolved_ratio() {
        assert!(AspectLock::ratio(f64::MAX, f64::MIN_POSITIVE).is_err());
        assert!(AspectLock::ratio(f64::MIN_POSITIVE, f64::MAX).is_err());
    }

    #[test]
    fn every_source_has_a_distinct_slug() {
        let sources = [
            SelectionSource::ClientOverlay,
            SelectionSource::CompositorOwned,
            SelectionSource::LayerShell,
            SelectionSource::Remembered,
            SelectionSource::Scripted,
        ];
        let mut slugs: Vec<&str> = sources.iter().map(|s| s.slug()).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), sources.len());
        assert!(SelectionSource::ClientOverlay.is_scrozz_drawn());
        assert!(SelectionSource::LayerShell.is_scrozz_drawn());
        assert!(!SelectionSource::CompositorOwned.is_scrozz_drawn());
    }

    #[test]
    fn a_compositor_owned_host_still_supports_the_modes_it_can_reach() {
        let caps = SelectionCapabilities::COMPOSITOR_OWNED;
        assert!(caps.supports(SelectionMode::Region));
        assert!(caps.supports(SelectionMode::Display));
        assert!(caps.supports(SelectionMode::AllDisplays));
        assert!(!SelectionCapabilities::NONE.supports(SelectionMode::Region));
    }
}
