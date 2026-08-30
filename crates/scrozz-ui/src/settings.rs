//! The ordinary settings window and its About surface.

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

pub use crate::recording_settings::RecordingSettingsAction;

const SETTINGS_VIEWPORT: &str = "scrozz-settings";
const WINDOW_SIZE: Vec2 = Vec2::new(800.0, 600.0);
const MIN_WINDOW_SIZE: Vec2 = Vec2::new(680.0, 520.0);
const APP_ICON_SIZE: f32 = 128.0;
const SIDEBAR_WIDTH: f32 = 188.0;
const TOOLBAR_HEIGHT: f32 = 94.0;

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
            && !self.replay_ocr_onboarding
            && !self.open_sharing
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

/// Desktop whose native Settings convention should be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPlatform {
    /// Icon categories in a top toolbar.
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

    /// Where category navigation belongs on this desktop.
    #[must_use]
    pub const fn navigation(self) -> Navigation {
        match self {
            Self::MacOs => Navigation::TopToolbar,
            Self::Windows | Self::Linux => Navigation::Sidebar,
        }
    }
}

/// Placement selected by [`SettingsPlatform`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Navigation {
    /// macOS preference-window toolbar.
    TopToolbar,
    /// Windows and Linux left navigation.
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
enum Pane {
    #[default]
    AfterCapture,
    Recording,
    RecentCapturesOverlay,
    TextRecognition,
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
pub struct SettingsWindow {
    open: bool,
    focus_requested: bool,
    app_icon: Option<TextureHandle>,
    icons: Option<IconStore>,
    pane: Pane,
    recording: Option<String>,
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

    /// Draws the settings viewport while it is open.
    ///
    /// Returns the edits the user asked for this frame. Nothing is applied here:
    /// the host owns registration, so it decides whether a change survives.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        build: BuildInfo,
        shortcuts: &[ShortcutRow],
        after_capture: &[AfterCaptureRow],
        recording_pane: RecordingPane,
        recent_captures_overlay: RecentCapturesOverlaySettings,
    ) -> SettingsEdits {
        let mut edits = SettingsEdits::default();
        if !self.open {
            self.recording = None;
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
        let platform = self.platform;

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
                build,
                shortcuts,
                after_capture,
                recording_pane.clone(),
                recent_captures_overlay,
                platform,
                &mut self.pane,
                &mut self.recording,
            );
        });

        self.open = open;
        if !self.open {
            self.recording = None;
        }
        edits
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
/// than maintaining mock screenshots.
pub fn render_preview(
    ui: &mut egui::Ui,
    platform: SettingsPlatform,
    icons: &IconStore,
    shortcuts: &[ShortcutRow],
) {
    let theme = theme_for(ui);
    let mut pane = Pane::Shortcuts;
    let mut recording = None;
    let _ = draw_settings(
        ui,
        &theme,
        icons,
        None,
        BuildInfo {
            version: "0.1.0",
            build: "100",
        },
        shortcuts,
        &[],
        RecordingPane::default(),
        RecentCapturesOverlaySettings::default(),
        platform,
        &mut pane,
        &mut recording,
    );
}

