//! Camera settings and explicit preview controls.

use egui::{
    CentralPanel, ComboBox, Frame, Image, Response, RichText, ScrollArea, Sense, Slider,
    TextureHandle, Ui, ViewportBuilder, WindowLevel, load::SizedTexture,
};
use scrozz_record::{
    CameraDevice, CameraDeviceId, CameraDeviceState, CameraPermission, CameraPreview,
    CameraRuntimeStatus,
    camera::render_camera_preview,
    overlay::move_camera,
    settings::{CameraSettings, CameraShape, OverlayAnchor},
};

use crate::{
    harness::{RecordingFixture, Scene, SceneCtx},
    recording_controls::{body, button, caption, heading, panel, section_label},
    theme::{Radius, Space, Text, Theme, corner},
};

/// Stable title used by the native camera-settings viewport.
pub const CAMERA_SETTINGS_WINDOW_TITLE: &str = "Scrozz Camera Settings";

/// Stable identity for the camera-settings viewport.
#[must_use]
pub fn viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("scrozz-camera-settings")
}

/// Ordinary focus-taking camera-settings window properties.
#[must_use]
pub fn viewport_builder() -> ViewportBuilder {
    ViewportBuilder::default()
        .with_title(CAMERA_SETTINGS_WINDOW_TITLE)
        .with_inner_size([620.0, 660.0])
        .with_min_inner_size([360.0, 480.0])
        .with_clamp_size_to_monitor_size(true)
        .with_resizable(true)
        .with_decorations(true)
        .with_transparent(false)
        .with_mouse_passthrough(false)
        .with_has_shadow(true)
        .with_taskbar(true)
        .with_active(false)
        .with_visible(false)
        .with_window_level(WindowLevel::Normal)
}

/// Owned state copied from the application into the settings viewport.
#[derive(Debug, Clone)]
pub struct CameraSettingsSnapshot {
    /// Current camera composition preferences.
    pub settings: CameraSettings,
    /// Enumerated native devices.
    pub devices: Vec<CameraDevice>,
    /// Stable selected device, or `None` for platform default.
    pub selected_device: Option<CameraDeviceId>,
    /// Device selection and camera enablement are fixed once target selection starts.
    pub capture_configuration_locked: bool,
    /// Permission state observed without prompting.
    pub permission: CameraPermission,
    /// Preview returned after an explicit user action.
    pub preview: Option<CameraPreview>,
    /// Pixel-free preview state, available before the first frame.
    pub preview_status: Option<scrozz_record::CameraRuntimeStatus>,
    /// Recoverable enumeration or preview error.
    pub error: Option<String>,
}

impl CameraSettingsSnapshot {
    /// Borrows this snapshot as a render model.
    #[must_use]
    pub fn model(&self) -> CameraSettingsModel<'_> {
        CameraSettingsModel {
            settings: self.settings,
            devices: &self.devices,
            selected_device: self.selected_device.as_ref(),
            capture_configuration_locked: self.capture_configuration_locked,
            permission: self.permission,
            preview: self.preview.as_ref(),
            preview_status: self.preview_status.as_ref(),
            error: self.error.as_deref(),
        }
    }
}

/// Immutable camera settings surface model.
#[derive(Debug, Clone)]
pub struct CameraSettingsModel<'a> {
    /// Current camera composition preferences.
    pub settings: CameraSettings,
    /// Enumerated native devices.
    pub devices: &'a [CameraDevice],
    /// Stable selected device, or `None` for platform default.
    pub selected_device: Option<&'a CameraDeviceId>,
    /// Whether starting/stopping/switching capture must wait for the next recording.
    pub capture_configuration_locked: bool,
    /// Permission state observed without prompting.
    pub permission: CameraPermission,
    /// Preview returned after an explicit preview action.
    pub preview: Option<&'a CameraPreview>,
    /// Pixel-free preview state, available before the first frame.
    pub preview_status: Option<&'a scrozz_record::CameraRuntimeStatus>,
    /// Recoverable enumeration or preview error.
    pub error: Option<&'a str>,
}

