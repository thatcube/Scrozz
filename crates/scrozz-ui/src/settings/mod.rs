//! The ordinary settings window and its About surface.

pub mod kit;
pub mod preview;
pub mod scenes;

use egui::{
    Align, Align2, Color32, Key, Layout, Modifiers, RichText, Sense, TextureHandle, TextureOptions,
    Vec2, WidgetInfo, WidgetType,
};

use scrozz_record::{EngineCapabilities, RecordingSettings};

use crate::{
    icons::{Icon, IconStore},
    onboarding::OcrSettings,
    recent_captures_overlay::{
        RecentCapturesAutoCloseAction, RecentCapturesOverlaySettings, RecentCapturesPlacement,
        RecentCapturesSaveBehavior,
    },
    recording_settings::RecordingSettingsPanel,
    theme::{Appearance, Space, Text, Theme},
};

pub use self::{
    preview::PreviewPlatform,
    scenes::{
        AUTO_PRESET_ID, SceneAssignment, SceneBackdrop, SceneCapture, SceneChoice, ScenePreset,
        ScenePreviewStyle, ScenesEvent, ScenesModel, ScenesPane,
    },
};
pub use crate::recording_settings::RecordingSettingsAction;

const SETTINGS_VIEWPORT: &str = "scrozz-settings";
const WINDOW_SIZE: Vec2 = Vec2::new(760.0, 580.0);
const MIN_WINDOW_SIZE: Vec2 = Vec2::new(640.0, 470.0);
const APP_ICON_SIZE: f32 = 84.0;
const SIDEBAR_WIDTH: f32 = 184.0;
const TOOLBAR_HEIGHT: f32 = 62.0;
const NAV_ITEM_WIDTH: f32 = 68.0;

/// Screenshot or recording column in the After Capture matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterCaptureMedia {
    /// Still screenshot actions.
    Screenshot,
    /// Completed recording actions.
    Recording,
}

impl AfterCaptureMedia {
    const fn label(self) -> &'static str {
        match self {
            Self::Screenshot => "Screenshot",
            Self::Recording => "Recording",
        }
    }
}

/// One matrix cell as the platform host resolved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AfterCaptureCell {
    /// Stored value, retained even while a feature is unavailable.
    pub enabled: bool,
    /// Whether this build and session can execute the action.
    pub available: bool,
    /// Clear explanation rendered instead of an inert checkbox.
    pub unavailable_reason: Option<String>,
}

/// One action row in the platform-adaptive After Capture matrix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AfterCaptureRow {
    /// Stable dotted setting key for the screenshot cell.
    pub screenshot_id: String,
    /// Stable dotted setting key for the recording cell, if the contract has one.
    pub recording_id: Option<String>,
    /// Approved user-facing action name.
    pub label: String,
    /// What the action does and what artifact it consumes.
    pub description: String,
    /// Screenshot state and capability.
    pub screenshot: AfterCaptureCell,
    /// Recording state and capability.
    pub recording: AfterCaptureCell,
}

/// One user-requested After Capture toggle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AfterCaptureEdit {
    /// Stable dotted setting key.
    pub id: String,
    /// Column that raised the edit.
    pub media: AfterCaptureMedia,
    /// Requested value.
    pub enabled: bool,
}

/// All edits raised during one Settings frame.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SettingsEdits {
    /// Shortcut changes.
    pub shortcuts: Vec<ShortcutEdit>,
    /// After Capture changes.
    pub after_capture: Vec<AfterCaptureEdit>,
    /// Recording interaction changes.
    pub recording: Vec<RecordingSettingsAction>,
    /// Complete Recent Captures Overlay configuration after an accepted edit.
    pub recent_captures_overlay: Option<RecentCapturesOverlaySettings>,
    /// Complete capture-fidelity configuration after an accepted edit.
    pub capture: Option<CaptureSettings>,
    /// Scene assignment and library requests, in the order the user made them.
    pub scenes: Vec<ScenesEvent>,
    /// The user asked to see the one-time text-recognition introduction again.
    pub replay_ocr_onboarding: bool,
    /// The user asked to configure private sharing.
    ///
    /// Sharing has a viewport of its own rather than a pane here, because it
    /// holds transient credential fields that must be zeroized the moment that
    /// window closes — a lifetime the Settings window does not have.
    pub open_sharing: bool,
}

impl SettingsEdits {
    /// Whether the frame requested no changes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shortcuts.is_empty()
            && self.after_capture.is_empty()
            && self.recording.is_empty()
            && self.recent_captures_overlay.is_none()
            && self.capture.is_none()
            && self.scenes.is_empty()
            && !self.replay_ocr_onboarding
            && !self.open_sharing
    }
}

/// Capture fidelity preferences that belong to the shutter, not to the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureSettings {
    /// Whether a window capture includes the window's own drop shadow.
    pub window_shadow: bool,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            window_shadow: true,
        }
    }
}

/// Everything the settings panes read during one frame.
///
/// A struct rather than a dozen positional parameters: panes come and go, and
/// each new one would otherwise widen five call sites and one `#[allow]`.
pub struct SettingsInput<'a> {
    /// Identity shown in About.
    pub build: BuildInfo,
    /// Editable shortcut rows, screenshots first then recording.
    pub shortcuts: &'a [ShortcutRow],
    /// The platform-resolved After Capture matrix.
    pub after_capture: &'a [AfterCaptureRow],
    /// Recording interaction settings and capabilities.
    pub recording: RecordingPane,
    /// Recent Captures Overlay configuration.
    pub recent_captures_overlay: RecentCapturesOverlaySettings,
    /// Scene default, per-capture assignments, and the preset library.
    pub scenes: ScenesModel,
    /// Capture fidelity preferences.
    pub capture: CaptureSettings,
    /// Which platform idiom the window follows.
    pub platform: SettingsPlatform,
}

impl Default for SettingsInput<'_> {
    fn default() -> Self {
        Self {
            build: BuildInfo {
                version: "0.1.0",
                build: "100",
            },
            shortcuts: &[],
            after_capture: &[],
            recording: RecordingPane::default(),
            recent_captures_overlay: RecentCapturesOverlaySettings::default(),
            scenes: ScenesModel::default(),
            capture: CaptureSettings::default(),
            platform: SettingsPlatform::current(),
        }
    }
}

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
    /// Nonblocking guidance for a binding that may overlap an OS default.
    pub advisory: Option<String>,
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

/// Desktop whose platform-specific Settings vocabulary should be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPlatform {
    /// macOS vocabulary and shortcuts.
    MacOs,
    /// Left navigation, matching Windows Settings.
    Windows,
    /// Desktop-neutral left navigation.
    Linux,
}

impl SettingsPlatform {
    /// The platform this binary was built for.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Linux
        }
    }

    /// Where category navigation belongs.
    ///
    /// Scrozz deliberately uses one left rail on every desktop so switching
    /// platforms does not move the window's primary navigation.
    #[must_use]
    pub const fn navigation(self) -> Navigation {
        let _ = self;
        Navigation::Sidebar
    }
}

/// Settings navigation placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Navigation {
    /// Legacy top toolbar placement retained for compatibility.
    TopToolbar,
    /// Shared left navigation used on every platform.
    Sidebar,
}

/// Whether opening Settings creates or reuses its viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenDisposition {
    /// No Settings viewport was open.
    FirstOpen,
    /// The existing Settings viewport should be focused.
    Reused,
}

/// Everything the Recording pane needs to draw one frame.
///
/// Passed by value because the settings window owns no recording state: the
/// host reads the live policy from the recording lifecycle each frame and
/// applies whatever the pane asks for, so the window can never hold a stale
/// copy of a privacy-relevant setting.
#[derive(Debug, Clone)]
pub struct RecordingPane {
    /// The policy currently being edited.
    pub settings: RecordingSettings,
    /// What the native engine can actually observe.
    pub capabilities: EngineCapabilities,
    /// Whether a recording is running, which locks the pane.
    pub active: bool,
    /// Live camera state, present only while a camera recording is running.
    ///
    /// Composition is the one preference the state machine accepts mid
    /// recording, so it stays reachable here when everything else is locked.
    pub camera: Option<Box<crate::camera_settings::CameraLiveSnapshot>>,
}

impl Default for RecordingPane {
    fn default() -> Self {
        Self {
            settings: RecordingSettings::shipped(),
            capabilities: EngineCapabilities::default(),
            active: false,
            camera: None,
        }
    }
}

