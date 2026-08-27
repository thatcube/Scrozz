//! The settings surface: sections of rows, a scrollable body, and a footer
//! that carries dirty/error state and the Save / Reset / Re-run onboarding
//! actions.
//!
//! [`render`] is the entire seam between this crate and the app: it takes an
//! immutable [`SettingsForm`](crate::form::SettingsForm) and an [`egui::Ui`],
//! draws the current frame, and returns a [`SettingsResponse`] describing what
//! the user did. It never writes to disk, never opens a file dialog, and never
//! decides a shortcut conflict — those are the app's job, reported back as
//! [`SettingsAction`]s for it to apply to its own copy of the form (typically
//! via [`crate::form::SettingsForm::apply`]) before the next frame. This is
//! what "reusable render API... rather than depending on persistence or
//! shell" (the settings/onboarding handoff) means in code: the render function
//! is a pure `(Ui, &SettingsForm) -> SettingsResponse`, and everything
//! stateful happens one level up, in the app.
//!
//! # The signature control
//!
//! A shortcut row is drawn as a live recorder: click it, press a chord, and it
//! shows immediately — no separate "record" mode dialog, no OS-level capture.
//! While a row's [`crate::form::ShortcutStatus`] is `Recording`, [`render`]
//! reads `egui`'s own key events directly (this crate has no shell dependency,
//! so it needs none: `egui` input is already in hand) and reports a finished
//! chord as [`SettingsAction::RowChanged`]. A conflict or an invalid chord is
//! drawn in the same spot, in the one alert hue the rest of the surface
//! otherwise never uses, so it cannot be mistaken for the calm resting state.
//!
//! # Ordinary window chrome is not drawn here
//!
//! The title bar, the close box, the window's own background material — all
//! of that is the app's concern. This module draws the content of the
//! settings window: sections, rows, and the footer bar.

use crate::form::{
    Row, RowChange, RowId, RowKind, SettingsForm, ShortcutChord, ShortcutStatus, Validation,
};
use crate::icons::Icon;
use crate::paint::{self, ControlState, Mod, Reveal, Surface};
use crate::theme::{Radius, Space, Text};
use egui::{Align2, Color32, Id, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

/// The one alert hue the surface uses, for a shortcut conflict or an invalid
/// filename template.
///
/// Matches [`crate::paint::BadgeTone::Alert`]'s red exactly, so a save-blocking
/// error and a drag-count badge read as the same kind of "stop" rather than
/// introducing a second red into a palette that otherwise has one.
const ALERT: Color32 = Color32::from_rgb(0xF2, 0x45, 0x3D);

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Something the settings surface asks the app to do.
///
/// Every variant is a request, not a completed fact: the app decides whether
/// and how to honour it (open a picker, persist to disk, start the onboarding
/// wizard again) and folds the result back into the [`SettingsForm`] it hands
/// to the next [`render`] call.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum SettingsAction {
    /// A row's value changed; apply it with
    /// [`SettingsForm::apply`](crate::form::SettingsForm::apply).
    RowChanged {
        /// Which row.
        row_id: RowId,
        /// What changed.
        change: RowChange,
    },
    /// The user clicked a path row's browse affordance. Open a native picker
    /// and, if one was chosen, apply the result as a
    /// [`RowChange::Path`](crate::form::RowChange::Path).
    BrowsePath {
        /// Which row.
        row_id: RowId,
    },
    /// A shortcut row started recording. Set its status to
    /// [`ShortcutStatus::Recording`] so the next frame reflects it.
    StartRecordingShortcut {
        /// Which row.
        row_id: RowId,
    },
    /// Recording was cancelled (Escape, or focus moved away) with no chord
    /// captured. Set the row's status back to
    /// [`ShortcutStatus::Idle`](crate::form::ShortcutStatus::Idle).
    StopRecordingShortcut {
        /// Which row.
        row_id: RowId,
    },
    /// The user asked to save. The form's own
    /// [`has_errors`](crate::form::SettingsForm::has_errors) has already been
    /// checked — this action is only ever emitted when it was `false`.
    Save,
    /// The user asked to discard changes back to the last saved state.
    Reset,
    /// The user asked to see the onboarding wizard again.
    RerunOnboarding,
}

/// What happened this frame.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SettingsResponse {
    /// Every action requested this frame, in the order the user triggered
    /// them. Ordinarily zero or one; more than one is possible (e.g. a toggle
    /// flip and a shortcut chord landing in the same frame) and each is
    /// independent.
    pub actions: Vec<SettingsAction>,
}