/// Draws the Recent Captures Overlay pane for deterministic visual review.
pub fn render_recent_captures_preview(
    ui: &mut egui::Ui,
    platform: SettingsPlatform,
    icons: &IconStore,
    settings: RecentCapturesOverlaySettings,
) {
    let theme = theme_for(ui);
    let mut pane = Pane::RecentCapturesOverlay;
    let mut recording = None;
    let _ = draw_settings(
        ui,
        &theme,
        icons,
        None,
        BuildInfo {
            version: "0.1.0",
            build: "100",
        },
        &[],
        &[],
        RecordingPane::default(),
        settings,
        platform,
        &mut pane,
        &mut recording,
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

#[allow(clippy::too_many_arguments)]
fn draw_settings(
    ui: &mut egui::Ui,
    theme: &Theme,
    icons: &IconStore,
    app_icon: Option<&TextureHandle>,
    build: BuildInfo,
    shortcuts: &[ShortcutRow],
    after_capture: &[AfterCaptureRow],
    recording_pane: RecordingPane,
    recent_captures_overlay: RecentCapturesOverlaySettings,
    platform: SettingsPlatform,
    pane: &mut Pane,
    recording: &mut Option<String>,
) -> SettingsEdits {
    crate::theme::apply_style(ui.style_mut(), theme);
    ui.painter()
        .rect_filled(ui.max_rect(), 0.0, theme.palette.canvas());
    match platform.navigation() {
        Navigation::TopToolbar => {
            draw_top_navigation(ui, theme, icons, pane, recording);
            let rect = egui::Rect::from_min_max(
                egui::pos2(ui.max_rect().left(), ui.max_rect().top() + TOOLBAR_HEIGHT),
                ui.max_rect().max,
            );
            let mut body = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(rect)
                    .layout(Layout::top_down(Align::LEFT)),
            );
            draw_body(
                &mut body,
                theme,
                icons,
                app_icon,
                build,
                shortcuts,
                after_capture,
                recording_pane.clone(),
                recent_captures_overlay,
                platform,
                pane,
                recording,
            )
        }
        Navigation::Sidebar => {
            let sidebar = egui::Rect::from_min_max(
                ui.max_rect().min,
                egui::pos2(ui.max_rect().left() + SIDEBAR_WIDTH, ui.max_rect().bottom()),
            );
            ui.painter()
                .rect_filled(sidebar, 0.0, theme.palette.card_fill);
            ui.painter().line_segment(
                [sidebar.right_top(), sidebar.right_bottom()],
                egui::Stroke::new(1.0, theme.palette.divider),
            );
            let mut nav = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(sidebar.shrink2(Vec2::new(Space::MD, Space::XL)))
                    .layout(Layout::top_down(Align::LEFT)),
            );
            draw_sidebar_navigation(&mut nav, theme, icons, pane, recording);

            let body_rect = egui::Rect::from_min_max(
                egui::pos2(sidebar.right(), ui.max_rect().top()),
                ui.max_rect().max,
            );
            let mut body = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(body_rect)
                    .layout(Layout::top_down(Align::LEFT)),
            );
            draw_body(
                &mut body,
                theme,
                icons,
                app_icon,
                build,
                shortcuts,
                after_capture,
                recording_pane.clone(),
                recent_captures_overlay,
                platform,
                pane,
                recording,
            )
        }
    }
}

fn draw_top_navigation(
    ui: &mut egui::Ui,
    theme: &Theme,
    icons: &IconStore,
    pane: &mut Pane,
    recording: &mut Option<String>,
) {
    let rect = egui::Rect::from_min_max(
        ui.max_rect().min,
        egui::pos2(ui.max_rect().right(), ui.max_rect().top() + TOOLBAR_HEIGHT),
    );
    ui.painter().rect_filled(rect, 0.0, theme.palette.card_fill);
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        egui::Stroke::new(1.0, theme.palette.divider),
    );
    let mut nav = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(Vec2::new(Space::XXL, Space::SM)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    nav.add_space(Space::LG);
    for (candidate, label, icon) in navigation_items() {
        if navigation_button(
            &mut nav,
            theme,
            icons,
            candidate,
            label,
            icon,
            *pane == candidate,
            true,
        ) {
            *pane = candidate;
            *recording = None;
        }
        nav.add_space(Space::SM);
    }
}

fn draw_sidebar_navigation(
    ui: &mut egui::Ui,
    theme: &Theme,
    icons: &IconStore,
    pane: &mut Pane,
    recording: &mut Option<String>,
) {
    ui.label(
        RichText::new("Scrozz")
            .font(theme.font(Text::Title))
            .color(theme.palette.text),
    );
    ui.add_space(Space::XL);
    for (candidate, label, icon) in navigation_items() {
        if navigation_button(
            ui,
            theme,
            icons,
            candidate,
            label,
            icon,
            *pane == candidate,
            false,
        ) {
            *pane = candidate;
            *recording = None;
        }
        ui.add_space(Space::XS);
    }
}