/// Which pane of the settings window is showing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// Capture fidelity: what the shutter records.
    Capture,
    /// Scene appearance: the default and the per-capture assignments.
    Scenes,
    /// Destinations and workflow once a capture finalizes.
    #[default]
    AfterCapture,
    /// Recording interaction overlays.
    Recording,
    /// The recent-captures corner surface.
    RecentCapturesOverlay,
    /// Text recognition.
    TextRecognition,
    /// Global shortcuts.
    Shortcuts,
    /// Identity and build.
    About,
}

impl Pane {
    /// Every pane, in navigation order, with its label and icon.
    pub const ALL: [(Self, &'static str, Icon); 8] = [
        (Self::Capture, "Capture", Icon::Viewfinder),
        (Self::Scenes, "Scenes", Icon::Palette),
        (Self::AfterCapture, "After Capture", Icon::Copy),
        (Self::Recording, "Recording", Icon::Video),
        (Self::RecentCapturesOverlay, "Recent", Icon::LayoutGrid),
        (Self::TextRecognition, "Text", Icon::Scan),
        (Self::Shortcuts, "Shortcuts", Icon::Settings),
        (Self::About, "About", Icon::AppWindow),
    ];

    /// A stable slug, for golden names and telemetry-free diagnostics.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Scenes => "scenes",
            Self::AfterCapture => "after-capture",
            Self::Recording => "recording",
            Self::RecentCapturesOverlay => "recent-captures",
            Self::TextRecognition => "text-recognition",
            Self::Shortcuts => "shortcuts",
            Self::About => "about",
        }
    }
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
pub struct SettingsWindow {
    open: bool,
    focus_requested: bool,
    app_icon: Option<TextureHandle>,
    icons: Option<IconStore>,
    pane: Pane,
    recording: Option<String>,
    scenes: ScenesPane,
    platform: SettingsPlatform,
    appearance: Appearance,
    #[cfg(target_os = "linux")]
    appearance_initialized: bool,
    #[cfg(target_os = "linux")]
    appearance_watcher: Option<dark_light::Watcher>,
}

impl Default for SettingsWindow {
    fn default() -> Self {
        Self {
            open: false,
            focus_requested: false,
            app_icon: None,
            icons: None,
            pane: Pane::default(),
            recording: None,
            scenes: ScenesPane::default(),
            platform: SettingsPlatform::current(),
            appearance: Appearance::Light,
            #[cfg(target_os = "linux")]
            appearance_initialized: false,
            #[cfg(target_os = "linux")]
            appearance_watcher: None,
        }
    }
}

impl SettingsWindow {
    /// Opens or focuses the settings window.
    pub fn open(&mut self) -> OpenDisposition {
        let disposition = if self.open {
            OpenDisposition::Reused
        } else {
            OpenDisposition::FirstOpen
        };
        self.open = true;
        self.focus_requested = true;
        disposition
    }

    /// Whether the ordinary Settings child viewport is open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Whether a row is currently waiting for the user to press a combination.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// Whether a text field in Settings currently owns the keyboard.
    ///
    /// Renaming a preset types letters that are also global shortcut triggers,
    /// so the host must stop routing keys to hotkeys while a field is live.
    #[must_use]
    pub fn is_editing_text(&self) -> bool {
        self.scenes.is_renaming()
    }

    /// Draws the settings viewport while it is open.
    ///
    /// Returns the edits the user asked for this frame. Nothing is applied here:
    /// the host owns registration, so it decides whether a change survives.
    pub fn show(&mut self, ctx: &egui::Context, input: &SettingsInput<'_>) -> SettingsEdits {
        let mut edits = SettingsEdits::default();
        if !self.open {
            self.forget_transient_edits();
            return edits;
        }

        let appearance = self.resolve_appearance(ctx);
        let app_icon = self
            .app_icon
            .get_or_insert_with(|| {
                ctx.load_texture(
                    "scrozz-settings-icon",
                    embedded_app_icon(),
                    TextureOptions::LINEAR,
                )
            })
            .clone();
        self.icons.get_or_insert_with(|| IconStore::new(ctx));
        let mut open = true;
        let focus_requested = std::mem::take(&mut self.focus_requested);
        let builder = viewport_builder(focus_requested);
        // The window, not the caller, decides which platform idiom to draw:
        // the host has no reason to know, and a golden overrides it directly.
        let input = SettingsInput {
            build: input.build,
            shortcuts: input.shortcuts,
            after_capture: input.after_capture,
            recording: input.recording.clone(),
            recent_captures_overlay: input.recent_captures_overlay,
            scenes: input.scenes.clone(),
            capture: input.capture,
            platform: self.platform,
        };

        ctx.show_viewport_immediate(settings_viewport_id(), builder, |settings_ui, _class| {
            if settings_ui
                .ctx()
                .input(|input| input.viewport().close_requested())
            {
                open = false;
            }
            let theme = Theme::for_appearance(appearance);
            let icons = self.icons.as_ref().expect("settings icons were loaded");
            request_foreground(settings_ui.ctx(), focus_requested);
            edits = draw_settings(
                settings_ui,
                &theme,
                icons,
                Some(&app_icon),
                &input,
                &mut PaneState {
                    pane: &mut self.pane,
                    recording: &mut self.recording,
                    scenes: &mut self.scenes,
                },
            );
        });

        self.set_open(open);
        edits
    }

    /// Records whether the viewport survived the frame.
    ///
    /// The only place `open` changes on the way down, so it is also the only
    /// place that has to hand the keyboard back.
    fn set_open(&mut self, open: bool) {
        self.open = open;
        if !self.open {
            self.forget_transient_edits();
        }
    }

    /// Drops every half-finished edit the window was holding.
    ///
    /// Both kinds suspend the app's global shortcuts while they are live — a
    /// shortcut recorder is waiting to swallow the next chord, and a rename
    /// field owns the keyboard. Clearing only one on close leaves the other
    /// asserting ownership of a keyboard that belongs to a window nobody can
    /// see, so the app stays deaf to its own shortcuts until Settings is
    /// reopened. Anything added here that captures typing must be cleared here
    /// too.
    fn forget_transient_edits(&mut self) {
        self.recording = None;
        self.scenes.cancel_rename();
    }

    fn resolve_appearance(&mut self, ctx: &egui::Context) -> Appearance {
        if let Some(theme) = ctx.system_theme() {
            self.appearance = appearance_for_theme(Some(theme), false);
            return self.appearance;
        }

        #[cfg(target_os = "linux")]
        {
            if !self.appearance_initialized {
                self.appearance_initialized = true;
                if let Ok(mode) = dark_light::detect() {
                    self.appearance = appearance_for_mode(mode, self.appearance);
                }
                self.appearance_watcher = dark_light::subscribe().ok();
            }
            if let Some(watcher) = &self.appearance_watcher {
                for mode in watcher.try_iter() {
                    self.appearance = appearance_for_mode(mode, self.appearance);
                }
            }
        }

        self.appearance
    }
}

/// Draws the real Settings content without creating a native viewport.
///
/// The golden harness uses this exact path to render platform layouts rather
/// than maintaining mock screenshots, so a golden can never drift from the
/// window the user actually opens.
pub fn render_preview(
    ui: &mut egui::Ui,
    mut pane: Pane,
    icons: &IconStore,
    input: &SettingsInput<'_>,
) {
    let theme = theme_for(ui);
    let mut recording = None;
    let mut scenes = ScenesPane::default();
    let _ = draw_settings(
        ui,
        &theme,
        icons,
        None,
        input,
        &mut PaneState {
            pane: &mut pane,
            recording: &mut recording,
            scenes: &mut scenes,
        },
    );
}

fn settings_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of(SETTINGS_VIEWPORT)
}

fn viewport_builder(active: bool) -> egui::ViewportBuilder {
    let builder = egui::ViewportBuilder::default()
        .with_title("Scrozz Settings")
        .with_app_id("com.thatcube.Scrozz.settings")
        .with_inner_size(WINDOW_SIZE)
        .with_min_inner_size(MIN_WINDOW_SIZE)
        .with_resizable(true)
        .with_decorations(true)
        .with_window_level(egui::WindowLevel::Normal);
    if active {
        builder.with_active(true)
    } else {
        builder
    }
}

fn request_foreground(ctx: &egui::Context, requested: bool) {
    if requested {
        // `Focus` activates the app and orders this window front on macOS,
        // calls the foreground-window path on Windows, and sends
        // `_NET_ACTIVE_WINDOW` on X11. It is emitted only after the user invokes
        // Settings; ordinary repainting never steals focus.
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }
}