impl SettingsResponse {
    /// Whether nothing happened this frame.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Layout constants
// ---------------------------------------------------------------------------

const ROW_H: f32 = 34.0;
const NOTE_H: f32 = 18.0;
const LABEL_FRACTION: f32 = 0.46;
const FOOTER_H: f32 = 72.0;
const FOOTER_H_WITH_ERROR: f32 = 92.0;

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Draws the settings form and reports what the user did.
///
/// The body scrolls; the footer (dirty/error state and the Save / Reset /
/// Re-run onboarding buttons) is pinned beneath it. Nothing here reads a
/// clock: this is a controls-only surface (no card, no capture is ever mid
/// motion here), and D19 already says controls do not animate, so every state
/// change is drawn instantly.
pub fn render(ui: &mut Ui, surface: &Surface<'_>, form: &SettingsForm) -> SettingsResponse {
    let mut response = SettingsResponse::default();
    let palette = surface.palette();
    let full = ui.max_rect();

    let footer_h = if form.has_errors() {
        FOOTER_H_WITH_ERROR
    } else {
        FOOTER_H
    };
    let footer_rect = Rect::from_min_max(pos2(full.left(), full.bottom() - footer_h), full.max);
    let body_rect = Rect::from_min_max(full.min, pos2(full.right(), footer_rect.top()));

    ui.painter()
        .rect_filled(full, Radius::CARD, palette.card_fill);

    let mut body_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(body_rect)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
    );
    egui::ScrollArea::vertical()
        .id_salt("scrozz.settings.scroll")
        .auto_shrink([false, false])
        .show(&mut body_ui, |ui| {
            ui.set_width(body_rect.width() - Space::LG * 2.0);
            ui.add_space(Space::SM);
            for row in form.rows() {
                ui.horizontal(|ui| {
                    ui.add_space(Space::LG);
                    let row_width = (ui.available_width() - Space::LG).max(0.0);
                    ui.vertical(|ui| {
                        ui.set_width(row_width);
                        draw_row(ui, surface, row, &mut response.actions);
                    });
                });
            }
            ui.add_space(Space::LG);
        });

    draw_footer(ui, surface, footer_rect, form, &mut response);
    response
}

fn draw_row(ui: &mut Ui, surface: &Surface<'_>, row: &Row, actions: &mut Vec<SettingsAction>) {
    if matches!(row.kind, RowKind::Section) {
        draw_section_header(ui, surface, row);
        return;
    }

    let note = row_note(row);
    let h = ROW_H + if note.is_some() { NOTE_H } else { 0.0 };
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let row_top = Rect::from_min_size(rect.min, vec2(rect.width(), ROW_H));

    let palette = surface.palette();
    let ink = if row.enabled {
        palette.text
    } else {
        palette.text_faint
    };
    ui.painter().text(
        row_top.left_center(),
        Align2::LEFT_CENTER,
        row.label,
        surface.font(Text::Label),
        ink,
    );

    let control_left = rect.left() + rect.width() * LABEL_FRACTION;
    let control_rect = Rect::from_min_size(
        pos2(control_left, row_top.top()),
        vec2(rect.right() - control_left, ROW_H),
    );

    let control_state = if row.enabled {
        ControlState::new()
    } else {
        ControlState::disabled()
    };

    match &row.kind {
        RowKind::Toggle { value } => {
            if let Some(new_value) =
                draw_toggle(ui, surface, control_rect, row.id, *value, control_state)
            {
                actions.push(SettingsAction::RowChanged {
                    row_id: row.id,
                    change: RowChange::Toggle(new_value),
                });
            }
        }
        RowKind::Dropdown { options, selected } => {
            if let Some(next) = draw_dropdown(
                ui,
                surface,
                control_rect,
                row.id,
                options,
                *selected,
                control_state,
            ) {
                actions.push(SettingsAction::RowChanged {
                    row_id: row.id,
                    change: RowChange::Dropdown(next),
                });
            }
        }
        RowKind::Slider {
            value,
            min,
            max,
            step,
            unit,
        } => {
            if let Some(next) = draw_slider(
                ui,
                surface,
                control_rect,
                row.id,
                *value,
                *min,
                *max,
                *step,
                *unit,
                control_state,
            ) {
                actions.push(SettingsAction::RowChanged {
                    row_id: row.id,
                    change: RowChange::Slider(next),
                });
            }
        }
        RowKind::Path {
            value,
            placeholder,
            browse_label,
        } => {
            if let Some(next) = draw_path(
                ui,
                surface,
                control_rect,
                row.id,
                value,
                placeholder,
                browse_label,
                control_state,
                actions,
            ) {
                actions.push(SettingsAction::RowChanged {
                    row_id: row.id,
                    change: RowChange::Path(next),
                });
            }
        }
        RowKind::Shortcut { chord, status } => {
            draw_shortcut_row(
                ui,
                surface,
                control_rect,
                row.id,
                chord.as_ref(),
                status,
                actions,
            );
        }
        RowKind::Template { value, .. } => {
            if let Some(next) = draw_text(ui, surface, control_rect, row.id, value, "") {
                actions.push(SettingsAction::RowChanged {
                    row_id: row.id,
                    change: RowChange::Template(next),
                });
            }
        }
        RowKind::TextField { value, placeholder } => {
            if let Some(next) = draw_text(ui, surface, control_rect, row.id, value, placeholder) {
                actions.push(SettingsAction::RowChanged {
                    row_id: row.id,
                    change: RowChange::Text(next),
                });
            }
        }
        RowKind::Section => unreachable!("handled above"),
    }

    if let Some((text, alert)) = note {
        let note_rect = Rect::from_min_size(
            pos2(rect.left(), row_top.bottom()),
            vec2(rect.width(), NOTE_H),
        );
        ui.painter().text(
            note_rect.left_center(),
            Align2::LEFT_CENTER,
            text,
            surface.font(Text::Caption),
            if alert { ALERT } else { palette.text_muted },
        );
    }

    let div_y = rect.bottom();
    paint::divider_h(ui.painter(), rect.left(), rect.right(), div_y, palette);
}

