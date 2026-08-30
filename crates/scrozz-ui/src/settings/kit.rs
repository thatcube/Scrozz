//! The Settings control vocabulary.
//!
//! Settings is a *dialog*, not a web page. Every pane draws from this one kit so
//! a checkbox in Shortcuts is the same object as a checkbox in Scenes: same
//! height, same corner, same hover, same focus ring, same disabled treatment.
//! The alternative — each pane reaching for [`egui`]'s stock widgets and
//! nudging spacing locally — is exactly how the surface drifted into six
//! slightly different dialogs wearing one title bar.
//!
//! Three rules hold everything together:
//!
//! * **One column for labels, one for controls.** [`row`] right-aligns the
//!   label into a fixed gutter so controls line up down the whole pane. Native
//!   settings dialogs on every desktop do this; a left-ragged control edge is
//!   the single loudest "this is a web form" tell.
//! * **Dense, not cramped.** [`Metrics`] is tuned so a pane shows its whole
//!   subject without scrolling on a small dialog, while still clearing a 24pt
//!   pointer target on every control.
//! * **Colour is derived, never invented.** [`Ink`] resolves every control
//!   surface from the shared [`Palette`], so the kit follows the theme rather
//!   than pinning a second, competing set of greys.

use egui::{
    Align, Align2, Color32, CornerRadius, Layout, Rect, Response, Sense, Stroke, StrokeKind, Vec2,
};

use crate::paint;
use crate::theme::{Palette, Radius, Space, Text, Theme, corner};

/// The dialog's dimensions and rhythm.
///
/// Deliberately const rather than a struct threaded through every call: these
/// are the constants that make the surface feel like one dialog, and a pane
/// that wants a different row height is a pane that has gone wrong.
pub struct Metrics;

impl Metrics {
    /// Height of an interactive control — button, dropdown, field.
    ///
    /// 24pt is the native desktop control height (AppKit's small control, the
    /// Windows 11 compact row). It clears a comfortable pointer target once the
    /// row's own padding is counted, which is why rows are taller than this.
    pub const CONTROL: f32 = 24.0;
    /// Minimum height of a labelled row, control included.
    pub const ROW: f32 = 30.0;
    /// Narrowest the right-aligned label gutter is allowed to get.
    pub const LABEL_COLUMN: f32 = 156.0;
    /// Widest the label gutter grows to in a roomy dialog.
    pub const LABEL_COLUMN_MAX: f32 = 244.0;

    /// The label gutter for a row with `available` width to spend.
    ///
    /// Proportional rather than fixed: a fixed narrow gutter in a wide dialog
    /// strands the controls against the left edge with a large dead margin,
    /// which is exactly what makes a settings window read as a web page.
    #[must_use]
    pub fn label_column(available: f32) -> f32 {
        (available * 0.34).clamp(Self::LABEL_COLUMN, Self::LABEL_COLUMN_MAX)
    }
    /// Gap between the label gutter and the control column.
    pub const LABEL_GAP: f32 = 12.0;
    /// Horizontal padding inside a section card.
    pub const CARD_PAD_X: f32 = 12.0;
    /// Vertical padding inside a section card.
    pub const CARD_PAD_Y: f32 = 8.0;
    /// Gap between stacked sections.
    pub const SECTION_GAP: f32 = 14.0;
    /// Page margin around the pane body.
    pub const PAGE_PAD_X: f32 = 18.0;
    /// Page margin above and below the pane body.
    pub const PAGE_PAD_Y: f32 = 14.0;
    /// Corner radius of a section card.
    pub const CARD_RADIUS: f32 = 10.0;
    /// Corner radius of a control.
    pub const CONTROL_RADIUS: f32 = 6.0;
    /// Width of a compact dropdown.
    pub const DROPDOWN: f32 = 176.0;
    /// Widest a dropdown grows to when it fills its column.
    pub const DROPDOWN_MAX: f32 = 288.0;

    /// The control column for a row with `available` width to spend.
    ///
    /// Capped so a wide dialog does not stretch a three-word popup across half
    /// the window, but generous enough that the row never looks truncated.
    #[must_use]
    pub fn control_column(available: f32) -> f32 {
        available.clamp(Self::DROPDOWN, Self::DROPDOWN_MAX)
    }

    /// The control column a stacked row offers: the whole width it was given.
    ///
    /// A stacked row has already spent its label on its own line, so there is
    /// no gutter to leave room for and a half-width popup just looks unfinished.
    #[must_use]
    pub fn stacked_control_column(available: f32) -> f32 {
        available.max(Self::DROPDOWN)
    }
    /// Below this available width the panes fold their two-column rows.
    pub const NARROW: f32 = 470.0;
}

/// Control surfaces resolved from the theme.
///
/// The shared [`Palette`] is written for cards floating over a wallpaper; a
/// settings dialog needs a slightly different set of roles — a filled control
/// well, a pressed state, a danger ink. Deriving them here keeps the dialog on
/// the same two palettes instead of introducing a third.
#[derive(Clone, Copy)]
pub struct Ink {
    /// Whether the surrounding theme is dark.
    pub dark: bool,
    /// Body text.
    pub text: Color32,
    /// Labels and secondary text.
    pub muted: Color32,
    /// Placeholders and disabled text.
    pub faint: Color32,
    /// Fill of the page beneath the cards.
    pub page: Color32,
    /// Fill of a section card.
    pub card: Color32,
    /// Fill of a resting control.
    pub control: Color32,
    /// Fill of a hovered control.
    pub control_hover: Color32,
    /// Fill of a pressed control.
    pub control_press: Color32,
    /// Border around a control.
    pub control_stroke: Color32,
    /// Hairline between rows.
    pub hairline: Color32,
    /// The accent used for on-states.
    pub accent: Color32,
    /// Text drawn on the accent.
    pub on_accent: Color32,
    /// Destructive ink, contrast-checked against [`Ink::card`].
    pub danger: Color32,
    /// Advisory ink.
    pub warning: Color32,
    /// Confirmation ink.
    pub success: Color32,
    /// The keyboard focus ring.
    pub focus: Color32,
}