/// Stable shortcut data used by platform golden renders.
#[must_use]
pub fn preview_shortcuts(platform: SettingsPlatform) -> Vec<ShortcutRow> {
    let symbols = match platform {
        SettingsPlatform::MacOs => ["⇧⌘0", "⇧⌘8", "⇧⌘9", "⇧⌘7", "⌃⇧⌘7"],
        SettingsPlatform::Windows => [
            "Win + Shift + 2",
            "Win + Shift + 4",
            "Win + Shift + 5",
            "Win + Shift + 3",
            "Win + Ctrl + Shift + 3",
        ],
        SettingsPlatform::Linux => [
            "Super + Shift + 2",
            "Super + Shift + 4",
            "Super + Shift + 5",
            "Super + Shift + 3",
            "Super + Ctrl + Shift + 3",
        ],
    };
    [
        ("capture.all-in-one", "All-in-One", symbols[0], true),
        ("capture.region", "Capture Region", symbols[1], true),
        ("capture.window", "Capture Window", symbols[2], true),
        ("capture.fullscreen", "Capture Display", symbols[3], true),
        (
            "capture.all-displays",
            "Capture All Displays",
            symbols[4],
            true,
        ),
        ("record.toggle", "Start / Stop Recording", "", false),
    ]
    .into_iter()
    .map(|(id, label, symbols, usable)| ShortcutRow {
        id: id.to_owned(),
        label: label.to_owned(),
        accelerator: symbols.to_owned(),
        symbols: symbols.to_owned(),
        is_default: true,
        usable,
        problem: None,
        advisory: None,
    })
    .collect()
}

// ---------------------------------------------------------------------------
// Shell
// ---------------------------------------------------------------------------

/// Mutable pane state carried between frames.
struct PaneState<'a> {
    pane: &'a mut Pane,
    recording: &'a mut Option<String>,
    scenes: &'a mut ScenesPane,
}

fn draw_settings(
    ui: &mut egui::Ui,
    theme: &Theme,
    icons: &IconStore,
    app_icon: Option<&TextureHandle>,
    input: &SettingsInput<'_>,
    state: &mut PaneState<'_>,
) -> SettingsEdits {
    crate::theme::apply_style(ui.style_mut(), theme);
    kit::install(ui, theme);
    ui.painter()
        .rect_filled(ui.max_rect(), 0.0, kit::page_fill(theme));
    match input.platform.navigation() {
        Navigation::TopToolbar => {
            draw_top_navigation(ui, theme, icons, state);
            let rect = egui::Rect::from_min_max(
                egui::pos2(ui.max_rect().left(), ui.max_rect().top() + TOOLBAR_HEIGHT),
                ui.max_rect().max,
            );
            let mut body = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(rect)
                    .layout(Layout::top_down(Align::LEFT)),
            );
            draw_body(&mut body, theme, icons, app_icon, input, state)
        }
        Navigation::Sidebar => {
            let ink = kit::Ink::new(theme);
            let sidebar = egui::Rect::from_min_max(
                ui.max_rect().min,
                egui::pos2(ui.max_rect().left() + SIDEBAR_WIDTH, ui.max_rect().bottom()),
            );
            ui.painter().rect_filled(sidebar, 0.0, ink.sidebar);
            ui.painter().line_segment(
                [sidebar.right_top(), sidebar.right_bottom()],
                egui::Stroke::new(1.0, kit::hairline(&theme.palette)),
            );
            let mut nav = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(sidebar.shrink2(Vec2::new(Space::LG, Space::XL)))
                    .layout(Layout::top_down(Align::LEFT)),
            );
            draw_sidebar_navigation(&mut nav, theme, icons, state);

            let body_rect = egui::Rect::from_min_max(
                egui::pos2(sidebar.right(), ui.max_rect().top()),
                ui.max_rect().max,
            );
            let mut body = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(body_rect)
                    .layout(Layout::top_down(Align::LEFT)),
            );
            draw_body(&mut body, theme, icons, app_icon, input, state)
        }
    }
}

fn draw_top_navigation(
    ui: &mut egui::Ui,
    theme: &Theme,
    icons: &IconStore,
    state: &mut PaneState<'_>,
) {
    let rect = egui::Rect::from_min_max(
        ui.max_rect().min,
        egui::pos2(ui.max_rect().right(), ui.max_rect().top() + TOOLBAR_HEIGHT),
    );
    ui.painter().rect_filled(rect, 0.0, theme.palette.card_fill);
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        egui::Stroke::new(1.0, kit::hairline(&theme.palette)),
    );
    let mut nav = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(Space::MD, Space::XS)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    nav.spacing_mut().item_spacing.x = Space::HAIR;
    for (candidate, label, icon) in Pane::ALL {
        if navigation_button(
            &mut nav,
            theme,
            icons,
            label,
            icon,
            *state.pane == candidate,
            true,
        ) {
            select(state, candidate);
        }
    }
}

fn draw_sidebar_navigation(
    ui: &mut egui::Ui,
    theme: &Theme,
    icons: &IconStore,
    state: &mut PaneState<'_>,
) {
    ui.add_space(Space::SM);
    ui.label(
        RichText::new("Settings")
            .font(theme.font(Text::Label))
            .color(theme.palette.text_muted),
    );
    ui.add_space(Space::LG);
    ui.spacing_mut().item_spacing.y = Space::XS;
    for (candidate, label, icon) in Pane::ALL {
        if navigation_button(
            ui,
            theme,
            icons,
            label,
            icon,
            *state.pane == candidate,
            false,
        ) {
            select(state, candidate);
        }
    }
}

fn select(state: &mut PaneState<'_>, candidate: Pane) {
    *state.pane = candidate;
    *state.recording = None;
    state.scenes.cancel_rename();
}

fn navigation_button(
    ui: &mut egui::Ui,
    theme: &Theme,
    icons: &IconStore,
    label: &str,
    icon: Icon,
    selected: bool,
    vertical: bool,
) -> bool {
    let ink = kit::Ink::new(theme);
    let size = if vertical {
        // Sizing to the label rather than a fixed slot: "After Capture" is far
        // wider than "Text", and a fixed width makes the long ones collide.
        let font = theme.font(Text::Caption);
        let width = ui.ctx().fonts_mut(|fonts| {
            fonts
                .layout_no_wrap(label.to_owned(), font, ink.text)
                .size()
                .x
        });
        Vec2::new(
            (width + Space::LG).max(NAV_ITEM_WIDTH),
            TOOLBAR_HEIGHT - Space::SM * 2.0,
        )
    } else {
        Vec2::new(ui.available_width(), 36.0)
    };
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    kit::activated(ui, &mut response);
    response
        .widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, true, selected, label));
    let fill = if selected {
        ink.navigation_selected
    } else if response.is_pointer_button_down_on() {
        ink.control_press
    } else if response.hovered() {
        ink.control_hover
    } else {
        Color32::TRANSPARENT
    };
    ui.painter()
        .rect_filled(rect, crate::theme::corner(10.0), fill);
    let text_tint = if selected || response.hovered() {
        ink.text
    } else {
        ink.muted
    };
    let icon_tint = if selected { ink.accent } else { text_tint };
    if vertical {
        icons.draw(
            ui.painter(),
            icon,
            egui::pos2(rect.center().x, rect.top() + 15.0),
            17.0,
            icon_tint,
        );
        ui.painter().text(
            egui::pos2(rect.center().x, rect.bottom() - 10.0),
            Align2::CENTER_CENTER,
            label,
            theme.font(Text::Caption),
            text_tint,
        );
    } else {
        icons.draw(
            ui.painter(),
            icon,
            egui::pos2(rect.left() + 17.0, rect.center().y),
            16.0,
            icon_tint,
        );
        ui.painter().text(
            egui::pos2(rect.left() + 38.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            theme.font(Text::Label),
            text_tint,
        );
    }
    if response.has_focus() {
        crate::paint::focus_ring(ui.painter(), rect, 10.0, &theme.palette);
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.clicked()
}

fn draw_body(
    ui: &mut egui::Ui,
    theme: &Theme,
    icons: &IconStore,
    app_icon: Option<&TextureHandle>,
    input: &SettingsInput<'_>,
    state: &mut PaneState<'_>,
) -> SettingsEdits {
    let mut edits = SettingsEdits::default();
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(
            kit::Metrics::PAGE_PAD_X as i8,
            kit::Metrics::PAGE_PAD_Y as i8,
        ))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            match *state.pane {
                Pane::Capture => {
                    edits.capture = draw_capture(ui, theme, input.capture);
                }
                Pane::Scenes => {
                    edits.scenes = draw_scenes(ui, theme, input, state.scenes);
                }
                Pane::AfterCapture => {
                    edits.after_capture =
                        draw_after_capture(ui, theme, input.after_capture, &mut edits.open_sharing);
                }
                Pane::Recording => {
                    edits.recording = draw_recording(ui, theme, input.recording.clone());
                }
                Pane::RecentCapturesOverlay => {
                    edits.recent_captures_overlay = draw_recent_captures_overlay(
                        ui,
                        theme,
                        input.recent_captures_overlay,
                        input.platform,
                    );
                }
                // OCR gets an ordinary pane here rather than a settings window
                // of its own: a second settings surface is a second place to
                // look, and the two would immediately disagree about theme,
                // focus, and which one the tray item opens.
                Pane::TextRecognition => {
                    edits.replay_ocr_onboarding = OcrSettings.body(ui, theme).show_onboarding;
                }
                Pane::Shortcuts => {
                    edits.shortcuts =
                        draw_shortcuts(ui, theme, icons, input.shortcuts, state.recording);
                }
                Pane::About => draw_about(ui, theme, app_icon, input.build),
            }
        });
    edits
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