/// The caption shown under a row: help text normally, or a validation/conflict
/// message when the row has one to show (which always takes priority over
/// help — an active error is more useful than a description of the setting).
fn row_note(row: &Row) -> Option<(String, bool)> {
    match &row.kind {
        RowKind::Template {
            validation: Validation::Invalid(reason),
            ..
        } => Some((reason.clone(), true)),
        RowKind::Shortcut { status, .. } => match status {
            ShortcutStatus::Conflict { with } => Some((format!("Already used by {with}."), true)),
            ShortcutStatus::Invalid { reason } => Some((reason.clone(), true)),
            ShortcutStatus::Recording => Some((
                "Press a key combination, or Esc to cancel.".to_owned(),
                false,
            )),
            ShortcutStatus::Idle => row.help.map(|h| (h.to_owned(), false)),
        },
        _ => row.help.map(|h| (h.to_owned(), false)),
    }
}

fn draw_section_header(ui: &mut Ui, surface: &Surface<'_>, row: &Row) {
    let palette = surface.palette();
    ui.add_space(Space::MD);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
    ui.painter().text(
        rect.left_center(),
        Align2::LEFT_CENTER,
        row.label,
        surface.font(Text::Title),
        palette.text,
    );
    if let Some(help) = row.help {
        let (r2, _) = ui.allocate_exact_size(vec2(ui.available_width(), 16.0), Sense::hover());
        ui.painter().text(
            r2.left_center(),
            Align2::LEFT_CENTER,
            help,
            surface.font(Text::Caption),
            palette.text_muted,
        );
    }
    ui.add_space(Space::XS);
}

/// A round-pill on/off switch. Instant flip on click, per D19.
fn draw_toggle(
    ui: &mut Ui,
    surface: &Surface<'_>,
    control_rect: Rect,
    row_id: RowId,
    value: bool,
    state: ControlState,
) -> Option<bool> {
    let palette = surface.palette();
    let track = Rect::from_min_size(
        pos2(control_rect.right() - 40.0, control_rect.center().y - 11.0),
        vec2(40.0, 22.0),
    );
    let response = ui.interact(
        track,
        Id::new(("scrozz.settings.toggle", row_id)),
        sense_for(state),
    );
    let track_fill = if !state.enabled {
        palette.chip_fill
    } else if value {
        palette.accent
    } else {
        palette.chip_fill
    };
    let painter = ui.painter();
    painter.rect_filled(track, Radius::pill(track.height()), track_fill);
    if response.has_focus() {
        paint::focus_ring(painter, track, Radius::pill(track.height()), palette);
    }
    let knob_x = if value {
        track.right() - 12.0
    } else {
        track.left() + 12.0
    };
    let knob_tint = if !state.enabled {
        palette.text_faint
    } else if value {
        palette.on_accent
    } else {
        palette.text
    };
    painter.circle_filled(pos2(knob_x, track.center().y), 8.0, knob_tint);
    if response.clicked() && state.enabled {
        Some(!value)
    } else {
        None
    }
}