fn navigation_items() -> [(Pane, &'static str, Icon); 6] {
    [
        (Pane::AfterCapture, "After Capture", Icon::Copy),
        (Pane::Recording, "Recording", Icon::Video),
        (
            Pane::RecentCapturesOverlay,
            "Recent Captures",
            Icon::LayoutGrid,
        ),
        (Pane::TextRecognition, "Text Recognition", Icon::Scan),
        (Pane::Shortcuts, "Shortcuts", Icon::Settings),
        (Pane::About, "About", Icon::AppWindow),
    ]
}

#[allow(clippy::too_many_arguments)]
fn navigation_button(
    ui: &mut egui::Ui,
    theme: &Theme,
    icons: &IconStore,
    _pane: Pane,
    label: &str,
    icon: Icon,
    selected: bool,
    vertical: bool,
) -> bool {
    let size = if vertical {
        Vec2::new(86.0, 72.0)
    } else {
        Vec2::new(ui.available_width(), 42.0)
    };
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let fill = if selected {
        theme.palette.active
    } else if response.hovered() {
        theme.palette.hover
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 10.0, fill);
    let ink = if selected {
        theme.palette.accent_hi
    } else {
        theme.palette.text_muted
    };
    if vertical {
        icons.draw(
            ui.painter(),
            icon,
            egui::pos2(rect.center().x, rect.top() + 24.0),
            26.0,
            ink,
        );
        ui.painter().text(
            egui::pos2(rect.center().x, rect.bottom() - 16.0),
            Align2::CENTER_CENTER,
            label,
            theme.font(Text::Caption),
            ink,
        );
    } else {
        icons.draw(
            ui.painter(),
            icon,
            egui::pos2(rect.left() + 22.0, rect.center().y),
            20.0,
            ink,
        );
        ui.painter().text(
            egui::pos2(rect.left() + 42.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            theme.font(Text::Label),
            ink,
        );
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.clicked()
}

#[allow(clippy::too_many_arguments)]
fn draw_body(
    ui: &mut egui::Ui,
    theme: &Theme,
    icons: &IconStore,
    app_icon: Option<&TextureHandle>,
    build: BuildInfo,
    shortcuts: &[ShortcutRow],
    after_capture: &[AfterCaptureRow],
    recording_pane: RecordingPane,
    recent_captures_overlay: RecentCapturesOverlaySettings,
    platform: SettingsPlatform,
    pane: &Pane,
    recording: &mut Option<String>,
) -> SettingsEdits {
    let mut edits = SettingsEdits::default();
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(Space::XXL as i8, Space::XL as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            match pane {
                Pane::AfterCapture => {
                    edits.after_capture =
                        draw_after_capture(ui, theme, after_capture, &mut edits.open_sharing);
                }
                Pane::Recording => {
                    edits.recording = draw_recording(ui, theme, recording_pane);
                }
                Pane::RecentCapturesOverlay => {
                    edits.recent_captures_overlay =
                        draw_recent_captures_overlay(ui, theme, recent_captures_overlay, platform);
                }
                // OCR gets an ordinary pane here rather than a settings window
                // of its own: a second settings surface is a second place to
                // look, and the two would immediately disagree about theme,
                // focus, and which one the tray item opens.
                Pane::TextRecognition => {
                    edits.replay_ocr_onboarding = OcrSettings.body(ui, theme).show_onboarding;
                }
                Pane::Shortcuts => {
                    edits.shortcuts = draw_shortcuts(ui, theme, icons, shortcuts, recording);
                }
                Pane::About => draw_about(ui, theme, app_icon, build),
            }
        });
    edits
}

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

fn draw_recent_captures_overlay(
    ui: &mut egui::Ui,
    theme: &Theme,
    current: RecentCapturesOverlaySettings,
    platform: SettingsPlatform,
) -> Option<RecentCapturesOverlaySettings> {
    let mut settings = current;
    ui.spacing_mut().interact_size.y = 44.0;
    ui.label(
        RichText::new("Recent Captures Overlay")
            .font(theme.font(Text::Title))
            .color(theme.palette.text),
    );
    ui.add_space(Space::XS);
    ui.label(
        RichText::new("Control where recent captures appear and what happens after you use them.")
            .font(theme.font(Text::Body))
            .color(theme.palette.text_muted),
    );
    ui.add_space(Space::LG);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            recent_captures_section(ui, theme, "Placement", |ui| {
                ui.horizontal_wrapped(|ui| {
                    placement_preview(ui, theme, settings.placement);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Screen side")
                                .font(theme.font(Text::Label))
                                .color(theme.palette.text),
                        );
                        ui.horizontal(|ui| {
                            ui.selectable_value(
                                &mut settings.placement,
                                RecentCapturesPlacement::Left,
                                "Left",
                            );
                            ui.selectable_value(
                                &mut settings.placement,
                                RecentCapturesPlacement::Right,
                                "Right",
                            );
                        });
                        ui.checkbox(
                            &mut settings.follow_active_display,
                            "Move to the active display",
                        )
                        .on_hover_text(
                            "Follow the display used by the pointer or current capture action. \
                             If it disconnects, Scrozz falls back to the primary display.",
                        );
                    });
                });
            });

            recent_captures_section(ui, theme, "Appearance", |ui| {
                ui.horizontal(|ui| {
                    ui.label("Card size");
                    ui.add(
                        egui::Slider::new(
                            &mut settings.card_width,
                            crate::stack::CardMetrics::MIN_WIDTH
                                ..=crate::stack::CardMetrics::MAX_WIDTH,
                        )
                        .step_by(8.0)
                        .suffix(" pt"),
                    )
                    .on_hover_text(
                        "Cards keep a 16:10 shape. Scrozz shows as many as fit on the display.",
                    );
                });
                ui.horizontal_wrapped(|ui| {
                    for (label, width) in [
                        ("Compact", crate::stack::CardMetrics::MIN_WIDTH),
                        ("Preferred", crate::stack::CardMetrics::PREFERRED_WIDTH),
                        ("Large", crate::stack::CardMetrics::MAX_WIDTH),
                    ] {
                        if ui
                            .selectable_label((settings.card_width - width).abs() < 1.0, label)
                            .clicked()
                        {
                            settings.card_width = width;
                        }
                    }
                });
            });

            recent_captures_section(ui, theme, "Automatic cleanup", |ui| {
                ui.checkbox(
                    &mut settings.auto_close_enabled,
                    "Close cards after a delay",
                );
                if settings.auto_close_enabled {
                    ui.indent("recent-captures-auto-close", |ui| {
                        egui::ComboBox::from_id_salt("recent-captures-auto-close-action")
                            .selected_text(match settings.auto_close_action {
                                RecentCapturesAutoCloseAction::Hide => "Hide when safely retained",
                                RecentCapturesAutoCloseAction::SaveThenHide => {
                                    "Save to Export Location, then hide"
                                }
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut settings.auto_close_action,
                                    RecentCapturesAutoCloseAction::Hide,
                                    "Hide when safely retained",
                                );
                                ui.selectable_value(
                                    &mut settings.auto_close_action,
                                    RecentCapturesAutoCloseAction::SaveThenHide,
                                    "Save to Export Location, then hide",
                                );
                            });
                        ui.horizontal(|ui| {
                            ui.label("After");
                            ui.add(
                                egui::Slider::new(&mut settings.auto_close_seconds, 5..=3_600)
                                    .logarithmic(true)
                                    .suffix(" sec"),
                            );
                        });
                        ui.label(
                            RichText::new(
                                "A card never closes if that would remove the only retained copy.",
                            )
                            .font(theme.font(Text::Caption))
                            .color(theme.palette.text_muted),
                        );
                    });
                }
            });

            recent_captures_section(ui, theme, "Action behavior", |ui| {
                ui.checkbox(
                    &mut settings.close_after_drag,
                    "Close after an accepted external drag",
                );
                let modifier = match platform {
                    SettingsPlatform::MacOs => "Option",
                    SettingsPlatform::Windows | SettingsPlatform::Linux => "Alt",
                };
                ui.label(
                    RichText::new(format!(
                        "Hold {modifier} while dragging to keep the card visible."
                    ))
                    .font(theme.font(Text::Caption))
                    .color(theme.palette.text_muted),
                );

                ui.add_enabled(
                    false,
                    egui::Checkbox::new(
                        &mut settings.close_after_upload,
                        "Close after a successful cloud upload",
                    ),
                );
                ui.label(
                    RichText::new(
                        "Unavailable until a cloud provider is configured. Failed or cancelled \
                         uploads always keep the card visible.",
                    )
                    .font(theme.font(Text::Caption))
                    .color(theme.palette.text_faint),
                );

                ui.horizontal(|ui| {
                    ui.label("Save button");
                    egui::ComboBox::from_id_salt("recent-captures-save-behavior")
                        .selected_text(match settings.save_behavior {
                            RecentCapturesSaveBehavior::ExportLocation => "Save to Export Location",
                            RecentCapturesSaveBehavior::ChooseDestination => "Choose destination",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut settings.save_behavior,
                                RecentCapturesSaveBehavior::ExportLocation,
                                "Save to Export Location",
                            );
                            ui.selectable_value(
                                &mut settings.save_behavior,
                                RecentCapturesSaveBehavior::ChooseDestination,
                                "Choose destination",
                            );
                        });
                });
                ui.label(
                    RichText::new(format!(
                        "Hold {modifier} when choosing Save to use the other behavior once."
                    ))
                    .font(theme.font(Text::Caption))
                    .color(theme.palette.text_muted),
                );
            });
        });

    (settings != current).then_some(settings.normalized())
}

