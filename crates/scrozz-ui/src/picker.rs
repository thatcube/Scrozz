//! Choosing a window by pointing at it.
//!
//! This module is the whole of CAP-03's interaction, minus the pixels: which
//! window is under the pointer, what rectangle to light up, what happens when
//! the user commits, and what happens when the window they were aiming at closes
//! first. It is a pure state machine over a snapshot of
//! [`scrozz_core::Window`] values, so every one of those questions is answerable
//! in a unit test with no display server, no compositor and no window manager —
//! which is the only way an agent can verify this flow at all (D25).
//!
//! [`paint`](crate::picker::paint) draws it. Nothing in here reads a clock or
//! touches egui state.
//!
//! # The four things that actually go wrong
//!
//! **Our own overlay is in the window list.** The picker runs inside a
//! fullscreen always-on-top window, so a naïve front-most hit test picks *the
//! picker* everywhere on screen and the user can never select anything. It is
//! not a rare edge case; it is the first thing that happens. [`WindowPicker::
//! excluding`] takes the ids to skip, and hit testing honours it always.
//!
//! **The window closes between hover and click.** The user aims at a dialog, it
//! dismisses itself, the click lands on an id that no longer exists. Capturing
//! it produces [`scrozz_core::Error::TargetGone`] *after* the overlay has already
//! torn down, which reads as a crash. So committing is done against a fresh
//! snapshot and can report [`Outcome::Vanished`], which the caller shows in
//! place rather than as a failure.
//!
//! **A window straddling two displays has no single scale factor.** Half of it
//! is 2× and half is 1×. The highlight is therefore computed in *logical*
//! desktop points, where both halves agree, and the scale is only resolved at
//! capture time from the display the window is predominantly on —
//! [`Highlight::spans_displays`] reports the straddle so the UI can say the
//! capture will be taken at one scale rather than silently producing an image
//! the user did not expect.
//!
//! **Minimised windows are listed but not on screen.** They are legitimately
//! capturable by name, and hovering them is meaningless because there is nothing
//! under the pointer to hover. Hit testing skips them; keyboard cycling skips
//! them too, because focusing something invisible leaves the user with a
//! highlight they cannot see.
//!
//! # Why the highlight radius may be platform-specific
//!
//! Backends carry a per-window radius when native pixels or system preferences
//! make one available. Otherwise the picker uses a platform-specific visual
//! estimate while preserving the OS-reported rectangular bounds exactly. Both
//! forms are selection chrome only: neither masks nor composites captured pixels,
//! whose native alpha remains the source of truth under D9.
//!
//! # Keyboard operation
//!
//! D13 requires complete keyboard-only operation, and a picker driven only by a
//! pointer is the obvious way to fail it. [`WindowPicker::focus_next`] and
//! [`WindowPicker::focus_prev`] cycle the same selection the pointer moves, so
//! there is one focused window and one highlight regardless of which device the
//! user is on.

pub mod paint;

use scrozz_core::{
    Display, DisplayId, LogicalPoint, LogicalRect, ScaleFactor, SourceApp, Window,
    WindowCornerRadius, WindowId,
};
use std::sync::{Arc, Mutex};

/// Native window configuration run before a standalone picker begins painting.
pub type NativePickerHook =
    Box<dyn FnOnce(&eframe::CreationContext<'_>) -> scrozz_core::Result<()>>;

/// The input method currently controlling the picker highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusMethod {
    /// Hovering or clicking with a pointer.
    Pointer,
    /// Cycling with Tab or Shift-Tab.
    Keyboard,
}

/// What the picker is currently pointing at.
#[derive(Debug, Clone, PartialEq)]
pub struct Highlight {
    /// The window this describes.
    pub id: WindowId,
    /// The window's visible picker frame in global logical desktop points.
    ///
    /// Normally this is the native capture frame. Sparse system surfaces may
    /// narrow it to their native-alpha content so transparent canvas does not
    /// intercept unrelated windows.
    pub bounds: LogicalRect,
    /// The native capture frame, which may be larger than [`Self::bounds`].
    pub capture_bounds: LogicalRect,
    /// The scale the capture will be taken at.
    ///
    /// The scale of the display the window is predominantly on, which is the
    /// same rule the capture backends use, so the number shown during selection
    /// is the number that ends up in the file.
    pub scale: ScaleFactor,
    /// Whether the window overlaps more than one display.
    ///
    /// True means [`Self::scale`] is a choice rather than a fact, and the UI
    /// should be able to say so.
    pub spans_displays: bool,
    /// Which application owns it, for the label chip and for the eventual
    /// history badge.
    pub source_app: SourceApp,
    /// Per-window visual radius when the backend could determine one.
    pub corner_radius: Option<WindowCornerRadius>,
    /// Whether pointer or keyboard input owns the current focus.
    pub focus_method: FocusMethod,
}

impl Highlight {
    /// The capture size in real pixels, for the dimension readout.
    #[must_use]
    pub fn pixel_size(&self) -> (u32, u32) {
        let physical = self.capture_bounds.to_physical(self.scale);
        (physical.pixel_width(), physical.pixel_height())
    }

    /// The label to show beside the highlight.
    ///
    /// The application name where there is one, because that is the shortest
    /// true description; the window title otherwise; and a plain fallback rather
    /// than an empty chip when the OS said nothing, since an empty chip reads as
    /// a rendering bug.
    #[must_use]
    pub fn label(&self) -> &str {
        self.source_app.badge().unwrap_or("Untitled window")
    }
}

/// The result of committing a selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The user chose this window and it still exists.
    Selected(WindowId),
    /// The user pressed Escape, or clicked where no window is.
    Cancelled,
    /// The window was chosen but has closed since it was last enumerated.
    ///
    /// Distinct from [`Self::Cancelled`] because the user did make a choice, and
    /// distinct from an error because nothing failed — the correct response is
    /// to re-enumerate and let them pick again, with the picker still up.
    Vanished(WindowId),
}

/// A terminal picker action whose triggering input has not necessarily been released yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalAction {
    /// Cancel selection.
    Cancel,
    /// Commit the window that was focused when the action began.
    Commit(WindowId),
}

/// Whether any input that could outlive a terminal picker action remains held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalInputState {
    keys_down: bool,
    modifiers_down: bool,
    pointer_down: bool,
}

impl TerminalInputState {
    /// Samples held input from the picker viewport that received the action.
    #[must_use]
    pub fn read(ctx: &egui::Context) -> Self {
        ctx.input(|input| Self {
            keys_down: !input.keys_down.is_empty(),
            modifiers_down: input.modifiers.any(),
            pointer_down: input.pointer.any_down(),
        })
    }