/// A cycling dropdown: click to advance to the next option. There is no
/// popup — a floating overlay layer is more machinery than a handful of fixed
/// choices needs, and cycling keeps the whole control a single hit target.
fn draw_dropdown(
    ui: &mut Ui,
    surface: &Surface<'_>,
    control_rect: Rect,
    row_id: RowId,
    options: &[&str],
    selected: usize,
    state: ControlState,
) -> Option<usize> {
    let palette = surface.palette();
    let rect = Rect::from_min_size(
        pos2(control_rect.right() - 200.0, control_rect.center().y - 15.0),
        vec2(200.0, 30.0),
    );
    let response = ui.interact(
        rect,
        Id::new(("scrozz.settings.dropdown", row_id)),
        sense_for(state),
    );
    let painter = ui.painter();
    let fill = if response.hovered() && state.enabled {
        palette.hover
    } else {
        palette.chip_fill
    };
    painter.rect_filled(rect, Radius::CHIP, fill);
    painter.rect_stroke(
        rect,
        Radius::CHIP,
        Stroke::new(1.0, palette.hairline),
        StrokeKind::Inside,
    );
    let label = options.get(selected).copied().unwrap_or("—");
    painter.text(
        rect.left_center() + vec2(Space::SM, 0.0),
        Align2::LEFT_CENTER,
        label,
        surface.font(Text::Label),
        if state.enabled {
            palette.text
        } else {
            palette.text_faint
        },
    );
    surface.icons.draw(
        painter,
        Icon::ChevronRight,
        pos2(rect.right() - 16.0, rect.center().y),
        14.0,
        if state.enabled {
            palette.text_muted
        } else {
            palette.text_faint
        },
    );
    if response.clicked() && state.enabled && !options.is_empty() {
        Some((selected + 1) % options.len())
    } else {
        None
    }
}

/// An integer slider, built from [`paint::stroke_width`] — the crate's one
/// existing custom slider — mapped onto `[min, max]`.
#[allow(clippy::too_many_arguments)]
fn draw_slider(
    ui: &mut Ui,
    surface: &Surface<'_>,
    control_rect: Rect,
    row_id: RowId,
    value: i64,
    min: i64,
    max: i64,
    step: i64,
    unit: Option<&str>,
    state: ControlState,
) -> Option<i64> {
    let palette = surface.palette();
    let value_w = 44.0;
    let value_rect = Rect::from_min_size(
        pos2(
            control_rect.right() - value_w,
            control_rect.center().y - 10.0,
        ),
        vec2(value_w, 20.0),
    );
    ui.painter().text(
        value_rect.right_center(),
        Align2::RIGHT_CENTER,
        format!("{value}{}", unit.unwrap_or("")),
        surface.font(Text::Label),
        if state.enabled {
            palette.text
        } else {
            palette.text_faint
        },
    );

    let track_rect = Rect::from_min_size(
        pos2(
            value_rect.left() - 150.0 - Space::SM,
            control_rect.center().y - 12.0,
        ),
        vec2(150.0, 24.0),
    );
    let span = (max - min).max(1);
    #[allow(clippy::cast_precision_loss)]
    let fraction = (value - min) as f32 / span as f32;
    let response = paint::stroke_width(
        ui,
        surface,
        track_rect,
        Id::new(("scrozz.settings.slider", row_id)),
        fraction,
    );
    if !state.enabled {
        return None;
    }
    if (response.dragged() || response.clicked())
        && let Some(pos) = response.interact_pointer_pos()
    {
        let track_l = track_rect.left() + Space::MD;
        let track_r = track_rect.right() - Space::MD;
        if track_r > track_l {
            let t = ((pos.x - track_l) / (track_r - track_l)).clamp(0.0, 1.0);
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let raw = min as f32 + t * span as f32;
            #[allow(clippy::cast_possible_truncation)]
            let stepped =
                min + ((raw - min as f32) / step.max(1) as f32).round() as i64 * step.max(1);
            return Some(stepped.clamp(min, max));
        }
    }
    None
}

/// A path field with a "Browse…" affordance. Typing edits the path directly;
/// the button reports [`SettingsAction::BrowsePath`] and never opens anything
/// itself — this crate has no filesystem access.
#[allow(clippy::too_many_arguments)]
fn draw_path(
    ui: &mut Ui,
    surface: &Surface<'_>,
    control_rect: Rect,
    row_id: RowId,
    value: &str,
    placeholder: &str,
    browse_label: &str,
    state: ControlState,
    actions: &mut Vec<SettingsAction>,
) -> Option<String> {
    let browse_w = 160.0;
    let browse_rect = Rect::from_min_size(
        pos2(
            control_rect.right() - browse_w,
            control_rect.center().y - 15.0,
        ),
        vec2(browse_w, 30.0),
    );
    if paint::pill_button_with_state(
        ui,
        surface,
        browse_rect,
        Id::new(("scrozz.settings.browse", row_id)),
        Icon::ArrowBarToDown,
        browse_label,
        false,
        state,
        Reveal::SHOWN,
    )
    .clicked()
        && state.enabled
    {
        actions.push(SettingsAction::BrowsePath { row_id });
    }

    let field_rect = Rect::from_min_size(
        control_rect.left_top(),
        vec2(
            (browse_rect.left() - Space::SM - control_rect.left()).max(0.0),
            ROW_H,
        ),
    );
    draw_text_field(ui, surface, field_rect, row_id, value, placeholder, state)
}

