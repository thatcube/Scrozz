//! Windows overlay host with a real per-pixel layered-window presenter.
//!
//! DXGI swap chains for ordinary `HWND`s are allowed to expose only opaque
//! composition. VMware's Windows ARM64 WARP adapter does exactly that, so both
//! eframe renderers fail there in different ways: Glow cannot create modern
//! OpenGL, while wgpu presents a black full-screen surface. This host keeps
//! winit and egui for the event/input model, rasterizes egui's meshes on the CPU,
//! and gives DWM premultiplied pixels through `UpdateLayeredWindow`.

use std::{
    ffi::c_void,
    time::{Duration, Instant},
};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use scrozz_core::Error as CoreError;
use scrozz_shell::windows::overlay::LayeredPresenter;
use scrozz_ui::{
    OverlayApp, OverlayHandle, OverlayOptions, harness::LiveSoftwareRenderer, overlay_app,
};
use winit::{
    application::ApplicationHandler,
    dpi::{PhysicalPosition, PhysicalSize},
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    platform::run_on_demand::EventLoopExtRunOnDemand,
    window::{Window, WindowId},
};

use crate::{
    fault::{CliError, CliResult},
    gui::app::{App, Tick},
    report::Report,
};

const TICK: Duration = Duration::from_millis(16);

#[derive(Clone, Copy)]
struct RestoreBounds {
    position: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
}

pub(super) fn run(app: App, handle: OverlayHandle, options: OverlayOptions) -> CliResult<Report> {
    let mut event_loop = EventLoop::new()
        .map_err(|error| platform_error(format!("creating the Windows event loop: {error}")))?;
    let mut runtime = Runtime::new(app, handle, options);
    event_loop
        .run_app_on_demand(&mut runtime)
        .map_err(|error| platform_error(format!("running the Windows event loop: {error}")))?;

    if let Some(error) = runtime.failure {
        return Err(error);
    }
    runtime.report.ok_or_else(|| {
        platform_error("the Windows overlay event loop ended before the app reported".to_owned())
    })
}

struct Runtime {
    app: App,
    handle: Option<OverlayHandle>,
    options: Option<OverlayOptions>,
    context: egui::Context,
    window: Option<Window>,
    state: Option<egui_winit::State>,
    overlay: Option<OverlayApp>,
    presenter: Option<LayeredPresenter>,
    restore_bounds: Option<RestoreBounds>,
    rasterizer: LiveSoftwareRenderer,
    report: Option<Report>,
    failure: Option<CliError>,
    stopped: bool,
    dirty: bool,
    presented_nonempty: bool,
    was_minimized: bool,
    was_animating: bool,
    next_tick: Instant,
}

impl Runtime {
    fn new(app: App, handle: OverlayHandle, mut options: OverlayOptions) -> Self {
        // The eframe hook cannot run because this host creates the winit window
        // directly. Native conversion happens in `open`, before the window is
        // shown, and its report is passed explicitly to OverlayApp.
        options.panel = None;
        Self {
            app,
            handle: Some(handle),
            options: Some(options),
            context: egui::Context::default(),
            window: None,
            state: None,
            overlay: None,
            presenter: None,
            restore_bounds: None,
            rasterizer: LiveSoftwareRenderer::default(),
            report: None,
            failure: None,
            stopped: false,
            dirty: true,
            presented_nonempty: false,
            was_minimized: false,
            was_animating: true,
            next_tick: Instant::now(),
        }
    }