fn recent_captures_section(
    ui: &mut egui::Ui,
    theme: &Theme,
    title: &str,
    contents: impl FnOnce(&mut egui::Ui),
) {
    ui.label(
        RichText::new(title)
            .font(theme.font(Text::Subtitle))
            .color(theme.palette.text),
    );
    ui.add_space(Space::XS);
    egui::Frame::new()
        .fill(theme.palette.card_fill)
        .stroke(egui::Stroke::new(1.0, theme.palette.hairline))
        .corner_radius(12)
        .inner_margin(egui::Margin::same(Space::MD as i8))
        .show(ui, contents);
    ui.add_space(Space::LG);
}

fn placement_preview(ui: &mut egui::Ui, theme: &Theme, placement: RecentCapturesPlacement) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(150.0, 96.0), Sense::hover());
    ui.painter().rect(
        rect.shrink(2.0),
        10.0,
        theme.palette.canvas(),
        egui::Stroke::new(1.0, theme.palette.divider),
        egui::StrokeKind::Inside,
    );
    let left = matches!(placement, RecentCapturesPlacement::Left);
    for index in 0..3 {
        let width = 48.0;
        let height = 30.0;
        let x = if left {
            rect.left() + 10.0
        } else {
            rect.right() - width - 10.0
        };
        let y = rect.bottom() - 10.0 - height - index as f32 * 8.0;
        let card = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(width, height));
        ui.painter().rect_filled(
            card,
            5.0,
            if index == 0 {
                theme.palette.accent
            } else {
                theme.palette.card_fill
            },
        );
        ui.painter().rect_stroke(
            card,
            5.0,
            egui::Stroke::new(1.0, theme.palette.hairline),
            egui::StrokeKind::Inside,
        );
    }
}