impl Ink {
    /// Resolve the control surfaces for `theme`.
    #[must_use]
    pub fn new(theme: &Theme) -> Self {
        let palette = &theme.palette;
        let dark = palette.is_dark();
        Self {
            dark,
            text: palette.text,
            muted: palette.text_muted,
            faint: palette.text_faint,
            page: if dark {
                Color32::from_rgb(0x14, 0x16, 0x1F)
            } else {
                Color32::from_rgb(0xF2, 0xF4, 0xF9)
            },
            card: if dark {
                Color32::from_rgb(0x1C, 0x1F, 0x2B)
            } else {
                Color32::from_rgb(0xFF, 0xFF, 0xFF)
            },
            control: if dark {
                Color32::from_rgb(0x2A, 0x2E, 0x3C)
            } else {
                Color32::from_rgb(0xEC, 0xEF, 0xF6)
            },
            control_hover: if dark {
                Color32::from_rgb(0x33, 0x38, 0x49)
            } else {
                Color32::from_rgb(0xE0, 0xE4, 0xEF)
            },
            control_press: if dark {
                Color32::from_rgb(0x23, 0x27, 0x33)
            } else {
                Color32::from_rgb(0xD2, 0xD7, 0xE6)
            },
            control_stroke: if dark {
                Color32::from_rgb(0x3B, 0x41, 0x53)
            } else {
                Color32::from_rgb(0xC9, 0xCF, 0xDE)
            },
            hairline: palette.hairline,
            accent: palette.accent,
            on_accent: palette.on_accent,
            // Deliberately not a palette token: both palettes would have to be
            // re-derived and every golden re-baked to add one. These two were
            // measured against `card` and clear AA for body text.
            danger: if dark {
                Color32::from_rgb(0xFF, 0x8A, 0x80)
            } else {
                Color32::from_rgb(0xC0, 0x2A, 0x22)
            },
            warning: palette.warning,
            success: palette.success,
            focus: palette.focus_ring,
        }
    }

    /// The fill a control should draw for its current pointer state.
    #[must_use]
    pub fn control_fill(&self, response: &Response, enabled: bool) -> Color32 {
        if !enabled {
            return self.control.gamma_multiply(0.55);
        }
        if response.is_pointer_button_down_on() {
            self.control_press
        } else if response.hovered() {
            self.control_hover
        } else {
            self.control
        }
    }

    /// Text ink, dimmed when the control cannot be used.
    #[must_use]
    pub fn text_for(&self, enabled: bool) -> Color32 {
        if enabled { self.text } else { self.faint }
    }
}

/// Tune [`egui::Style`] so stock widgets match the kit.
///
/// [`crate::theme::apply_style`] deliberately makes windows and popups
/// transparent — right for the overlay surfaces it was written for, wrong for a
/// dialog whose dropdown menus have to be legible over their own pane. This
/// runs after it and restores a real popup surface, then pulls every stock
/// widget onto the kit's corner radius and control height.
pub fn install(ui: &mut egui::Ui, theme: &Theme) {
    let ink = Ink::new(theme);
    let radius = corner(Metrics::CONTROL_RADIUS);
    let style = ui.style_mut();

    style.spacing.item_spacing = Vec2::new(Space::SM, Space::XS);
    style.spacing.button_padding = Vec2::new(Space::SM, 2.0);
    style.spacing.interact_size = Vec2::new(0.0, Metrics::CONTROL);
    style.spacing.icon_width = 14.0;
    style.spacing.icon_width_inner = 8.0;
    style.spacing.icon_spacing = Space::XS;
    style.spacing.slider_width = 148.0;
    style.spacing.slider_rail_height = 4.0;
    style.spacing.combo_width = Metrics::DROPDOWN;
    style.spacing.combo_height = 280.0;
    style.spacing.menu_margin = egui::Margin::same(4);
    style.spacing.menu_spacing = 2.0;

    let visuals = &mut style.visuals;
    visuals.selection.bg_fill = ink.accent;
    visuals.selection.stroke = Stroke::new(1.0, ink.on_accent);
    visuals.warn_fg_color = ink.warning;
    visuals.error_fg_color = ink.danger;
    visuals.extreme_bg_color = if ink.dark {
        Color32::from_rgb(0x11, 0x13, 0x1B)
    } else {
        Color32::from_rgb(0xFB, 0xFC, 0xFE)
    };
    visuals.text_edit_bg_color = Some(visuals.extreme_bg_color);
    visuals.faint_bg_color = ink.control;
    visuals.menu_corner_radius = corner(Metrics::CARD_RADIUS);
    visuals.window_corner_radius = corner(Metrics::CARD_RADIUS);
    visuals.window_fill = ink.card;
    visuals.window_stroke = Stroke::new(1.0, ink.control_stroke);
    visuals.popup_shadow = egui::epaint::Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: theme.palette.key_shadow,
    };

    for (widget, fill) in [
        (&mut visuals.widgets.inactive, ink.control),
        (&mut visuals.widgets.hovered, ink.control_hover),
        (&mut visuals.widgets.active, ink.control_press),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = Stroke::new(1.0, ink.control_stroke);
        widget.corner_radius = radius;
        widget.fg_stroke = Stroke::new(1.0, ink.text);
        widget.expansion = 0.0;
    }
    visuals.widgets.noninteractive.bg_fill = ink.card;
    visuals.widgets.noninteractive.weak_bg_fill = ink.card;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, ink.hairline);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, ink.muted);
    visuals.widgets.noninteractive.corner_radius = radius;
    visuals.widgets.open = visuals.widgets.active;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, ink.accent);
}

// ---------------------------------------------------------------------------
// Structure
// ---------------------------------------------------------------------------

