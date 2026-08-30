//! Scenes: which presentation treatment a capture arrives already wearing.
//!
//! Scenes answers *appearance*. After Capture answers *destination*. Keeping
//! those two questions on separate panes is the whole point of this surface:
//! the old single `Apply Smart Frame` checkbox sat in a list of "copy, save,
//! upload" and could only say yes or no to one global look, which is not how
//! anybody actually works. A window capture wants a shadow and a soft backdrop;
//! a region capture pasted into a bug report wants nothing at all.
//!
//! The model is deliberately small:
//!
//! * One **default**, which is [`SceneChoice::None`], [`SceneChoice::Auto`], or
//!   a **named** preset.
//! * One row per capture type, always visible, each either deferring to the
//!   default or naming its own choice.
//! * A library of presets, of which `Auto` is built in and immutable.
//!
//! There are no unnamed per-type values. A per-type override that carried its
//! own anonymous settings would be a preset you cannot find, rename or reuse,
//! and the pane would need an editor to make it reachable. Assignments point at
//! names; presets are made in the editor, where there is a real canvas.

use egui::{Rect, Sense, Stroke, StrokeKind, Vec2};

use super::kit::{self, ButtonKind, Ink};
use super::preview::{self, PreviewPlatform, Subject};
use crate::theme::{Space, Text, Theme, corner};

/// The identifier of the immutable built-in preset.
pub const AUTO_PRESET_ID: &str = "auto";

/// A capture type that can carry a Scene.
///
/// Every one of these is always shown. A row that only appears once you have
/// used the feature is a row nobody discovers.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum SceneCapture {
    /// A dragged region.
    Region,
    /// A single window.
    Window,
    /// One whole display.
    FullScreen,
    /// Every display at once.
    AllDisplays,
    /// A stitched scrolling capture.
    Scrolling,
}

impl SceneCapture {
    /// Every capture type, in the order the pane lists them.
    pub const ALL: [Self; 5] = [
        Self::Region,
        Self::Window,
        Self::FullScreen,
        Self::AllDisplays,
        Self::Scrolling,
    ];

    /// The settings-key fragment.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Region => "region",
            Self::Window => "window",
            Self::FullScreen => "full-screen",
            Self::AllDisplays => "all-displays",
            Self::Scrolling => "scrolling",
        }
    }

    /// The row label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Region => "Region",
            Self::Window => "Window",
            Self::FullScreen => "Full Screen",
            Self::AllDisplays => "All Displays",
            Self::Scrolling => "Scrolling",
        }
    }

    /// Parse a settings-key fragment.
    #[must_use]
    pub fn from_slug(slug: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.slug() == slug)
    }

    /// What a preview of this capture type depicts.
    #[must_use]
    pub const fn subject(self) -> Subject {
        match self {
            Self::Region => Subject::Region,
            Self::Window => Subject::Window,
            Self::FullScreen => Subject::FullScreen,
            Self::AllDisplays => Subject::AllDisplays,
            Self::Scrolling => Subject::Scrolling,
        }
    }
}

/// A resolved Scene: what a capture will actually be dressed in.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum SceneChoice {
    /// Leave the capture exactly as taken.
    #[default]
    None,
    /// Let Scrozz pick a treatment from the capture itself.
    Auto,
    /// A named preset, by id.
    Preset(String),
}

impl SceneChoice {
    /// The stored form: `none`, `auto`, or `preset:<id>`.
    #[must_use]
    pub fn to_value(&self) -> String {
        match self {
            Self::None => "none".to_owned(),
            Self::Auto => "auto".to_owned(),
            Self::Preset(id) => format!("preset:{id}"),
        }
    }

    /// Parse the stored form. Unknown text resolves to [`SceneChoice::None`]
    /// rather than failing, because a settings file naming a preset that has
    /// since been deleted must not stop the app from starting.
    #[must_use]
    pub fn from_value(value: &str) -> Self {
        match value {
            "auto" => Self::Auto,
            other => other
                .strip_prefix("preset:")
                .filter(|id| !id.is_empty())
                .map_or(Self::None, |id| Self::Preset(id.to_owned())),
        }
    }

    /// How the choice reads in a menu, resolving preset ids to names.
    #[must_use]
    pub fn label(&self, presets: &[ScenePreset]) -> String {
        match self {
            Self::None => "None".to_owned(),
            Self::Auto => "Auto".to_owned(),
            Self::Preset(id) => presets
                .iter()
                .find(|preset| &preset.id == id)
                .map_or_else(|| format!("Missing preset ({id})"), |p| p.name.clone()),
        }
    }
}

