//! The ordinary settings window and its About surface.

use egui::{
    Align, Color32, Key, Layout, Modifiers, RichText, Sense, TextureHandle, TextureOptions, Vec2,
};

use crate::theme::{Appearance, Space, Text, Theme};

const SETTINGS_VIEWPORT: &str = "scrozz-settings";
const WINDOW_SIZE: Vec2 = Vec2::new(680.0, 470.0);
const ICON_SIZE: f32 = 132.0;

/// One editable shortcut, as the settings window needs to see it.
///
/// Deliberately plain strings rather than the app's shortcut types: this crate
/// draws surfaces and knows nothing about registering hotkeys, and keeping the
/// dependency pointing one way means the pane can be exercised in a test without
/// a window server, a tray, or a real key grab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutRow {
    /// Stable identifier the host uses to route an edit back to an action.
    pub id: String,
    /// Human name of the action, e.g. `Capture Area`.
    pub label: String,
    /// The configured accelerator; empty means deliberately unassigned.
    pub accelerator: String,
    /// The same combination spelled for this platform, e.g. `⇧⌘8`.
    pub symbols: String,
    /// Whether this row still holds the value Scrozz ships with.
    pub is_default: bool,
    /// Whether the action can run at all in this session.
    pub usable: bool,
    /// Why this row is not in force, if it is not.
    pub problem: Option<String>,
}

/// A change the user asked for, for the host to validate and apply.
///
/// The pane reports intent and never mutates the shortcut set itself. Registering
/// a global hotkey can fail — the combination may be owned by the system or
/// another application — and only the host can find that out, so the pane must
/// not draw a change as though it had already taken effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortcutEdit {
    /// Bind this action to this accelerator.
    Set {
        /// Which action.
        id: String,
        /// The new combination, in `Cmd+Shift+8` spelling.
        accelerator: String,
    },
    /// Leave this action deliberately unbound.
    Clear {
        /// Which action.
        id: String,
    },
    /// Put this action back to the shipped default.
    Reset {
        /// Which action.
        id: String,
    },
    /// Put every action back to the shipped default.
    ResetAll,
}

/// Which pane of the settings window is showing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Pane {
    #[default]
    Shortcuts,
    About,
}

/// Identity displayed in Settings > About.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
    /// Marketing/application version.
    pub version: &'static str,
    /// Package build number.
    pub build: &'static str,
}

impl BuildInfo {
    /// A compact, copy-friendly representation of this exact build.
    #[must_use]
    pub fn label(self) -> String {
        format!("Version {} (Build {})", self.version, self.build)
    }
}

/// Persistent state for the settings viewport.
#[derive(Default)]
pub struct SettingsWindow {
    open: bool,
    focus_requested: bool,
    icon: Option<TextureHandle>,
    pane: Pane,
    recording: Option<String>,
}

impl SettingsWindow {
    /// Opens or focuses the settings window.
    pub fn open(&mut self) {
        self.open = true;
        self.focus_requested = true;
    }