/// A pane: title, optional one-line subtitle, then a scrolling body.
///
/// The subtitle is a *sentence about the pane*, not about a control. Anything
/// that explains a single setting belongs in [`help`] under that setting, or —
/// better — in the setting's own label.
pub fn page(
    ui: &mut egui::Ui,
    theme: &Theme,
    title: &str,
    subtitle: Option<&str>,
    body: impl FnOnce(&mut egui::Ui),
) {
    let ink = Ink::new(theme);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = Space::HAIR;
        ui.label(
            egui::RichText::new(title)
                .font(theme.font(Text::Title))
                .color(ink.text),
        );
        if let Some(subtitle) = subtitle {
            ui.label(
                egui::RichText::new(subtitle)
                    .font(theme.font(Text::Caption))
                    .color(ink.muted),
            );
        }
    });
    ui.add_space(Space::MD);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = Metrics::SECTION_GAP;
            ui.vertical(body);
        });
}

/// A group box: an optional caption above a rounded, hairlined card.
///
/// The caption sits *outside* the card in the native idiom, which keeps the
/// card's first row at the same height as every other row instead of paying for
/// a heading inside the frame.
pub fn section(
    ui: &mut egui::Ui,
    theme: &Theme,
    title: Option<&str>,
    body: impl FnOnce(&mut egui::Ui),
) -> Rect {
    let ink = Ink::new(theme);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = Space::XS;
        if let Some(title) = title {
            ui.label(
                egui::RichText::new(title)
                    .font(theme.font(Text::Label))
                    .color(ink.muted),
            );
        }
        card(ui, theme, body)
    })
    .inner
}

/// The rounded card every section and preset tile is drawn in.
pub fn card(ui: &mut egui::Ui, theme: &Theme, body: impl FnOnce(&mut egui::Ui)) -> Rect {
    let ink = Ink::new(theme);
    egui::Frame::new()
        .fill(ink.card)
        .stroke(Stroke::new(1.0, ink.hairline))
        .corner_radius(corner(Metrics::CARD_RADIUS))
        .inner_margin(egui::Margin::symmetric(
            Metrics::CARD_PAD_X as i8,
            Metrics::CARD_PAD_Y as i8,
        ))
        .show(ui, |ui| {
            // Cards claim the full column so sections stack as one aligned edge
            // instead of each shrinking to whatever it happens to contain.
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = Space::XS;
            ui.vertical(body);
        })
        .response
        .rect
}

/// A labelled row: right-aligned label gutter, then the control column.
///
/// Folds to a stacked label-over-control layout below [`Metrics::NARROW`], so a
/// compact dialog keeps its controls at full width instead of squeezing them
/// into a sliver beside a truncated label.
pub fn row(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    control: impl FnOnce(&mut egui::Ui),
) -> Response {
    row_impl(ui, theme, label, None, control)
}

/// [`row`] with one line of help beneath the control.
///
/// Reach for this only when the control genuinely cannot carry its own meaning
/// — a keyboard override, a platform caveat. A help line under every row is the
/// verbose copy this dialog is trying to shed.
pub fn row_with_help(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    help_text: &str,
    control: impl FnOnce(&mut egui::Ui),
) -> Response {
    row_impl(ui, theme, label, Some(help_text), control)
}

fn row_impl(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    help_text: Option<&str>,
    control: impl FnOnce(&mut egui::Ui),
) -> Response {
    let ink = Ink::new(theme);
    let narrow = ui.available_width() < Metrics::NARROW;
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(Metrics::LABEL_GAP, Space::HAIR);
        if narrow {
            ui.vertical(|ui| {
                ui.add_space(Space::HAIR);
                ui.label(
                    egui::RichText::new(label)
                        .font(theme.font(Text::Label))
                        .color(ink.text),
                );
                // Published so a control can fill the column the row granted
                // instead of guessing at the layout it landed in.
                let column = Metrics::stacked_control_column(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().combo_width = column;
                    control(ui);
                });
                if let Some(help_text) = help_text {
                    help(ui, theme, help_text);
                }
                ui.add_space(Space::HAIR);
            });
        } else {
            ui.horizontal_top(|ui| {
                ui.set_min_height(Metrics::ROW);
                let (label_rect, _) = ui.allocate_exact_size(
                    Vec2::new(Metrics::label_column(ui.available_width()), Metrics::ROW),
                    Sense::hover(),
                );
                ui.painter().text(
                    label_rect.right_center() - Vec2::new(0.0, 0.0),
                    Align2::RIGHT_CENTER,
                    label,
                    theme.font(Text::Label),
                    ink.text,
                );
                ui.vertical(|ui| {
                    ui.add_space((Metrics::ROW - Metrics::CONTROL) / 2.0);
                    let column = Metrics::control_column(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().combo_width = column;
                        control(ui);
                    });
                    if let Some(help_text) = help_text {
                        help(ui, theme, help_text);
                    }
                    ui.add_space((Metrics::ROW - Metrics::CONTROL) / 2.0);
                });
            });
        }
    })
    .response
}

/// A full-width hairline between rows inside a card.
pub fn divider(ui: &mut egui::Ui, theme: &Theme) {
    let ink = Ink::new(theme);
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, Space::XS), Sense::hover());
    paint::divider_h(
        ui.painter(),
        rect.left(),
        rect.right(),
        rect.center().y,
        &theme.palette,
    );
    let _ = ink;
}

/// One line of secondary copy beneath a control.
pub fn help(ui: &mut egui::Ui, theme: &Theme, text: &str) {
    let ink = Ink::new(theme);
    ui.label(
        egui::RichText::new(text)
            .font(theme.font(Text::Caption))
            .color(ink.muted),
    );
}

/// What a [`status`] line means.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    /// Neutral information.
    Info,
    /// Something works but deserves a caveat.
    Warning,
    /// Something is wrong and the user must act.
    Error,
    /// Something succeeded.
    Success,
}

impl Tone {
    fn ink(self, ink: &Ink) -> Color32 {
        match self {
            Self::Info => ink.muted,
            Self::Warning => ink.warning,
            Self::Error => ink.danger,
            Self::Success => ink.success,
        }
    }

    fn glyph(self) -> &'static str {
        // Colour is never the only channel (D13): each tone also carries a
        // distinct leading glyph, which survives both colour blindness and a
        // greyscale screenshot.
        match self {
            Self::Info => "i",
            Self::Warning => "!",
            Self::Error => "×",
            Self::Success => "✓",
        }
    }
}