/// What one capture type is set to.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum SceneAssignment {
    /// Follow whatever the default is.
    #[default]
    UseDefault,
    /// Override the default with a specific choice.
    Explicit(SceneChoice),
}

impl SceneAssignment {
    /// The stored form; `default` means "follow the default".
    #[must_use]
    pub fn to_value(&self) -> String {
        match self {
            Self::UseDefault => "default".to_owned(),
            Self::Explicit(choice) => choice.to_value(),
        }
    }

    /// Parse the stored form.
    #[must_use]
    pub fn from_value(value: &str) -> Self {
        if value == "default" {
            Self::UseDefault
        } else {
            Self::Explicit(SceneChoice::from_value(value))
        }
    }

    /// What this row actually resolves to, given the pane's default.
    #[must_use]
    pub fn resolve(&self, default: &SceneChoice) -> SceneChoice {
        match self {
            Self::UseDefault => default.clone(),
            Self::Explicit(choice) => choice.clone(),
        }
    }

    /// The menu text, which names the resolved default rather than making the
    /// reader hold it in their head.
    #[must_use]
    pub fn label(&self, default: &SceneChoice, presets: &[ScenePreset]) -> String {
        match self {
            Self::UseDefault => format!("Use Default ({})", default.label(presets)),
            Self::Explicit(choice) => choice.label(presets),
        }
    }
}

/// The backdrop a preset paints behind the capture.
///
/// A settings-side echo of the annotate crate's preset background rather than a
/// re-export: the pane only needs enough to draw a 90-point tile, and the tile
/// must keep rendering if the editor's model gains a variant this pane has no
/// opinion about.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SceneBackdrop {
    /// Derived from the capture. Drawn as a neutral studio wash.
    #[default]
    Automatic,
    /// Nothing behind the capture; drawn as a checkerboard.
    Transparent,
    /// One flat colour.
    Solid([u8; 3]),
    /// A two-colour field, top-left to bottom-right.
    Gradient([u8; 3], [u8; 3]),
}

/// Everything the pane needs to draw a preset tile.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ScenePreviewStyle {
    /// Air around the capture, as a fraction of the tile's short edge.
    pub padding: f32,
    /// Capture corner rounding, as a fraction of the capture's short edge.
    pub corner_radius: f32,
    /// Whether the capture drops a shadow.
    pub shadow: bool,
    /// What is painted behind the capture.
    pub backdrop: SceneBackdrop,
}

impl Default for ScenePreviewStyle {
    fn default() -> Self {
        Self {
            padding: 0.12,
            corner_radius: 0.06,
            shadow: true,
            backdrop: SceneBackdrop::Automatic,
        }
    }
}

impl ScenePreviewStyle {
    /// Map the editor's stored preset settings onto a tile-sized description.
    ///
    /// Padding and radius are absolute points against a capture whose size is
    /// unknown here, so both are normalised against a nominal 1200-point wide
    /// capture. The tile is a diagram, not a measurement.
    #[must_use]
    pub fn from_preset(settings: &scrozz_annotate::smart_frame::SmartFramePresetSettings) -> Self {
        use scrozz_annotate::smart_frame::PresetBackground;
        const NOMINAL: f32 = 1200.0;
        let backdrop = match &settings.background {
            PresetBackground::Automatic
            | PresetBackground::Generated(_)
            | PresetBackground::BuiltIn(_)
            | PresetBackground::BlurredSource { .. } => SceneBackdrop::Automatic,
            PresetBackground::ResolvedGenerated(background) => SceneBackdrop::Gradient(
                [background.start.r, background.start.g, background.start.b],
                [background.end.r, background.end.g, background.end.b],
            ),
            PresetBackground::Transparent => SceneBackdrop::Transparent,
            PresetBackground::Solid(color) => SceneBackdrop::Solid([color.r, color.g, color.b]),
            PresetBackground::Gradient { start, end } => {
                SceneBackdrop::Gradient([start.r, start.g, start.b], [end.r, end.g, end.b])
            }
        };
        Self {
            padding: ((settings.padding as f32) / NOMINAL).clamp(0.0, 0.3),
            corner_radius: ((settings.corner_radius as f32) / NOMINAL).clamp(0.0, 0.25),
            shadow: settings.shadow > 0.0,
            backdrop,
        }
    }
}