    fn open(&mut self, event_loop: &ActiveEventLoop) -> CliResult<()> {
        let options = self.options.take().ok_or_else(|| {
            platform_error("the Windows overlay options were consumed twice".to_owned())
        })?;
        let handle = self.handle.take().ok_or_else(|| {
            platform_error("the Windows overlay handle was consumed twice".to_owned())
        })?;

        // `transparent(true)` asks winit to configure DWM blur-behind. This
        // presenter owns per-pixel alpha itself, so avoid mixing the two DWM
        // composition models on one HWND.
        let viewport = overlay_app::viewport(options.geometry)
            .with_transparent(false)
            .with_visible(false);
        let window =
            egui_winit::create_window(&self.context, event_loop, &viewport).map_err(|error| {
                platform_error(format!("creating the Windows overlay HWND: {error}"))
            })?;
        let hwnd = {
            let raw = window.window_handle().map_err(|error| {
                platform_error(format!("reading the Windows overlay HWND: {error}"))
            })?;
            let RawWindowHandle::Win32(win32) = raw.as_raw() else {
                return Err(platform_error(
                    "winit created a non-Win32 window for the Windows overlay".to_owned(),
                ));
            };
            win32.hwnd.get()
        };

        // SAFETY: the raw handle borrows `window`, which remains owned by this
        // runtime, and `resumed` runs on the window/event-loop owning thread.
        let panel = unsafe { super::panel::convert_hwnd_layered_bitmap(hwnd as *mut c_void) };
        if !panel.non_activating {
            return Err(platform_error(format!(
                "configuring the non-activating Windows overlay: {}",
                panel.detail
            )));
        }

        let mut presenter = LayeredPresenter::new(hwnd).map_err(CliError::Core)?;
        let size = window.inner_size();
        presenter
            .present_transparent(size.width, size.height)
            .map_err(CliError::Core)?;

        let overlay = OverlayApp::from_context(&self.context, handle, options, panel.clone());
        let mut state = egui_winit::State::new(
            self.context.clone(),
            egui::ViewportId::ROOT,
            event_loop,
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        egui_winit::update_viewport_info(
            state
                .egui_input_mut()
                .viewports
                .entry(egui::ViewportId::ROOT)
                .or_default(),
            &self.context,
            &window,
            true,
        );

        tracing::info!(
            detail = %panel.detail,
            "the overlay is a non-activating per-pixel layered window"
        );
        window.set_visible(true);
        let restore_bounds = current_restore_bounds(&window)?;
        window.request_redraw();
        self.next_tick = Instant::now() + TICK;

        self.window = Some(window);
        self.state = Some(state);
        self.overlay = Some(overlay);
        self.presenter = Some(presenter);
        self.restore_bounds = Some(restore_bounds);
        Ok(())
    }

    fn draw(&mut self) -> Result<FrameOutcome, CoreError> {
        let window = self
            .window
            .as_ref()
            .ok_or_else(|| CoreError::Platform("the Windows overlay has no window".to_owned()))?;
        let state = self.state.as_mut().ok_or_else(|| {
            CoreError::Platform("the Windows overlay has no egui input state".to_owned())
        })?;
        let overlay = self.overlay.as_mut().ok_or_else(|| {
            CoreError::Platform("the Windows overlay has no UI surface".to_owned())
        })?;
        let presenter = self.presenter.as_mut().ok_or_else(|| {
            CoreError::Platform("the Windows overlay has no layered presenter".to_owned())
        })?;
        let size = window.inner_size();
        if window.is_minimized() == Some(true) || size.width == 0 || size.height == 0 {
            // Minimized and show-desktop windows legitimately report zero. Do
            // not run egui and create texture deltas that cannot be consumed;
            // the restored size event will force a new native presentation.
            self.dirty = true;
            return Ok(FrameOutcome::Continue);
        }

        egui_winit::update_viewport_info(
            state
                .egui_input_mut()
                .viewports
                .entry(egui::ViewportId::ROOT)
                .or_default(),
            &self.context,
            window,
            false,
        );
        let input = state.take_egui_input(window);
        let output = self.context.run_ui(input, |ui| overlay.show(ui));
        let egui::FullOutput {
            platform_output,
            mut textures_delta,
            shapes,
            pixels_per_point,
            mut viewport_output,
        } = output;
        state.handle_platform_output(window, platform_output);

        let Some(root) = viewport_output.remove(&egui::ViewportId::ROOT) else {
            textures_delta.clear();
            return Err(CoreError::Platform(
                "egui returned no root viewport for the Windows overlay".to_owned(),
            ));
        };
        if !viewport_output.is_empty() {
            textures_delta.clear();
            return Err(CoreError::Unsupported {
                what: "secondary Windows overlay viewports".to_owned(),
                why: "the per-pixel layered host currently owns exactly one HWND".to_owned(),
            });
        }

        let close_requested = root
            .commands
            .iter()
            .any(|command| matches!(command, egui::ViewportCommand::Close));
        let mut requested_actions = Vec::new();
        egui_winit::process_viewport_commands(
            &self.context,
            state
                .egui_input_mut()
                .viewports
                .entry(egui::ViewportId::ROOT)
                .or_default(),
            root.commands,
            window,
            &mut requested_actions,
        );
        if close_requested {
            textures_delta.clear();
            return Ok(FrameOutcome::Close);
        }
        if !requested_actions.is_empty() {
            textures_delta.clear();
            return Err(CoreError::Unsupported {
                what: "clipboard or screenshot actions from the Windows overlay renderer"
                    .to_owned(),
                why: "capture cards use Scrozz's explicit native actions; silently dropping an \
                      egui integration action would report success without doing it"
                    .to_owned(),
            });
        }

        let has_texture_changes = !textures_delta.set.is_empty() || !textures_delta.free.is_empty();
        let has_pixels = !shapes.is_empty();
        let animating = root.repaint_delay == Duration::ZERO;
        let was_animating = std::mem::replace(&mut self.was_animating, animating);
        let should_present = should_present(
            self.dirty,
            has_texture_changes,
            self.presented_nonempty,
            animating,
            was_animating,
        );

        if has_pixels && should_present {
            let frame = self.rasterizer.render(
                &self.context,
                textures_delta,
                shapes,
                pixels_per_point,
                size.width,
                size.height,
            )?;
            window.pre_present_notify();
            presenter.present_premultiplied_rgba(frame.width(), frame.height(), frame.as_rgba())?;
            self.presented_nonempty = true;
        } else if !has_pixels && (self.presented_nonempty || self.dirty) {
            // A resize must recreate the transparent layered bitmap even when
            // there are no cards. Otherwise the DIB retains its old dimensions
            // until some later capture happens to produce a mesh.
            if has_texture_changes {
                let _ = self.rasterizer.render(
                    &self.context,
                    textures_delta,
                    shapes,
                    pixels_per_point,
                    size.width,
                    size.height,
                )?;
            } else {
                textures_delta.clear();
            }
            presenter.present_transparent(size.width, size.height)?;
            self.presented_nonempty = false;
        } else if has_texture_changes {
            // Texture deltas carry a drop assertion. Applying them through an
            // empty render keeps the atlas synchronized without a native upload.
            let _ = self.rasterizer.render(
                &self.context,
                textures_delta,
                shapes,
                pixels_per_point,
                size.width,
                size.height,
            )?;
        } else {
            textures_delta.clear();
        }

        self.dirty = false;
        Ok(FrameOutcome::Continue)
    }

    fn finish(&mut self, event_loop: &ActiveEventLoop) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let report = self.app.report();
        self.app.shut_down();
        self.report = Some(report);
        if let Some(window) = &self.window {
            window.set_visible(false);
        }
        event_loop.exit();
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: CliError) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.app.shut_down();
        self.failure = Some(error);
        if let Some(window) = &self.window {
            window.set_visible(false);
        }
        event_loop.exit();
    }
}