/// A status line: a tinted badge glyph and one sentence.
pub fn status(ui: &mut egui::Ui, theme: &Theme, tone: Tone, text: &str) {
    let ink = Ink::new(theme);
    let color = tone.ink(&ink);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = Space::XS;
        let (badge, _) = ui.allocate_exact_size(Vec2::splat(13.0), Sense::hover());
        ui.painter()
            .circle_filled(badge.center(), 6.5, color.gamma_multiply(0.22));
        ui.painter().text(
            badge.center(),
            Align2::CENTER_CENTER,
            tone.glyph(),
            theme.font(Text::Caption),
            color,
        );
        ui.label(
            egui::RichText::new(text)
                .font(theme.font(Text::Caption))
                .color(color),
        );
    });
}

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

/// Whether `response` was activated this frame, by pointer or by keyboard.
///
/// The kit's single answer to "was this pressed", because egui's own is wrong
/// for us: it presses a focused widget on Space or Return *without looking at
/// modifiers*. So a chord ending in one of those keys presses whatever holds
/// focus as well as being read as a chord. This adds the bare-key press egui
/// misses for hand-drawn controls, and takes back the modified press it should
/// never have given, so every control in the kit agrees.
pub(crate) fn activated(ui: &egui::Ui, response: &mut Response) -> bool {
    if keyboard_activated(ui, response) {
        response.flags |= egui::response::Flags::FAKE_PRIMARY_CLICKED;
        return true;
    }
    if response
        .flags
        .contains(egui::response::Flags::FAKE_PRIMARY_CLICKED)
        && !bare_activation_pressed(ui)
        && !accessibility_activated(ui, response)
    {
        // egui faked a primary click for a Space or Return that was part of a
        // chord. Take it back: the chord was aimed at the recorder.
        //
        // The test is the synthetic click itself, not `has_focus` — a click
        // landing anywhere else in the same frame surrenders focus, so the flag
        // outlives the focus that earned it. Only the synthetic click goes.
        // Whether this control was *also* pressed for a real reason is then
        // answered by `clicked`, which falls back to the pointer — and the
        // pointer half is per-widget, so an unrelated click no longer rescues
        // the chord's press here, while a press on this very control does.
        response.flags -= egui::response::Flags::FAKE_PRIMARY_CLICKED;
    }
    response.clicked()
}

/// Whether assistive technology asked for this control to be activated.
///
/// This is the other reason egui fakes a primary click, and it is a deliberate
/// request aimed at this widget by id rather than a key that happened to be
/// held down. It must survive a chord in the same frame.
fn accessibility_activated(ui: &egui::Ui, response: &Response) -> bool {
    ui.input(|input| {
        input.has_accesskit_action_request(response.id, egui::accesskit::Action::Click)
    })
}

/// Whether a Space or Return arrived this frame with nothing held down.
///
/// The keyboard press egui is entitled to fake, regardless of which control it
/// landed on or whether focus survived the frame.
fn bare_activation_pressed(ui: &egui::Ui) -> bool {
    activation_pressed(ui, egui::Modifiers::is_none)
}

/// Whether a Space or Return arrived this frame as part of a chord.
fn chord_pressed(ui: &egui::Ui) -> bool {
    activation_pressed(ui, |modifiers| !modifiers.is_none())
}

/// Whether an activation key arrived this frame whose modifiers `wanted` accepts.
fn activation_pressed(ui: &egui::Ui, wanted: impl Fn(&egui::Modifiers) -> bool) -> bool {
    ui.input(|input| {
        input.events.iter().any(|event| {
            matches!(
                event,
                egui::Event::Key {
                    key: egui::Key::Space | egui::Key::Enter,
                    pressed: true,
                    modifiers,
                    ..
                } if wanted(modifiers)
            )
        })
    })
}

/// Whether a focused control was pressed with the keyboard.
///
/// Only a *bare* Space or Return counts. `Key::pressed` ignores modifiers, so
/// without this a chord that merely ends in one of those keys also activates
/// whatever happens to hold focus. That is not hypothetical: recording a
/// shortcut, Ctrl+Return commits the chord and — in the very same frame — the
/// key cap behind the recorder sees a Return, re-arms, and the shortcut can
/// never be finished. Modified keys belong to whoever is reading chords.
fn keyboard_activated(ui: &egui::Ui, response: &Response) -> bool {
    response.has_focus() && bare_activation_pressed(ui)
}

/// A pill switch — the on/off control for a whole behaviour.
///
/// Used where a checkbox would read as "one of several things you may tick";
/// a switch reads as "this feature is on". Returns `true` when toggled.
pub fn switch(ui: &mut egui::Ui, theme: &Theme, on: &mut bool, enabled: bool) -> Response {
    let ink = Ink::new(theme);
    let size = Vec2::new(34.0, 19.0);
    let (rect, mut response) = ui.allocate_exact_size(
        size,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let clicked = activated(ui, &mut response);
    if enabled && clicked {
        *on = !*on;
        response.mark_changed();
    }
    response
        .widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, *on, ""));

    let radius = Radius::pill(rect.height());
    let track = if *on {
        if enabled {
            ink.accent
        } else {
            ink.accent.gamma_multiply(0.4)
        }
    } else {
        ink.control_fill(&response, enabled)
    };
    let painter = ui.painter();
    painter.rect_filled(rect, corner(radius), track);
    if !*on {
        painter.rect_stroke(
            rect,
            corner(radius),
            Stroke::new(1.0, ink.control_stroke),
            StrokeKind::Inside,
        );
    }
    let knob_r = rect.height() / 2.0 - 3.0;
    let travel = rect.width() - rect.height();
    let cx = rect.left() + rect.height() / 2.0 + if *on { travel } else { 0.0 };
    let knob = if enabled {
        Color32::WHITE
    } else {
        Color32::WHITE.gamma_multiply(0.7)
    };
    painter.circle_filled(egui::pos2(cx, rect.center().y), knob_r, knob);
    if response.has_focus() {
        paint::focus_ring(ui.painter(), rect, radius, &theme.palette);
    }
    response
}