/// One entry in the Scene library.
#[derive(Clone, PartialEq, Debug)]
pub struct ScenePreset {
    /// Stable identifier used by assignments.
    pub id: String,
    /// What the user calls it.
    pub name: String,
    /// Built-in presets cannot be renamed, duplicated over, or deleted.
    pub builtin: bool,
    /// How the tile draws.
    pub style: ScenePreviewStyle,
}

impl ScenePreset {
    /// The immutable built-in.
    #[must_use]
    pub fn auto() -> Self {
        Self {
            id: AUTO_PRESET_ID.to_owned(),
            name: "Auto".to_owned(),
            builtin: true,
            style: ScenePreviewStyle::default(),
        }
    }
}

/// Everything the Scenes pane draws.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct ScenesModel {
    /// The Scene used wherever a capture type does not name its own.
    pub default: SceneChoice,
    /// Per-capture-type assignments, in [`SceneCapture::ALL`] order.
    pub assignments: Vec<(SceneCapture, SceneAssignment)>,
    /// The library, `Auto` first.
    pub presets: Vec<ScenePreset>,
    /// Whether a recent capture exists to open in the editor. When false,
    /// `Create from Capture…` starts a capture instead.
    pub has_recent_capture: bool,
}

impl ScenesModel {
    /// The assignment for `kind`, defaulting to "follow the default".
    #[must_use]
    pub fn assignment(&self, kind: SceneCapture) -> SceneAssignment {
        self.assignments
            .iter()
            .find(|(candidate, _)| *candidate == kind)
            .map_or(SceneAssignment::UseDefault, |(_, value)| value.clone())
    }

    /// User presets, i.e. everything the pane will let you rename or delete.
    pub fn user_presets(&self) -> impl Iterator<Item = &ScenePreset> {
        self.presets.iter().filter(|preset| !preset.builtin)
    }

    /// Stable, host-free data for deterministic visual review.
    #[must_use]
    pub fn preview() -> Self {
        Self {
            default: SceneChoice::Auto,
            assignments: vec![
                (SceneCapture::Region, SceneAssignment::UseDefault),
                (
                    SceneCapture::Window,
                    SceneAssignment::Explicit(SceneChoice::Preset("studio".to_owned())),
                ),
                (
                    SceneCapture::FullScreen,
                    SceneAssignment::Explicit(SceneChoice::None),
                ),
                (SceneCapture::AllDisplays, SceneAssignment::UseDefault),
                (SceneCapture::Scrolling, SceneAssignment::UseDefault),
            ],
            presets: vec![
                ScenePreset::auto(),
                ScenePreset {
                    id: "studio".to_owned(),
                    name: "Studio".to_owned(),
                    builtin: false,
                    style: ScenePreviewStyle {
                        padding: 0.11,
                        corner_radius: 0.09,
                        shadow: true,
                        backdrop: SceneBackdrop::Gradient([0x5B, 0x63, 0xD3], [0x2A, 0xB7, 0xC8]),
                    },
                },
                ScenePreset {
                    id: "paper".to_owned(),
                    name: "Paper".to_owned(),
                    builtin: false,
                    style: ScenePreviewStyle {
                        padding: 0.07,
                        corner_radius: 0.0,
                        shadow: false,
                        backdrop: SceneBackdrop::Solid([0xF2, 0xEF, 0xE7]),
                    },
                },
                ScenePreset {
                    id: "cutout".to_owned(),
                    name: "Cutout".to_owned(),
                    builtin: false,
                    style: ScenePreviewStyle {
                        padding: 0.04,
                        corner_radius: 0.12,
                        shadow: true,
                        backdrop: SceneBackdrop::Transparent,
                    },
                },
            ],
            has_recent_capture: true,
        }
    }
}

/// What the pane asks the host to do.
///
/// The pane never mutates the model. Every one of these can fail on the host
/// side — a rename can collide, a delete can be refused while a preset is in
/// use — and a pane that had already applied the change would then be showing a
/// lie. This is the same contract the rest of Settings uses for shortcuts.
#[derive(Clone, PartialEq, Debug)]
pub enum ScenesEvent {
    /// Set the default Scene.
    SetDefault(SceneChoice),
    /// Set one capture type's assignment.
    SetAssignment(SceneCapture, SceneAssignment),
    /// Open the most recent capture in the editor so a Scene can be built from
    /// it; if there is none, start a capture first.
    CreateFromCapture,
    /// Rename a user preset. The host validates length and collisions.
    RenamePreset {
        /// Which preset.
        id: String,
        /// The requested name, already trimmed.
        name: String,
    },
    /// Copy a preset, built-in ones included.
    DuplicatePreset(String),
    /// Delete a user preset. Assignments pointing at it fall back to the
    /// default on the host side.
    DeletePreset(String),
}