    fn merge(&mut self, other: Self) {
        self.keys_down |= other.keys_down;
        self.modifiers_down |= other.modifiers_down;
        self.pointer_down |= other.pointer_down;
    }

    const fn all_released(self) -> bool {
        !self.keys_down && !self.modifiers_down && !self.pointer_down
    }
}

/// Holds the first terminal action until its complete input chord is released.
///
/// A picker that restores focus or closes on the key-down frame sends the
/// matching key-up event to the previously focused application. Keeping this
/// gate alive until every held key, modifier, and pointer button is up prevents
/// that input from leaking across the focus handoff.
#[derive(Debug, Default)]
pub struct TerminalInputGate {
    pending: Option<TerminalAction>,
}

impl TerminalInputGate {
    /// Records the first terminal action and returns it once all input is up.
    ///
    /// The focused id is copied on the triggering frame so pointer movement
    /// during key release cannot silently change what Return will capture.
    #[must_use]
    pub fn update(
        &mut self,
        intent: paint::Intent,
        close_requested: bool,
        focused: Option<&WindowId>,
        input: TerminalInputState,
    ) -> Option<TerminalAction> {
        if self.pending.is_none() {
            self.pending = if close_requested || intent == paint::Intent::Cancel {
                Some(TerminalAction::Cancel)
            } else if intent == paint::Intent::Commit {
                Some(
                    focused
                        .cloned()
                        .map_or(TerminalAction::Cancel, TerminalAction::Commit),
                )
            } else {
                None
            };
        }

        if input.all_released() {
            self.pending.take()
        } else {
            None
        }
    }

    /// Whether a terminal action is waiting for its input to be released.
    #[must_use]
    pub const fn is_waiting(&self) -> bool {
        self.pending.is_some()
    }
}

/// A live window-selection session.
///
/// Constructed from a snapshot of the window list, driven by pointer and
/// keyboard, and consulted for what to draw.
#[derive(Debug, Clone)]
pub struct WindowPicker {
    /// Front-most first, as [`scrozz_core::TargetEnumerator::windows`] promises.
    windows: Vec<Window>,
    displays: Vec<Display>,
    /// Scrozz's own windows, which must never be selectable.
    excluded: Vec<WindowId>,
    focused: Option<WindowId>,
    /// Last reported pointer position, retained as an anchor while keys own focus.
    pointer: Option<LogicalPoint>,
    /// Keyboard focus must survive frames where the overlay reports no pointer.
    keyboard_focus: bool,
}

impl WindowPicker {
    /// Starts a session over a window snapshot and the display layout.
    #[must_use]
    pub fn new(windows: Vec<Window>, displays: Vec<Display>) -> Self {
        Self {
            windows,
            displays,
            excluded: Vec::new(),
            focused: None,
            pointer: None,
            keyboard_focus: false,
        }
    }

    /// Excludes windows belonging to Scrozz itself.
    ///
    /// The overlay covers the screen, so without this the picker selects itself
    /// everywhere and nothing else can ever be chosen.
    #[must_use]
    pub fn excluding(mut self, ids: impl IntoIterator<Item = WindowId>) -> Self {
        self.excluded.extend(ids);
        // Anything already focused that is now excluded must not stay focused.
        if self
            .focused
            .as_ref()
            .is_some_and(|id| self.excluded.contains(id))
        {
            self.focused = None;
            self.pointer = None;
            self.keyboard_focus = false;
        }
        self
    }

    /// The windows a user could actually point at, front-most first.
    ///
    /// Excludes Scrozz's own windows, anything not currently on screen, and
    /// anything with no area — all three of which are present in real window
    /// lists and none of which can be hovered.
    pub fn candidates(&self) -> impl Iterator<Item = &Window> {
        self.windows.iter().filter(move |window| {
            window.is_visible
                && !window.picker_bounds().is_empty()
                && !self.excluded.contains(&window.id)
        })
    }

    /// How many windows are selectable.
    #[must_use]
    pub fn candidate_count(&self) -> usize {
        self.candidates().count()
    }