    /// Whether a row is currently waiting for the user to press a combination.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// Draws the settings viewport while it is open.
    ///
    /// Returns the edits the user asked for this frame. Nothing is applied here:
    /// the host owns registration, so it decides whether a change survives.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        build: BuildInfo,
        shortcuts: &[ShortcutRow],
    ) -> Vec<ShortcutEdit> {
        let mut edits = Vec::new();
        if !self.open {
            self.recording = None;
            return edits;
        }

        let icon = self
            .icon
            .get_or_insert_with(|| {
                ctx.load_texture(
                    "scrozz-settings-icon",
                    embedded_app_icon(),
                    TextureOptions::LINEAR,
                )
            })
            .clone();
        let mut open = true;

        let mut builder = egui::ViewportBuilder::default()
            .with_title("Scrozz Settings")
            .with_app_id("com.thatcube.Scrozz.settings")
            .with_inner_size(WINDOW_SIZE)
            .with_min_inner_size(WINDOW_SIZE)
            .with_max_inner_size(WINDOW_SIZE)
            .with_resizable(false);
        if std::mem::take(&mut self.focus_requested) {
            builder = builder.with_active(true);
        }

        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of(SETTINGS_VIEWPORT),
            builder,
            |settings_ui, _class| {
                if settings_ui
                    .ctx()
                    .input(|input| input.viewport().close_requested())
                {
                    open = false;
                }
                let theme = theme_for(settings_ui);
                settings_ui.painter().rect_filled(
                    settings_ui.max_rect(),
                    0.0,
                    theme.palette.canvas(),
                );
                egui::Frame::new()
                    .inner_margin(egui::Margin::same(Space::XXL as i8))
                    .show(settings_ui, |ui| {
                        self.draw_header(ui, &theme);
                        ui.add_space(Space::LG);
                        ui.separator();
                        ui.add_space(Space::LG);
                        match self.pane {
                            Pane::Shortcuts => {
                                edits = self.draw_shortcuts(ui, &theme, shortcuts);
                            }
                            Pane::About => draw_about(ui, &theme, &icon, build),
                        }
                    });
            },
        );

        self.open = open;
        if !self.open {
            self.recording = None;
        }
        edits
    }

    fn draw_header(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let palette = theme.palette;
        ui.horizontal(|ui| {
            ui.heading(
                RichText::new("Settings")
                    .font(theme.font(Text::Title))
                    .color(palette.text),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                for (pane, name) in [(Pane::About, "About"), (Pane::Shortcuts, "Shortcuts")] {
                    let selected = self.pane == pane;
                    let fill = if selected {
                        palette.active
                    } else {
                        palette.card_fill_raised
                    };
                    let ink = if selected {
                        palette.accent_hi
                    } else {
                        palette.text_muted
                    };
                    let response = egui::Frame::new()
                        .fill(fill)
                        .corner_radius(9)
                        .inner_margin(egui::Margin::symmetric(14, 7))
                        .show(ui, |ui| {
                            ui.label(RichText::new(name).font(theme.font(Text::Label)).color(ink));
                        })
                        .response
                        .interact(Sense::click());
                    if response.clicked() {
                        self.pane = pane;
                        self.recording = None;
                    }
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                }
            });
        });
    }

    fn draw_shortcuts(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        rows: &[ShortcutRow],
    ) -> Vec<ShortcutEdit> {
        let palette = theme.palette;
        let mut edits = Vec::new();

        // Read the keyboard once, before any row draws, so an armed row cannot
        // swallow a chord that a later row also sees.
        let captured = self
            .recording
            .as_ref()
            .and_then(|_| ui.ctx().input(capture_chord));

        if let Some(id) = self.recording.clone() {
            match captured {
                Some(Chord::Cancelled) => self.recording = None,
                Some(Chord::Cleared) => {
                    edits.push(ShortcutEdit::Clear { id });
                    self.recording = None;
                }
                Some(Chord::Pressed(accelerator)) => {
                    edits.push(ShortcutEdit::Set { id, accelerator });
                    self.recording = None;
                }
                None => {}
            }
        }

        egui::ScrollArea::vertical()
            .max_height(250.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for row in rows {
                    self.draw_shortcut_row(ui, theme, row, &mut edits);
                    ui.add_space(Space::SM);
                }
            });

        ui.add_space(Space::MD);
        ui.separator();
        ui.add_space(Space::MD);
        ui.horizontal(|ui| {
            let hint = if self.recording.is_some() {
                "Press a combination. Esc cancels, Delete unassigns."
            } else {
                "Click a shortcut to change it."
            };
            ui.label(
                RichText::new(hint)
                    .font(theme.font(Text::Caption))
                    .color(palette.text_faint),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let all_default = rows.iter().all(|row| row.is_default);
                if ui
                    .add_enabled(!all_default, egui::Button::new("Reset all"))
                    .clicked()
                {
                    self.recording = None;
                    edits.push(ShortcutEdit::ResetAll);
                }
            });
        });

        edits
    }

    fn draw_shortcut_row(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        row: &ShortcutRow,
        edits: &mut Vec<ShortcutEdit>,
    ) {
        let palette = theme.palette;
        let armed = self.recording.as_deref() == Some(row.id.as_str());
        egui::Frame::new()
            .fill(palette.card_fill_raised)
            .stroke(egui::Stroke::new(
                1.0,
                if row.problem.is_some() {
                    problem_ink(palette.appearance)
                } else if armed {
                    palette.accent
                } else {
                    palette.hairline
                },
            ))
            .corner_radius(10)
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(&row.label)
                                .font(theme.font(Text::Label))
                                .color(if row.usable {
                                    palette.text
                                } else {
                                    palette.text_faint
                                }),
                        );
                        if !row.usable {
                            ui.label(
                                RichText::new("unavailable in this session")
                                    .font(theme.font(Text::Caption))
                                    .color(palette.text_faint),
                            );
                        }
                    });

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(!row.is_default, egui::Button::new("Reset"))
                            .on_hover_text("Back to the shipped default")
                            .clicked()
                        {
                            self.recording = None;
                            edits.push(ShortcutEdit::Reset { id: row.id.clone() });
                        }
                        if ui
                            .add_enabled(!row.accelerator.is_empty(), egui::Button::new("Clear"))
                            .on_hover_text("Leave this action unassigned")
                            .clicked()
                        {
                            self.recording = None;
                            edits.push(ShortcutEdit::Clear { id: row.id.clone() });
                        }

                        let caption = if armed {
                            "Press keys…".to_owned()
                        } else if row.symbols.is_empty() {
                            "Unassigned".to_owned()
                        } else {
                            row.symbols.clone()
                        };
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(caption).font(theme.font(Text::Label)).color(
                                        if armed {
                                            palette.accent_hi
                                        } else {
                                            palette.text
                                        },
                                    ),
                                )
                                .min_size(Vec2::new(120.0, 26.0))
                                .fill(if armed {
                                    palette.accent
                                } else {
                                    palette.chip_fill
                                }),
                            )
                            .clicked()
                        {
                            self.recording = if armed { None } else { Some(row.id.clone()) };
                        }
                    });
                });

                if let Some(problem) = &row.problem {
                    ui.add_space(Space::XS);
                    ui.label(
                        RichText::new(problem)
                            .font(theme.font(Text::Caption))
                            .color(problem_ink(palette.appearance)),
                    );
                }
            });
    }
}