/// Transient pane state: which preset is being renamed, and the draft name.
#[derive(Clone, Debug, Default)]
pub struct ScenesPane {
    renaming: Option<(String, String)>,
}

impl ScenesPane {
    /// Whether a rename field currently owns the keyboard.
    #[must_use]
    pub fn is_renaming(&self) -> bool {
        self.renaming.is_some()
    }

    /// Arms a rename the way clicking Rename would, for tests that need the
    /// pane to be holding the keyboard.
    #[cfg(test)]
    pub fn begin_rename_for_test(&mut self, id: &str, draft: &str) {
        self.renaming = Some((id.to_owned(), draft.to_owned()));
    }

    /// Abandon an in-progress rename without committing it.
    ///
    /// Called when the user navigates away: a draft name left armed in a pane
    /// nobody is looking at would commit on the next stray Enter.
    pub fn cancel_rename(&mut self) {
        self.renaming = None;
    }

    /// Draw the pane, collecting what the user asked for.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        model: &ScenesModel,
        platform: PreviewPlatform,
    ) -> Vec<ScenesEvent> {
        let mut events = Vec::new();
        kit::page(
            ui,
            theme,
            "Scenes",
            Some("How captures are presented. Where they go is After Capture."),
            |ui| {
                self.assignments_section(ui, theme, model, &mut events);
                self.library_section(ui, theme, model, platform, &mut events);
            },
        );
        events
    }

    fn assignments_section(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        model: &ScenesModel,
        events: &mut Vec<ScenesEvent>,
    ) {
        kit::section(ui, theme, None, |ui| {
            let mut default = model.default.clone();
            kit::row(ui, theme, "Default", |ui| {
                if choice_dropdown(ui, theme, "scenes-default", &mut default, &model.presets) {
                    events.push(ScenesEvent::SetDefault(default.clone()));
                }
            });
            kit::divider(ui, theme);
            for kind in SceneCapture::ALL {
                let mut assignment = model.assignment(kind);
                kit::row(ui, theme, kind.label(), |ui| {
                    if assignment_dropdown(
                        ui,
                        theme,
                        kind,
                        &mut assignment,
                        &model.default,
                        &model.presets,
                    ) {
                        events.push(ScenesEvent::SetAssignment(kind, assignment.clone()));
                    }
                });
            }
        });
    }

    fn library_section(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        model: &ScenesModel,
        platform: PreviewPlatform,
        events: &mut Vec<ScenesEvent>,
    ) {
        let ink = Ink::new(theme);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Presets")
                    .font(theme.font(Text::Label))
                    .color(ink.muted),
            );
            kit::trailing(ui, |ui| {
                let label = if model.has_recent_capture {
                    "Create from Capture…"
                } else {
                    "Capture and Create…"
                };
                if kit::button(ui, theme, label, ButtonKind::Primary, true).clicked() {
                    events.push(ScenesEvent::CreateFromCapture);
                }
            });
        });
        kit::card(ui, theme, |ui| {
            let tile = Vec2::new(TILE_WIDTH, TILE_HEIGHT);
            let per_row = ((ui.available_width() + Space::SM) / (tile.x + Space::SM))
                .floor()
                .max(1.0) as usize;
            for chunk in model.presets.chunks(per_row) {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = Space::SM;
                    for preset in chunk {
                        self.tile(ui, theme, preset, platform, events);
                    }
                });
                ui.add_space(Space::XS);
            }
            if model.presets.iter().all(|preset| preset.builtin) {
                kit::help(
                    ui,
                    theme,
                    "Build a Scene in the editor, then save it here to reuse it.",
                );
            }
        });
    }

    fn tile(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        preset: &ScenePreset,
        platform: PreviewPlatform,
        events: &mut Vec<ScenesEvent>,
    ) {
        let ink = Ink::new(theme);
        ui.allocate_ui(Vec2::new(TILE_WIDTH, TILE_HEIGHT), |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = Space::XS;
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(TILE_WIDTH, PREVIEW_HEIGHT), Sense::hover());
                draw_preview(ui, theme, rect, preset.style, Subject::Window, platform);
                ui.painter().rect_stroke(
                    rect,
                    corner(kit::card_corner() - 2.0),
                    Stroke::new(1.0, ink.control_stroke),
                    StrokeKind::Inside,
                );

                let renaming = self
                    .renaming
                    .as_ref()
                    .is_some_and(|(id, _)| id == &preset.id);
                if renaming {
                    let mut draft = self
                        .renaming
                        .as_ref()
                        .map(|(_, draft)| draft.clone())
                        .unwrap_or_default();
                    let response = kit::text_field(ui, theme, &mut draft, "Name", TILE_WIDTH);
                    if let Some((_, stored)) = self.renaming.as_mut() {
                        stored.clone_from(&draft);
                    }
                    let committed = response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    let cancelled = ui.input(|input| input.key_pressed(egui::Key::Escape));
                    if committed {
                        let name = draft.trim().to_owned();
                        if !name.is_empty() && name != preset.name {
                            events.push(ScenesEvent::RenamePreset {
                                id: preset.id.clone(),
                                name,
                            });
                        }
                        self.renaming = None;
                    } else if cancelled {
                        self.renaming = None;
                    }
                } else {
                    ui.label(
                        egui::RichText::new(&preset.name)
                            .font(theme.font(Text::Body))
                            .color(ink.text),
                    );
                }

                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = Space::XS;
                    if kit::small_button(
                        ui,
                        theme,
                        "Rename",
                        ButtonKind::Quiet,
                        !preset.builtin && !renaming,
                    )
                    .clicked()
                    {
                        self.renaming = Some((preset.id.clone(), preset.name.clone()));
                    }
                    if kit::small_button(ui, theme, "Duplicate", ButtonKind::Quiet, true).clicked()
                    {
                        events.push(ScenesEvent::DuplicatePreset(preset.id.clone()));
                    }
                    if kit::small_button(
                        ui,
                        theme,
                        "Delete",
                        ButtonKind::Destructive,
                        !preset.builtin,
                    )
                    .clicked()
                    {
                        events.push(ScenesEvent::DeletePreset(preset.id.clone()));
                    }
                });
            });
        });
    }
}