/// Semantic action raised by the camera settings surface.
#[derive(Debug, Clone, PartialEq)]
pub enum CameraSettingsAction {
    /// Close settings and release any preview session.
    Close,
    /// Persist composition preferences.
    SettingsChanged(CameraSettings),
    /// Persist a stable device preference.
    DeviceChanged(Option<CameraDeviceId>),
    /// Request permission and start preview for the selected device.
    StartPreview,
    /// Stop preview and release the camera immediately.
    StopPreview,
}

/// Draws the camera settings in its ordinary viewport.
#[must_use]
pub fn show_window(
    ui: &mut Ui,
    snapshot: &CameraSettingsSnapshot,
    theme: &Theme,
) -> CameraSettingsResponse {
    CentralPanel::default()
        .frame(
            Frame::new()
                .fill(theme.palette.canvas())
                .inner_margin(Space::XL),
        )
        .show(ui, |ui| {
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        CameraSettingsPanel::new(snapshot.model(), theme).show(ui)
                    })
                    .inner
                })
                .inner
        })
        .inner
}

/// Result of drawing camera settings.
#[derive(Debug)]
pub struct CameraSettingsResponse {
    /// Ordered semantic actions from this pass.
    pub actions: Vec<CameraSettingsAction>,
    /// Explicit preview/stop control for keyboard and accessibility tests.
    pub preview_response: Response,
}

/// Accessible camera settings panel.
pub struct CameraSettingsPanel<'a> {
    model: CameraSettingsModel<'a>,
    theme: &'a Theme,
}

impl<'a> CameraSettingsPanel<'a> {
    /// Creates a settings panel over caller-owned state.
    #[must_use]
    pub const fn new(model: CameraSettingsModel<'a>, theme: &'a Theme) -> Self {
        Self { model, theme }
    }

