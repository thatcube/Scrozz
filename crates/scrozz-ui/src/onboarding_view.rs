//! The first-run onboarding wizard — decision D26.
//!
//! Four things, and only four, need saying before Scrozz gets out of the way:
//! how to send a capture to another app, how to take one, where it goes, and —
//! on Linux, where a Wayland compositor refuses to hand out global hotkeys —
//! the line the user has to add to their own compositor config. That is the
//! whole tour. D26 is deliberately short: a longer one gets skipped, and a
//! shorter one leaves the one platform-specific step nobody would otherwise
//! find on their own.
//!
//! # Not a permissions wall
//!
//! Onboarding never blocks anything. [`OnboardingAction::Skip`] ends the flow
//! from any step, [`OnboardingOutcome::Dismissed::rerunnable`] is always
//! `true`, and nothing here requests an OS permission — that is the app's
//! concern, on its own screen, at the point it is actually needed. This module
//! only ever teaches; it never gates.
//!
//! # Illustrations are placeholders, honestly
//!
//! There is no illustrator on this project yet, so each topic's picture is
//! built from the same primitives the real capture stack draws with —
//! [`crate::paint::ghost_card`], [`crate::paint::motion_streaks`],
//! [`crate::paint::dashed_path`], the dock's own chevron — rather than an
//! invented graphic in a different visual language. When real artwork ships,
//! only [`illustration`] changes; the four topics, their copy and the wizard
//! chrome around them do not.

use crate::icons::Icon;
use crate::paint::{self, ControlState, Reveal, Surface};
use crate::theme::{Radius, Space, Text};
use egui::{Align2, Color32, Id, Rect, Stroke, StrokeKind, Ui, pos2, vec2};

const ALERT: Color32 = Color32::from_rgb(0xF2, 0x45, 0x3D);

// ---------------------------------------------------------------------------
// The four topics
// ---------------------------------------------------------------------------

/// One of the exactly four things D26 covers.
///
/// Closed: onboarding is short *because* the set cannot silently grow. A fifth
/// topic is a design decision, not a drive-by addition — hence `#[non_exhaustive]`
/// rather than a plain closed enum, so adding one is still a compile-time
/// prompt to update every `match` that cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OnboardingTopic {
    /// Pushing a card right or up drags it straight into another app.
    DragOut,
    /// The keyboard shortcut that takes a capture.
    CaptureHotkey,
    /// The configured folder where capture files are written.
    WhereCapturesGo,
    /// On Linux, a wlroots compositor will not hand an app a global hotkey, so
    /// the user has to bind one themselves.
    CompositorKeybinding,
}

impl OnboardingTopic {
    /// All four topics, in the order the wizard presents them.
    #[must_use]
    pub const fn all() -> [Self; 4] {
        [
            Self::DragOut,
            Self::CaptureHotkey,
            Self::WhereCapturesGo,
            Self::CompositorKeybinding,
        ]
    }