const TILE_WIDTH: f32 = 176.0;
const PREVIEW_HEIGHT: f32 = 92.0;
const TILE_HEIGHT: f32 = PREVIEW_HEIGHT + 46.0;

/// Draw a preset's framing around a fictional screenshot.
pub fn draw_preview(
    ui: &egui::Ui,
    theme: &Theme,
    rect: Rect,
    style: ScenePreviewStyle,
    subject: Subject,
    platform: PreviewPlatform,
) {
    let painter = ui.painter();
    let radius = corner(kit::card_corner() - 2.0);
    match style.backdrop {
        SceneBackdrop::Automatic => {
            // A neutral studio wash: two soft blobs over a mid tone, which is
            // what "resolved from the capture" produces in practice without
            // pretending to know a capture the pane has never seen.
            painter.rect_filled(rect, radius, egui::Color32::from_rgb(0x3D, 0x44, 0x6B));
            crate::paint::soft_blob(
                painter,
                rect.left_top() + Vec2::new(rect.width() * 0.3, rect.height() * 0.2),
                rect.width() * 0.55,
                egui::Color32::from_rgb(0x7A, 0x86, 0xE8),
                90,
            );
            crate::paint::soft_blob(
                painter,
                rect.right_bottom() - Vec2::new(rect.width() * 0.22, rect.height() * 0.18),
                rect.width() * 0.45,
                egui::Color32::from_rgb(0x46, 0xC8, 0xC0),
                70,
            );
        }
        SceneBackdrop::Transparent => checkerboard(painter, rect, radius),
        SceneBackdrop::Solid([r, g, b]) => {
            painter.rect_filled(rect, radius, egui::Color32::from_rgb(r, g, b));
        }
        SceneBackdrop::Gradient([sr, sg, sb], [er, eg, eb]) => {
            // No gradient primitive in egui; sixteen bands over the diagonal is
            // indistinguishable from one at tile size.
            let steps = 16;
            for step in 0..steps {
                let t = step as f32 / (steps - 1) as f32;
                let lerp = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t) as u8;
                let band = Rect::from_min_size(
                    egui::pos2(
                        rect.left(),
                        rect.top() + rect.height() * (step as f32 / steps as f32),
                    ),
                    Vec2::new(rect.width(), rect.height() / steps as f32 + 1.0),
                );
                painter.rect_filled(
                    band.intersect(rect),
                    if step == 0 {
                        crate::theme::corner_top(kit::card_corner() - 2.0)
                    } else if step == steps - 1 {
                        crate::theme::corner_bottom(kit::card_corner() - 2.0)
                    } else {
                        corner(0.0)
                    },
                    egui::Color32::from_rgb(lerp(sr, er), lerp(sg, eg), lerp(sb, eb)),
                );
            }
        }
    }

    let pad = rect.height().min(rect.width()) * style.padding;
    let shot = rect.shrink(pad.max(1.0));
    if style.shadow {
        crate::paint::soft_shadow(
            painter,
            shot,
            shot.height() * style.corner_radius,
            &theme.palette,
            0.8,
        );
    }
    let shot_radius = (shot.height().min(shot.width()) * style.corner_radius).clamp(0.0, 12.0);
    painter.rect_filled(shot, corner(shot_radius), egui::Color32::WHITE);
    let clip = painter.with_clip_rect(shot);
    preview::draw(&clip, shot, subject, platform);
}