    /// Whether there is nothing at all to pick.
    ///
    /// A true and useful state, not an error: a desktop with every window
    /// minimised has no candidates, and the picker must say so rather than open
    /// a blank overlay the user has to guess their way out of.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.candidate_count() == 0
    }

    /// Moves the pointer, updating what is focused.
    ///
    /// Returns `true` when the focused window changed, so a caller can repaint
    /// only when something moved rather than every mouse event.
    pub fn point_at(&mut self, position: LogicalPoint) -> bool {
        if self.pointer == Some(position) {
            return false;
        }
        self.pointer = Some(position);
        self.keyboard_focus = false;
        let hit = self.window_at(position).map(|window| window.id.clone());
        let changed = hit != self.focused;
        self.focused = hit;
        changed
    }

    /// Removes pointer-owned focus when the pointer leaves every display.
    ///
    /// A keyboard-owned highlight remains selected. Its last pointer position is
    /// retained so a passive leave/re-enter sequence cannot masquerade as mouse
    /// movement and reset the Tab cycle.
    pub fn clear_pointer(&mut self) -> bool {
        if self.keyboard_focus {
            return false;
        }
        self.pointer = None;
        let changed = self.focused.is_some();
        self.focused = None;
        changed
    }

    /// The front-most selectable window containing `position`.
    ///
    /// Front-most, not largest or nearest: the list is in z-order and the window
    /// the user sees at that point is the first one that covers it. Anything
    /// else picks a window hidden behind the one they are looking at.
    #[must_use]
    pub fn window_at(&self, position: LogicalPoint) -> Option<&Window> {
        self.candidates()
            .find(|window| contains(window.picker_bounds(), position))
    }

    /// The currently focused window's id.
    #[must_use]
    pub fn focused_id(&self) -> Option<&WindowId> {
        self.focused.as_ref()
    }

    /// The focused window, if it is still in the snapshot.
    #[must_use]
    pub fn focused(&self) -> Option<&Window> {
        let id = self.focused.as_ref()?;
        self.windows.iter().find(|window| &window.id == id)
    }

    /// What to draw, if anything is focused.
    #[must_use]
    pub fn highlight(&self) -> Option<Highlight> {
        let focus_method = if self.keyboard_focus {
            FocusMethod::Keyboard
        } else {
            FocusMethod::Pointer
        };
        self.focused()
            .map(|window| self.describe_with_focus(window, focus_method))
    }

    /// A pointer-style highlight for an arbitrary window, used by fixtures and tests.
    #[must_use]
    pub fn describe(&self, window: &Window) -> Highlight {
        self.describe_with_focus(window, FocusMethod::Pointer)
    }

    fn describe_with_focus(&self, window: &Window, focus_method: FocusMethod) -> Highlight {
        let (scale, spans) = self.scale_of(window);
        Highlight {
            id: window.id.clone(),
            bounds: window.picker_bounds(),
            capture_bounds: window.bounds,
            scale,
            spans_displays: spans,
            source_app: SourceApp::from_window(window),
            corner_radius: window.corner_radius,
            focus_method,
        }
    }

    /// Moves focus to the next selectable window, wrapping.
    ///
    /// Returns the newly focused id. D13: this is the whole keyboard path, and
    /// it deliberately shares one focus value with the pointer so the two cannot
    /// disagree about what is highlighted.
    pub fn focus_next(&mut self) -> Option<&WindowId> {
        self.step(1)
    }

    /// Moves focus to the previous selectable window, wrapping.
    pub fn focus_prev(&mut self) -> Option<&WindowId> {
        self.step(-1)
    }

    fn step(&mut self, delta: isize) -> Option<&WindowId> {
        let order: Vec<WindowId> = self.candidates().map(|window| window.id.clone()).collect();
        if order.is_empty() {
            self.focused = None;
            self.keyboard_focus = true;
            return None;
        }

        let next = match self
            .focused
            .as_ref()
            .and_then(|id| order.iter().position(|candidate| candidate == id))
        {
            // Wrapping arithmetic on a signed index, then a modulus that is
            // never negative: `-1 % n` is `-1` in Rust, which would index
            // nothing on the first back-tab.
            Some(current) => {
                let len = order.len() as isize;
                (((current as isize + delta) % len) + len) % len
            }
            // Nothing focused yet: forward starts at the front-most window,
            // backward at the rearmost, which is what each key implies.
            None if delta >= 0 => 0,
            None => (order.len() - 1) as isize,
        };

        self.focused = order.get(next as usize).cloned();
        self.keyboard_focus = true;
        self.focused.as_ref()
    }

    /// Replaces the window snapshot, keeping focus where it still makes sense.
    ///
    /// Pointer-owned focus follows the pointer because a stale id would let the
    /// user click on a highlight that moved. Keyboard-owned focus is kept if the
    /// window survived and dropped if it did not, rather than being reset by a
    /// stationary pointer during the live commit check.
    pub fn refresh(&mut self, windows: Vec<Window>) {
        self.windows = windows;

        if !self.keyboard_focus
            && let Some(pointer) = self.pointer
        {
            // Force a fresh hit-test even though the physical pointer did not
            // move: the windows underneath it may have.
            self.pointer = None;
            self.point_at(pointer);
            return;
        }

        if self.focused().is_none() {
            self.focused = None;
        }
    }

    /// Commits the current focus.
    ///
    /// `live` is a freshly taken window list. Committing against the snapshot
    /// the picker was built from would happily return an id for a window that
    /// closed while the user was deciding, and the capture would then fail after
    /// the overlay had gone — so the check happens here, while there is still a
    /// picker on screen to report it in.
    #[must_use]
    pub fn commit(&self, live: &[Window]) -> Outcome {
        let Some(id) = self.focused.clone() else {
            return Outcome::Cancelled;
        };

        if live.iter().any(|window| window.id == id) {
            Outcome::Selected(id)
        } else {
            Outcome::Vanished(id)
        }
    }

    /// Commits against the picker's own snapshot.
    ///
    /// For callers that have just refreshed, and for tests. Prefer
    /// [`Self::commit`] anywhere a re-enumeration is cheap, which is everywhere
    /// the user has just clicked.
    #[must_use]
    pub fn commit_unchecked(&self) -> Outcome {
        self.focused
            .clone()
            .map_or(Outcome::Cancelled, Outcome::Selected)
    }

    /// The display layout this picker was built with.
    #[must_use]
    pub fn displays(&self) -> &[Display] {
        &self.displays
    }

    /// The smallest global logical rectangle containing every connected display.
    ///
    /// This is the single picker window's geometry on platforms with one global
    /// logical coordinate transform. Windows uses one native viewport per display
    /// instead because mixed DPI makes that transform discontinuous.
    #[must_use]
    pub fn desktop_bounds(&self) -> LogicalRect {
        desktop_bounds(&self.displays)
    }

    /// The scale a window will be captured at, and whether it straddles.
    ///
    /// The predominant display is chosen by overlap *area*, not by the centre
    /// point: a window can have its centre in a gap between two monitors, or on
    /// the smaller of the two halves it is split across, and area is the measure
    /// that matches what the user sees.
    fn scale_of(&self, window: &Window) -> (ScaleFactor, bool) {
        let mut touched = 0usize;
        let mut best: Option<(f64, ScaleFactor)> = None;

        for display in &self.displays {
            let area = overlap_area(display.bounds, window.picker_bounds());
            if area <= 0.0 {
                continue;
            }
            touched += 1;
            if best.is_none_or(|(most, _)| area > most) {
                best = Some((area, display.scale));
            }
        }

        // Falling back to the window's declared home display, then to the
        // primary, then to 1×: a window dragged fully off-screen overlaps
        // nothing, and reporting a scale of zero would divide the capture size
        // by nothing at all.
        let scale = best.map(|(_, scale)| scale).unwrap_or_else(|| {
            self.display(&window.display)
                .or_else(|| self.displays.iter().find(|display| display.is_primary))
                .map_or(ScaleFactor::IDENTITY, |display| display.scale)
        });

        (scale, touched > 1)
    }

    fn display(&self, id: &DisplayId) -> Option<&Display> {
        self.displays.iter().find(|display| &display.id == id)
    }
}

/// Whether a rectangle contains a point, half-open on the far edges.
///
/// Half-open matters: two windows tiled edge to edge share a boundary column,
/// and a closed test hits both, so which one is picked depends on list order at
/// exactly the pixel a user is most likely to be on.
fn contains(rect: LogicalRect, point: LogicalPoint) -> bool {
    point.x >= rect.origin.x
        && point.y >= rect.origin.y
        && point.x < rect.origin.x + rect.size.width
        && point.y < rect.origin.y + rect.size.height
}

fn overlap_area(a: LogicalRect, b: LogicalRect) -> f64 {
    let left = a.origin.x.max(b.origin.x);
    let top = a.origin.y.max(b.origin.y);
    let right = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let bottom = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    ((right - left).max(0.0)) * ((bottom - top).max(0.0))
}