    /// This topic's position in [`Self::all`], zero-based.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::DragOut => 0,
            Self::CaptureHotkey => 1,
            Self::WhereCapturesGo => 2,
            Self::CompositorKeybinding => 3,
        }
    }

    /// A stable slug, for logs and golden baseline names.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::DragOut => "drag-out",
            Self::CaptureHotkey => "capture-hotkey",
            Self::WhereCapturesGo => "where-captures-go",
            Self::CompositorKeybinding => "compositor-keybinding",
        }
    }

    /// The headline shown for this topic.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::DragOut => "Drag it to send it",
            Self::CaptureHotkey => "One shortcut takes a capture",
            Self::WhereCapturesGo => "Choose where captures are saved",
            Self::CompositorKeybinding => "On Linux, bind the key yourself",
        }
    }

    /// The plain-English explanation shown under the headline.
    #[must_use]
    pub const fn body(self) -> &'static str {
        match self {
            Self::DragOut => {
                "Push a capture right or up and it lifts, tilts, and follows your \
                 pointer straight into another app, document, or folder. \
                 There's no separate share button; the direction is the action."
            }
            Self::CaptureHotkey => {
                "Press the shortcut below to capture a region. It works whether \
                 Scrozz is in the background or focused, and you can change it \
                 any time in Settings → Shortcuts."
            }
            Self::WhereCapturesGo => {
                "When you choose Save on a capture, Scrozz writes it to the folder \
                 below. You can choose any folder now or change it later in Settings."
            }
            Self::CompositorKeybinding => {
                "Some Wayland compositors don't let apps register global shortcuts. \
                 If this system needs a manual binding, add the exact line below to \
                 your compositor config and reload it."
            }
        }
    }

    fn body_for(self, values: &OnboardingContent) -> &'static str {
        match self {
            Self::DragOut if !values.drag_out_available => {
                "Direct drag-out is not available in this build. Use Copy to send \
                 the image through the clipboard, or Save to create a file."
            }
            Self::CaptureHotkey if !values.capture_hotkey_available => {
                "Region capture is not available in this desktop session, so Scrozz \
                 has not registered its shortcut. Available capture actions remain \
                 in the menu."
            }
            _ => self.body(),
        }
    }

    /// Whether this topic only applies on Linux under a wlroots compositor.
    ///
    /// The wizard still shows it in the same closed set of four everywhere —
    /// this crate has no platform detection of its own — but the app is free
    /// to route straight past it with [`OnboardingAction::Skip`] or start a
    /// re-run at a later topic when it already knows the platform does not
    /// need this step.
    #[must_use]
    pub const fn platform_specific(self) -> bool {
        matches!(self, Self::CompositorKeybinding)
    }
}

/// App-provided values shown inside the four fixed onboarding topics.
///
/// The UI owns their presentation, while the app owns the real shortcut,
/// capture folder, compositor detection, and persistence error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnboardingContent {
    /// The display form of the current capture-region shortcut.
    pub capture_shortcut: String,
    /// The currently configured capture folder.
    pub capture_folder: String,
    /// Whether the native host can drag a capture card into another app.
    pub drag_out_available: bool,
    /// Whether region capture and its global shortcut are available this session.
    pub capture_hotkey_available: bool,
    /// The exact compositor line to add, or `None` when no manual binding is
    /// needed on this system.
    pub compositor_config: Option<String>,
    /// A non-blocking persistence or platform error.
    pub error: Option<String>,
}

impl Default for OnboardingContent {
    fn default() -> Self {
        Self {
            capture_shortcut: "Super+Shift+4".to_owned(),
            capture_folder: "~/Pictures/Scrozz".to_owned(),
            drag_out_available: true,
            capture_hotkey_available: true,
            compositor_config: Some(
                "bindsym Mod4+Shift+4 exec scrozz capture --interactive region".to_owned(),
            ),
            error: None,
        }
    }
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

/// Where the wizard is right now: just the current topic.
///
/// Deliberately this small. There is no "seen" set, no per-topic flag — the
/// four topics are always presented in the same order and the only thing that
/// varies between one render and the next is which one is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnboardingState {
    topic: OnboardingTopic,
}

impl OnboardingState {
    /// The wizard at its first topic.
    #[must_use]
    pub const fn start() -> Self {
        Self::at(OnboardingTopic::DragOut)
    }

    /// The wizard at a specific topic, e.g. to re-run starting from the one
    /// platform-specific step.
    #[must_use]
    pub const fn at(topic: OnboardingTopic) -> Self {
        Self { topic }
    }

    /// The topic currently showing.
    #[must_use]
    pub const fn topic(self) -> OnboardingTopic {
        self.topic
    }

    /// One-based step number, for a "step 2 of 4" caption.
    #[must_use]
    pub const fn step_number(self) -> usize {
        self.topic.index() + 1
    }

    /// Total number of steps — always 4 (D26).
    #[must_use]
    pub const fn step_count() -> usize {
        OnboardingTopic::all().len()
    }

    /// Whether [`OnboardingAction::Back`] would move to an earlier topic.
    #[must_use]
    pub const fn can_go_back(self) -> bool {
        self.topic.index() > 0
    }