/// The modifier that inverts a capture preference for one shot.
/// The modifier that suppresses a behaviour for a single capture.
const fn one_shot_modifier(platform: SettingsPlatform) -> &'static str {
    match platform {
        SettingsPlatform::MacOs => "Option",
        SettingsPlatform::Windows | SettingsPlatform::Linux => "Alt",
    }
}

fn draw_capture(
    ui: &mut egui::Ui,
    theme: &Theme,
    current: CaptureSettings,
) -> Option<CaptureSettings> {
    let mut settings = current;
    kit::page(ui, theme, "Capture", None, |ui| {
        kit::section(ui, theme, Some("Window"), |ui| {
            kit::row_with_help(
                ui,
                theme,
                "Drop shadow",
                "Captures the shadow the system draws around a window.",
                |ui| {
                    kit::switch(ui, theme, &mut settings.window_shadow, true);
                },
            );
        });
    });
    (settings != current).then_some(settings)
}

// ---------------------------------------------------------------------------
// Scenes
// ---------------------------------------------------------------------------

fn draw_scenes(
    ui: &mut egui::Ui,
    theme: &Theme,
    input: &SettingsInput<'_>,
    pane: &mut ScenesPane,
) -> Vec<ScenesEvent> {
    let platform = match input.platform {
        SettingsPlatform::MacOs => PreviewPlatform::MacOs,
        SettingsPlatform::Windows => PreviewPlatform::Windows,
        SettingsPlatform::Linux => PreviewPlatform::Linux,
    };
    pane.ui(ui, theme, &input.scenes, platform)
}

// ---------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------

/// Draws the Recording pane, which owns every interaction-overlay preference.
///
/// The pane is a thin host: the panel itself is the one renderer used by the
/// settings window, the deterministic harness, and the golden plan, so what a
/// reviewer sees in a golden is the surface the user actually gets.
fn draw_recording(
    ui: &mut egui::Ui,
    theme: &Theme,
    pane: RecordingPane,
) -> Vec<RecordingSettingsAction> {
    egui::ScrollArea::vertical()
        .id_salt("settings-recording-pane")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let panel = RecordingSettingsPanel::new(pane.settings, pane.capabilities, theme)
                .with_active_recording(pane.active);
            match pane.camera.as_deref() {
                Some(camera) => panel.with_camera(camera.model()).show(ui).actions,
                None => panel.show(ui).actions,
            }
        })
        .inner
}

// ---------------------------------------------------------------------------
// Recent Captures
// ---------------------------------------------------------------------------

fn draw_recent_captures_overlay(
    ui: &mut egui::Ui,
    theme: &Theme,
    current: RecentCapturesOverlaySettings,
    platform: SettingsPlatform,
) -> Option<RecentCapturesOverlaySettings> {
    let mut settings = current;
    let modifier = one_shot_modifier(platform);

    kit::page(ui, theme, "Recent Captures", None, |ui| {
        kit::section(ui, theme, Some("Placement"), |ui| {
            kit::row(ui, theme, "Screen side", |ui| {
                kit::segmented(
                    ui,
                    theme,
                    &mut settings.placement,
                    &[
                        (RecentCapturesPlacement::Left, "Left"),
                        (RecentCapturesPlacement::Right, "Right"),
                    ],
                );
                ui.add_space(Space::SM);
                placement_preview(ui, theme, settings.placement);
            });
            kit::divider(ui, theme);
            kit::row(ui, theme, "Active display", |ui| {
                kit::checkbox(
                    ui,
                    theme,
                    &mut settings.follow_active_display,
                    "Follow the pointer",
                    true,
                )
                .on_hover_text(
                    "Use the display holding the pointer or the current capture. \
                     Falls back to the primary display if it disconnects.",
                );
            });
        });

        kit::section(ui, theme, Some("Card size"), |ui| {
            kit::row(ui, theme, "Width", |ui| {
                kit::slider(
                    ui,
                    theme,
                    &mut settings.card_width,
                    crate::stack::CardMetrics::MIN_WIDTH..=crate::stack::CardMetrics::MAX_WIDTH,
                    |value| format!("{value:.0} pt"),
                );
            });
            kit::row(ui, theme, "Presets", |ui| {
                for (label, width) in [
                    ("Compact", crate::stack::CardMetrics::MIN_WIDTH),
                    ("Preferred", crate::stack::CardMetrics::PREFERRED_WIDTH),
                    ("Large", crate::stack::CardMetrics::MAX_WIDTH),
                ] {
                    let selected = (settings.card_width - width).abs() < 1.0;
                    let kind = if selected {
                        kit::ButtonKind::Primary
                    } else {
                        kit::ButtonKind::Secondary
                    };
                    if kit::small_button(ui, theme, label, kind, true).clicked() {
                        settings.card_width = width;
                    }
                }
            });
        });

        kit::section(ui, theme, Some("Closing"), |ui| {
            kit::row(ui, theme, "Automatically", |ui| {
                kit::switch(ui, theme, &mut settings.auto_close_enabled, true);
            });
            let auto = settings.auto_close_enabled;
            kit::row(ui, theme, "Then", |ui| {
                let label = match settings.auto_close_action {
                    RecentCapturesAutoCloseAction::Hide => "Hide when safely retained",
                    RecentCapturesAutoCloseAction::SaveThenHide => "Save, then hide",
                };
                ui.add_enabled_ui(auto, |ui| {
                    kit::dropdown(
                        ui,
                        theme,
                        "recent-captures-auto-close-action",
                        label,
                        210.0,
                        |ui| {
                            for (value, text) in [
                                (
                                    RecentCapturesAutoCloseAction::Hide,
                                    "Hide when safely retained",
                                ),
                                (
                                    RecentCapturesAutoCloseAction::SaveThenHide,
                                    "Save, then hide",
                                ),
                            ] {
                                if kit::menu_item(
                                    ui,
                                    theme,
                                    settings.auto_close_action == value,
                                    text,
                                )
                                .clicked()
                                {
                                    settings.auto_close_action = value;
                                }
                            }
                        },
                    );
                });
            });
            kit::row_with_help(
                ui,
                theme,
                "After",
                "A card never closes while it holds the only retained copy.",
                |ui| {
                    ui.add_enabled_ui(auto, |ui| {
                        let mut seconds = settings.auto_close_seconds as f32;
                        if kit::slider(ui, theme, &mut seconds, 5.0..=3600.0, format_seconds)
                            .changed()
                        {
                            settings.auto_close_seconds = seconds.round() as u32;
                        }
                    });
                },
            );
        });

        kit::section(ui, theme, Some("Actions"), |ui| {
            kit::row_with_help(
                ui,
                theme,
                "After a drag",
                &format!("Hold {modifier} while dragging to keep the card."),
                |ui| {
                    kit::checkbox(ui, theme, &mut settings.close_after_drag, "Close", true);
                },
            );
            kit::divider(ui, theme);
            kit::row_with_help(
                ui,
                theme,
                "After an upload",
                "Needs a cloud provider. Failed uploads always keep the card.",
                |ui| {
                    kit::checkbox(ui, theme, &mut settings.close_after_upload, "Close", false);
                },
            );
            kit::divider(ui, theme);
            kit::row_with_help(
                ui,
                theme,
                "Save button",
                &format!("Hold {modifier} to use the other behaviour once."),
                |ui| {
                    let label = match settings.save_behavior {
                        RecentCapturesSaveBehavior::ExportLocation => "Save to Export Location",
                        RecentCapturesSaveBehavior::ChooseDestination => "Choose destination",
                    };
                    kit::dropdown(ui, theme, "recent-captures-save", label, 210.0, |ui| {
                        for (value, text) in [
                            (
                                RecentCapturesSaveBehavior::ExportLocation,
                                "Save to Export Location",
                            ),
                            (
                                RecentCapturesSaveBehavior::ChooseDestination,
                                "Choose destination",
                            ),
                        ] {
                            if kit::menu_item(ui, theme, settings.save_behavior == value, text)
                                .clicked()
                            {
                                settings.save_behavior = value;
                            }
                        }
                    });
                },
            );
        });
    });

    (settings != current).then_some(settings.normalized())
}