    /// Draws the panel without touching a camera.
    pub fn show(self, ui: &mut Ui) -> CameraSettingsResponse {
        let mut settings = self.model.settings;
        let original = settings;
        let mut selected = self.model.selected_device.cloned();
        let original_selected = selected.clone();
        let mut actions = Vec::new();
        let mut preview_response = None;
        let width = ui.available_width().clamp(300.0, 520.0);

        panel(ui, self.theme, width, |ui| {
            heading(ui, self.theme, "Camera");
            body(
                ui,
                self.theme,
                "Add a camera only when you explicitly enable it. Preview and recording use the same crop, mirror, and mask.",
            );
            if let Some(error) = self.model.error {
                ui.add_space(Space::SM);
                ui.colored_label(self.theme.palette.warning, error);
            }
            ui.add_space(Space::MD);

            ui.add_enabled(
                !self.model.capture_configuration_locked,
                egui::Checkbox::new(&mut settings.enabled, "Include camera in recordings"),
            );
            ui.add_enabled_ui(settings.enabled, |ui| {
                ui.horizontal(|ui| {
                    section_label(ui, self.theme, "DEVICE");
                    ui.add_enabled_ui(!self.model.capture_configuration_locked, |ui| {
                        ComboBox::from_id_salt("camera-settings-device")
                            .selected_text(selected_device_label(
                                self.model.devices,
                                selected.as_ref(),
                            ))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut selected, None, "System default");
                                for device in self.model.devices {
                                    let label = match device.state {
                                        CameraDeviceState::Available => device.name.clone(),
                                        CameraDeviceState::Busy => {
                                            format!("{} — busy", device.name)
                                        }
                                        CameraDeviceState::Disconnected => {
                                            format!("{} — disconnected", device.name)
                                        }
                                        CameraDeviceState::PermissionDenied => {
                                            format!("{} — permission denied", device.name)
                                        }
                                    };
                                    ui.add_enabled_ui(
                                        device.state == CameraDeviceState::Available,
                                        |ui| {
                                            ui.selectable_value(
                                                &mut selected,
                                                Some(device.id.clone()),
                                                label,
                                            );
                                        },
                                    );
                                }
                            });
                    });
                });
                if self.model.capture_configuration_locked {
                    caption(
                        ui,
                        self.theme,
                        "Camera capture and device selection apply to the next recording. Composition remains live.",
                    );
                }

                let preview_active = self
                    .model
                    .preview_status
                    .is_some_and(|status| status.active);
                if let Some(status) = self.model.preview_status {
                    ui.colored_label(
                        if status.active {
                            self.theme.palette.success
                        } else {
                            self.theme.palette.warning
                        },
                        if status.active {
                            "Camera active — privacy indicator visible"
                        } else {
                            "Camera unavailable"
                        },
                    );
                }
                if let Some(preview) = self.model.preview
                    && let Some(texture) = preview_texture(
                        ui.ctx(),
                        preview,
                        "scrozz-camera-settings-preview",
                        "scrozz.camera.settings.preview",
                        320,
                        180,
                    )
                {
                    let size = texture.size;
                    let response = ui.add(
                        Image::from_texture(texture)
                            .fit_to_exact_size(size)
                            .corner_radius(corner(Radius::BUTTON))
                            .sense(Sense::hover()),
                    );
                    response.clone().on_hover_text("Live camera preview");
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Image,
                            true,
                            "Live camera preview",
                        )
                    });
                }
                let preview_allowed =
                    matches!(
                        self.model.permission,
                        CameraPermission::Authorized | CameraPermission::NotDetermined
                    ) && selected_device_available(self.model.devices, selected.as_ref());
                let response = button(
                    ui,
                    self.theme,
                    if preview_active {
                        "Stop preview"
                    } else {
                        "Preview camera"
                    },
                    preview_active,
                    preview_active || preview_allowed,
                );
                if response.clicked() {
                    actions.push(if preview_active {
                        CameraSettingsAction::StopPreview
                    } else {
                        CameraSettingsAction::StartPreview
                    });
                }

                preview_response = Some(response);
                match self.model.permission {
                    CameraPermission::Denied => {
                        caption(
                            ui,
                            self.theme,
                            "Camera access is denied in system settings.",
                        );
                    }
                    CameraPermission::Restricted => {
                        caption(
                            ui,
                            self.theme,
                            "Camera access is restricted by device policy.",
                        );
                    }
                    CameraPermission::Unsupported => {
                        caption(ui, self.theme, "No native camera adapter is available.");
                    }
                    CameraPermission::NotDetermined | CameraPermission::Authorized => {}
                }

                ui.add_space(Space::MD);
                section_label(ui, self.theme, "COMPOSITION");
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(&mut settings.presenter, false, "Picture in picture");
                    ui.selectable_value(&mut settings.presenter, true, "Presenter");
                });
                labeled_combo(
                    ui,
                    self.theme,
                    "Position",
                    "camera-settings-position",
                    anchor_label(settings.position),
                    |ui| {
                        let previous = settings.position;
                        for anchor in OverlayAnchor::ALL {
                            ui.selectable_value(
                                &mut settings.position,
                                anchor,
                                anchor_label(anchor),
                            );
                        }
                        if settings.position != previous {
                            settings.placement = None;
                        }
                    },
                );
                if settings.presenter {
                    caption(
                        ui,
                        self.theme,
                        "Presenter mode fills the canvas; your PiP shape is kept for when you switch back.",
                    );
                } else {
                    labeled_combo(
                        ui,
                        self.theme,
                        "Shape",
                        "camera-settings-shape",
                        shape_label(settings.shape),
                        |ui| {
                            for shape in CameraShape::ALL {
                                ui.selectable_value(&mut settings.shape, shape, shape_label(shape));
                            }
                        },
                    );
                }
                ui.add(
                    Slider::new(
                        &mut settings.size,
                        CameraSettings::MIN_SIZE..=CameraSettings::MAX_SIZE,
                    )
                    .text("Camera size")
                    .custom_formatter(|value, _| format!("{:.0}%", value * 100.0)),
                );
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut settings.mirror, "Mirror preview and video");
                    ui.checkbox(&mut settings.border, "Border");
                    ui.checkbox(&mut settings.shadow, "Shadow");
                });
                if settings.presenter {
                    ui.checkbox(&mut settings.presenter_screen, "Show shared-screen inset");
                }
            });
            ui.add_space(Space::LG);
            if button(ui, self.theme, "Done", false, true).clicked() {
                actions.push(CameraSettingsAction::Close);
            }
        });

        if settings != original {
            actions.push(CameraSettingsAction::SettingsChanged(settings));
        }

        if selected != original_selected {
            actions.push(CameraSettingsAction::DeviceChanged(selected));
        }
        CameraSettingsResponse {
            actions,
            preview_response: preview_response
                .expect("the camera panel always draws its preview control"),
        }
    }
}