/// What a frame of keyboard input meant to an armed shortcut row.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Chord {
    /// The user pressed a usable combination.
    Pressed(String),
    /// The user asked to leave the action unassigned.
    Cleared,
    /// The user backed out without changing anything.
    Cancelled,
}

/// Reads one chord out of a frame of input.
///
/// A bare modifier is not a shortcut, and neither is a key pressed while the
/// window is merely being typed into, so only a non-modifier key press counts.
/// Escape and Delete are intercepted before they can be bound, because a shortcut
/// recorder that lets you bind the key that cancels it has no way out.
fn capture_chord(input: &egui::InputState) -> Option<Chord> {
    for event in &input.events {
        let egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } = event
        else {
            continue;
        };
        return Some(match key {
            Key::Escape => Chord::Cancelled,
            Key::Delete | Key::Backspace => Chord::Cleared,
            key => match spell(*key, *modifiers) {
                Some(accelerator) => Chord::Pressed(accelerator),
                // A key with no accelerator spelling is not a refusal to record,
                // it is simply not a chord — keep listening rather than closing
                // the recorder on the user.
                None => continue,
            },
        });
    }
    None
}

/// Spells an egui key press the way the hotkey parser expects to read it.
///
/// egui names some keys differently from the DOM-ish table the registrar uses
/// (`OpenBracket` against `BracketLeft`), and a combination with no modifier at
/// all is refused here rather than downstream: a global hotkey bound to a bare
/// letter would swallow that letter everywhere on the system.
fn spell(key: Key, modifiers: Modifiers) -> Option<String> {
    if !(modifiers.ctrl
        || modifiers.alt
        || modifiers.shift
        || modifiers.command
        || modifiers.mac_cmd)
    {
        return None;
    }
    let named = match key {
        Key::OpenBracket => "BracketLeft",
        Key::CloseBracket => "BracketRight",
        Key::Backtick => "Backquote",
        Key::Equals => "Equal",
        Key::Plus | Key::Colon | Key::Pipe | Key::Questionmark | Key::Exclamationmark => {
            // Shifted punctuation has no unshifted key code to register, and
            // binding the shifted glyph would silently register a different key.
            return None;
        }
        other => other.name(),
    };

    let mut spelled = String::new();
    if modifiers.ctrl && !modifiers.mac_cmd {
        spelled.push_str("Ctrl+");
    }
    if modifiers.alt {
        spelled.push_str("Alt+");
    }
    if modifiers.shift {
        spelled.push_str("Shift+");
    }
    // `command` is the platform's primary modifier — Cmd on macOS, Ctrl
    // elsewhere — and `Cmd` is the spelling the parser maps back onto it.
    if modifiers.mac_cmd || (modifiers.command && !modifiers.ctrl) {
        spelled.push_str("Cmd+");
    }
    spelled.push_str(named);
    Some(spelled)
}