    /// Whether this is the final topic, i.e. the primary button should read
    /// "Finish" rather than "Next".
    #[must_use]
    pub const fn is_last(self) -> bool {
        self.topic.index() + 1 == Self::step_count()
    }

    /// Advances the state machine.
    ///
    /// [`OnboardingAction::Skip`] and [`OnboardingAction::Finish`] always end
    /// the flow — per D26 this is never a permissions wall, so there is no
    /// state from which the user cannot leave. [`OnboardingAction::Back`] and
    /// [`OnboardingAction::Next`] that would move outside `0..4` are no-ops
    /// rather than errors, since a stray click on a disabled nav button is an
    /// ordinary UI event, not a bug to report.
    #[must_use]
    pub fn apply(self, action: OnboardingAction) -> OnboardingOutcome {
        match action {
            OnboardingAction::Back => {
                if self.can_go_back() {
                    let topics = OnboardingTopic::all();
                    OnboardingOutcome::Continue(Self::at(topics[self.topic.index() - 1]))
                } else {
                    OnboardingOutcome::Continue(self)
                }
            }
            OnboardingAction::Next => {
                if self.is_last() {
                    OnboardingOutcome::Continue(self)
                } else {
                    let topics = OnboardingTopic::all();
                    OnboardingOutcome::Continue(Self::at(topics[self.topic.index() + 1]))
                }
            }
            OnboardingAction::Skip | OnboardingAction::Finish => {
                OnboardingOutcome::Dismissed { rerunnable: true }
            }
        }
    }
}

/// A button press in the wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OnboardingAction {
    /// Return to the previous topic.
    Back,
    /// Advance to the next topic.
    Next,
    /// End the flow immediately, from any topic.
    Skip,
    /// End the flow from the last topic.
    Finish,
}

/// What happened after [`OnboardingState::apply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OnboardingOutcome {
    /// The wizard continues at a (possibly unchanged) state.
    Continue(OnboardingState),
    /// The wizard is done. Always re-runnable — see the module's own docs —
    /// so the app is expected to offer "Show onboarding again" from settings
    /// unconditionally rather than only when it was skipped.
    Dismissed {
        /// Always `true` today; carried as a field rather than hard-coded at
        /// every call site so a future exception is one field, not a grep.
        rerunnable: bool,
    },
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// What the wizard chrome asks the app to do this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OnboardingResponse {
    /// The button pressed this frame, if any. The app applies it via
    /// [`OnboardingState::apply`] and re-renders with the result.
    pub action: Option<OnboardingAction>,
}