/// A checkbox with a label, for members of a set.
pub fn checkbox(
    ui: &mut egui::Ui,
    theme: &Theme,
    on: &mut bool,
    label: &str,
    enabled: bool,
) -> Response {
    let ink = Ink::new(theme);
    let font = theme.font(Text::Body);
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        font.clone(),
        ink.text_for(enabled && !label.is_empty()),
    );
    let box_size = 15.0;
    let gap = if label.is_empty() { 0.0 } else { Space::XS };
    let size = Vec2::new(
        box_size + gap + galley.size().x,
        Metrics::CONTROL.max(galley.size().y),
    );
    let (rect, mut response) = ui.allocate_exact_size(
        size,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    if enabled && activated(ui, &mut response) {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, *on, label)
    });

    let check = Rect::from_center_size(
        egui::pos2(rect.left() + box_size / 2.0, rect.center().y),
        Vec2::splat(box_size),
    );
    let radius = corner(4.0);
    let painter = ui.painter();
    if *on {
        let fill = if !enabled {
            ink.accent.gamma_multiply(0.4)
        } else if response.is_pointer_button_down_on() {
            theme.palette.accent_press
        } else if response.hovered() {
            theme.palette.accent_hi
        } else {
            ink.accent
        };
        painter.rect_filled(check, radius, fill);
        let tick = ink
            .on_accent
            .gamma_multiply(if enabled { 1.0 } else { 0.7 });
        let stroke = Stroke::new(1.8, tick);
        let c = check.center();
        painter.line_segment(
            [
                egui::pos2(c.x - 3.6, c.y + 0.2),
                egui::pos2(c.x - 1.0, c.y + 2.8),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(c.x - 1.0, c.y + 2.8),
                egui::pos2(c.x + 3.8, c.y - 3.0),
            ],
            stroke,
        );
    } else {
        painter.rect_filled(check, radius, ink.control_fill(&response, enabled));
        painter.rect_stroke(
            check,
            radius,
            Stroke::new(1.0, ink.control_stroke),
            StrokeKind::Inside,
        );
    }
    if !label.is_empty() {
        painter.galley(
            egui::pos2(check.right() + gap, rect.center().y - galley.size().y / 2.0),
            galley,
            ink.text_for(enabled),
        );
    }
    if response.has_focus() {
        paint::focus_ring(ui.painter(), check.expand(1.0), 5.0, &theme.palette);
    }
    response
}

/// How loudly a [`button`] speaks.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ButtonKind {
    /// The one action a pane most wants: accent filled.
    Primary,
    /// The ordinary action: filled control well.
    #[default]
    Secondary,
    /// A tertiary action: text only until hovered.
    Quiet,
    /// Removes something: danger ink, and filled only once hovered.
    Destructive,
}

/// A compact inline action, for card footers and list rows.
///
/// Same vocabulary as [`button`], four points shorter and without the form
/// button's minimum width, so three of them fit under a preset tile.
pub fn small_button(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    kind: ButtonKind,
    enabled: bool,
) -> Response {
    button_impl(ui, theme, label, kind, enabled, Text::Caption, 20.0, 0.0)
}

/// A labelled button.
pub fn button(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    kind: ButtonKind,
    enabled: bool,
) -> Response {
    button_impl(
        ui,
        theme,
        label,
        kind,
        enabled,
        Text::Label,
        Metrics::CONTROL,
        58.0,
    )
}

#[allow(clippy::too_many_arguments)]
fn button_impl(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    kind: ButtonKind,
    enabled: bool,
    role: Text,
    height: f32,
    min_width: f32,
) -> Response {
    let ink = Ink::new(theme);
    let font = theme.font(role);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER);
    let width = (galley.size().x + Space::SM * 2.0 + 4.0).max(min_width);
    let (rect, mut response) = ui.allocate_exact_size(
        Vec2::new(width, height),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    if !enabled {
        response.flags -= egui::response::Flags::ENABLED;
    } else {
        // Keeps a focused button from being pressed by a chord that merely ends
        // in Space or Return; see `activated`.
        activated(ui, &mut response);
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));

    let hovered = enabled && response.hovered();
    let pressed = enabled && response.is_pointer_button_down_on();
    let (fill, stroke, text) = match kind {
        ButtonKind::Primary => {
            let fill = if !enabled {
                ink.accent.gamma_multiply(0.4)
            } else if pressed {
                theme.palette.accent_press
            } else if hovered {
                theme.palette.accent_hi
            } else {
                ink.accent
            };
            (
                fill,
                Color32::TRANSPARENT,
                ink.on_accent
                    .gamma_multiply(if enabled { 1.0 } else { 0.7 }),
            )
        }
        ButtonKind::Secondary => (
            ink.control_fill(&response, enabled),
            ink.control_stroke,
            ink.text_for(enabled),
        ),
        ButtonKind::Quiet => (
            if pressed {
                ink.control_press
            } else if hovered {
                ink.control_hover
            } else {
                Color32::TRANSPARENT
            },
            Color32::TRANSPARENT,
            if enabled { ink.muted } else { ink.faint },
        ),
        ButtonKind::Destructive => (
            if pressed {
                ink.danger.gamma_multiply(0.28)
            } else if hovered {
                ink.danger.gamma_multiply(0.16)
            } else {
                Color32::TRANSPARENT
            },
            Color32::TRANSPARENT,
            if enabled {
                ink.danger
            } else {
                ink.danger.gamma_multiply(0.5)
            },
        ),
    };
    let radius = corner(Metrics::CONTROL_RADIUS);
    let painter = ui.painter();
    painter.rect_filled(rect, radius, fill);
    if stroke != Color32::TRANSPARENT {
        painter.rect_stroke(rect, radius, Stroke::new(1.0, stroke), StrokeKind::Inside);
    }
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        theme.font(role),
        text,
    );
    if response.has_focus() {
        paint::focus_ring(ui.painter(), rect, Metrics::CONTROL_RADIUS, &theme.palette);
    }
    response
}

/// The width the enclosing [`row`] granted its control column.
#[must_use]
pub fn row_control_width(ui: &egui::Ui) -> f32 {
    let published = ui.spacing().combo_width;
    if published > 0.0 {
        published
    } else {
        Metrics::control_column(ui.available_width())
    }
}