fn format_seconds(value: f32) -> String {
    let seconds = value.round() as u32;
    if seconds < 60 {
        format!("{seconds} s")
    } else if seconds.is_multiple_of(60) {
        format!("{} min", seconds / 60)
    } else {
        format!("{} min {} s", seconds / 60, seconds % 60)
    }
}

fn placement_preview(ui: &mut egui::Ui, theme: &Theme, placement: RecentCapturesPlacement) {
    let ink = kit::Ink::new(theme);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(58.0, 36.0), Sense::hover());
    ui.painter().rect(
        rect,
        crate::theme::corner(5.0),
        ink.page,
        egui::Stroke::new(1.0, ink.control_stroke),
        egui::StrokeKind::Inside,
    );
    let left = matches!(placement, RecentCapturesPlacement::Left);
    for index in 0..3 {
        let width = 18.0;
        let height = 11.0;
        let x = if left {
            rect.left() + 4.0
        } else {
            rect.right() - width - 4.0
        };
        let y = rect.bottom() - 4.0 - height - index as f32 * 3.5;
        let card = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(width, height));
        ui.painter().rect_filled(
            card,
            crate::theme::corner(2.5),
            if index == 0 { ink.accent } else { ink.control },
        );
    }
}

// ---------------------------------------------------------------------------
// Shortcuts
// ---------------------------------------------------------------------------

fn draw_shortcuts(
    ui: &mut egui::Ui,
    theme: &Theme,
    icons: &IconStore,
    rows: &[ShortcutRow],
    recording: &mut Option<String>,
) -> Vec<ShortcutEdit> {
    let mut edits = Vec::new();

    // Read the keyboard once, before any row draws, so an armed row cannot
    // swallow a chord that a later row also sees.
    let captured = recording
        .as_ref()
        .and_then(|_| ui.ctx().input(capture_chord));

    if let Some(id) = recording.clone() {
        match captured {
            Some(Chord::Cancelled) => *recording = None,
            Some(Chord::Cleared) => {
                edits.push(ShortcutEdit::Clear { id });
                *recording = None;
            }
            Some(Chord::Pressed(accelerator)) => {
                edits.push(ShortcutEdit::Set { id, accelerator });
                *recording = None;
            }
            None => {}
        }
    }

    let subtitle = if recording.is_some() {
        "Press a combination. Esc cancels, Delete unassigns."
    } else {
        "Click a shortcut to record a new combination."
    };
    let all_default = rows.iter().all(|row| row.is_default);
    let split = rows
        .iter()
        .position(|row| row.id.starts_with("record."))
        .unwrap_or(rows.len());

    kit::page(ui, theme, "Shortcuts", Some(subtitle), |ui| {
        if split > 0 {
            draw_shortcut_group(
                ui,
                theme,
                icons,
                "Screenshots",
                Icon::Viewfinder,
                &rows[..split],
                recording,
                &mut edits,
            );
        }
        if split < rows.len() {
            draw_shortcut_group(
                ui,
                theme,
                icons,
                "Recording",
                Icon::Video,
                &rows[split..],
                recording,
                &mut edits,
            );
        }
        kit::trailing(ui, |ui| {
            if kit::button(
                ui,
                theme,
                "Reset All",
                kit::ButtonKind::Secondary,
                !all_default,
            )
            .clicked()
            {
                *recording = None;
                edits.push(ShortcutEdit::ResetAll);
            }
        });
    });

    edits
}

#[allow(clippy::too_many_arguments)]
fn draw_shortcut_group(
    ui: &mut egui::Ui,
    theme: &Theme,
    icons: &IconStore,
    title: &str,
    icon: Icon,
    rows: &[ShortcutRow],
    recording: &mut Option<String>,
    edits: &mut Vec<ShortcutEdit>,
) {
    let ink = kit::Ink::new(theme);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = Space::XS;
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
        icons.draw(ui.painter(), icon, rect.center(), 14.0, ink.muted);
        ui.label(
            RichText::new(title)
                .font(theme.font(Text::Label))
                .color(ink.muted),
        );
    });
    ui.add_space(Space::XS);
    kit::card(ui, theme, |ui| {
        ui.set_width(ui.available_width());
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                kit::divider(ui, theme);
            }
            draw_shortcut_row(ui, theme, row, recording, edits);
        }
    });
}

fn draw_shortcut_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    row: &ShortcutRow,
    recording: &mut Option<String>,
    edits: &mut Vec<ShortcutEdit>,
) {
    let ink = kit::Ink::new(theme);
    let armed = recording.as_deref() == Some(row.id.as_str());
    ui.horizontal(|ui| {
        ui.set_min_height(kit::Metrics::ROW);
        ui.spacing_mut().item_spacing.x = Space::XS;
        ui.label(
            RichText::new(&row.label)
                .font(theme.font(Text::Label))
                .color(ink.text_for(row.usable)),
        );
        kit::trailing(ui, |ui| {
            if kit::small_button(ui, theme, "Reset", kit::ButtonKind::Quiet, !row.is_default)
                .on_hover_text("Back to the shipped default")
                .clicked()
            {
                *recording = None;
                edits.push(ShortcutEdit::Reset { id: row.id.clone() });
            }
            if kit::small_button(
                ui,
                theme,
                "Clear",
                kit::ButtonKind::Quiet,
                !row.accelerator.is_empty(),
            )
            .on_hover_text("Leave this action unassigned")
            .clicked()
            {
                *recording = None;
                edits.push(ShortcutEdit::Clear { id: row.id.clone() });
            }
            let caption = if armed {
                "Press keys…"
            } else if row.symbols.is_empty() {
                "Unassigned"
            } else {
                row.symbols.as_str()
            };
            let response =
                kit::key_cap_button(ui, theme, caption, armed || !row.symbols.is_empty());
            let accessible = format!("{}: {caption}", row.label);
            response
                .widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, accessible.clone()));
            if response.clicked() {
                *recording = if armed { None } else { Some(row.id.clone()) };
            }
        });
    });
    if let Some(problem) = &row.problem {
        kit::status(ui, theme, kit::Tone::Error, problem);
    } else if let Some(advisory) = &row.advisory {
        kit::status(ui, theme, kit::Tone::Warning, advisory);
    } else if !row.usable {
        kit::status(ui, theme, kit::Tone::Info, "Unavailable in this session.");
    }
}

// ---------------------------------------------------------------------------
// After Capture
// ---------------------------------------------------------------------------

fn draw_after_capture(
    ui: &mut egui::Ui,
    theme: &Theme,
    rows: &[AfterCaptureRow],
    open_sharing: &mut bool,
) -> Vec<AfterCaptureEdit> {
    let mut edits = Vec::new();
    kit::page(
        ui,
        theme,
        "After Capture",
        Some("Copy runs first; every action uses the same finalized capture."),
        |ui| {
            if ui.available_width() >= kit::Metrics::NARROW {
                draw_after_capture_table(ui, theme, rows, &mut edits);
            } else {
                draw_after_capture_cards(ui, theme, rows, &mut edits);
            }
            // The one row above that depends on a *remote* service gets its
            // configuration where the user is already reading about it, rather
            // than somewhere they would have to be told about.
            kit::section(ui, theme, Some("Sharing"), |ui| {
                kit::row_with_help(
                    ui,
                    theme,
                    "Upload target",
                    "Your own S3-compatible bucket. Credentials stay in the platform keychain.",
                    |ui| {
                        if kit::button(ui, theme, "Configure…", kit::ButtonKind::Secondary, true)
                            .clicked()
                        {
                            *open_sharing = true;
                        }
                    },
                );
            });
        },
    );
    edits
}