/// Draws one step of the onboarding wizard and reports what was pressed.
///
/// Takes the state by value, not by reference: nothing here mutates it, and a
/// step of a four-topic wizard is cheap enough to copy. Ordinary window chrome
/// — the title bar, the close box — is the app's own concern; this draws only
/// the wizard card itself.
pub fn render(
    ui: &mut Ui,
    surface: &Surface<'_>,
    state: OnboardingState,
    values: &OnboardingContent,
) -> OnboardingResponse {
    let mut response = OnboardingResponse::default();
    let palette = surface.palette();
    let full = ui.max_rect();
    let painter = ui.painter().clone();

    paint::glass_panel(&painter, full, Radius::CARD, palette, true);

    let pad = Space::XXL;
    let content = full.shrink(pad);

    // --- illustration (D26: a placeholder built from real surface grammar) -
    let illo_h = (content.height() * 0.40).max(120.0);
    let illo = Rect::from_min_size(content.min, vec2(content.width(), illo_h));
    illustration(&painter, surface, illo, state.topic(), values);

    // --- step indicator -----------------------------------------------------
    let dots_y = illo.bottom() + Space::LG;
    draw_step_dots(&painter, palette, content, dots_y, state);

    // --- headline and body ---------------------------------------------------
    let text_top = dots_y + Space::LG;
    painter.text(
        pos2(content.left(), text_top),
        Align2::LEFT_TOP,
        state.topic().title(),
        surface.font(Text::Display),
        palette.text,
    );
    let body_top = text_top + 40.0;
    let body_rect = Rect::from_min_max(
        pos2(content.left(), body_top),
        pos2(content.right(), content.bottom() - 56.0),
    );
    let wrapped_body = wrap(
        state.topic().body_for(values),
        body_rect.width(),
        surface,
        Text::Subtitle,
    );
    painter.text(
        body_rect.left_top(),
        Align2::LEFT_TOP,
        &wrapped_body,
        surface.font(Text::Subtitle),
        palette.text_muted,
    );

    let detail = match state.topic() {
        OnboardingTopic::WhereCapturesGo => Some(values.capture_folder.as_str()),
        OnboardingTopic::CompositorKeybinding => Some(
            values
                .compositor_config
                .as_deref()
                .unwrap_or("No extra setup is needed on this system."),
        ),
        OnboardingTopic::DragOut | OnboardingTopic::CaptureHotkey => None,
    };
    if let Some(detail) = detail {
        // Anchored under the wrapped copy's own line count, not a fixed
        // offset from the bottom of `body_rect` — the body text's length
        // varies per topic, and a fixed offset let the two overlap once the
        // copy wrapped to a third line.
        let line_count = wrapped_body.matches('\n').count() + 1;
        let line_height = Text::Subtitle.size() * 1.5;
        #[allow(clippy::cast_precision_loss)]
        let code_top = body_rect.top() + line_count as f32 * line_height + Space::SM;
        let code_rect = Rect::from_min_size(
            pos2(body_rect.left(), code_top),
            vec2(body_rect.width(), 34.0),
        );
        draw_detail_sample(&painter, surface, code_rect, detail);
    }

    // --- nav buttons ----------------------------------------------------------
    let bar_y = content.bottom() - 34.0;
    let bar = Rect::from_min_max(
        pos2(content.left(), bar_y),
        pos2(content.right(), content.bottom()),
    );
    if let Some(error) = &values.error {
        painter.text(
            pos2(bar.left(), bar.top() - Space::MD),
            Align2::LEFT_BOTTOM,
            error,
            surface.font(Text::Caption),
            ALERT,
        );
    }

    let skip_rect = Rect::from_min_size(bar.left_top(), vec2(64.0, 34.0));
    if text_button(
        ui,
        surface,
        skip_rect,
        Id::new("scrozz.onboarding.skip"),
        "Skip",
    ) {
        response.action = Some(OnboardingAction::Skip);
    }

    let next_label = if state.is_last() { "Finish" } else { "Next" };
    let next_rect = Rect::from_min_size(pos2(bar.right() - 108.0, bar.top()), vec2(108.0, 34.0));
    if paint::pill_button(
        ui,
        surface,
        next_rect,
        Id::new("scrozz.onboarding.next"),
        if state.is_last() {
            Icon::Check
        } else {
            Icon::ChevronRight
        },
        next_label,
        true,
        Reveal::SHOWN,
    )
    .clicked()
    {
        response.action = Some(if state.is_last() {
            OnboardingAction::Finish
        } else {
            OnboardingAction::Next
        });
    }

    let back_rect = Rect::from_min_size(pos2(next_rect.left() - 96.0, bar.top()), vec2(88.0, 34.0));
    let back_state = ControlState::new().selected(false);
    let back_state = if state.can_go_back() {
        back_state
    } else {
        ControlState::disabled()
    };
    if paint::pill_button_with_state(
        ui,
        surface,
        back_rect,
        Id::new("scrozz.onboarding.back"),
        Icon::ArrowBackUp,
        "Back",
        false,
        back_state,
        Reveal::SHOWN,
    )
    .clicked()
        && state.can_go_back()
    {
        response.action = Some(OnboardingAction::Back);
    }

    response
}

/// A quiet, iconless text button — for "Skip", which must never look as
/// prominent as the primary action.
fn text_button(ui: &mut Ui, surface: &Surface<'_>, rect: Rect, id: Id, label: &str) -> bool {
    let response = ui.interact(rect, id, egui::Sense::click());
    let palette = surface.palette();
    let hovered = surface.interactive && response.hovered();
    let tint = if hovered {
        palette.text
    } else {
        palette.text_faint
    };
    ui.painter().text(
        rect.left_center(),
        Align2::LEFT_CENTER,
        label,
        surface.font(Text::Label),
        tint,
    );
    response.clicked()
}