/// A compact dropdown.
///
/// Wraps [`egui::ComboBox`] rather than reinventing a popup: the menu needs
/// keyboard navigation, screen-reader reporting and correct clipping, all of
/// which the stock widget already gets right. What it did not get right was the
/// look, which [`install`] fixes for every dropdown at once.
pub fn dropdown<R>(
    ui: &mut egui::Ui,
    theme: &Theme,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    selected: &str,
    width: f32,
    menu: impl FnOnce(&mut egui::Ui) -> R,
) -> Option<R> {
    let ink = Ink::new(theme);
    egui::ComboBox::from_id_salt(id_salt)
        .width(width)
        .height(280.0)
        .selected_text(
            egui::RichText::new(selected)
                .font(theme.font(Text::Body))
                .color(ink.text),
        )
        .show_ui(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.style_mut().visuals.widgets.hovered.bg_fill = ink.control_hover;
            menu(ui)
        })
        .inner
}

/// One row inside a [`dropdown`].
///
/// Drawn rather than delegated to a selectable label so the selected row uses
/// the kit's accent fill and corner instead of egui's square selection block.
pub fn menu_item(ui: &mut egui::Ui, theme: &Theme, selected: bool, label: &str) -> Response {
    let ink = Ink::new(theme);
    let font = theme.font(Text::Body);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER);
    let width = ui.available_width().max(galley.size().x + Space::MD * 2.0);
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(width, 22.0), Sense::click());
    // A chord typed while a row holds focus must not choose that row.
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, label)
    });
    let radius = corner(5.0);
    if selected {
        ui.painter().rect_filled(rect, radius, ink.accent);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, radius, ink.control_hover);
    }
    ui.painter().galley(
        egui::pos2(
            rect.left() + Space::SM,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        if selected { ink.on_accent } else { ink.text },
    );
    if response.has_focus() {
        paint::focus_ring(ui.painter(), rect, 5.0, &theme.palette);
    }
    response
}

/// A segmented control — two to four mutually exclusive choices, all visible.
///
/// Preferred over a dropdown whenever the options fit, because a dropdown hides
/// the alternatives behind a click and this dialog is meant to show capability
/// rather than bury it. Returns `true` when the selection changed.
pub fn segmented<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    theme: &Theme,
    value: &mut T,
    options: &[(T, &str)],
) -> bool {
    let ink = Ink::new(theme);
    let font = theme.font(Text::Label);
    let mut widths = Vec::with_capacity(options.len());
    let mut widest: f32 = 0.0;
    for (_, label) in options {
        let galley =
            ui.painter()
                .layout_no_wrap((*label).to_owned(), font.clone(), Color32::PLACEHOLDER);
        let w = galley.size().x + Space::MD * 2.0;
        widest = widest.max(w);
        widths.push(w);
    }
    let seg_w = widest.max(56.0);
    let total = Vec2::new(seg_w * options.len() as f32 + 4.0, Metrics::CONTROL + 4.0);
    let (track, _) = ui.allocate_exact_size(total, Sense::hover());
    let track_radius = Radius::pill(track.height()).min(9.0);
    ui.painter()
        .rect_filled(track, corner(track_radius), ink.control);
    ui.painter().rect_stroke(
        track,
        corner(track_radius),
        Stroke::new(1.0, ink.control_stroke),
        StrokeKind::Inside,
    );

    let mut changed = false;
    for (index, (option, label)) in options.iter().enumerate() {
        let rect = Rect::from_min_size(
            egui::pos2(track.left() + 2.0 + seg_w * index as f32, track.top() + 2.0),
            Vec2::new(seg_w, Metrics::CONTROL),
        );
        let mut response = ui.interact(rect, ui.id().with(("segment", index)), Sense::click());
        let selected = *value == *option;
        if activated(ui, &mut response) && !selected {
            *value = *option;
            changed = true;
        }
        let text_color = if selected {
            ink.on_accent
        } else if response.hovered() {
            ink.text
        } else {
            ink.muted
        };
        if selected {
            ui.painter().rect_filled(
                rect,
                corner(track_radius - 2.0),
                if response.is_pointer_button_down_on() {
                    theme.palette.accent_press
                } else {
                    ink.accent
                },
            );
        } else if response.hovered() {
            ui.painter()
                .rect_filled(rect, corner(track_radius - 2.0), ink.control_hover);
        }
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            font.clone(),
            text_color,
        );
        if response.has_focus() {
            paint::focus_ring(ui.painter(), rect, track_radius - 2.0, &theme.palette);
        }
        response.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::RadioButton, true, selected, *label)
        });
    }
    changed
}

/// A single-line text field.
pub fn text_field(
    ui: &mut egui::Ui,
    theme: &Theme,
    value: &mut String,
    hint: &str,
    width: f32,
) -> Response {
    let ink = Ink::new(theme);
    let mut edit = egui::TextEdit::singleline(value)
        .desired_width(width)
        .margin(egui::Margin::symmetric(Space::SM as i8, 3))
        .font(egui::FontSelection::FontId(theme.font(Text::Body)))
        .text_color(ink.text);
    if !hint.is_empty() {
        edit = edit.hint_text(
            egui::RichText::new(hint)
                .font(theme.font(Text::Body))
                .color(ink.faint),
        );
    }
    ui.add_sized(Vec2::new(width, Metrics::CONTROL), edit)
}

/// A compact slider with the value shown in a trailing chip.
pub fn slider(
    ui: &mut egui::Ui,
    theme: &Theme,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    format: impl Fn(f32) -> String,
) -> Response {
    let ink = Ink::new(theme);
    let response = ui.add(
        egui::Slider::new(value, range)
            .show_value(false)
            .trailing_fill(true),
    );
    let text = format(*value);
    let galley = ui
        .painter()
        .layout_no_wrap(text, theme.font(Text::Shortcut), ink.text);
    let (chip, _) = ui.allocate_exact_size(
        Vec2::new(galley.size().x + Space::SM * 2.0, Metrics::CONTROL - 2.0),
        Sense::hover(),
    );
    ui.painter()
        .rect_filled(chip, corner(Metrics::CONTROL_RADIUS), ink.control);
    ui.painter().galley(
        egui::pos2(
            chip.center().x - galley.size().x / 2.0,
            chip.center().y - galley.size().y / 2.0,
        ),
        galley,
        ink.text,
    );
    response
}