fn draw_shortcuts(
    ui: &mut egui::Ui,
    theme: &Theme,
    icons: &IconStore,
    rows: &[ShortcutRow],
    recording: &mut Option<String>,
) -> Vec<ShortcutEdit> {
    let palette = theme.palette;
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

    ui.label(
        RichText::new("Shortcuts")
            .font(theme.font(Text::Title))
            .color(palette.text),
    );
    ui.add_space(Space::XS);
    ui.label(
        RichText::new("Click a shortcut to record a new key combination.")
            .font(theme.font(Text::Body))
            .color(palette.text_muted),
    );
    ui.add_space(Space::XL);

    let split = rows
        .iter()
        .position(|row| row.id.starts_with("record."))
        .unwrap_or(rows.len());
    egui::ScrollArea::vertical()
        .max_height((ui.available_height() - 54.0).max(180.0))
        .auto_shrink([false, false])
        .show(ui, |ui| {
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
                ui.add_space(Space::XL);
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
        });

    ui.add_space(Space::MD);
    ui.horizontal(|ui| {
        let hint = if recording.is_some() {
            "Press a combination. Esc cancels, Delete unassigns."
        } else {
            "Changes are saved as soon as the system accepts them."
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
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
        icons.draw(
            ui.painter(),
            icon,
            rect.center(),
            20.0,
            theme.palette.text_muted,
        );
        ui.label(
            RichText::new(title)
                .font(theme.font(Text::Subtitle))
                .color(theme.palette.text),
        );
    });
    ui.add_space(Space::SM);
    egui::Frame::new()
        .fill(theme.palette.card_fill)
        .stroke(egui::Stroke::new(1.0, theme.palette.hairline))
        .corner_radius(12)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (index, row) in rows.iter().enumerate() {
                draw_shortcut_row(ui, theme, row, index, recording, edits);
                if index + 1 < rows.len() {
                    ui.painter().line_segment(
                        [
                            egui::pos2(ui.min_rect().left() + 16.0, ui.min_rect().bottom()),
                            egui::pos2(ui.min_rect().right() - 16.0, ui.min_rect().bottom()),
                        ],
                        egui::Stroke::new(1.0, theme.palette.divider),
                    );
                }
            }
        });
}