/// Naive word-wrap to a pixel width, using the same font measurement egui
/// itself uses for the role. Good enough for the two or three lines of copy
/// this surface ever shows; a wizard card is not a text editor.
fn wrap(text: &str, max_width: f32, surface: &Surface<'_>, role: Text) -> String {
    let font = surface.font(role);
    let mut out = String::new();
    let mut line_width = 0.0_f32;
    for word in text.split_whitespace() {
        let word_width = word.chars().count() as f32 * font.size * 0.55;
        if line_width > 0.0 && line_width + word_width > max_width {
            out.push('\n');
            line_width = 0.0;
        } else if !out.is_empty() && !out.ends_with('\n') {
            out.push(' ');
            line_width += font.size * 0.3;
        }
        out.push_str(word);
        line_width += word_width;
    }
    out
}

/// The row of step dots under the illustration: filled for the current step,
/// hollow for the rest. Instant, per D19 — there is nothing to animate between
/// two static dots.
fn draw_step_dots(
    painter: &egui::Painter,
    palette: &crate::theme::Palette,
    content: Rect,
    y: f32,
    state: OnboardingState,
) {
    let count = OnboardingState::step_count();
    let spacing = 16.0;
    let total_w = (count.saturating_sub(1)) as f32 * spacing;
    let start_x = content.center().x - total_w / 2.0;
    for i in 0..count {
        let center = pos2(start_x + i as f32 * spacing, y);
        if i == state.topic().index() {
            painter.circle_filled(center, 4.0, palette.accent);
        } else {
            painter.circle_stroke(center, 4.0, Stroke::new(1.2, palette.text_faint));
        }
    }
}

/// A monospace panel showing an app-provided path or compositor line.
fn draw_detail_sample(painter: &egui::Painter, surface: &Surface<'_>, rect: Rect, detail: &str) {
    let palette = surface.palette();
    painter.rect_filled(rect, Radius::CHIP, palette.chip_fill);
    painter.rect_stroke(
        rect,
        Radius::CHIP,
        Stroke::new(1.0, palette.hairline),
        StrokeKind::Inside,
    );
    painter.text(
        rect.left_center() + vec2(Space::MD, 0.0),
        Align2::LEFT_CENTER,
        detail,
        egui::FontId::monospace(Text::Shortcut.size()),
        palette.text,
    );
}

/// Draws the placeholder illustration for one topic.
///
/// Built entirely from primitives the real capture stack already draws with —
/// see the module's own doc comment for why. Nothing here is final artwork.
fn illustration(
    painter: &egui::Painter,
    surface: &Surface<'_>,
    rect: Rect,
    topic: OnboardingTopic,
    values: &OnboardingContent,
) {
    let palette = surface.palette();
    painter.rect_filled(rect, Radius::CARD, palette.card_fill);
    painter.rect_stroke(
        rect,
        Radius::CARD,
        Stroke::new(1.0, palette.hairline),
        StrokeKind::Inside,
    );

    match topic {
        OnboardingTopic::DragOut => {
            let card_size = vec2(96.0, 64.0);
            let rest = Rect::from_center_size(rect.center() + vec2(-24.0, 18.0), card_size);
            let lifted = Rect::from_center_size(rest.center() + vec2(46.0, -34.0), card_size);
            paint::ghost_card(painter, rest, Radius::THUMB, 0.0, 60);
            painter.rect_filled(lifted, Radius::THUMB, palette.card_fill_raised);
            painter.rect_stroke(
                lifted,
                Radius::THUMB,
                Stroke::new(1.0, palette.accent),
                StrokeKind::Inside,
            );
            paint::motion_streaks(painter, rest.center(), vec2(1.0, -0.7), 4, 10.0);
            paint::dashed_path(
                painter,
                &[rest.center(), lifted.center()],
                Stroke::new(1.5, palette.accent),
                5.0,
                4.0,
            );
        }
        OnboardingTopic::CaptureHotkey => {
            paint::chip_label(painter, surface, rect.center(), &values.capture_shortcut);
        }
        OnboardingTopic::WhereCapturesGo => {
            let folder = Rect::from_center_size(rect.center(), vec2(180.0, 96.0));
            let tab = Rect::from_min_size(folder.min + vec2(12.0, -12.0), vec2(62.0, 20.0));
            painter.rect_filled(tab, Radius::CHIP, palette.card_fill_raised);
            painter.rect_filled(folder, Radius::THUMB, palette.card_fill_raised);
            painter.rect_stroke(
                folder,
                Radius::THUMB,
                Stroke::new(1.0, palette.accent),
                StrokeKind::Inside,
            );
            painter.text(
                folder.center(),
                Align2::CENTER_CENTER,
                "Captures",
                surface.font(Text::Label),
                palette.text,
            );
        }
        OnboardingTopic::CompositorKeybinding => {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "\u{2328}",
                egui::FontId::proportional(40.0),
                palette.text_muted,
            );
        }
    }
}