/// The smallest global logical rectangle containing `displays`.
///
/// Falls back to a useful desktop-sized rectangle when enumeration returned no
/// usable bounds. The picker can then render its "no visible windows" state
/// rather than asking a windowing library to create a zero-sized window.
#[must_use]
pub fn desktop_bounds(displays: &[Display]) -> LogicalRect {
    let mut usable = displays
        .iter()
        .map(|display| display.bounds)
        .filter(|bounds| !bounds.is_empty());
    let Some(first) = usable.next() else {
        return LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            scrozz_core::LogicalSize::new(1440.0, 900.0),
        );
    };

    let mut left = first.origin.x;
    let mut top = first.origin.y;
    let mut right = first.origin.x + first.size.width;
    let mut bottom = first.origin.y + first.size.height;
    for bounds in usable {
        left = left.min(bounds.origin.x);
        top = top.min(bounds.origin.y);
        right = right.max(bounds.origin.x + bounds.size.width);
        bottom = bottom.max(bounds.origin.y + bounds.size.height);
    }

    LogicalRect::new(
        LogicalPoint::new(left, top),
        scrozz_core::LogicalSize::new(right - left, bottom - top),
    )
}

/// A native viewport covering the full logical desktop for window selection.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn viewport(bounds: LogicalRect) -> egui::ViewportBuilder {
    base_viewport()
        .with_position(egui::pos2(bounds.origin.x as f32, bounds.origin.y as f32))
        .with_inner_size(egui::vec2(
            bounds.size.width as f32,
            bounds.size.height as f32,
        ))
}

fn base_viewport() -> egui::ViewportBuilder {
    let builder = egui::ViewportBuilder::default()
        .with_title("Choose a window")
        .with_app_id("com.scrozz.window-picker")
        .with_decorations(false)
        .with_resizable(false)
        .with_transparent(true)
        .with_has_shadow(false)
        .with_taskbar(false)
        .with_window_level(egui::WindowLevel::AlwaysOnTop)
        .with_active(true);

    #[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
    let builder = builder
        .with_window_type(egui::X11WindowType::Dock)
        .with_override_redirect(true);

    builder
}

/// One monitor-sized native picker viewport.
///
/// Windows needs one viewport per monitor because its mixed-DPI desktop has no
/// single logical coordinate transform. `with_monitor` lets winit place each
/// borderless surface directly in the monitor's real pixel space.
#[must_use]
pub fn display_viewport(index: usize) -> egui::ViewportBuilder {
    base_viewport().with_monitor(index)
}

/// Stable identity for one monitor's picker viewport.
#[must_use]
pub fn display_viewport_id(display: &Display) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(("scrozz.window-picker", &display.id.0))
}

/// Stable identity for the single-desktop picker viewport.
#[must_use]
pub fn viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("scrozz.window-picker")
}

/// Shows one picker surface per display and combines their input.
///
/// This is used on Windows, where each monitor can have a different native
/// scale and therefore cannot share one correctly mapped logical viewport.
pub fn interact_display_viewports(
    root: &egui::Context,
    picker: &mut WindowPicker,
    theme: &crate::Theme,
    notice: Option<&str>,
) -> (paint::Intent, bool, TerminalInputState) {
    let displays = picker.displays.clone();
    let mut combined = paint::Intent::None;
    let mut close_requested = false;
    let mut terminal_input = TerminalInputState::default();

    for (index, display) in displays.iter().enumerate() {
        let mut intent = paint::Intent::None;
        root.show_viewport_immediate(
            display_viewport_id(display),
            display_viewport(index),
            |ctx, _class| {
                close_requested |= ctx.input(|input| input.viewport().close_requested());
                terminal_input.merge(TerminalInputState::read(ctx));
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE.fill(egui::Color32::TRANSPARENT))
                    .show(ctx, |ui| {
                        intent = paint::interact_display(
                            ui,
                            picker,
                            display.bounds.origin,
                            theme,
                            notice,
                        );
                    });
            },
        );

        combined = match (combined, intent) {
            (paint::Intent::Cancel, _) | (_, paint::Intent::Cancel) => paint::Intent::Cancel,
            (paint::Intent::Commit, _) | (_, paint::Intent::Commit) => paint::Intent::Commit,
            _ => paint::Intent::None,
        };
    }

    (combined, close_requested, terminal_input)
}

/// Closes every picker viewport that may be alive.
pub fn close_viewports(ctx: &egui::Context, displays: &[Display]) {
    ctx.send_viewport_cmd_to(viewport_id(), egui::ViewportCommand::Visible(false));
    ctx.send_viewport_cmd_to(viewport_id(), egui::ViewportCommand::Close);
    for display in displays {
        ctx.send_viewport_cmd_to(
            display_viewport_id(display),
            egui::ViewportCommand::Visible(false),
        );
        ctx.send_viewport_cmd_to(display_viewport_id(display), egui::ViewportCommand::Close);
    }
}

/// Whether a window belongs to this Scrozz process.
///
/// Application identity is the only reliable discriminator. Window titles are
/// deliberately ignored: a browser page titled "Scrozz" is still a perfectly
/// valid capture target.
#[must_use]
pub fn is_scrozz_window(window: &Window) -> bool {
    let application_is_scrozz = window
        .application
        .as_deref()
        .is_some_and(|name| name.trim().eq_ignore_ascii_case("scrozz"));
    let identifier_is_scrozz = window.application_id.as_deref().is_some_and(|identifier| {
        let identifier = identifier.trim().to_ascii_lowercase();
        matches!(
            identifier.as_str(),
            "scrozz"
                | "scrozz.exe"
                | "com.thatcube.scrozz"
                | "com.scrozz.overlay"
                | "com.scrozz.window-picker"
        )
    });

    application_is_scrozz || identifier_is_scrozz
}

/// Runs a standalone in-process window picker and returns the chosen id.
///
/// The menu-bar app embeds the same state machine in its existing eframe loop;
/// this entry point is for `scrozz capture --interactive window` when no GUI
/// instance is handling the command.
///
/// # Errors
///
/// Returns a platform error when the native selection window cannot open, or
/// propagates a fresh-enumeration error encountered while committing.
pub fn pick_window(
    windows: Vec<Window>,
    displays: Vec<Display>,
    refresh: Arc<dyn Fn() -> scrozz_core::Result<Vec<Window>> + Send + Sync>,
) -> scrozz_core::Result<Outcome> {
    pick_window_with_native_hook(windows, displays, refresh, None)
}