const AFTER_CAPTURE_COLUMN: f32 = 124.0;

fn draw_after_capture_table(
    ui: &mut egui::Ui,
    theme: &Theme,
    rows: &[AfterCaptureRow],
    edits: &mut Vec<AfterCaptureEdit>,
) {
    let ink = kit::Ink::new(theme);
    kit::section(ui, theme, Some("Actions"), |ui| {
        ui.set_width(ui.available_width());
        let heading = |ui: &mut egui::Ui, text: &str| {
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(AFTER_CAPTURE_COLUMN, 18.0), Sense::hover());
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                text,
                theme.font(Text::Caption),
                ink.muted,
            );
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.add_space(1.0);
            kit::trailing(ui, |ui| {
                heading(ui, "Recording");
                heading(ui, "Screenshot");
            });
        });
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                kit::divider(ui, theme);
            }
            ui.horizontal(|ui| {
                ui.set_min_height(kit::Metrics::ROW);
                ui.spacing_mut().item_spacing.x = 0.0;
                let label = ui.label(
                    RichText::new(&row.label)
                        .font(theme.font(Text::Body))
                        .color(ink.text),
                );
                label.on_hover_text(&row.description);
                kit::trailing(ui, |ui| {
                    ui.allocate_ui_with_layout(
                        Vec2::new(AFTER_CAPTURE_COLUMN, kit::Metrics::ROW),
                        Layout::top_down(Align::Center),
                        |ui| {
                            draw_after_capture_cell(
                                ui,
                                theme,
                                row,
                                AfterCaptureMedia::Recording,
                                &row.recording,
                                edits,
                            );
                        },
                    );
                    ui.allocate_ui_with_layout(
                        Vec2::new(AFTER_CAPTURE_COLUMN, kit::Metrics::ROW),
                        Layout::top_down(Align::Center),
                        |ui| {
                            draw_after_capture_cell(
                                ui,
                                theme,
                                row,
                                AfterCaptureMedia::Screenshot,
                                &row.screenshot,
                                edits,
                            );
                        },
                    );
                });
            });
        }
    });
}

fn draw_after_capture_cards(
    ui: &mut egui::Ui,
    theme: &Theme,
    rows: &[AfterCaptureRow],
    edits: &mut Vec<AfterCaptureEdit>,
) {
    for row in rows {
        kit::section(ui, theme, None, |ui| {
            ui.set_width(ui.available_width());
            draw_after_capture_label(ui, theme, row);
            for (media, cell) in [
                (AfterCaptureMedia::Screenshot, &row.screenshot),
                (AfterCaptureMedia::Recording, &row.recording),
            ] {
                kit::row(ui, theme, media.label(), |ui| {
                    draw_after_capture_cell(ui, theme, row, media, cell, edits);
                });
            }
        });
    }
}

fn draw_after_capture_label(ui: &mut egui::Ui, theme: &Theme, row: &AfterCaptureRow) {
    let ink = kit::Ink::new(theme);
    ui.label(
        RichText::new(&row.label)
            .font(theme.font(Text::Label))
            .color(ink.text),
    )
    .on_hover_text(&row.description);
}

fn draw_after_capture_cell(
    ui: &mut egui::Ui,
    theme: &Theme,
    row: &AfterCaptureRow,
    media: AfterCaptureMedia,
    cell: &AfterCaptureCell,
    edits: &mut Vec<AfterCaptureEdit>,
) {
    let ink = kit::Ink::new(theme);
    let id = match media {
        AfterCaptureMedia::Screenshot => Some(row.screenshot_id.as_str()),
        AfterCaptureMedia::Recording => row.recording_id.as_deref(),
    };
    if let Some(id) = id.filter(|_| cell.available) {
        let mut enabled = cell.enabled;
        let response = kit::checkbox(ui, theme, &mut enabled, "", true);
        let accessible = format!(
            "{} for {}: {}",
            row.label,
            media.label(),
            if enabled { "enabled" } else { "disabled" }
        );
        response.widget_info(|| {
            WidgetInfo::selected(WidgetType::Checkbox, true, enabled, accessible.clone())
        });
        if response.changed() {
            edits.push(AfterCaptureEdit {
                id: id.to_owned(),
                media,
                enabled,
            });
        }
        response.on_hover_text(format!("{} for {}", row.label, media.label()));
        return;
    }

    let reason = cell
        .unavailable_reason
        .as_deref()
        .unwrap_or("This action is unavailable in this build.");
    let response = ui.label(
        RichText::new(unavailable_summary(reason))
            .font(theme.font(Text::Caption))
            .color(ink.faint),
    );
    let accessible = format!("{} for {} unavailable: {reason}", row.label, media.label());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, false, accessible.clone()));
    response.on_hover_text(reason);
}

/// A three-word summary of a full unavailability reason.
///
/// The long reason is the accessible name and the hover text; this is what fits
/// under a checkbox column. Matched most specific first, because several
/// reasons legitimately mention the same feature.
fn unavailable_summary(reason: &str) -> &'static str {
    if reason.contains("provider") {
        "Provider required"
    } else if reason.contains("does not apply to a recording")
        || reason.contains("only for screenshots")
        || reason.contains("only to screenshots")
    {
        "Screenshots only"
    } else if reason.contains("clipboard flavour") {
        "No file clipboard"
    } else if reason.contains("second automatic export") {
        "Already saved"
    } else if reason.contains("Screen recording") {
        "Recording not available"
    } else if reason.contains("Pin to Screen") {
        "Not implemented"
    } else {
        "Not available here"
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
    capture_events(&input.events)
}

fn capture_events(events: &[egui::Event]) -> Option<Chord> {
    for event in events {
        let egui::Event::Key {
            key,
            physical_key,
            pressed: true,
            repeat: false,
            modifiers,
            ..
        } = event
        else {
            continue;
        };
        if is_modifier_key(*key) || physical_key.is_some_and(is_modifier_key) {
            continue;
        }
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

const fn is_modifier_key(key: Key) -> bool {
    matches!(
        key,
        Key::ShiftLeft
            | Key::ShiftRight
            | Key::ControlLeft
            | Key::ControlRight
            | Key::AltLeft
            | Key::AltRight
            | Key::SuperLeft
            | Key::SuperRight
    )
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
    if modifiers.ctrl {
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
    Theme::for_appearance(appearance_for_theme(
        ui.ctx().system_theme(),
        ui.visuals().dark_mode,
    ))
}

const fn appearance_for_theme(
    system_theme: Option<egui::Theme>,
    fallback_dark: bool,
) -> Appearance {
    match system_theme {
        Some(egui::Theme::Dark) => Appearance::Dark,
        Some(egui::Theme::Light) => Appearance::Light,
        None if fallback_dark => Appearance::Dark,
        None => Appearance::Light,
    }
}

#[cfg(target_os = "linux")]
const fn appearance_for_mode(mode: dark_light::Mode, fallback: Appearance) -> Appearance {
    match mode {
        dark_light::Mode::Dark => Appearance::Dark,
        dark_light::Mode::Light => Appearance::Light,
        dark_light::Mode::Unspecified => fallback,
    }
}

fn draw_about(ui: &mut egui::Ui, theme: &Theme, icon: Option<&TextureHandle>, build: BuildInfo) {
    let ink = kit::Ink::new(theme);
    ui.add_space(Space::MD);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = Space::LG;
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(APP_ICON_SIZE), Sense::hover());
        if let Some(icon) = icon {
            ui.painter().image(
                icon.id(),
                rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = Space::HAIR;
            ui.label(
                RichText::new("Scrozz")
                    .font(theme.font(Text::Display))
                    .color(ink.text),
            );
            ui.label(
                RichText::new("Screenshots and screen recording, without limits.")
                    .font(theme.font(Text::Body))
                    .color(ink.muted),
            );
            ui.add_space(Space::SM);
            ui.label(
                RichText::new(build.label())
                    .font(theme.font(Text::Label))
                    .color(ink.text),
            );
            ui.label(
                RichText::new("Free forever. Open source.")
                    .font(theme.font(Text::Caption))
                    .color(ink.faint),
            );
        });
    });
}

fn embedded_app_icon() -> egui::ColorImage {
    let image = image::load_from_memory(include_bytes!("../../../../assets/icons/icon-256.png"))
        .expect("the embedded Scrozz icon must be valid PNG")
        .into_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw())
}

/// Headless golden scene for the platform-adaptive After Capture pane.
pub struct AfterCaptureSettingsScene;