// ===========================================================================
// The harness scene
// ===========================================================================

/// Renders the onboarding wizard for the harness.
///
/// `ctx.millis()` selects which of the four topics to render: `0` through `3`,
/// one per [`OnboardingTopic`]. This is not an animation duration — D19
/// forbids controls from animating and nothing here does — it is reused
/// purely as the deterministic instant selector the rest of the harness
/// already keys every golden baseline on (see [`crate::harness::KeyInstant`]).
///
/// The icon store is built once in [`setup`](crate::harness::Scene::setup)
/// and cached for the render's lifetime rather than rebuilt in every
/// [`ui`](crate::harness::Scene::ui) call — see
/// [`crate::settings_view::SettingsScene`]'s own doc comment for why a
/// per-frame [`crate::icons::IconStore`] silently renders every icon as a
/// flat, untextured square under this harness's multi-pass renderer.
pub struct OnboardingScene {
    icons: std::sync::Mutex<Option<crate::icons::IconStore>>,
}

impl OnboardingScene {
    /// A scene with no icons uploaded yet; [`crate::harness::Scene::setup`]
    /// populates them.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            icons: std::sync::Mutex::new(None),
        }
    }
}

impl Default for OnboardingScene {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::harness::Scene for OnboardingScene {
    fn name(&self) -> &str {
        "onboarding"
    }

    fn setup(&self, ctx: &egui::Context) {
        crate::theme::install_fonts(ctx);
        crate::theme::install_style(
            ctx,
            &crate::theme::Theme::for_appearance(theme_appearance(ctx)),
        );
        if let Ok(mut slot) = self.icons.lock() {
            *slot = Some(crate::icons::IconStore::new(ctx));
        }
    }