fn draw_shortcut_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    row: &ShortcutRow,
    index: usize,
    recording: &mut Option<String>,
    edits: &mut Vec<ShortcutEdit>,
) {
    let palette = theme.palette;
    let armed = recording.as_deref() == Some(row.id.as_str());
    egui::Frame::new()
        .fill(if index.is_multiple_of(2) {
            palette.card_fill
        } else {
            palette.card_fill_raised
        })
        .inner_margin(egui::Margin::symmetric(16, 12))
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
                        *recording = None;
                        edits.push(ShortcutEdit::Reset { id: row.id.clone() });
                    }
                    if ui
                        .add_enabled(!row.accelerator.is_empty(), egui::Button::new("Clear"))
                        .on_hover_text("Leave this action unassigned")
                        .clicked()
                    {
                        *recording = None;
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
                            .min_size(Vec2::new(170.0, 32.0))
                            .fill(if armed {
                                palette.accent
                            } else {
                                palette.chip_fill
                            }),
                        )
                        .clicked()
                    {
                        *recording = if armed { None } else { Some(row.id.clone()) };
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
            } else if let Some(advisory) = &row.advisory {
                ui.add_space(Space::XS);
                ui.label(
                    RichText::new(advisory)
                        .font(theme.font(Text::Caption))
                        .color(palette.text_muted),
                );
            }
        });
}

fn draw_after_capture(
    ui: &mut egui::Ui,
    theme: &Theme,
    rows: &[AfterCaptureRow],
    open_sharing: &mut bool,
) -> Vec<AfterCaptureEdit> {
    let palette = theme.palette;
    let mut edits = Vec::new();
    ui.label(
        RichText::new("After Capture")
            .font(theme.font(Text::Title))
            .color(palette.text),
    );
    ui.add_space(Space::XS);
    ui.label(
        RichText::new(
            "Choose any combination. Screenshot pixels are finalized once, then Copy runs \
             first; other actions use that same immutable capture.",
        )
        .font(theme.font(Text::Body))
        .color(palette.text),
    );
    ui.add_space(Space::LG);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if ui.available_width() >= 650.0 {
                draw_after_capture_table(ui, theme, rows, &mut edits);
            } else {
                draw_after_capture_cards(ui, theme, rows, &mut edits);
            }
            // The one row above that depends on a *remote* service gets its
            // configuration where the user is already reading about it, rather
            // than somewhere they would have to be told about.
            ui.add_space(Space::LG);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Private sharing")
                        .font(theme.font(Text::Label))
                        .color(palette.text),
                );
                if ui.button("Configure Sharing…").clicked() {
                    *open_sharing = true;
                }
            });
            ui.add_space(Space::XS);
            ui.label(
                RichText::new(
                    "Upload uses your own S3-compatible bucket. Credentials live in the \
                     platform keychain, never in the settings file.",
                )
                .font(theme.font(Text::Caption))
                .color(palette.text_muted),
            );
        });
    edits
}

fn draw_after_capture_table(
    ui: &mut egui::Ui,
    theme: &Theme,
    rows: &[AfterCaptureRow],
    edits: &mut Vec<AfterCaptureEdit>,
) {
    let palette = theme.palette;
    egui::Frame::new()
        .fill(palette.card_fill_raised)
        .stroke(egui::Stroke::new(1.0, palette.hairline))
        .corner_radius(12)
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            egui::Grid::new("after-capture-matrix")
                .num_columns(3)
                .striped(true)
                .spacing(Vec2::new(Space::LG, Space::MD))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("Action")
                            .font(theme.font(Text::Label))
                            .color(palette.text),
                    );
                    for heading in ["Screenshot", "Recording"] {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new(heading)
                                    .font(theme.font(Text::Label))
                                    .color(palette.text),
                            );
                        });
                    }
                    ui.end_row();

                    for row in rows {
                        ui.allocate_ui_with_layout(
                            Vec2::new(310.0, 58.0),
                            Layout::top_down(Align::Min),
                            |ui| draw_after_capture_label(ui, theme, row),
                        );
                        ui.allocate_ui_with_layout(
                            Vec2::new(145.0, 58.0),
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
                        ui.allocate_ui_with_layout(
                            Vec2::new(145.0, 58.0),
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
                        ui.end_row();
                    }
                });
        });
}