/// Runs a standalone picker after applying required native window behavior.
///
/// The application uses this form on macOS to raise the picker above the Dock
/// and menu bar before accepting input. Other callers can use [`pick_window`]
/// when eframe's native viewport behavior is sufficient.
///
/// # Errors
///
/// Returns an error from `native_hook`, the native picker, or live enumeration.
pub fn pick_window_with_native_hook(
    mut windows: Vec<Window>,
    displays: Vec<Display>,
    refresh: Arc<dyn Fn() -> scrozz_core::Result<Vec<Window>> + Send + Sync>,
    native_hook: Option<NativePickerHook>,
) -> scrozz_core::Result<Outcome> {
    #[cfg(target_os = "linux")]
    let mut native_focus = scrozz_shell::X11FocusLease::before_window()?;
    #[cfg(target_os = "macos")]
    let reveal_after_native_hook = native_hook.is_some();
    windows.retain(|window| !is_scrozz_window(window));
    let result: Arc<Mutex<Option<scrozz_core::Result<Outcome>>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&result);
    #[cfg(target_os = "windows")]
    let host_viewport = base_viewport()
        .with_inner_size(egui::vec2(1.0, 1.0))
        .with_visible(false);
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let host_viewport = viewport(desktop_bounds(&displays));
    #[cfg(target_os = "macos")]
    let host_viewport = viewport(desktop_bounds(&displays)).with_visible(!reveal_after_native_hook);

    eframe::run_native(
        "Choose a window",
        eframe::NativeOptions {
            viewport: host_viewport,
            persist_window: false,
            ..Default::default()
        },
        Box::new(move |cc| {
            if let Some(hook) = native_hook {
                hook(cc)?;
                #[cfg(target_os = "macos")]
                cc.egui_ctx
                    .send_viewport_cmd(egui::ViewportCommand::Visible(true));
            }
            crate::theme::install_fonts(&cc.egui_ctx);
            let theme = crate::theme::Theme::dark();
            crate::theme::install_style(&cc.egui_ctx, &theme);
            #[cfg(target_os = "linux")]
            let native_focus = {
                use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};

                let handle = cc.window_handle().map_err(|error| {
                    scrozz_core::Error::Platform(format!(
                        "could not read the X11 picker window handle: {error}"
                    ))
                })?;
                let window = match handle.as_raw() {
                    RawWindowHandle::Xlib(handle) => {
                        u32::try_from(handle.window).map_err(|_| {
                            scrozz_core::Error::Platform(format!(
                                "X11 picker window id does not fit in 32 bits: {}",
                                handle.window
                            ))
                        })?
                    }
                    RawWindowHandle::Xcb(handle) => handle.window.get(),
                    other => {
                        return Err(scrozz_core::Error::Platform(format!(
                            "in-process Linux window picking requires X11, got {other:?}"
                        ))
                        .into());
                    }
                };
                native_focus.attach_window(window)?;
                native_focus
            };
            Ok(Box::new(PickerApp {
                picker: WindowPicker::new(windows, displays),
                refresh,
                result: sink,
                theme,
                notice: None,
                terminal_input: TerminalInputGate::default(),
                #[cfg(target_os = "linux")]
                native_focus,
            }))
        }),
    )
    .map_err(|error| {
        scrozz_core::Error::Platform(format!("the window picker could not open: {error}"))
    })?;

    result
        .lock()
        .map_err(|_| scrozz_core::Error::Platform("the window picker panicked".to_owned()))?
        .take()
        .unwrap_or(Ok(Outcome::Cancelled))
}

struct PickerApp {
    picker: WindowPicker,
    refresh: Arc<dyn Fn() -> scrozz_core::Result<Vec<Window>> + Send + Sync>,
    result: Arc<Mutex<Option<scrozz_core::Result<Outcome>>>>,
    theme: crate::theme::Theme,
    notice: Option<String>,
    terminal_input: TerminalInputGate,
    #[cfg(target_os = "linux")]
    native_focus: scrozz_shell::X11FocusLease,
}

fn store_terminal_result(
    result: &Mutex<Option<scrozz_core::Result<Outcome>>>,
    outcome: scrozz_core::Result<Outcome>,
) {
    if let Ok(mut slot) = result.lock()
        && slot.is_none()
    {
        *slot = Some(outcome);
    }
}