#[derive(Clone)]
struct PreviewTexture {
    sequence: u64,
    settings: CameraSettings,
    handle: TextureHandle,
}

/// Uploads the freshest camera frame, rendered exactly as it is recorded.
///
/// The cache key is the frame counter plus the composition, so a texture is
/// re-rendered when a new frame arrives *or* when the mask, crop or mirror
/// changes — never once per repaint, and never stale after a live change.
fn preview_texture(
    ctx: &egui::Context,
    preview: &CameraPreview,
    cache_id: &'static str,
    texture_name: &'static str,
    max_width: u32,
    max_height: u32,
) -> Option<SizedTexture> {
    let id = egui::Id::new(cache_id);
    let sequence = preview.status.frames_received;
    let mut state = ctx.data_mut(|data| data.get_temp::<PreviewTexture>(id));
    if state
        .as_ref()
        .is_none_or(|texture| texture.sequence != sequence || texture.settings != preview.settings)
    {
        let (width, height) = preview_size(preview, max_width, max_height);
        let rendered =
            render_camera_preview(&preview.frame, width, height, preview.settings).ok()?;
        let mut rgba = rendered.data;
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        let image =
            egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba);
        if let Some(state) = &mut state {
            state.handle.set(image, egui::TextureOptions::LINEAR);
            state.sequence = sequence;
            state.settings = preview.settings;
        } else {
            state = Some(PreviewTexture {
                sequence,
                settings: preview.settings,
                handle: ctx.load_texture(texture_name, image, egui::TextureOptions::LINEAR),
            });
        }
        if let Some(state) = &state {
            ctx.data_mut(|data| data.insert_temp(id, state.clone()));
        }
    }
    state
        .as_ref()
        .map(|state| SizedTexture::from_handle(&state.handle))
}

/// Owned live-camera values carried across the application/overlay thread seam.
#[derive(Debug, Clone)]
pub struct CameraLiveSnapshot {
    /// Composition currently applied to the running recording.
    pub settings: CameraSettings,
    /// Pixel-free native camera health.
    pub status: CameraRuntimeStatus,
    /// Latest camera frame on the corrected recording clock.
    pub preview: Option<CameraPreview>,
    /// Whether composition may change right now.
    pub enabled: bool,
}

impl CameraLiveSnapshot {
    /// Borrows this snapshot as a render model.
    #[must_use]
    pub fn model(&self) -> CameraLiveModel<'_> {
        CameraLiveModel {
            settings: self.settings,
            status: &self.status,
            preview: self.preview.as_ref(),
            enabled: self.enabled,
        }
    }
}

/// Immutable live-camera model shown beside a running recording.
#[derive(Debug, Clone, Copy)]
pub struct CameraLiveModel<'a> {
    /// Composition currently applied to the running recording.
    pub settings: CameraSettings,
    /// Pixel-free native camera health.
    pub status: &'a CameraRuntimeStatus,
    /// Latest camera frame on the corrected recording clock.
    pub preview: Option<&'a CameraPreview>,
    /// Whether composition may change right now.
    pub enabled: bool,
}