/// The colour an inline shortcut error is drawn in.
///
/// Deliberately local rather than a palette token: this is the first error
/// surface in the design system, and minting a shared `danger` colour would mean
/// re-deriving both palettes and re-baking every golden snapshot for one label.
/// Both values clear 4.5:1 against their card fill.
const fn problem_ink(appearance: Appearance) -> Color32 {
    match appearance {
        Appearance::Dark => Color32::from_rgb(0xFF, 0x8A, 0x80),
        Appearance::Light => Color32::from_rgb(0xC0, 0x2A, 0x22),
    }
}

fn theme_for(ui: &egui::Ui) -> Theme {
    let appearance = if ui.visuals().dark_mode {
        Appearance::Dark
    } else {
        Appearance::Light
    };
    Theme::for_appearance(appearance)
}

fn draw_about(ui: &mut egui::Ui, theme: &Theme, icon: &TextureHandle, build: BuildInfo) {
    let palette = theme.palette;

    ui.horizontal(|ui| {
        ui.add_space(Space::XL);
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(ICON_SIZE), Sense::hover());
        ui.painter().image(
            icon.id(),
            rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );

        ui.add_space(Space::HUGE);
        ui.vertical(|ui| {
            ui.add_space(Space::MD);
            ui.label(
                RichText::new("Scrozz")
                    .font(theme.font(Text::Display))
                    .color(palette.text),
            );
            ui.add_space(Space::XS);
            ui.label(
                RichText::new("Screenshots and screen recording, without limits.")
                    .font(theme.font(Text::Subtitle))
                    .color(palette.text_muted),
            );
            ui.add_space(Space::XL);

            egui::Frame::new()
                .fill(palette.card_fill_raised)
                .stroke(egui::Stroke::new(1.0, palette.hairline))
                .corner_radius(12)
                .inner_margin(egui::Margin::symmetric(16, 11))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(build.label())
                            .font(theme.font(Text::Label))
                            .color(palette.text),
                    );
                });

            ui.add_space(Space::XL);
            ui.label(
                RichText::new("Free forever. Open source.")
                    .font(theme.font(Text::Body))
                    .color(palette.text_faint),
            );
        });
    });
}