fn checkerboard(painter: &egui::Painter, rect: Rect, radius: egui::CornerRadius) {
    let light = egui::Color32::from_rgb(0xE6, 0xE8, 0xEE);
    let dark = egui::Color32::from_rgb(0xCE, 0xD2, 0xDC);
    painter.rect_filled(rect, radius, light);
    let cell = 7.0;
    let clip = painter.with_clip_rect(rect);
    let mut y = rect.top();
    let mut row = 0;
    while y < rect.bottom() {
        let mut x = rect.left() + if row % 2 == 0 { 0.0 } else { cell };
        while x < rect.right() {
            clip.rect_filled(
                Rect::from_min_size(egui::pos2(x, y), Vec2::splat(cell)).intersect(rect),
                corner(0.0),
                dark,
            );
            x += cell * 2.0;
        }
        y += cell;
        row += 1;
    }
}

fn choice_dropdown(
    ui: &mut egui::Ui,
    theme: &Theme,
    id_salt: &str,
    choice: &mut SceneChoice,
    presets: &[ScenePreset],
) -> bool {
    let selected = choice.label(presets);
    let mut changed = false;
    let width = kit::row_control_width(ui);
    kit::dropdown(ui, theme, id_salt, &selected, width, |ui| {
        for option in choices(presets) {
            let picked = *choice == option;
            if kit::menu_item(ui, theme, picked, &option.label(presets)).clicked() && !picked {
                *choice = option;
                changed = true;
            }
        }
    });
    changed
}

fn assignment_dropdown(
    ui: &mut egui::Ui,
    theme: &Theme,
    kind: SceneCapture,
    assignment: &mut SceneAssignment,
    default: &SceneChoice,
    presets: &[ScenePreset],
) -> bool {
    let selected = assignment.label(default, presets);
    let mut changed = false;
    kit::dropdown(
        ui,
        theme,
        ("scenes-assignment", kind.slug()),
        &selected,
        kit::row_control_width(ui),
        |ui| {
            let mut options = vec![SceneAssignment::UseDefault];
            options.extend(choices(presets).into_iter().map(SceneAssignment::Explicit));
            for option in options {
                let picked = *assignment == option;
                if kit::menu_item(ui, theme, picked, &option.label(default, presets)).clicked()
                    && !picked
                {
                    *assignment = option;
                    changed = true;
                }
            }
        },
    );
    changed
}