/// A plain single-line text field.
fn draw_text(
    ui: &mut Ui,
    surface: &Surface<'_>,
    control_rect: Rect,
    row_id: RowId,
    value: &str,
    placeholder: &str,
) -> Option<String> {
    draw_text_field(
        ui,
        surface,
        control_rect,
        row_id,
        value,
        placeholder,
        ControlState::new(),
    )
}

/// The shared text-field primitive behind [`draw_text`] and [`draw_path`].
///
/// This is the one control in the surface built on a stock `egui` widget
/// rather than hand-painted: a caret, selection and IME composition are a
/// text editor's job, not this crate's, and re-deriving them would not read as
/// more polished — only as a worse text editor. Everything around it (the
/// frame, the placeholder, the disabled tint) is still drawn by hand so it
/// still looks like the rest of the surface.
fn draw_text_field(
    ui: &mut Ui,
    surface: &Surface<'_>,
    rect: Rect,
    row_id: RowId,
    value: &str,
    placeholder: &str,
    state: ControlState,
) -> Option<String> {
    let palette = surface.palette();
    let painter = ui.painter().clone();
    painter.rect_filled(rect, Radius::CHIP, palette.chip_fill);
    painter.rect_stroke(
        rect,
        Radius::CHIP,
        Stroke::new(1.0, palette.hairline),
        StrokeKind::Inside,
    );

    if !state.enabled {
        painter.text(
            rect.left_center() + vec2(Space::SM, 0.0),
            Align2::LEFT_CENTER,
            if value.is_empty() { placeholder } else { value },
            surface.font(Text::Body),
            palette.text_faint,
        );
        return None;
    }

    let buf_id = Id::new(("scrozz.settings.buf", row_id));
    let mut buf = ui
        .ctx()
        .data(|d| d.get_temp::<String>(buf_id))
        .unwrap_or_else(|| value.to_owned());

    let text_color = palette.text;
    let out = egui::TextEdit::singleline(&mut buf)
        .id(Id::new(("scrozz.settings.textedit", row_id)))
        .hint_text(placeholder)
        .frame(egui::Frame::NONE)
        .font(surface.font(Text::Body))
        .text_color(text_color)
        .desired_width(rect.width() - Space::SM * 2.0)
        .show(
            &mut ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(Space::SM, 0.0)))),
        );

    let response = out.response;
    if !response.has_focus() {
        // Not being edited: always reflect the form's own value, so an
        // external reset or an app-side update is never masked by a stale
        // scratch buffer.
        buf = value.to_owned();
    }
    ui.ctx().data_mut(|d| d.insert_temp(buf_id, buf.clone()));

    if response.changed() { Some(buf) } else { None }
}

/// The live shortcut recorder — the surface's signature control.
///
/// Idle shows the current chord (or "Not set"); clicking it requests
/// [`SettingsAction::StartRecordingShortcut`]. While `status` is
/// [`ShortcutStatus::Recording`], this reads `egui`'s own key events for the
/// current frame directly: a fully-formed chord is reported as a
/// [`RowChange::ShortcutRecorded`], and Escape reports
/// [`SettingsAction::StopRecordingShortcut`]. A conflict or an invalid chord
/// is drawn in [`ALERT`] rather than the accent, so it cannot be mistaken for
/// an ordinary idle chord.
fn draw_shortcut_row(
    ui: &mut Ui,
    surface: &Surface<'_>,
    control_rect: Rect,
    row_id: RowId,
    chord: Option<&ShortcutChord>,
    status: &ShortcutStatus,
    actions: &mut Vec<SettingsAction>,
) {
    let palette = surface.palette();
    let rect = Rect::from_min_size(
        pos2(control_rect.right() - 150.0, control_rect.center().y - 15.0),
        vec2(150.0, 30.0),
    );
    let recording = matches!(status, ShortcutStatus::Recording);
    let alert = status.blocks_save();

    let response = ui.interact(
        rect,
        Id::new(("scrozz.settings.shortcut", row_id)),
        Sense::click(),
    );

    let border = if alert {
        ALERT
    } else if recording {
        palette.accent
    } else {
        palette.hairline
    };
    let fill = if recording {
        palette.chip_fill
    } else if response.hovered() {
        palette.hover
    } else {
        palette.chip_fill
    };
    let painter = ui.painter();
    painter.rect_filled(rect, Radius::CHIP, fill);
    painter.rect_stroke(
        rect,
        Radius::CHIP,
        Stroke::new(1.4, border),
        StrokeKind::Inside,
    );

    let label = if recording {
        "Press keys…".to_owned()
    } else {
        chord.map_or_else(|| "Not set".to_owned(), ShortcutChord::glyphs)
    };
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        surface.font(Text::Shortcut),
        if alert { ALERT } else { palette.text },
    );

    if response.clicked() {
        if recording {
            actions.push(SettingsAction::StopRecordingShortcut { row_id });
        } else {
            actions.push(SettingsAction::StartRecordingShortcut { row_id });
        }
    }

    if recording {
        capture_shortcut(ui, row_id, actions);
    }
}