impl crate::harness::Scene for AfterCaptureSettingsScene {
    fn name(&self) -> &str {
        "after-capture-settings"
    }

    fn setup(&self, ctx: &egui::Context) {
        crate::theme::install_fonts(ctx);
    }

    fn ui(&self, ui: &mut egui::Ui, ctx: &crate::harness::SceneCtx<'_>) {
        let theme = if ctx.theme == egui::Theme::Dark {
            Theme::for_appearance(Appearance::Dark)
        } else {
            Theme::for_appearance(Appearance::Light)
        };
        crate::theme::install_style(ui.ctx(), &theme);
        ui.painter()
            .rect_filled(ui.max_rect(), 0.0, theme.palette.canvas());
        egui::Frame::new()
            .inner_margin(egui::Margin::same(Space::XXL as i8))
            .show(ui, |ui| {
                ui.heading(
                    RichText::new("Settings")
                        .font(theme.font(Text::Title))
                        .color(theme.palette.text),
                );
                ui.add_space(Space::LG);
                ui.separator();
                ui.add_space(Space::LG);
                let _ = draw_after_capture(ui, &theme, &preview_after_capture_rows(), &mut false);
            });
    }
}

/// Stable After Capture data for deterministic visual review.
#[must_use]
pub fn preview_after_capture_rows() -> Vec<AfterCaptureRow> {
    const COPY_RECORDING_UNAVAILABLE: &str = "Copying a recording needs a platform file-reference clipboard flavour, which is not implemented in this build.";
    const SAVE_RECORDING_UNAVAILABLE: &str = "A recording is already written to its destination when it finalizes, so a second automatic export is not implemented in this build.";
    const UPLOAD_UNAVAILABLE: &str = "Uploading here would hold the shutter open on a remote host. Press Upload on the card instead: it runs on its own worker and copies the link when it lands.";
    const PIN_UNAVAILABLE: &str = "Pin to Screen is not implemented in this build.";
    let available = |enabled| AfterCaptureCell {
        enabled,
        available: true,
        unavailable_reason: None,
    };
    let unavailable = |enabled, reason: &str| AfterCaptureCell {
        enabled,
        available: false,
        unavailable_reason: Some(reason.to_owned()),
    };
    // Presentation moved to Scenes; this pane is destinations and workflow only.
    let mut rows: Vec<AfterCaptureRow> = Vec::new();
    // The recording column mirrors the application's own availability table:
    // the two cells recording actually honours are live, and every other cell
    // names the specific capability that is missing rather than claiming that
    // recording itself is unimplemented.
    rows.extend([
        (
            "show-recent-captures-overlay",
            "Show Recent Captures Overlay",
            "Keep the completed capture in the nonactivating recent-captures corner surface.",
            true,
            true,
            None,
            None,
        ),
        (
            "copy-to-clipboard",
            "Copy to clipboard",
            "Copy screenshot pixels immediately, or a retained recording file reference where supported.",
            true,
            false,
            None,
            Some(COPY_RECORDING_UNAVAILABLE),
        ),
        (
            "save-automatically",
            "Save automatically",
            "Write once to the configured export location with collision-safe naming.",
            false,
            false,
            None,
            Some(SAVE_RECORDING_UNAVAILABLE),
        ),
        (
            "upload-and-copy-link",
            "Upload and copy link",
            "Use the configured cloud provider, then copy its shareable link.",
            false,
            false,
            Some(UPLOAD_UNAVAILABLE),
            Some(UPLOAD_UNAVAILABLE),
        ),
        (
            "open-editor",
            "Open Editor",
            "Open the annotation editor for screenshots or the video editor for recordings.",
            false,
            false,
            None,
            None,
        ),
        (
            "pin-to-screen",
            "Pin to Screen",
            "Keep a screenshot visible in a floating window.",
            false,
            false,
            Some(PIN_UNAVAILABLE),
            Some("Pin to Screen holds a still image and does not apply to a recording."),
        ),
    ]
    .into_iter()
    .map(
        |(
            slug,
            label,
            description,
            screenshot_enabled,
            recording_enabled,
            screenshot_unavailable,
            recording_unavailable,
        )| {
            let screenshot = screenshot_unavailable.map_or_else(
                || available(screenshot_enabled),
                |reason| unavailable(screenshot_enabled, reason),
            );
            let recording = recording_unavailable.map_or_else(
                || available(recording_enabled),
                |reason| unavailable(recording_enabled, reason),
            );
            AfterCaptureRow {
                screenshot_id: format!("capture.{slug}"),
                recording_id: (slug != "pin-to-screen").then(|| format!("record.{slug}")),
                label: label.to_owned(),
                description: description.to_owned(),
                screenshot,
                recording,
            }
        },
    )
    .collect::<Vec<_>>());
    rows
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
    fn first_open_and_reopen_both_request_foregrounding() {
        let mut window = SettingsWindow::default();
        assert_ne!(
            settings_viewport_id(),
            egui::ViewportId::ROOT,
            "Settings must remain a child; replacing the root lets eframe stop the app"
        );
        assert_eq!(window.open(), OpenDisposition::FirstOpen);
        assert!(window.open);
        assert!(std::mem::take(&mut window.focus_requested));

        assert_eq!(window.open(), OpenDisposition::Reused);
        assert!(std::mem::take(&mut window.focus_requested));
    }

    #[test]
    fn shortcut_state_changes_never_request_window_focus() {
        let mut window = SettingsWindow::default();
        window.open();
        assert!(std::mem::take(&mut window.focus_requested));

        window.recording = Some("capture.region".to_owned());
        window.recording = None;
        assert!(
            !window.focus_requested,
            "only an explicit Settings action may foreground the viewport"
        );
    }

    #[test]
    fn settings_is_an_explicitly_normal_level_window() {
        let builder = viewport_builder(true);
        assert_eq!(builder.window_level, Some(egui::WindowLevel::Normal));
        assert_eq!(builder.active, Some(true));
        assert_eq!(builder.resizable, Some(true));
        assert_eq!(viewport_builder(false).active, None);
    }

    #[test]
    fn focus_is_emitted_only_for_a_settings_request() {
        let commands = |requested| {
            let ctx = egui::Context::default();
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                request_foreground(ui.ctx(), requested);
            });
            output.textures_delta.clear();
            output
                .viewport_output
                .into_values()
                .flat_map(|viewport| viewport.commands)
                .filter(|command| matches!(command, egui::ViewportCommand::Focus))
                .count()
        };
        assert_eq!(commands(false), 0);
        assert_eq!(commands(true), 1);
    }

    #[test]
    fn settings_navigation_stays_left_aligned_on_every_platform() {
        assert_eq!(SettingsPlatform::MacOs.navigation(), Navigation::Sidebar);
        assert_eq!(SettingsPlatform::Windows.navigation(), Navigation::Sidebar);
        assert_eq!(SettingsPlatform::Linux.navigation(), Navigation::Sidebar);
    }

    #[test]
    fn settings_appearance_tracks_the_viewport_visuals() {
        assert_eq!(appearance_for_theme(None, false), Appearance::Light);
        assert_eq!(appearance_for_theme(None, true), Appearance::Dark);
        assert_eq!(
            appearance_for_theme(Some(egui::Theme::Light), true),
            Appearance::Light
        );
        assert_eq!(
            appearance_for_theme(Some(egui::Theme::Dark), false),
            Appearance::Dark
        );
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
        assert_eq!(press(Key::A, all), Some("Ctrl+Alt+Shift+Cmd+A".to_owned()));
    }

    fn key_event(
        key: Key,
        physical_key: Option<Key>,
        pressed: bool,
        modifiers: Modifiers,
    ) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key,
            pressed,
            repeat: false,
            modifiers,
        }
    }

    fn captured(events: Vec<egui::Event>) -> Option<Chord> {
        capture_events(&events)
    }

    #[test]
    fn left_and_right_modifier_keys_never_commit_as_the_primary_key() {
        for modifier in [
            Key::ShiftLeft,
            Key::ShiftRight,
            Key::ControlLeft,
            Key::ControlRight,
            Key::AltLeft,
            Key::AltRight,
            Key::SuperLeft,
            Key::SuperRight,
        ] {
            assert_eq!(
                captured(vec![
                    key_event(
                        modifier,
                        Some(modifier),
                        true,
                        Modifiers {
                            alt: matches!(modifier, Key::AltLeft | Key::AltRight),
                            ctrl: matches!(modifier, Key::ControlLeft | Key::ControlRight),
                            shift: matches!(modifier, Key::ShiftLeft | Key::ShiftRight),
                            mac_cmd: matches!(modifier, Key::SuperLeft | Key::SuperRight),
                            command: matches!(modifier, Key::SuperLeft | Key::SuperRight),
                        },
                    ),
                    key_event(modifier, Some(modifier), false, Modifiers::NONE,),
                ]),
                None,
                "{modifier:?} must remain a modifier"
            );
        }
    }

    #[test]
    fn either_side_of_every_modifier_produces_the_same_ordered_chord() {
        let all = Modifiers {
            alt: true,
            ctrl: true,
            shift: true,
            mac_cmd: true,
            command: true,
        };
        for modifiers in [
            [
                Key::ControlLeft,
                Key::AltLeft,
                Key::ShiftLeft,
                Key::SuperLeft,
            ],
            [
                Key::SuperRight,
                Key::ShiftRight,
                Key::AltRight,
                Key::ControlRight,
            ],
        ] {
            let mut events: Vec<_> = modifiers
                .into_iter()
                .map(|key| key_event(key, Some(key), true, all))
                .collect();
            events.push(key_event(Key::Num0, Some(Key::Num0), true, all));
            assert_eq!(
                captured(events),
                Some(Chord::Pressed("Ctrl+Alt+Shift+Cmd+0".to_owned()))
            );
        }
    }

    #[test]
    fn key_up_events_cannot_replace_the_captured_non_modifier_key() {
        let mut shifted_command = CMD;
        shifted_command.shift = true;
        assert_eq!(
            captured(vec![
                key_event(Key::ShiftLeft, Some(Key::ShiftLeft), true, shifted_command,),
                key_event(Key::Num0, Some(Key::Num0), true, shifted_command),
                key_event(Key::Num0, Some(Key::Num0), false, shifted_command),
                key_event(Key::ShiftLeft, Some(Key::ShiftLeft), false, CMD),
                key_event(Key::SuperLeft, Some(Key::SuperLeft), false, Modifiers::NONE,),
            ]),
            Some(Chord::Pressed("Shift+Cmd+0".to_owned()))
        );
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
    fn finishing_a_chord_on_the_armed_row_does_not_re_arm_it() {
        // The cap that armed recording is the widget holding focus while the
        // chord is typed, and egui presses a focused widget on Space or Return
        // without checking modifiers. Ctrl+Return would then commit the chord
        // and, in the same frame, re-arm the row — or press Reset or Clear if
        // one of those held focus. Whichever control the keyboard is on, a
        // chord belongs to the recorder and must change nothing.
        let theme = Theme::dark();
        let row = ShortcutRow {
            id: "capture.region".to_owned(),
            label: "Region".to_owned(),
            accelerator: "Cmd+5".to_owned(),
            symbols: "\u{2318}5".to_owned(),
            // Not the shipped default, so Reset is live and focusable too.
            is_default: false,
            usable: true,
            problem: None,
            advisory: None,
        };

        let draw = |ctx: &egui::Context,
                    input: egui::RawInput,
                    recording: &mut Option<String>,
                    edits: &mut Vec<ShortcutEdit>| {
            let mut output = ctx.run_ui(input, |ui| {
                draw_shortcut_row(ui, &theme, &row, recording, edits);
            });
            output.textures_delta.clear();
        };

        // Reset, Clear and the key cap: every focusable control in the row.
        for stop in 1..=3 {
            let ctx = egui::Context::default();
            crate::theme::install_fonts(&ctx);
            let mut recording = Some(row.id.clone());
            let mut edits = Vec::new();
            draw(&ctx, egui::RawInput::default(), &mut recording, &mut edits);

            for _ in 0..stop {
                draw(
                    &ctx,
                    egui::RawInput {
                        events: vec![key_event(Key::Tab, None, true, Modifiers::NONE)],
                        ..Default::default()
                    },
                    &mut recording,
                    &mut edits,
                );
            }
            let focused = ctx.memory(egui::Memory::focused);
            assert!(focused.is_some(), "tab {stop} should land on a control");
            edits.clear();

            for key in [Key::Enter, Key::Space] {
                ctx.memory_mut(|memory| memory.request_focus(focused.unwrap()));
                draw(
                    &ctx,
                    egui::RawInput {
                        events: vec![key_event(
                            key,
                            None,
                            true,
                            Modifiers {
                                ctrl: true,
                                ..Modifiers::NONE
                            },
                        )],
                        ..Default::default()
                    },
                    &mut recording,
                    &mut edits,
                );
                assert_eq!(
                    recording.as_deref(),
                    Some(row.id.as_str()),
                    "{key:?} in a chord must leave the armed row armed (control {stop})"
                );
                assert!(
                    edits.is_empty(),
                    "{key:?} in a chord must not press a focused button (control {stop})"
                );
            }
        }
    }

    #[test]
    fn a_closed_window_forgets_every_half_finished_edit() {
        // Otherwise reopening Settings would silently eat the first chord the
        // user typed into whatever row happened to be armed last time — and a
        // rename left armed keeps claiming the keyboard, so the app answers no
        // global shortcut at all until Settings is opened and closed again.
        let mut window = SettingsWindow {
            recording: Some("capture.region".to_owned()),
            ..SettingsWindow::default()
        };
        window.scenes.begin_rename_for_test("studio", "Studio");
        assert!(window.is_editing_text());

        let ctx = egui::Context::default();
        let edits = window.show(&ctx, &SettingsInput::default());

        assert!(edits.is_empty());
        assert!(!window.is_recording());
        assert!(
            !window.is_editing_text(),
            "a closed window must hand the keyboard back"
        );
    }

    #[test]
    fn closing_the_viewport_hands_the_keyboard_back() {
        // The close-button path, rather than the already-closed early return:
        // while a rename is live the host suspends global shortcuts, so a
        // rename that survives the close makes the whole app deaf to them.
        let mut window = SettingsWindow::default();
        window.open();
        window.scenes.begin_rename_for_test("studio", "Studio");
        window.recording = Some("capture.region".to_owned());
        assert!(window.is_editing_text());
        assert!(window.is_recording());

        window.set_open(false);

        assert!(!window.is_open());
        assert!(!window.is_recording());
        assert!(
            !window.is_editing_text(),
            "global shortcuts must work again once Settings is gone"
        );
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

    #[test]
    fn preview_matrix_uses_approved_vocabulary_and_explicit_unavailable_states() {
        let rows = preview_after_capture_rows();
        assert_eq!(rows.len(), 6);
        assert!(
            rows.iter().all(|row| !row.label.contains("Quick Access")),
            "{rows:?}"
        );
        // Presentation belongs to Scenes; After Capture is destinations only.
        assert!(
            rows.iter().all(|row| !row.label.contains("Smart Frame")),
            "{rows:?}"
        );
        for row in &rows {
            for cell in [&row.screenshot, &row.recording] {
                assert!(
                    cell.available || cell.unavailable_reason.is_some(),
                    "{} has a mystery disabled cell",
                    row.label
                );
            }
        }
        let pin = rows
            .iter()
            .find(|row| row.label == "Pin to Screen")
            .unwrap();
        assert!(pin.recording_id.is_none());
        assert!(!pin.recording.available);

        // The recording column must never say recording itself is missing:
        // the overlay and the editor are wired, and the other cells name their
        // own specific gap.
        for row in &rows {
            let reason = row.recording.unavailable_reason.as_deref().unwrap_or("");
            assert!(
                !reason.contains("Screen recording is not implemented"),
                "{} still claims recording is unimplemented",
                row.label
            );
        }
        let overlay = rows
            .iter()
            .find(|row| row.label == "Show Recent Captures Overlay")
            .unwrap();
        assert!(overlay.recording.available && overlay.recording.enabled);
        assert_eq!(
            overlay.recording_id.as_deref(),
            Some("record.show-recent-captures-overlay")
        );

        let editor = rows.iter().find(|row| row.label == "Open Editor").unwrap();
        assert!(editor.recording.available && !editor.recording.enabled);
        assert_eq!(editor.recording_id.as_deref(), Some("record.open-editor"));
    }

    #[test]
    fn settings_edits_report_both_kinds_of_change() {
        let mut edits = SettingsEdits::default();
        assert!(edits.is_empty());
        edits.after_capture.push(AfterCaptureEdit {
            id: "capture.copy-to-clipboard".to_owned(),
            media: AfterCaptureMedia::Screenshot,
            enabled: false,
        });
        assert!(!edits.is_empty());
    }
}