/// Every choice a menu offers: the two built-ins, then every named preset.
fn choices(presets: &[ScenePreset]) -> Vec<SceneChoice> {
    let mut options = vec![SceneChoice::None, SceneChoice::Auto];
    options.extend(
        presets
            .iter()
            .filter(|preset| !preset.builtin)
            .map(|preset| SceneChoice::Preset(preset.id.clone())),
    );
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library() -> Vec<ScenePreset> {
        vec![
            ScenePreset::auto(),
            ScenePreset {
                id: "studio".to_owned(),
                name: "Studio".to_owned(),
                builtin: false,
                style: ScenePreviewStyle::default(),
            },
        ]
    }

    #[test]
    fn choices_round_trip_through_stored_values() {
        for choice in [
            SceneChoice::None,
            SceneChoice::Auto,
            SceneChoice::Preset("studio".to_owned()),
        ] {
            assert_eq!(SceneChoice::from_value(&choice.to_value()), choice);
        }
    }

    #[test]
    fn an_unknown_stored_choice_falls_back_to_none() {
        assert_eq!(SceneChoice::from_value("nonsense"), SceneChoice::None);
        assert_eq!(SceneChoice::from_value("preset:"), SceneChoice::None);
    }

    #[test]
    fn assignments_round_trip_and_default_is_distinguishable() {
        for assignment in [
            SceneAssignment::UseDefault,
            SceneAssignment::Explicit(SceneChoice::Auto),
            SceneAssignment::Explicit(SceneChoice::Preset("studio".to_owned())),
        ] {
            assert_eq!(
                SceneAssignment::from_value(&assignment.to_value()),
                assignment
            );
        }
    }

    #[test]
    fn use_default_resolves_to_the_default() {
        let default = SceneChoice::Preset("studio".to_owned());
        assert_eq!(
            SceneAssignment::UseDefault.resolve(&default),
            SceneChoice::Preset("studio".to_owned())
        );
        assert_eq!(
            SceneAssignment::Explicit(SceneChoice::None).resolve(&default),
            SceneChoice::None
        );
    }

    #[test]
    fn use_default_names_the_resolved_default() {
        let presets = library();
        assert_eq!(
            SceneAssignment::UseDefault.label(&SceneChoice::Preset("studio".to_owned()), &presets),
            "Use Default (Studio)"
        );
        assert_eq!(
            SceneAssignment::UseDefault.label(&SceneChoice::Auto, &presets),
            "Use Default (Auto)"
        );
    }

    #[test]
    fn a_deleted_preset_still_renders_a_label() {
        let presets = library();
        assert_eq!(
            SceneChoice::Preset("gone".to_owned()).label(&presets),
            "Missing preset (gone)"
        );
    }

    #[test]
    fn every_capture_type_has_a_unique_slug_and_survives_a_round_trip() {
        let mut slugs: Vec<_> = SceneCapture::ALL.iter().map(|k| k.slug()).collect();
        let count = slugs.len();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), count);
        for kind in SceneCapture::ALL {
            assert_eq!(SceneCapture::from_slug(kind.slug()), Some(kind));
        }
    }

    #[test]
    fn menus_filter_builtins_structurally_and_keep_user_presets() {
        let mut presets = library();
        presets.push(ScenePreset {
            id: "auto-2".to_owned(),
            name: "Auto (Custom)".to_owned(),
            builtin: false,
            style: ScenePreviewStyle::default(),
        });
        presets.push(ScenePreset {
            id: "future-built-in".to_owned(),
            name: "Future Built-in".to_owned(),
            builtin: true,
            style: ScenePreviewStyle::default(),
        });
        let options = choices(&presets);
        let autos = options
            .iter()
            .filter(|choice| matches!(choice, SceneChoice::Auto))
            .count();
        assert_eq!(autos, 1);
        assert!(!options.contains(&SceneChoice::Preset(AUTO_PRESET_ID.to_owned())));
        assert!(options.contains(&SceneChoice::Preset("auto-2".to_owned())));
        assert!(!options.contains(&SceneChoice::Preset("future-built-in".to_owned())));
    }

    #[test]
    fn a_model_without_a_row_falls_back_to_the_default() {
        let model = ScenesModel {
            default: SceneChoice::Auto,
            assignments: vec![(
                SceneCapture::Window,
                SceneAssignment::Explicit(SceneChoice::None),
            )],
            presets: library(),
            has_recent_capture: false,
        };
        assert_eq!(
            model.assignment(SceneCapture::Region),
            SceneAssignment::UseDefault
        );
        assert_eq!(
            model.assignment(SceneCapture::Window),
            SceneAssignment::Explicit(SceneChoice::None)
        );
    }

    #[test]
    fn preview_style_normalises_absolute_preset_measurements() {
        let settings = scrozz_annotate::smart_frame::SmartFramePresetSettings {
            padding: 120.0,
            corner_radius: 60.0,
            ..Default::default()
        };
        let style = ScenePreviewStyle::from_preset(&settings);
        assert!((style.padding - 0.1).abs() < 0.001);
        assert!((style.corner_radius - 0.05).abs() < 0.001);
    }
}