/// Reads this frame's key events while a shortcut row is recording.
///
/// Escape cancels with no chord. Any other key, combined with whatever
/// modifiers are currently held, finishes the recording. `egui`'s
/// `Modifiers::mac_cmd` is the only modifier mapped to [`Mod::Cmd`] — the
/// literal Windows/Super key is not something `egui` models as a modifier on
/// other platforms, so a shortcut using it is recorded as [`Mod::Ctrl`] or
/// [`Mod::Opt`] there instead, matching how the rest of the app already spells
/// its shortcuts on non-Mac platforms (see [`Mod::glyph`]).
fn capture_shortcut(ui: &Ui, row_id: RowId, actions: &mut Vec<SettingsAction>) {
    let events = ui.ctx().input(|i| i.events.clone());
    let modifiers = ui.ctx().input(|i| i.modifiers);
    for event in events {
        let egui::Event::Key {
            key,
            pressed: true,
            repeat: false,
            ..
        } = event
        else {
            continue;
        };
        if key == egui::Key::Escape {
            actions.push(SettingsAction::StopRecordingShortcut { row_id });
            return;
        }
        let mut mods = Vec::new();
        if modifiers.ctrl && !modifiers.mac_cmd {
            mods.push(Mod::Ctrl);
        }
        if modifiers.alt {
            mods.push(Mod::Opt);
        }
        if modifiers.shift {
            mods.push(Mod::Shift);
        }
        if modifiers.mac_cmd {
            mods.push(Mod::Cmd);
        }
        let chord = ShortcutChord::with_mods(mods, key.name());
        actions.push(SettingsAction::RowChanged {
            row_id,
            change: RowChange::ShortcutRecorded(chord),
        });
        return;
    }
}

fn sense_for(state: ControlState) -> Sense {
    if state.enabled {
        Sense::click()
    } else {
        Sense::hover()
    }
}

// ---------------------------------------------------------------------------
// The footer: dirty/error state and Save / Reset / Re-run onboarding
// ---------------------------------------------------------------------------

fn draw_footer(
    ui: &mut Ui,
    surface: &Surface<'_>,
    rect: Rect,
    form: &SettingsForm,
    response: &mut SettingsResponse,
) {
    let palette = surface.palette();
    paint::divider_h(ui.painter(), rect.left(), rect.right(), rect.top(), palette);
    ui.painter()
        .rect_filled(rect, 0.0, palette.card_fill_raised);

    let errors = form.errors();
    let status_y = rect.top() + 20.0;
    if let Some(message) = form.notice() {
        ui.painter().text(
            pos2(rect.left() + Space::LG, status_y),
            Align2::LEFT_CENTER,
            message,
            surface.font(Text::Caption),
            ALERT,
        );
    } else if let Some((_, message)) = errors.first() {
        ui.painter().text(
            pos2(rect.left() + Space::LG, status_y),
            Align2::LEFT_CENTER,
            format!("Can't save yet: {message}"),
            surface.font(Text::Caption),
            ALERT,
        );
    } else if form.is_dirty() {
        ui.painter().text(
            pos2(rect.left() + Space::LG, status_y),
            Align2::LEFT_CENTER,
            "You have unsaved changes.",
            surface.font(Text::Caption),
            palette.text_muted,
        );
    } else {
        ui.painter().text(
            pos2(rect.left() + Space::LG, status_y),
            Align2::LEFT_CENTER,
            "Everything is saved.",
            surface.font(Text::Caption),
            palette.text_faint,
        );
    }

    let button_y = rect.bottom() - 34.0;
    let rerun_rect =
        Rect::from_min_size(pos2(rect.left() + Space::LG, button_y), vec2(220.0, 30.0));
    if paint::pill_button(
        ui,
        surface,
        rerun_rect,
        Id::new("scrozz.settings.rerun-onboarding"),
        Icon::Viewfinder,
        "Show Onboarding Again",
        false,
        Reveal::SHOWN,
    )
    .clicked()
    {
        response.actions.push(SettingsAction::RerunOnboarding);
    }

    let save_rect = Rect::from_min_size(pos2(rect.right() - 96.0, button_y), vec2(96.0, 30.0));
    let save_state = if !errors.is_empty() {
        ControlState::disabled()
    } else {
        ControlState::new()
    };
    if paint::pill_button_with_state(
        ui,
        surface,
        save_rect,
        Id::new("scrozz.settings.save"),
        Icon::DeviceFloppy,
        "Save",
        true,
        save_state,
        Reveal::SHOWN,
    )
    .clicked()
        && errors.is_empty()
    {
        response.actions.push(SettingsAction::Save);
    }

    let reset_rect = Rect::from_min_size(pos2(save_rect.left() - 92.0, button_y), vec2(84.0, 30.0));
    let reset_state = if form.is_dirty() {
        ControlState::new()
    } else {
        ControlState::disabled()
    };
    if paint::pill_button_with_state(
        ui,
        surface,
        reset_rect,
        Id::new("scrozz.settings.reset"),
        Icon::ArrowBackUp,
        "Reset",
        false,
        reset_state,
        Reveal::SHOWN,
    )
    .clicked()
        && form.is_dirty()
    {
        response.actions.push(SettingsAction::Reset);
    }
}