impl ApplicationHandler for Runtime {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.stopped {
            return;
        }
        if let Err(error) = self.open(event_loop) {
            self.fail(event_loop, error);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        if window.id() != window_id || self.stopped {
            return;
        }

        match event {
            WindowEvent::CloseRequested => self.finish(event_loop),
            WindowEvent::RedrawRequested => {
                if self.app.tick() == Tick::Stop {
                    self.finish(event_loop);
                } else {
                    match self.draw() {
                        Ok(FrameOutcome::Continue) => {}
                        Ok(FrameOutcome::Close) => self.finish(event_loop),
                        Err(error) => self.fail(event_loop, CliError::Core(error)),
                    }
                }
            }
            event @ (WindowEvent::Resized(_)
            | WindowEvent::Moved(_)
            | WindowEvent::ScaleFactorChanged { .. }) => {
                let minimized = window.is_minimized() == Some(true);
                let restore = restore_after_minimize(&mut self.was_minimized, minimized);
                // WM_SIZE can report minimized-icon dimensions for this
                // borderless tool window. Replay the last non-iconic geometry
                // on restore, but keep genuine move, resize and DPI changes.
                if restore && let Some(bounds) = self.restore_bounds {
                    window.set_outer_position(bounds.position);
                    let _ = window.request_inner_size(bounds.size);
                } else if !minimized {
                    match current_restore_bounds(window) {
                        Ok(bounds) => self.restore_bounds = Some(bounds),
                        Err(error) => {
                            self.fail(event_loop, error);
                            return;
                        }
                    }
                }
                self.dirty = true;
                if let Some(state) = self.state.as_mut() {
                    let _ = state.on_window_event(window, &event);
                }
                if !minimized {
                    window.request_redraw();
                }
            }
            event => {
                if let Some(state) = self.state.as_mut() {
                    let response = state.on_window_event(window, &event);
                    if response.repaint {
                        self.dirty = true;
                        window.request_redraw();
                    }
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.stopped {
            return;
        }
        let now = Instant::now();
        if now >= self.next_tick
            && let Some(window) = &self.window
        {
            window.request_redraw();
            self.next_tick = now + TICK;
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_tick));
    }
}

fn restore_after_minimize(was_minimized: &mut bool, minimized: bool) -> bool {
    if minimized {
        *was_minimized = true;
        false
    } else {
        std::mem::take(was_minimized)
    }
}

fn current_restore_bounds(window: &Window) -> CliResult<RestoreBounds> {
    Ok(RestoreBounds {
        position: window.outer_position().map_err(|error| {
            platform_error(format!(
                "reading the Windows overlay restore position: {error}"
            ))
        })?,
        size: window.inner_size(),
    })
}

fn platform_error(message: String) -> CliError {
    CliError::Core(CoreError::Platform(message))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameOutcome {
    Continue,
    Close,
}

const fn should_present(
    dirty: bool,
    has_texture_changes: bool,
    presented_nonempty: bool,
    animating: bool,
    was_animating: bool,
) -> bool {
    dirty || has_texture_changes || !presented_nonempty || animating || was_animating
}

#[cfg(test)]
mod tests {
    use super::{restore_after_minimize, should_present};

    #[test]
    fn restore_is_requested_once_after_a_minimized_transition() {
        let mut was_minimized = false;

        assert!(!restore_after_minimize(&mut was_minimized, false));
        assert!(!restore_after_minimize(&mut was_minimized, true));
        assert!(restore_after_minimize(&mut was_minimized, false));
        assert!(!restore_after_minimize(&mut was_minimized, false));
    }

    #[test]
    fn the_first_settled_frame_after_an_animation_is_presented() {
        assert!(should_present(false, false, true, false, true));
        assert!(!should_present(false, false, true, false, false));
    }
}