fn draw_after_capture_cards(
    ui: &mut egui::Ui,
    theme: &Theme,
    rows: &[AfterCaptureRow],
    edits: &mut Vec<AfterCaptureEdit>,
) {
    let palette = theme.palette;
    for row in rows {
        egui::Frame::new()
            .fill(palette.card_fill_raised)
            .stroke(egui::Stroke::new(1.0, palette.hairline))
            .corner_radius(12)
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                draw_after_capture_label(ui, theme, row);
                ui.add_space(Space::SM);
                ui.columns(2, |columns| {
                    let (screenshot, recording) = columns.split_at_mut(1);
                    screenshot[0].label(
                        RichText::new(AfterCaptureMedia::Screenshot.label())
                            .font(theme.font(Text::Caption))
                            .color(palette.text),
                    );
                    draw_after_capture_cell(
                        &mut screenshot[0],
                        theme,
                        row,
                        AfterCaptureMedia::Screenshot,
                        &row.screenshot,
                        edits,
                    );
                    recording[0].label(
                        RichText::new(AfterCaptureMedia::Recording.label())
                            .font(theme.font(Text::Caption))
                            .color(palette.text),
                    );
                    draw_after_capture_cell(
                        &mut recording[0],
                        theme,
                        row,
                        AfterCaptureMedia::Recording,
                        &row.recording,
                        edits,
                    );
                });
            });
        ui.add_space(Space::SM);
    }
}

fn draw_after_capture_label(ui: &mut egui::Ui, theme: &Theme, row: &AfterCaptureRow) {
    let palette = theme.palette;
    ui.label(
        RichText::new(&row.label)
            .font(theme.font(Text::Label))
            .color(palette.text),
    );
    ui.label(
        RichText::new(&row.description)
            .font(theme.font(Text::Caption))
            .color(palette.text),
    );
}

fn draw_after_capture_cell(
    ui: &mut egui::Ui,
    theme: &Theme,
    row: &AfterCaptureRow,
    media: AfterCaptureMedia,
    cell: &AfterCaptureCell,
    edits: &mut Vec<AfterCaptureEdit>,
) {
    let palette = theme.palette;
    let id = match media {
        AfterCaptureMedia::Screenshot => Some(row.screenshot_id.as_str()),
        AfterCaptureMedia::Recording => row.recording_id.as_deref(),
    };
    if let Some(id) = id.filter(|_| cell.available) {
        let mut enabled = cell.enabled;
        let response = ui.add_sized(
            Vec2::new(44.0, 36.0),
            egui::Checkbox::without_text(&mut enabled),
        );
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
        RichText::new("Unavailable")
            .font(theme.font(Text::Caption))
            .color(palette.text)
            .strong(),
    );
    let accessible = format!("{} for {} unavailable: {reason}", row.label, media.label());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, false, accessible.clone()));
    response.on_hover_text(reason);
    ui.label(
        RichText::new(unavailable_summary(reason))
            .font(theme.font(Text::Caption))
            .color(palette.text),
    );
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
    let palette = theme.palette;

    ui.horizontal(|ui| {
        ui.add_space(Space::XL);
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(APP_ICON_SIZE), Sense::hover());
        if let Some(icon) = icon {
            ui.painter().image(
                icon.id(),
                rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }

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

fn preview_after_capture_rows() -> Vec<AfterCaptureRow> {
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
    let mut rows = vec![AfterCaptureRow {
        screenshot_id: "after-capture.apply-smart-frame".to_owned(),
        recording_id: None,
        label: "Apply Smart Frame".to_owned(),
        description: "Create one adaptive presentation revision before any screenshot action runs."
            .to_owned(),
        screenshot: available(false),
        recording: unavailable(false, "Smart Frame applies only to screenshots."),
    }];
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
    fn platform_navigation_follows_native_conventions() {
        assert_eq!(SettingsPlatform::MacOs.navigation(), Navigation::TopToolbar);
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
            &[],
            RecordingPane::default(),
            RecentCapturesOverlaySettings::default(),
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

    #[test]
    fn preview_matrix_uses_approved_vocabulary_and_explicit_unavailable_states() {
        let rows = preview_after_capture_rows();
        // Smart Frame plus the six-cell After Capture matrix.
        assert_eq!(rows.len(), 7);
        assert!(
            rows.iter().all(|row| !row.label.contains("Quick Access")),
            "{rows:?}"
        );
        assert_eq!(rows[0].label, "Apply Smart Frame");
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