// ===========================================================================
// Sample data — the harness's own reference schema
// ===========================================================================

/// A representative settings form, in the shape the real app is expected to
/// build: sections for capture, recording, output and shortcuts. This is
/// **not** the production schema — the real app owns its own settings and
/// maps them onto [`Row`]s at its own ids — it is the harness's fixed
/// reference dataset, so a golden baseline has something real to show.
#[must_use]
pub fn sample_form() -> SettingsForm {
    SettingsForm::new(vec![
        Row::section("s.capture", "Capture"),
        Row::toggle("capture.sound", "Play a sound when capturing", None, true),
        Row::toggle(
            "capture.show_cursor",
            "Show the cursor in captures",
            None,
            false,
        ),
        Row::dropdown(
            "capture.mode",
            "Default capture mode",
            Some("What starts when you press the capture shortcut"),
            vec!["Region", "Window", "Full Screen"],
            0,
        ),
        Row::slider(
            "capture.countdown",
            "Countdown before capture",
            None,
            0,
            0,
            5,
            1,
            Some("s"),
        ),
        Row::section("s.recording", "Recording"),
        Row::toggle("recording.system_audio", "Record system audio", None, true),
        Row::toggle("recording.microphone", "Record microphone", None, false),
        Row::dropdown(
            "recording.quality",
            "Video quality",
            None,
            vec!["Standard", "High", "Lossless"],
            1,
        ),
        Row::section("s.output", "Output"),
        Row::path(
            "output.directory",
            "Save captures to",
            None,
            "~/Pictures/Scrozz",
            "No folder chosen",
            "Choose Folder…",
        ),
        Row::template(
            "output.filename_template",
            "Filename template",
            Some("Uses {app}, {date}, {time} and {seq}"),
            "{app} {date} {time}",
        ),
        Row::toggle(
            "output.auto_copy",
            "Copy to clipboard automatically",
            None,
            true,
        ),
        Row::section("s.shortcuts", "Shortcuts"),
        Row::shortcut(
            "shortcuts.capture_region",
            "Capture region",
            None,
            Some(ShortcutChord::with_mods(vec![Mod::Shift, Mod::Cmd], "4")),
        ),
        Row::shortcut(
            "shortcuts.capture_window",
            "Capture window",
            None,
            Some(ShortcutChord::with_mods(vec![Mod::Shift, Mod::Cmd], "5")),
        ),
        Row::shortcut(
            "shortcuts.toggle_recording",
            "Start/stop recording",
            None,
            Some(ShortcutChord::with_mods(
                vec![Mod::Shift, Mod::Cmd, Mod::Opt],
                "5",
            )),
        ),
        Row::shortcut("shortcuts.open_history", "Open capture history", None, None),
    ])
}

/// The same form with several rows edited — dirty, no errors.
#[must_use]
pub fn sample_form_edited() -> SettingsForm {
    let mut form = sample_form();
    form.apply("capture.show_cursor", RowChange::Toggle(true));
    form.apply("capture.countdown", RowChange::Slider(3));
    form.apply(
        "output.filename_template",
        RowChange::Template("{app}-{date}-{seq}".to_owned()),
    );
    form.apply(
        "shortcuts.open_history",
        RowChange::ShortcutRecorded(ShortcutChord::with_mods(vec![Mod::Shift, Mod::Cmd], "H")),
    );
    form
}