fn embedded_app_icon() -> egui::ColorImage {
    let image = image::load_from_memory(include_bytes!("../../../assets/icons/icon-256.png"))
        .expect("the embedded Scrozz icon must be valid PNG")
        .into_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: Key, modifiers: Modifiers) -> Option<String> {
        spell(key, modifiers)
    }

    /// Runs one frame and drops its output without leaking a texture delta.
    ///
    /// `egui`'s `FullOutput` panics on drop if its texture deltas were never
    /// applied, which a headless test has no way to do.
    fn frame(ctx: &egui::Context, input: egui::RawInput) {
        let mut output = ctx.run_ui(input, |_| {});
        output.textures_delta.clear();
    }

    const CMD: Modifiers = Modifiers {
        alt: false,
        ctrl: false,
        shift: false,
        mac_cmd: true,
        command: true,
    };

    #[test]
    fn a_recorded_chord_is_spelled_the_way_the_parser_reads_it() {
        let mut modifiers = CMD;
        modifiers.shift = true;
        assert_eq!(press(Key::Num8, modifiers), Some("Shift+Cmd+8".to_owned()));
    }

    #[test]
    fn a_bare_key_is_not_a_shortcut() {
        // A global hotkey on an unmodified letter would swallow that letter
        // everywhere on the system, which is not a preference worth offering.
        assert_eq!(press(Key::A, Modifiers::NONE), None);
        assert_eq!(press(Key::F5, Modifiers::NONE), None);
    }

    #[test]
    fn modifiers_are_spelled_in_a_fixed_order() {
        // So that `Ctrl+Shift+A` and `Shift+Ctrl+A` cannot become two different
        // stored strings for one combination.
        let all = Modifiers {
            alt: true,
            ctrl: true,
            shift: true,
            mac_cmd: true,
            command: true,
        };
        assert_eq!(press(Key::A, all), Some("Alt+Shift+Cmd+A".to_owned()));
    }

    #[test]
    fn egui_key_names_are_translated_where_they_differ() {
        // egui calls it `OpenBracket`; the registrar's table calls it
        // `BracketLeft`, and an untranslated name silently fails to parse.
        assert_eq!(
            press(Key::OpenBracket, CMD),
            Some("Cmd+BracketLeft".to_owned())
        );
        assert_eq!(press(Key::Equals, CMD), Some("Cmd+Equal".to_owned()));
        assert_eq!(press(Key::Backtick, CMD), Some("Cmd+Backquote".to_owned()));
    }

    #[test]
    fn shifted_punctuation_is_refused_rather_than_mis_registered() {
        // There is no `+` key to grab; binding it would quietly register
        // whatever unshifted key happens to sit underneath.
        assert_eq!(press(Key::Plus, CMD), None);
        assert_eq!(press(Key::Questionmark, CMD), None);
    }

    #[test]
    fn escape_cancels_and_delete_unassigns() {
        // The recorder has to reserve some way out, and binding the key that
        // cancels it would leave the user stuck in it.
        let events = |key| egui::RawInput {
            events: vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
            ..Default::default()
        };
        let ctx = egui::Context::default();
        frame(&ctx, events(Key::Escape));
        assert_eq!(ctx.input(capture_chord), Some(Chord::Cancelled));
        frame(&ctx, events(Key::Delete));
        assert_eq!(ctx.input(capture_chord), Some(Chord::Cleared));
    }

    #[test]
    fn a_recorded_press_becomes_a_set_edit() {
        let ctx = egui::Context::default();
        frame(
            &ctx,
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: Key::Num7,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers { shift: true, ..CMD },
                }],
                ..Default::default()
            },
        );
        assert_eq!(
            ctx.input(capture_chord),
            Some(Chord::Pressed("Shift+Cmd+7".to_owned()))
        );
    }

    #[test]
    fn a_closed_window_forgets_that_it_was_recording() {
        // Otherwise reopening Settings would silently eat the first chord the
        // user typed into whatever row happened to be armed last time.
        let mut window = SettingsWindow {
            recording: Some("capture.region".to_owned()),
            ..SettingsWindow::default()
        };
        let ctx = egui::Context::default();
        let edits = window.show(
            &ctx,
            BuildInfo {
                version: "0",
                build: "0",
            },
            &[],
        );
        assert!(edits.is_empty());
        assert!(!window.is_recording());
    }

    #[test]
    fn build_label_names_both_identifiers() {
        assert_eq!(
            BuildInfo {
                version: "2026.8.28",
                build: "92",
            }
            .label(),
            "Version 2026.8.28 (Build 92)"
        );
    }
}