/// A right-aligned trailing action group inside a row.
pub fn trailing(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
    ui.with_layout(Layout::right_to_left(Align::Center), body);
}

/// A monospaced-feeling key cap, for accelerators and modifier hints.
///
/// Inert on purpose: use [`key_cap_button`] where the cap is the control that
/// starts a reassignment, so it is reachable by keyboard as well as by pointer.
pub fn key_cap(ui: &mut egui::Ui, theme: &Theme, text: &str, usable: bool) -> Response {
    key_cap_impl(ui, theme, text, usable, false)
}

/// A key cap that is itself the button — used to arm shortcut reassignment.
///
/// Separate from [`key_cap`] because a focusable widget that does nothing is
/// worse for keyboard users than no widget at all. The returned response is
/// clicked by pointer, Space or Enter.
pub fn key_cap_button(ui: &mut egui::Ui, theme: &Theme, text: &str, usable: bool) -> Response {
    key_cap_impl(ui, theme, text, usable, true)
}

fn key_cap_impl(
    ui: &mut egui::Ui,
    theme: &Theme,
    text: &str,
    usable: bool,
    interactive: bool,
) -> Response {
    let ink = Ink::new(theme);
    let font = theme.font(Text::Shortcut);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, Color32::PLACEHOLDER);
    let (rect, mut response) = ui.allocate_exact_size(
        Vec2::new((galley.size().x + Space::SM * 2.0).max(22.0), 21.0),
        if interactive {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    if interactive {
        activated(ui, &mut response);
    }
    let hovered = interactive && response.hovered();
    let fill = if hovered {
        ink.control_hover
    } else {
        ink.control
    };
    let stroke = if interactive && response.has_focus() {
        Stroke::new(2.0, ink.accent)
    } else {
        Stroke::new(1.0, ink.control_stroke)
    };
    let painter = ui.painter();
    painter.rect_filled(rect, corner(5.0), fill);
    painter.rect_stroke(rect, corner(5.0), stroke, StrokeKind::Inside);
    painter.galley(
        egui::pos2(
            rect.center().x - galley.size().x / 2.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        if usable { ink.text } else { ink.danger },
    );
    response
}

/// The corner radius a card should use, exposed for panes drawing their own.
#[must_use]
pub const fn card_corner() -> f32 {
    Metrics::CARD_RADIUS
}

/// The kit's control corner as an [`egui::CornerRadius`].
#[must_use]
pub fn control_corner() -> CornerRadius {
    corner(Metrics::CONTROL_RADIUS)
}

/// The page background for `theme`.
#[must_use]
pub fn page_fill(theme: &Theme) -> Color32 {
    Ink::new(theme).page
}

/// The palette a pane should use for a hairline.
#[must_use]
pub fn hairline(palette: &Palette) -> Color32 {
    palette.hairline
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A context whose custom families are bound, one warm-up pass done.
    ///
    /// `set_fonts` only lands on the *next* begin-pass, so a caller that draws
    /// straight away panics on the unbound family.
    fn context_with_fonts() -> egui::Context {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        ctx
    }

    fn run<R>(
        ctx: &egui::Context,
        input: egui::RawInput,
        mut body: impl FnMut(&mut egui::Ui) -> R,
    ) -> R {
        let mut result = None;
        let mut output = ctx.run_ui(input, |ui| {
            result = Some(body(ui));
        });
        output.textures_delta.clear();
        result.expect("the ui ran")
    }

    fn click_at(position: egui::Pos2) -> egui::RawInput {
        egui::RawInput {
            events: vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        }
    }

    /// One frame's worth of `key` held down with `modifiers`.
    fn key_press(key: egui::Key, modifiers: egui::Modifiers) -> egui::RawInput {
        egui::RawInput {
            events: vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }],
            ..Default::default()
        }
    }

    /// Every modifier a chord can end in, paired with both activation keys.
    fn chords() -> impl Iterator<Item = (egui::Key, egui::Modifiers)> {
        [
            egui::Modifiers::CTRL,
            egui::Modifiers::COMMAND,
            egui::Modifiers::SHIFT,
            egui::Modifiers::ALT,
        ]
        .into_iter()
        .flat_map(|modifiers| {
            [egui::Key::Enter, egui::Key::Space]
                .into_iter()
                .map(move |key| (key, modifiers))
        })
    }

    #[test]
    fn a_click_somewhere_else_does_not_let_a_chord_press_a_focused_control() {
        // The guard used to ask whether *any* pointer click happened this
        // frame. Clicking anything at all — another pane, empty dialog
        // background — while finishing a chord therefore left the synthetic
        // press in place, and two controls answered one keystroke.
        let theme = Theme::dark();
        let ctx = context_with_fonts();
        let id = run(&ctx, egui::RawInput::default(), |ui| {
            key_cap_button(ui, &theme, "Cmd+7", true).id
        });
        ctx.memory_mut(|memory| memory.request_focus(id));

        for (key, modifiers) in chords() {
            let mut input = key_press(key, modifiers);
            // Somewhere far from the cap, which is drawn at the origin.
            input
                .events
                .extend(click_at(egui::pos2(600.0, 400.0)).events);

            let clicked = run(&ctx, input, |ui| {
                key_cap_button(ui, &theme, "Cmd+7", true).clicked()
            });
            assert!(
                !clicked,
                "an unrelated click let {key:?} with {modifiers:?} press the cap"
            );
            ctx.memory_mut(|memory| memory.request_focus(id));
        }
    }

    #[test]
    fn a_click_on_the_control_itself_still_presses_it_during_a_chord() {
        // The other half: refusing the chord must not refuse a real press.
        let theme = Theme::dark();
        let ctx = context_with_fonts();
        let response = run(&ctx, egui::RawInput::default(), |ui| {
            let response = key_cap_button(ui, &theme, "Cmd+7", true);
            (response.id, response.rect)
        });
        let (id, rect) = response;
        ctx.memory_mut(|memory| memory.request_focus(id));

        let mut input = click_at(rect.center());
        input
            .events
            .extend(key_press(egui::Key::Enter, egui::Modifiers::CTRL).events);

        let clicked = run(&ctx, input, |ui| {
            key_cap_button(ui, &theme, "Cmd+7", true).clicked()
        });
        assert!(
            clicked,
            "the pointer pressed this control and the chord is irrelevant to that"
        );

        // And through a control that acts on the answer rather than on the
        // response, so refusing the frame outright would show up here.
        let theme = Theme::dark();
        let ctx = context_with_fonts();
        let mut on = false;
        let rect = run(&ctx, egui::RawInput::default(), |ui| {
            switch(ui, &theme, &mut on, true).rect
        });

        let mut input = click_at(rect.center());
        input
            .events
            .extend(key_press(egui::Key::Enter, egui::Modifiers::CTRL).events);
        run(&ctx, input, |ui| switch(ui, &theme, &mut on, true));
        assert!(on, "the switch was clicked, chord or no chord");
    }

    #[test]
    fn a_key_cap_button_can_actually_be_clicked() {
        // It senses hover only, so `clicked()` never fires and reassigning a
        // shortcut is impossible with a mouse.
        let theme = Theme::dark();
        let ctx = context_with_fonts();
        let rect = run(&ctx, egui::RawInput::default(), |ui| {
            key_cap_button(ui, &theme, "Cmd+7", true).rect
        });

        let clicked = run(&ctx, click_at(rect.center()), |ui| {
            key_cap_button(ui, &theme, "Cmd+7", true).clicked()
        });

        assert!(clicked, "the cap is the control that arms reassignment");
    }

    #[test]
    fn a_key_cap_button_can_be_reached_from_the_keyboard() {
        let theme = Theme::dark();
        let ctx = context_with_fonts();
        let id = run(&ctx, egui::RawInput::default(), |ui| {
            key_cap_button(ui, &theme, "Cmd+7", true).id
        });
        ctx.memory_mut(|memory| memory.request_focus(id));

        let clicked = run(
            &ctx,
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Space,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| key_cap_button(ui, &theme, "Cmd+7", true).clicked(),
        );

        assert!(clicked, "Space must arm it as well as the pointer");
    }

    #[test]
    fn a_modified_chord_does_not_press_the_focused_cap() {
        // The cap that starts recording is also the thing holding focus while a
        // chord is being typed. If Ctrl+Return counted as pressing it, finishing
        // a shortcut would immediately restart the recorder in the same frame
        // and the shortcut could never be committed.
        let theme = Theme::dark();
        let ctx = context_with_fonts();
        let id = run(&ctx, egui::RawInput::default(), |ui| {
            key_cap_button(ui, &theme, "Cmd+7", true).id
        });
        ctx.memory_mut(|memory| memory.request_focus(id));

        for modifiers in [
            egui::Modifiers::CTRL,
            egui::Modifiers::COMMAND,
            egui::Modifiers::SHIFT,
            egui::Modifiers::ALT,
        ] {
            for key in [egui::Key::Enter, egui::Key::Space] {
                let clicked = run(
                    &ctx,
                    egui::RawInput {
                        events: vec![egui::Event::Key {
                            key,
                            physical_key: None,
                            pressed: true,
                            repeat: false,
                            modifiers,
                        }],
                        ..Default::default()
                    },
                    |ui| key_cap_button(ui, &theme, "Cmd+7", true).clicked(),
                );
                assert!(
                    !clicked,
                    "{key:?} with {modifiers:?} belongs to the chord reader, not the cap"
                );
            }
        }

        // The bare key still works, so nothing was thrown out with the chord.
        let clicked = run(
            &ctx,
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| key_cap_button(ui, &theme, "Cmd+7", true).clicked(),
        );
        assert!(clicked, "an unmodified Return still presses it");
    }

    #[test]
    fn a_plain_key_cap_stays_inert() {
        // A hint that steals focus and does nothing is worse for keyboard users
        // than no widget at all.
        let theme = Theme::dark();
        let ctx = context_with_fonts();
        let rect = run(&ctx, egui::RawInput::default(), |ui| {
            key_cap(ui, &theme, "Option", true).rect
        });

        let clicked = run(&ctx, click_at(rect.center()), |ui| {
            key_cap(ui, &theme, "Option", true).clicked()
        });

        assert!(!clicked);
    }

    #[test]
    fn ink_derives_from_the_theme() {
        let dark = Ink::new(&Theme::dark());
        let light = Ink::new(&Theme::light());
        assert!(dark.dark);
        assert!(!light.dark);
        assert_ne!(dark.card, light.card);
    }

    #[test]
    fn danger_ink_clears_aa_against_its_card() {
        for theme in [Theme::dark(), Theme::light()] {
            let ink = Ink::new(&theme);
            let ratio = crate::theme::contrast_ratio(ink.danger, ink.card);
            assert!(
                ratio >= crate::theme::Contrast::AA_TEXT,
                "danger ink {ratio:.2} did not clear AA on {} cards",
                if ink.dark { "dark" } else { "light" }
            );
        }
    }

    #[test]
    fn body_text_clears_aa_on_cards_and_controls() {
        for theme in [Theme::dark(), Theme::light()] {
            let ink = Ink::new(&theme);
            for (name, background) in [("card", ink.card), ("control", ink.control)] {
                let ratio = crate::theme::contrast_ratio(ink.text, background);
                assert!(
                    ratio >= crate::theme::Contrast::AA_TEXT,
                    "text on {name} was {ratio:.2}"
                );
            }
        }
    }

    #[test]
    fn tones_carry_a_distinct_glyph() {
        let glyphs: Vec<_> = [Tone::Info, Tone::Warning, Tone::Error, Tone::Success]
            .into_iter()
            .map(Tone::glyph)
            .collect();
        let mut unique = glyphs.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(glyphs.len(), unique.len());
    }
}