    fn ui(&self, ui: &mut Ui, ctx: &crate::harness::SceneCtx<'_>) {
        let theme = crate::theme::Theme::for_appearance(theme_appearance_from(ctx.theme));
        let guard = self
            .icons
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let empty = crate::icons::IconStore::empty();
        let icons = guard.as_ref().unwrap_or(&empty);
        let motion = crate::motion::Motion::at_ms(ctx.millis());
        let surface = Surface::still(&theme, icons, motion);

        let topics = OnboardingTopic::all();
        let idx = (ctx.millis() as usize).min(topics.len() - 1);
        let state = OnboardingState::at(topics[idx]);
        render(ui, &surface, state, &OnboardingContent::default());
    }
}

fn theme_appearance(_ctx: &egui::Context) -> crate::theme::Appearance {
    crate::theme::Appearance::Dark
}

fn theme_appearance_from(theme: egui::Theme) -> crate::theme::Appearance {
    match theme {
        egui::Theme::Dark => crate::theme::Appearance::Dark,
        egui::Theme::Light => crate::theme::Appearance::Light,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_drag_out() {
        assert_eq!(OnboardingState::start().topic(), OnboardingTopic::DragOut);
        assert_eq!(OnboardingState::start().step_number(), 1);
        assert!(!OnboardingState::start().can_go_back());
    }

    #[test]
    fn unavailable_runtime_features_are_described_honestly() {
        let content = OnboardingContent {
            drag_out_available: false,
            capture_hotkey_available: false,
            ..OnboardingContent::default()
        };

        assert!(
            OnboardingTopic::DragOut
                .body_for(&content)
                .contains("not available")
        );
        assert!(
            OnboardingTopic::CaptureHotkey
                .body_for(&content)
                .contains("not available")
        );
        assert!(
            OnboardingTopic::WhereCapturesGo
                .body_for(&content)
                .starts_with("When you choose Save")
        );
    }

    #[test]
    fn next_advances_through_all_four_topics_in_order() {
        let mut state = OnboardingState::start();
        let expected = [
            OnboardingTopic::DragOut,
            OnboardingTopic::CaptureHotkey,
            OnboardingTopic::WhereCapturesGo,
            OnboardingTopic::CompositorKeybinding,
        ];
        for (i, topic) in expected.iter().enumerate() {
            assert_eq!(state.topic(), *topic, "step {i}");
            match state.apply(OnboardingAction::Next) {
                OnboardingOutcome::Continue(next) => state = next,
                OnboardingOutcome::Dismissed { .. } => {
                    assert_eq!(i, expected.len() - 1, "only the last Next may dismiss");
                }
            }
        }
    }

    #[test]
    fn next_on_the_last_topic_is_a_no_op_not_a_dismiss() {
        let state = OnboardingState::at(OnboardingTopic::CompositorKeybinding);
        assert!(state.is_last());
        match state.apply(OnboardingAction::Next) {
            OnboardingOutcome::Continue(next) => assert_eq!(next, state),
            OnboardingOutcome::Dismissed { .. } => panic!("Next must not dismiss; Finish does"),
        }
    }

    #[test]
    fn finish_on_the_last_topic_dismisses_and_is_rerunnable() {
        let state = OnboardingState::at(OnboardingTopic::CompositorKeybinding);
        assert_eq!(
            state.apply(OnboardingAction::Finish),
            OnboardingOutcome::Dismissed { rerunnable: true }
        );
    }

    #[test]
    fn skip_dismisses_from_any_topic() {
        for topic in OnboardingTopic::all() {
            let state = OnboardingState::at(topic);
            assert_eq!(
                state.apply(OnboardingAction::Skip),
                OnboardingOutcome::Dismissed { rerunnable: true },
                "topic {topic:?} must be skippable"
            );
        }
    }

    #[test]
    fn back_moves_to_the_previous_topic() {
        let state = OnboardingState::at(OnboardingTopic::WhereCapturesGo);
        match state.apply(OnboardingAction::Back) {
            OnboardingOutcome::Continue(prev) => {
                assert_eq!(prev.topic(), OnboardingTopic::CaptureHotkey);
            }
            OnboardingOutcome::Dismissed { .. } => panic!("Back must not dismiss"),
        }
    }

    #[test]
    fn back_on_the_first_topic_is_a_no_op() {
        let state = OnboardingState::start();
        match state.apply(OnboardingAction::Back) {
            OnboardingOutcome::Continue(same) => assert_eq!(same, state),
            OnboardingOutcome::Dismissed { .. } => panic!("Back must not dismiss"),
        }
    }

    #[test]
    fn only_the_compositor_keybinding_topic_is_platform_specific() {
        for topic in OnboardingTopic::all() {
            assert_eq!(
                topic.platform_specific(),
                topic == OnboardingTopic::CompositorKeybinding
            );
        }
    }

    #[test]
    fn step_count_is_exactly_four() {
        assert_eq!(OnboardingState::step_count(), 4);
        assert_eq!(OnboardingTopic::all().len(), 4);
    }

    #[test]
    fn is_last_is_true_only_for_the_fourth_topic() {
        for topic in OnboardingTopic::all() {
            let state = OnboardingState::at(topic);
            assert_eq!(
                state.is_last(),
                topic == OnboardingTopic::CompositorKeybinding
            );
        }
    }
}