/// Draws the live camera composition controls.
///
/// Returns the changed composition, and only when something actually moved, so
/// a caller never re-sends an identical value into the running native session.
///
/// The camera is the one recording preference that *can* change live, so this
/// stays reachable while everything else in the live pane is locked. Nothing
/// here touches a camera: pixels and status arrive as values.
pub(crate) fn live_controls(
    ui: &mut Ui,
    theme: &Theme,
    model: CameraLiveModel<'_>,
) -> Option<CameraSettings> {
    let mut settings = model.settings;
    let original = settings;
    let status = model.status;

    ui.horizontal(|ui| {
        let (rect, response) = ui.allocate_exact_size(egui::Vec2::splat(10.0), Sense::hover());
        ui.painter().circle_filled(
            rect.center(),
            4.0,
            if status.active {
                theme.palette.success
            } else {
                theme.palette.warning
            },
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Image,
                true,
                if status.active {
                    "Camera active; the system privacy indicator is visible"
                } else {
                    "Camera unavailable"
                },
            )
        });
        section_label(
            ui,
            theme,
            if status.active {
                "CAMERA ACTIVE"
            } else {
                "CAMERA UNAVAILABLE"
            },
        );
        if status.dropped_frames != 0 {
            caption(ui, theme, format!("{} dropped", status.dropped_frames));
        }
    });
    if let Some(message) = status.warning.as_deref() {
        caption(ui, theme, message);
    }

    if let Some(preview) = model.preview
        && let Some(texture) = preview_texture(
            ui.ctx(),
            preview,
            "scrozz-recording-camera-preview",
            "scrozz.recording.camera.preview",
            192,
            128,
        )
    {
        let size = texture.size;
        let response = ui.add(
            Image::from_texture(texture)
                .fit_to_exact_size(size)
                .corner_radius(corner(Radius::BUTTON))
                .sense(Sense::drag()),
        );
        response
            .clone()
            .on_hover_text("Drag to place the camera; the preview is composed exactly as recorded");
        response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Image,
                true,
                "Live camera preview; drag to place",
            )
        });
        if model.enabled
            && response.dragged()
            && let Some(pointer) = response.interact_pointer_pos()
        {
            let local = pointer - response.rect.min;
            let output = scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(0.0, 0.0),
                scrozz_core::LogicalSize::new(
                    f64::from(response.rect.width()),
                    f64::from(response.rect.height()),
                ),
            );
            if let Ok(moved) = move_camera(
                output,
                preview.frame.oriented_aspect(),
                4.0,
                8.0,
                scrozz_core::LogicalPoint::new(f64::from(local.x), f64::from(local.y)),
                settings,
            ) {
                settings = moved;
            }
        }
    }

    ui.add_enabled_ui(model.enabled, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut settings.presenter, false, "Picture in picture");
            ui.selectable_value(&mut settings.presenter, true, "Presenter");
        });
        labeled_combo(
            ui,
            theme,
            "Position",
            "recording-camera-position",
            anchor_label(settings.position),
            |ui| {
                let previous = settings.position;
                for anchor in OverlayAnchor::ALL {
                    ui.selectable_value(&mut settings.position, anchor, anchor_label(anchor));
                }
                if settings.position != previous {
                    settings.placement = None;
                }
            },
        );
        if settings.presenter {
            caption(
                ui,
                theme,
                "Presenter fills the canvas; the picture-in-picture shape is kept.",
            );
        } else {
            labeled_combo(
                ui,
                theme,
                "Shape",
                "recording-camera-shape",
                shape_label(settings.shape),
                |ui| {
                    for shape in CameraShape::ALL {
                        ui.selectable_value(&mut settings.shape, shape, shape_label(shape));
                    }
                },
            );
        }
        ui.add(
            Slider::new(
                &mut settings.size,
                CameraSettings::MIN_SIZE..=CameraSettings::MAX_SIZE,
            )
            .text("Camera size")
            .custom_formatter(|value, _| format!("{:.0}%", value * 100.0)),
        );
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut settings.mirror, "Mirror");
            ui.checkbox(&mut settings.border, "Border");
            ui.checkbox(&mut settings.shadow, "Shadow");
            if settings.presenter {
                ui.checkbox(&mut settings.presenter_screen, "Screen inset");
            }
        });
    });

    (settings != original).then_some(settings)
}