impl PickerApp {
    fn finish(&mut self, ctx: &egui::Context, outcome: scrozz_core::Result<Outcome>) {
        #[cfg(target_os = "linux")]
        if let Err(error) = self.native_focus.restore() {
            tracing::warn!("X11 picker focus restoration reported: {error}");
        }
        store_terminal_result(self.result.as_ref(), outcome);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn commit(&mut self, ctx: &egui::Context, id: WindowId) {
        let live = match (self.refresh)() {
            Ok(mut windows) => {
                windows.retain(|window| !is_scrozz_window(window));
                windows
            }
            Err(error) => {
                self.finish(ctx, Err(error));
                return;
            }
        };

        if live.iter().any(|window| window.id == id) {
            self.finish(ctx, Ok(Outcome::Selected(id)));
        } else {
            self.picker.refresh(live);
            self.notice = Some(format!(
                "That window closed. Choose another window. ({})",
                id.0
            ));
        }
    }
}

impl eframe::App for PickerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        #[cfg(target_os = "linux")]
        match self.native_focus.maintain() {
            Ok(true) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
            Ok(false) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(16));
            }
            Err(error) => {
                self.finish(&ctx, Err(error));
                return;
            }
        }
        let root_close_requested = ctx.input(|input| input.viewport().close_requested());

        #[cfg(target_os = "windows")]
        let (intent, child_close_requested, terminal_input) =
            interact_display_viewports(&ctx, &mut self.picker, &self.theme, self.notice.as_deref());

        #[cfg(not(target_os = "windows"))]
        let (intent, child_close_requested, terminal_input) = {
            let bounds = self.picker.desktop_bounds();
            (
                paint::interact(
                    ui,
                    &mut self.picker,
                    bounds.origin,
                    &self.theme,
                    self.notice.as_deref(),
                ),
                false,
                TerminalInputState::read(&ctx),
            )
        };

        let terminal = self.terminal_input.update(
            intent,
            root_close_requested || child_close_requested,
            self.picker.focused_id(),
            terminal_input,
        );
        if self.terminal_input.is_waiting() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
        let Some(terminal) = terminal else {
            return;
        };

        match terminal {
            TerminalAction::Cancel => self.finish(&ctx, Ok(Outcome::Cancelled)),
            TerminalAction::Commit(id) => self.commit(&ctx, id),
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::fixtures::{self, PickerFixture};
    use scrozz_core::LogicalSize;

    fn at(x: f64, y: f64) -> LogicalPoint {
        LogicalPoint::new(x, y)
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> LogicalRect {
        LogicalRect::new(at(x, y), LogicalSize::new(width, height))
    }

    #[test]
    fn the_front_most_window_wins_where_two_overlap() {
        let mut picker = fixtures::overlapping().into_picker();
        assert!(picker.point_at(at(300.0, 300.0)));
        assert_eq!(picker.focused_id(), Some(&WindowId("front".to_owned())));
    }

    #[test]
    fn keyboard_focus_survives_until_the_pointer_actually_moves() {
        let mut picker = fixtures::overlapping().into_picker();
        let pointer = at(300.0, 300.0);
        picker.point_at(pointer);
        let pointer_window = picker.focused_id().cloned().expect("pointer hit");
        picker.focus_next();
        assert_ne!(picker.focused_id(), Some(&pointer_window));

        assert!(
            !picker.point_at(pointer),
            "a stationary pointer is not input"
        );
        assert_ne!(
            picker.focused_id(),
            Some(&pointer_window),
            "a frame with no mouse movement must not undo Tab selection"
        );

        picker.point_at(at(301.0, 300.0));
        assert_eq!(
            picker.focused_id(),
            Some(&pointer_window),
            "real pointer movement takes focus back"
        );
    }

    #[test]
    fn keyboard_focus_survives_when_the_pointer_leaves_on_the_next_frame() {
        let mut picker = fixtures::overlapping().into_picker();
        picker.point_at(at(300.0, 300.0));
        let pointer_window = picker.focused_id().cloned().expect("pointer hit");
        picker.focus_next();
        let keyboard_window = picker.focused_id().cloned().expect("keyboard focus");
        assert_ne!(keyboard_window, pointer_window);

        assert!(
            !picker.clear_pointer(),
            "pointer absence must not clear keyboard focus"
        );
        assert_eq!(picker.focused_id(), Some(&keyboard_window));
        assert_eq!(
            picker.highlight().expect("highlight").focus_method,
            FocusMethod::Keyboard
        );
        assert!(
            !picker.point_at(at(300.0, 300.0)),
            "passive pointer re-entry at the same position is not movement"
        );
        assert_eq!(picker.focused_id(), Some(&keyboard_window));

        picker.point_at(at(301.0, 300.0));
        assert_eq!(
            picker.highlight().expect("highlight").focus_method,
            FocusMethod::Pointer
        );
    }

    #[test]
    fn refreshing_for_keyboard_commit_does_not_restore_pointer_focus() {
        let fixture = fixtures::overlapping();
        let mut picker = fixture.clone().into_picker();
        picker.point_at(at(300.0, 300.0));
        let pointer_window = picker.focused_id().cloned().expect("pointer hit");
        picker.focus_next();
        let keyboard_window = picker.focused_id().cloned().expect("keyboard focus");
        assert_ne!(keyboard_window, pointer_window);

        picker.refresh(fixture.windows);

        assert_eq!(
            picker.focused_id(),
            Some(&keyboard_window),
            "live commit enumeration must preserve keyboard focus"
        );
    }

    #[test]
    fn a_point_outside_every_window_focuses_nothing() {
        let mut picker = fixtures::overlapping().into_picker();
        picker.point_at(at(300.0, 300.0));
        assert!(picker.point_at(at(5.0, 5.0)));
        assert_eq!(picker.focused_id(), None);
        assert_eq!(picker.highlight(), None);
    }

    #[test]
    fn our_own_overlay_is_never_selectable() {
        // The overlay covers the whole desktop and is front-most, so without the
        // exclusion it is the answer at every single point.
        let picker = fixtures::with_our_overlay().into_picker();
        assert!(
            picker
                .window_at(at(300.0, 300.0))
                .is_some_and(|window| { window.id != WindowId("scrozz-overlay".to_owned()) }),
            "the picker selected Scrozz's own overlay"
        );
        assert_eq!(picker.candidate_count(), 2);
    }

    #[test]
    fn application_identity_recognises_our_windows_without_matching_titles() {
        let fixture = fixtures::with_our_overlay();
        assert!(is_scrozz_window(&fixture.windows[0]));

        let mut foreign = fixture.windows[1].clone();
        foreign.title = Some("Scrozz — release notes".to_owned());
        assert!(
            !is_scrozz_window(&foreign),
            "a foreign window title must not make it look like our overlay"
        );
    }

    #[test]
    fn a_separate_app_with_scrozz_in_its_bundle_id_remains_selectable() {
        let mut target = fixtures::overlapping().windows[0].clone();
        target.application = Some("Scrozz Native Window Target".to_owned());
        target.application_id = Some("com.thatcube.scrozz.native-window-target".to_owned());

        assert!(
            !is_scrozz_window(&target),
            "bundle-id components are not proof that a window belongs to this process"
        );
    }

    #[test]
    fn minimised_windows_are_listed_but_not_hoverable() {
        let picker = fixtures::with_minimised().into_picker();
        // The minimised window's bounds still cover this point.
        assert_eq!(picker.window_at(at(900.0, 700.0)), None);
        assert_eq!(picker.candidate_count(), 1);
    }

    #[test]
    fn tiled_windows_sharing_an_edge_resolve_to_exactly_one() {
        let picker = fixtures::tiled().into_picker();
        // x = 500 is the shared boundary: left ends there, right begins there.
        let hit = picker.window_at(at(500.0, 300.0)).map(|w| w.id.clone());
        assert_eq!(hit, Some(WindowId("right".to_owned())));
        assert_eq!(
            picker.window_at(at(499.0, 300.0)).map(|w| w.id.clone()),
            Some(WindowId("left".to_owned()))
        );
    }

    #[test]
    fn a_display_viewport_targets_the_native_monitor_directly() {
        let viewport = display_viewport(2);
        assert_eq!(viewport.monitor, Some(2));
        assert_eq!(viewport.position, None);
        assert_eq!(viewport.inner_size, None);
    }

    #[test]
    fn the_highlight_traces_the_true_bounds_untouched() {
        let mut picker = fixtures::overlapping().into_picker();
        picker.point_at(at(300.0, 300.0));
        let highlight = picker.highlight().expect("a window is focused");

        let expected = picker
            .focused()
            .expect("focused window is in the snapshot")
            .bounds;
        assert_eq!(
            highlight.bounds, expected,
            "the highlight must not inset, expand or round the window's frame"
        );
    }

    #[test]
    fn a_window_on_the_retina_display_reports_its_own_scale() {
        let picker = fixtures::mixed_dpi().into_picker();
        let retina = picker
            .window_at(at(400.0, 400.0))
            .expect("a window on the 2x display");
        let highlight = picker.describe(retina);

        assert_eq!(highlight.scale.get(), 2.0);
        assert!(!highlight.spans_displays);
        // 600x400 logical at 2x.
        assert_eq!(highlight.pixel_size(), (1200, 800));
    }

    #[test]
    fn a_window_on_the_one_x_display_is_not_captured_at_the_retina_scale() {
        let picker = fixtures::mixed_dpi().into_picker();
        let external = picker
            .window_at(at(2200.0, 400.0))
            .expect("a window on the 1x display");
        let highlight = picker.describe(external);

        assert_eq!(
            highlight.scale.get(),
            1.0,
            "using the primary display's scale would double this capture"
        );
        assert_eq!(highlight.pixel_size(), (500, 300));
    }

    #[test]
    fn a_straddling_window_takes_the_scale_of_the_display_it_mostly_covers() {
        let picker = fixtures::mixed_dpi().into_picker();
        let straddling = picker
            .window_at(at(1900.0, 400.0))
            .expect("the straddling window");
        let highlight = picker.describe(straddling);

        assert!(
            highlight.spans_displays,
            "the UI cannot warn about a straddle it is not told about"
        );
        // 300 pt on the 2x side, 500 pt on the 1x side: the 1x display wins.
        assert_eq!(highlight.scale.get(), 1.0);
    }

    #[test]
    fn a_window_dragged_entirely_off_screen_still_reports_a_usable_scale() {
        let picker = fixtures::mixed_dpi().into_picker();
        let stray = Window {
            id: WindowId("stray".to_owned()),
            title: None,
            application: None,
            application_id: None,
            bounds: LogicalRect::new(
                LogicalPoint::new(-9000.0, -9000.0),
                LogicalSize::new(100.0, 100.0),
            ),
            picker_bounds: None,
            corner_radius: None,
            display: DisplayId("retina".to_owned()),
            is_visible: true,
        };
        let highlight = picker.describe(&stray);
        assert_eq!(highlight.scale.get(), 2.0);
        assert!(!highlight.spans_displays);
    }

    #[test]
    fn committing_a_window_that_closed_reports_it_vanished_rather_than_selected() {
        let fixture = fixtures::overlapping();
        let mut picker = fixture.clone().into_picker();
        picker.point_at(at(300.0, 300.0));

        // The user was aiming at "front"; it closed while they decided.
        let live: Vec<Window> = fixture
            .windows
            .into_iter()
            .filter(|window| window.id != WindowId("front".to_owned()))
            .collect();

        assert_eq!(
            picker.commit(&live),
            Outcome::Vanished(WindowId("front".to_owned()))
        );
    }

    #[test]
    fn committing_with_nothing_focused_is_a_cancellation_not_a_vanishing() {
        let picker = fixtures::overlapping().into_picker();
        assert_eq!(picker.commit(&[]), Outcome::Cancelled);
    }

    #[test]
    fn a_programmatic_close_cannot_overwrite_a_committed_selection() {
        let result = Mutex::new(None);
        let selected = WindowId("front".to_owned());

        store_terminal_result(&result, Ok(Outcome::Selected(selected.clone())));
        store_terminal_result(&result, Ok(Outcome::Cancelled));

        let outcome = result
            .into_inner()
            .expect("result lock")
            .expect("terminal result")
            .expect("successful picker result");
        assert_eq!(outcome, Outcome::Selected(selected));
    }

    #[test]
    fn terminal_actions_wait_for_every_key_modifier_and_pointer_button() {
        let focused = WindowId("front".to_owned());
        let held_states = [
            TerminalInputState {
                keys_down: true,
                ..TerminalInputState::default()
            },
            TerminalInputState {
                modifiers_down: true,
                ..TerminalInputState::default()
            },
            TerminalInputState {
                pointer_down: true,
                ..TerminalInputState::default()
            },
        ];

        for held in held_states {
            let mut gate = TerminalInputGate::default();
            assert_eq!(
                gate.update(paint::Intent::Commit, false, Some(&focused), held),
                None
            );
            assert!(gate.is_waiting());
            assert_eq!(
                gate.update(
                    paint::Intent::None,
                    false,
                    None,
                    TerminalInputState::default()
                ),
                Some(TerminalAction::Commit(focused.clone()))
            );
            assert!(!gate.is_waiting());
        }
    }

    #[test]
    fn terminal_input_sampling_tracks_escape_and_enter_through_release() {
        for key in [egui::Key::Escape, egui::Key::Enter] {
            let ctx = egui::Context::default();
            let event = |pressed| egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            };
            let mut output = ctx.run_ui(
                egui::RawInput {
                    focused: true,
                    events: vec![event(true)],
                    ..Default::default()
                },
                |_| {},
            );
            output.textures_delta.clear();
            assert!(
                TerminalInputState::read(&ctx).keys_down,
                "{key:?} must remain held after its key-down frame"
            );

            let mut output = ctx.run_ui(
                egui::RawInput {
                    focused: true,
                    events: vec![event(false)],
                    ..Default::default()
                },
                |_| {},
            );
            output.textures_delta.clear();
            assert!(
                TerminalInputState::read(&ctx).all_released(),
                "{key:?} key-up must unlock terminal finalization"
            );
        }
    }

    #[test]
    fn terminal_input_sampling_tracks_modifiers_and_pointer_buttons() {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(
            egui::RawInput {
                focused: true,
                events: vec![
                    egui::Event::ModifiersChanged(egui::Modifiers::SHIFT),
                    egui::Event::PointerButton {
                        pos: egui::pos2(10.0, 10.0),
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::SHIFT,
                    },
                ],
                ..Default::default()
            },
            |_| {},
        );
        output.textures_delta.clear();
        let held = TerminalInputState::read(&ctx);
        assert!(held.modifiers_down);
        assert!(held.pointer_down);

        let mut output = ctx.run_ui(
            egui::RawInput {
                focused: true,
                events: vec![
                    egui::Event::ModifiersChanged(egui::Modifiers::NONE),
                    egui::Event::PointerButton {
                        pos: egui::pos2(10.0, 10.0),
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |_| {},
        );
        output.textures_delta.clear();
        assert!(TerminalInputState::read(&ctx).all_released());
    }

    #[test]
    fn escape_waits_for_release_before_cancelling() {
        let mut gate = TerminalInputGate::default();
        assert_eq!(
            gate.update(
                paint::Intent::Cancel,
                false,
                None,
                TerminalInputState {
                    keys_down: true,
                    ..TerminalInputState::default()
                }
            ),
            None
        );
        assert_eq!(
            gate.update(
                paint::Intent::None,
                false,
                None,
                TerminalInputState::default()
            ),
            Some(TerminalAction::Cancel)
        );
    }

    #[test]
    fn a_pending_commit_keeps_the_window_focused_on_the_key_down_frame() {
        let mut gate = TerminalInputGate::default();
        let first = WindowId("front".to_owned());
        let later = WindowId("back".to_owned());

        assert_eq!(
            gate.update(
                paint::Intent::Commit,
                false,
                Some(&first),
                TerminalInputState {
                    keys_down: true,
                    ..TerminalInputState::default()
                }
            ),
            None
        );
        assert_eq!(
            gate.update(
                paint::Intent::Commit,
                false,
                Some(&later),
                TerminalInputState::default()
            ),
            Some(TerminalAction::Commit(first))
        );
    }

    #[test]
    fn refreshing_moves_focus_to_whatever_is_now_under_the_pointer() {
        let fixture = fixtures::overlapping();
        let mut picker = fixture.clone().into_picker();
        picker.point_at(at(300.0, 300.0));
        assert_eq!(picker.focused_id(), Some(&WindowId("front".to_owned())));

        let remaining: Vec<Window> = fixture
            .windows
            .into_iter()
            .filter(|window| window.id != WindowId("front".to_owned()))
            .collect();
        picker.refresh(remaining);

        assert_eq!(
            picker.focused_id(),
            Some(&WindowId("back".to_owned())),
            "the highlight must follow the pointer, not linger on a closed window"
        );
    }

    #[test]
    fn refreshing_without_a_pointer_drops_focus_on_a_window_that_closed() {
        let fixture = fixtures::overlapping();
        let mut picker = fixture.clone().into_picker();
        picker.focus_next();
        let first = picker.focused_id().cloned().expect("keyboard focus");

        let remaining: Vec<Window> = fixture
            .windows
            .into_iter()
            .filter(|window| window.id != first)
            .collect();
        picker.refresh(remaining);

        assert_eq!(picker.focused_id(), None);
    }

    #[test]
    fn keyboard_cycling_wraps_in_both_directions() {
        let mut picker = fixtures::overlapping().into_picker();

        assert_eq!(picker.focus_next(), Some(&WindowId("front".to_owned())));
        assert_eq!(picker.focus_next(), Some(&WindowId("back".to_owned())));
        // Wraps forward.
        assert_eq!(picker.focus_next(), Some(&WindowId("front".to_owned())));
        // And backward, which is where a naive `% len` returns nothing.
        assert_eq!(picker.focus_prev(), Some(&WindowId("back".to_owned())));
    }

    #[test]
    fn back_tabbing_from_nothing_starts_at_the_rearmost_window() {
        let mut picker = fixtures::overlapping().into_picker();
        assert_eq!(picker.focus_prev(), Some(&WindowId("back".to_owned())));
    }

    #[test]
    fn keyboard_cycling_skips_our_overlay_and_minimised_windows() {
        let mut picker = fixtures::with_minimised().into_picker();
        let first = picker.focus_next().cloned();
        let second = picker.focus_next().cloned();
        assert_eq!(first, second, "one candidate must cycle back to itself");
        assert_ne!(first, Some(WindowId("minimised".to_owned())));
    }

    #[test]
    fn an_empty_desktop_is_reported_rather_than_hidden() {
        let picker = PickerFixture {
            name: "empty",
            windows: Vec::new(),
            displays: fixtures::single_display(),
            excluded: Vec::new(),
        }
        .into_picker();

        assert!(picker.is_empty());
        assert_eq!(picker.highlight(), None);
    }

    #[test]
    fn the_label_prefers_the_application_over_the_title() {
        let mut picker = fixtures::overlapping().into_picker();
        picker.point_at(at(300.0, 300.0));
        assert_eq!(picker.highlight().expect("focused").label(), "Safari");
    }

    #[test]
    fn a_backend_radius_reaches_the_highlight_without_changing_bounds() {
        let mut fixture = fixtures::overlapping();
        fixture.windows[0].corner_radius = Some(WindowCornerRadius::Measured(15.5));
        let window = fixture.windows[0].clone();
        let picker = fixture.into_picker();
        let highlight = picker.describe(&window);

        assert_eq!(
            highlight.corner_radius,
            Some(WindowCornerRadius::Measured(15.5))
        );
        assert_eq!(highlight.bounds, window.bounds);
    }

    #[test]
    fn a_window_with_no_metadata_still_gets_a_readable_label() {
        let picker = fixtures::overlapping().into_picker();
        let anonymous = Window {
            id: WindowId("anon".to_owned()),
            title: None,
            application: None,
            application_id: None,
            bounds: LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(10.0, 10.0)),
            picker_bounds: None,
            corner_radius: None,
            display: DisplayId("main".to_owned()),
            is_visible: true,
        };
        assert_eq!(picker.describe(&anonymous).label(), "Untitled window");
    }

    #[test]
    fn sparse_system_surface_only_intercepts_its_visible_picker_bounds() {
        let mut fixture = fixtures::overlapping();
        let mut dock = fixture.windows[0].clone();
        dock.id = WindowId("dock".to_owned());
        dock.bounds = rect(0.0, 0.0, 1440.0, 900.0);
        dock.picker_bounds = Some(rect(420.0, 820.0, 600.0, 80.0));
        fixture.windows.insert(0, dock);
        let picker = fixture.into_picker();

        assert_eq!(
            picker.window_at(at(300.0, 200.0)).map(|window| &window.id),
            Some(&WindowId("front".to_owned())),
            "transparent native canvas must not swallow an application window"
        );
        let dock = picker.window_at(at(500.0, 850.0)).expect("visible Dock");
        assert_eq!(dock.id, WindowId("dock".to_owned()));

        let highlight = picker.describe(dock);
        assert_eq!(highlight.bounds, rect(420.0, 820.0, 600.0, 80.0));
        assert_eq!(highlight.capture_bounds, rect(0.0, 0.0, 1440.0, 900.0));
        assert_eq!(highlight.pixel_size(), (2880, 1800));
    }

    #[test]
    fn zero_area_windows_are_not_candidates() {
        let picker = fixtures::with_zero_area().into_picker();
        assert_eq!(picker.candidate_count(), 1);
        assert_eq!(picker.window_at(at(50.0, 50.0)), None);
    }

    #[test]
    fn clearing_the_pointer_drops_the_highlight() {
        let mut picker = fixtures::overlapping().into_picker();
        picker.point_at(at(300.0, 300.0));
        assert!(picker.clear_pointer());
        assert_eq!(picker.highlight(), None);
        // Idempotent: a second clear reports no change.
        assert!(!picker.clear_pointer());
    }

    #[test]
    fn pointing_at_the_same_window_twice_reports_no_change() {
        let mut picker = fixtures::overlapping().into_picker();
        assert!(picker.point_at(at(300.0, 300.0)));
        assert!(
            !picker.point_at(at(310.0, 310.0)),
            "a repaint per mouse event is a repaint per mouse event"
        );
    }
}

pub mod fixtures;