/// The form with a shortcut conflict blocking save.
#[must_use]
pub fn sample_form_conflict() -> SettingsForm {
    let mut form = sample_form();
    form.apply(
        "shortcuts.capture_window",
        RowChange::ShortcutRecorded(ShortcutChord::with_mods(vec![Mod::Shift, Mod::Cmd], "4")),
    );
    form.set_shortcut_status(
        "shortcuts.capture_window",
        ShortcutStatus::Conflict {
            with: "Capture region".to_owned(),
        },
    );
    form
}

// ===========================================================================
// The harness scene
// ===========================================================================

/// Renders the settings surface for the harness.
///
/// `ctx.millis()` selects which of the three sample states to show — `0`
/// default, `1` edited, `2` a shortcut conflict — for exactly the reason
/// [`crate::onboarding_view::OnboardingScene`] documents: it is a state
/// selector reused from the same mechanism, not an animation duration, since
/// D19 gives this controls-only surface nothing to animate.
///
/// # Why the icon store is cached here, not built fresh per frame
///
/// [`crate::icons::IconStore::new`] uploads a *fresh* `TextureHandle` for
/// every icon on every call, and a `TextureHandle` frees its texture on drop.
/// One render of a `Scene` runs several passes over the same `egui::Context`
/// (a discarded warm-up pass or two, then the captured one; see
/// [`crate::harness::Scene`]'s own contract) — if the store were rebuilt and
/// dropped inside every [`Scene::ui`](crate::harness::Scene::ui) call, each
/// pass would free the very textures its own meshes just referenced before
/// the harness ever rasterises them, and every icon would silently fall back
/// to a flat, untextured fill. Building it once in
/// [`Scene::setup`](crate::harness::Scene::setup) — which runs exactly once
/// per render, before any pass — and holding it for the render's lifetime
/// avoids that entirely.
pub struct SettingsScene {
    icons: std::sync::Mutex<Option<crate::icons::IconStore>>,
}

impl SettingsScene {
    /// A scene with no icons uploaded yet; [`crate::harness::Scene::setup`]
    /// populates them.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            icons: std::sync::Mutex::new(None),
        }
    }
}

impl Default for SettingsScene {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::harness::Scene for SettingsScene {
    fn name(&self) -> &str {
        "settings"
    }

    fn setup(&self, ctx: &egui::Context) {
        crate::theme::install_fonts(ctx);
        crate::theme::install_style(
            ctx,
            &crate::theme::Theme::for_appearance(crate::theme::Appearance::Dark),
        );
        if let Ok(mut slot) = self.icons.lock() {
            *slot = Some(crate::icons::IconStore::new(ctx));
        }
    }

    fn ui(&self, ui: &mut Ui, ctx: &crate::harness::SceneCtx<'_>) {
        let appearance = match ctx.theme {
            egui::Theme::Dark => crate::theme::Appearance::Dark,
            egui::Theme::Light => crate::theme::Appearance::Light,
        };
        let theme = crate::theme::Theme::for_appearance(appearance);
        let guard = self
            .icons
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let empty = crate::icons::IconStore::empty();
        let icons = guard.as_ref().unwrap_or(&empty);
        let motion = crate::motion::Motion::at_ms(ctx.millis());
        let surface = Surface::still(&theme, icons, motion);

        let form = match ctx.millis() {
            0 => sample_form(),
            1 => sample_form_edited(),
            _ => sample_form_conflict(),
        };
        render(ui, &surface, &form);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_form_starts_clean() {
        let form = sample_form();
        assert!(!form.is_dirty());
        assert!(!form.has_errors());
    }

    #[test]
    fn sample_form_edited_is_dirty_with_no_errors() {
        let form = sample_form_edited();
        assert!(form.is_dirty());
        assert!(!form.has_errors());
    }

    #[test]
    fn sample_form_conflict_has_exactly_one_error() {
        let form = sample_form_conflict();
        assert!(form.has_errors());
        assert_eq!(form.errors().len(), 1);
        assert_eq!(form.errors()[0].0, "shortcuts.capture_window");
    }

    #[test]
    fn settings_response_default_is_empty() {
        assert!(SettingsResponse::default().is_empty());
    }

    #[test]
    fn row_note_prefers_conflict_over_help() {
        let row = Row::shortcut(
            "x",
            "X",
            Some("some help text"),
            Some(ShortcutChord::bare("4")),
        );
        let RowKind::Shortcut { status, .. } = &row.kind else {
            unreachable!()
        };
        assert_eq!(*status, ShortcutStatus::Idle);
        // Idle with help falls back to the help text.
        let (text, alert) = row_note(&row).unwrap();
        assert_eq!(text, "some help text");
        assert!(!alert);
    }

    #[test]
    fn row_note_surfaces_template_validation_errors() {
        let row = Row::template("t", "Template", None, "{oops}");
        let (text, alert) = row_note(&row).unwrap();
        assert!(text.contains("oops"));
        assert!(alert);
    }
}