pub(crate) fn preview_size(preview: &CameraPreview, max_width: u32, max_height: u32) -> (u32, u32) {
    if !preview.settings.presenter && preview.settings.shape.is_square() {
        let side = max_width.min(max_height).max(1);
        return (side, side);
    }
    if preview.settings.presenter {
        let aspect = preview
            .output_aspect
            .unwrap_or_else(|| f64::from(max_width) / f64::from(max_height));
        let width = (f64::from(max_height) * aspect).round();
        if width <= f64::from(max_width) {
            return (width.max(1.0) as u32, max_height.max(1));
        }
        return (
            max_width.max(1),
            (f64::from(max_width) / aspect).round().max(1.0) as u32,
        );
    }
    let aspect = preview.frame.oriented_aspect();
    let width = (f64::from(max_height) * aspect).round();
    if width <= f64::from(max_width) {
        (width.max(1.0) as u32, max_height.max(1))
    } else {
        (
            max_width.max(1),
            (f64::from(max_width) / aspect).round().max(1.0) as u32,
        )
    }
}

fn labeled_combo(
    ui: &mut Ui,
    theme: &Theme,
    label: &str,
    id: &'static str,
    selected: &'static str,
    add_contents: impl FnOnce(&mut Ui),
) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(label)
                .font(theme.font(Text::Label))
                .color(theme.palette.text_muted),
        );
        ComboBox::from_id_salt(id)
            .selected_text(selected)
            .show_ui(ui, add_contents);
    });
}

fn selected_device_label(devices: &[CameraDevice], selected: Option<&CameraDeviceId>) -> String {
    let Some(selected) = selected else {
        return "System default".to_owned();
    };
    devices
        .iter()
        .find(|device| device.id == *selected)
        .map(|device| device.name.clone())
        .unwrap_or_else(|| "Selected camera unavailable".to_owned())
}

fn selected_device_available(devices: &[CameraDevice], selected: Option<&CameraDeviceId>) -> bool {
    match selected {
        Some(selected) => devices
            .iter()
            .any(|device| device.id == *selected && device.state == CameraDeviceState::Available),
        None => devices
            .iter()
            .any(|device| device.state == CameraDeviceState::Available),
    }
}

fn anchor_label(anchor: OverlayAnchor) -> &'static str {
    match anchor {
        OverlayAnchor::TopLeft => "Top left",
        OverlayAnchor::TopCenter => "Top center",
        OverlayAnchor::TopRight => "Top right",
        OverlayAnchor::BottomLeft => "Bottom left",
        OverlayAnchor::BottomCenter => "Bottom center",
        OverlayAnchor::BottomRight => "Bottom right",
    }
}

fn shape_label(shape: CameraShape) -> &'static str {
    match shape {
        CameraShape::Circle => "Circle",
        CameraShape::Rounded => "Rounded rectangle",
        CameraShape::Square => "Square",
        CameraShape::Rectangle => "Rectangle",
    }
}

/// Deterministic camera-settings renderer.
#[derive(Debug, Default)]
pub struct CameraSettingsScene;

impl Scene for CameraSettingsScene {
    fn name(&self) -> &'static str {
        "camera-settings"
    }

    fn setup(&self, ctx: &egui::Context) {
        crate::recording_controls::install_scene_theme(ctx);
    }

    fn ui(&self, ui: &mut Ui, ctx: &SceneCtx<'_>) {
        let Some(RecordingFixture::CameraSettings(snapshot)) = ctx.fixture.recording.as_ref()
        else {
            ui.label("camera-settings scene received the wrong fixture");
            return;
        };
        let theme = crate::recording_controls::scene_theme(ctx.theme);
        let _ = show_window(ui, snapshot, &theme);
    }
}
