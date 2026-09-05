//! Scrolling capture: acquisition, cancellation and target resolution.
//!
//! Scrolling is the one still-capture path that also needs *input*, and the one
//! whose result is assembled over time rather than read in a single call. Both
//! facts are why it lives here rather than inside [`crate::commands`]: the
//! ordinary capture path stays a straight line, and everything that can be
//! cancelled, salvaged, or refused mid-session is in one file.
//!
//! # What this module owns
//!
//! * Resolving a display selector into a concrete scrolling target — one window
//!   on one display, or a Wayland portal choice — and re-resolving it after the
//!   user has had time to focus something else.
//! * Cropping each captured frame to the part of the window that is actually
//!   inside the display's usable area, so chrome outside it can never enter the
//!   stitched output.
//! * The terminal cancellation contract: one Ctrl+C keeps the stitched prefix,
//!   two discard it, and neither can turn an already-published file back into a
//!   cancellation report.
//!
//! The stitching itself belongs to [`scrozz_stitch`], and the input synthesis to
//! [`scrozz_capture`]. This module only joins them to a target.

use std::io::IsTerminal;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU8, Ordering},
};
use std::time::Duration;

use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, CaptureTarget, Display, Error as CoreError, Frame,
    LogicalPoint, LogicalRect, LogicalSize, PhysicalSize, ScaleFactor, ScrollAxis, ScrollControl,
    ScrollGesture, Window, WindowId,
};
use scrozz_stitch::{
    CancelAction, CancelSignal, FrameSource, Progress, ScrollSession, ScrollSessionConfig,
    ThreadPacer,
};

use crate::{
    cli::DisplaySelector,
    commands::is_wayland,
    fault::{CliError, CliResult},
    platform,
};

/// The window id that means "whatever the user picks in the desktop portal".
///
/// It is deliberately not an OS window id: Wayland never reveals one, and
/// inventing a plausible-looking id would let a consumer treat it as evidence of
/// a title, owner, or position it cannot possibly have.
pub(crate) const WAYLAND_PORTAL_PICKER_WINDOW_ID: &str = "xdg-desktop-portal-picker";

pub(crate) fn scrolling_capture(
    backend: &dyn CaptureBackend,
    request: CaptureRequest,
    axis: ScrollAxis,
    acquisition_cancellation: Option<&scrozz_capture::CaptureCancellation>,
) -> CliResult<(Capture, TerminalCancellation)> {
    let mut cancel = TerminalCancellation::install()?;
    let acquisition_cancellation = acquisition_cancellation
        .cloned()
        .unwrap_or_else(|| cancel.acquisition.clone());
    let target = resolve_scrolling_target(backend, request)?;
    let capture = if std::io::stderr().is_terminal() {
        eprintln!(
            "scrozz: scrolling capture started; press Ctrl+C once to keep the stitched result, \
             or twice quickly to discard it"
        );
        scrolling_capture_target_with_cancellation(
            target,
            axis,
            &mut cancel,
            &acquisition_cancellation,
            report_terminal_scroll_progress,
        )
    } else {
        scrolling_capture_target_with_cancellation(
            target,
            axis,
            &mut cancel,
            &acquisition_cancellation,
            |event| tracing::debug!(?event, "scrolling capture progress"),
        )
    }?;
    Ok((capture, cancel))
}

pub(crate) struct TerminalHandler {
    state: Arc<AtomicU8>,
    acquisition: Arc<Mutex<scrozz_capture::CaptureCancellation>>,
    install_error: Option<String>,
}

static TERMINAL_HANDLER: OnceLock<TerminalHandler> = OnceLock::new();

pub(crate) struct TerminalCancellation {
    state: Arc<AtomicU8>,
    acquisition: scrozz_capture::CaptureCancellation,
}

impl TerminalCancellation {
    fn install() -> CliResult<Self> {
        let handler = TERMINAL_HANDLER.get_or_init(|| {
            let state = Arc::new(AtomicU8::new(0));
            let acquisition = Arc::new(Mutex::new(scrozz_capture::CaptureCancellation::new()));
            let signal_state = Arc::clone(&state);
            let signal_acquisition = Arc::clone(&acquisition);
            let install_error = ctrlc::set_handler(move || {
                let _ = advance_terminal_cancellation(&signal_state);
                signal_acquisition
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .cancel();
            })
            .err()
            .map(|error| error.to_string());
            TerminalHandler {
                state,
                acquisition,
                install_error,
            }
        });
        if let Some(error) = &handler.install_error {
            return Err(CliError::Core(CoreError::Platform(format!(
                "could not install scrolling-capture cancellation: {error}"
            ))));
        }
        let acquisition = scrozz_capture::CaptureCancellation::new();
        *handler
            .acquisition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = acquisition.clone();
        handler.state.store(0, Ordering::Release);
        Ok(Self {
            state: Arc::clone(&handler.state),
            acquisition,
        })
    }
}

impl CancelSignal for TerminalCancellation {
    fn cancellation(&mut self) -> Option<CancelAction> {
        match self.state.load(Ordering::Acquire) {
            1 => Some(CancelAction::Keep),
            2 => Some(CancelAction::Abort),
            _ => None,
        }
    }
}

pub(crate) fn fail_if_terminal_abort(cancel: &mut Option<TerminalCancellation>) -> CliResult<()> {
    if cancel
        .as_mut()
        .and_then(|signal| signal.cancellation())
        .is_some_and(|action| action == CancelAction::Abort)
    {
        return Err(CliError::Core(CoreError::Cancelled));
    }
    Ok(())
}

pub(crate) fn seal_terminal_output(cancel: &mut Option<TerminalCancellation>) -> CliResult<()> {
    let Some(cancel) = cancel else {
        return Ok(());
    };
    loop {
        match cancel.state.load(Ordering::Acquire) {
            2 => return Err(CliError::Core(CoreError::Cancelled)),
            3 => return Ok(()),
            state @ (0 | 1) => {
                if cancel
                    .state
                    .compare_exchange(state, 3, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Ok(());
                }
            }
            _ => return Ok(()),
        }
    }
}

pub(crate) fn advance_terminal_cancellation(state: &AtomicU8) -> Option<u8> {
    loop {
        let current = state.load(Ordering::Acquire);
        let next = match current {
            0 => 1,
            1 => 2,
            // Publication has begun. A late signal must not report cancellation
            // for output that is already becoming visible.
            _ => return None,
        };
        if state
            .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Some(next);
        }
    }
}

pub(crate) fn report_terminal_scroll_progress(progress: Progress) {
    match progress {
        Progress::Prepared {
            driver,
            automatic: true,
            ..
        } => eprintln!("scrozz: {driver} will move the selected window"),
        Progress::Prepared {
            manual_reason: Some(reason),
            ..
        } => eprintln!("scrozz: scroll the selected window manually; {reason}"),
        Progress::Prepared { .. } => {
            eprintln!("scrozz: scroll the selected window manually while capture follows");
        }
        Progress::FrameCaptured { frame } => {
            eprintln!("scrozz: captured viewport {frame}");
        }
        Progress::WaitingForManualScroll => {
            eprintln!("scrozz: waiting for one scroll in any direction");
        }
        Progress::DirectionDetected { direction } => {
            eprintln!("scrozz: following {direction:?} movement");
        }
        Progress::Advanced {
            frame,
            delta,
            output_extent,
            ..
        } => eprintln!(
            "scrozz: stitched viewport {frame} ({delta} px movement, {output_extent} px total)"
        ),
        Progress::Stalled { count } => {
            eprintln!("scrozz: no movement detected (probe {count})");
        }
        Progress::WaitingForOverlap { reason } => {
            eprintln!("scrozz: scroll back slowly to reconnect: {reason}");
        }
        Progress::AwaitingFinish { reason } => {
            eprintln!("scrozz: acquisition paused ({reason:?}); choose Finish or Discard");
        }
        Progress::Interrupted { reason } => {
            eprintln!("scrozz: keeping the valid stitched prefix after: {reason}");
        }
        Progress::Finished { reason, .. } => {
            eprintln!("scrozz: scrolling capture finished ({reason:?})");
        }
    }
}

pub(crate) fn scrolling_capture_with<C, F>(
    backend: &dyn CaptureBackend,
    request: CaptureRequest,
    axis: ScrollAxis,
    cancel: &mut C,
    progress: F,
) -> CliResult<Capture>
where
    C: CancelSignal,
    F: FnMut(Progress),
{
    let target = resolve_scrolling_target(backend, request)?;
    scrolling_capture_target_with(target, axis, cancel, progress)
}

#[derive(Debug, Clone)]
pub(crate) struct ScrollingTarget {
    request: CaptureRequest,
    context: ScrollingContext,
}

#[derive(Debug, Clone)]
pub(crate) enum ScrollingContext {
    Native {
        display: Box<Display>,
        viewport: LogicalRect,
        window: WindowId,
        crop: Option<FrameCrop>,
        selection: Option<RelativeViewport>,
    },
    ManualPortal,
}

impl ScrollingTarget {
    pub(crate) fn new(
        request: CaptureRequest,
        display: Display,
        viewport: LogicalRect,
        window: WindowId,
    ) -> Self {
        Self {
            request,
            context: ScrollingContext::Native {
                display: Box::new(display),
                viewport,
                window,
                crop: None,
                selection: None,
            },
        }
    }

    fn with_crop(mut self, crop: FrameCrop) -> Self {
        if let ScrollingContext::Native {
            crop: target_crop, ..
        } = &mut self.context
        {
            *target_crop = Some(crop);
        }
        self
    }

    fn with_selection(mut self, selection: RelativeViewport) -> Self {
        if let ScrollingContext::Native {
            selection: target_selection,
            ..
        } = &mut self.context
        {
            *target_selection = Some(selection);
        }
        self
    }

    fn manual_portal(request: CaptureRequest) -> Self {
        Self {
            request,
            context: ScrollingContext::ManualPortal,
        }
    }

    pub(crate) fn capture_target(&self) -> CaptureTarget {
        self.request.target.clone()
    }

    pub(crate) fn may_synthesize_scroll(&self) -> bool {
        matches!(self.context, ScrollingContext::Native { .. })
    }

    /// Whether synthetic scrolling needs the compact Recent Captures root to
    /// ignore pointer input.
    ///
    /// macOS posts wheel events directly to the selected process after
    /// revalidating its window identity, so making visible HUD/cards
    /// click-through there is both unnecessary and actively harmful.
    pub(crate) fn requires_overlay_passthrough(&self) -> bool {
        self.may_synthesize_scroll() && !cfg!(target_os = "macos")
    }

    pub(crate) fn overlay_surface(&self) -> Option<(LogicalRect, LogicalRect, ScaleFactor)> {
        match &self.context {
            ScrollingContext::Native {
                display, viewport, ..
            } => Some((
                LogicalRect::new(
                    LogicalPoint::new(
                        viewport.origin.x - display.work_area.origin.x,
                        viewport.origin.y - display.work_area.origin.y,
                    ),
                    viewport.size,
                ),
                display.work_area,
                display.scale,
            )),
            ScrollingContext::ManualPortal => None,
        }
    }

    pub(crate) fn refresh(self, backend: &dyn CaptureBackend) -> CliResult<Self> {
        if matches!(&self.context, ScrollingContext::ManualPortal) {
            return Ok(self);
        }
        let windows = backend.windows()?;
        let displays = backend.displays()?;
        self.refresh_from_snapshots(windows, displays)
    }

    fn refresh_from_snapshots(
        self,
        windows: Vec<Window>,
        displays: Vec<Display>,
    ) -> CliResult<Self> {
        let (window_id, selection, preferred_display, preferred_scale) = match &self.context {
            ScrollingContext::ManualPortal => return Ok(self),
            ScrollingContext::Native {
                display,
                window,
                selection,
                ..
            } => (
                window.clone(),
                *selection,
                display.id.clone(),
                display.scale,
            ),
        };
        let window = windows
            .into_iter()
            .find(|window| window.id == window_id)
            .ok_or_else(|| {
                CliError::Core(CoreError::TargetGone(format!(
                    "window {} vanished before scrolling capture started",
                    window_id.0
                )))
            })?;
        if !window.is_visible || window.bounds.is_empty() {
            return Err(CliError::Core(CoreError::TargetGone(format!(
                "window {} is no longer visible for scrolling capture",
                window_id.0
            ))));
        }
        if cfg!(target_os = "windows")
            && window.display != preferred_display
            && displays
                .iter()
                .find(|display| display.id == window.display)
                .is_some_and(|display| display.scale != preferred_scale)
        {
            return Err(CliError::Core(CoreError::TargetGone(format!(
                "window {} moved to a display with a different scale; redraw the scrolling area",
                window_id.0
            ))));
        }
        match selection {
            Some(selection) => {
                let viewport = selection.resolve(window.bounds);
                let display = display_for_refreshed_region(
                    &displays,
                    &window,
                    viewport,
                    &preferred_display,
                )
                .ok_or_else(|| {
                    CliError::Core(CoreError::TargetGone(format!(
                        "the selected area for window {} is no longer contained by a connected display",
                        window_id.0
                    )))
                })?;
                resolved_native_scrolling_region_target(self.request, display, window, viewport)
            }
            None => {
                let display = displays
                    .into_iter()
                    .find(|display| display.id == window.display)
                    .ok_or_else(|| {
                        CliError::Core(CoreError::TargetGone(format!(
                            "display {} containing window {} is no longer connected",
                            window.display.0, window_id.0
                        )))
                    })?;
                resolved_native_scrolling_target(self.request, display, window)
            }
        }
    }

    fn session_config(
        &self,
        axis: ScrollAxis,
        control: Option<ScrollControl>,
    ) -> CliResult<ScrollSessionConfig> {
        let mut config = match &self.context {
            ScrollingContext::Native {
                display,
                viewport,
                window,
                ..
            } => scroll_session_config(display, *viewport, axis, window.clone()),
            ScrollingContext::ManualPortal => {
                let at = LogicalPoint::new(0.0, 0.0);
                let gesture = match axis {
                    ScrollAxis::Vertical => ScrollGesture::down(at, 1.0),
                    ScrollAxis::Horizontal => ScrollGesture::right(at, 1.0),
                };
                let mut config = ScrollSessionConfig::new(gesture);
                if cfg!(target_os = "macos") {
                    config.settle_delay = Duration::from_millis(280);
                    config.manual_poll_interval = Duration::from_millis(150);
                }
                Ok(config)
            }
        }?;
        if let Some(control) = control {
            config = config.with_control(control);
            if control == ScrollControl::Manual {
                config.max_frames = 400;
            }
        }
        Ok(config)
    }

    fn direction_detecting_session_config(
        &self,
        control: ScrollControl,
    ) -> CliResult<ScrollSessionConfig> {
        let (vertical_amount, horizontal_amount) = match &self.context {
            ScrollingContext::Native { viewport, .. } => {
                (viewport.size.height * 0.65, viewport.size.width * 0.65)
            }
            ScrollingContext::ManualPortal => (1.0, 1.0),
        };
        let mut config = self
            .session_config(ScrollAxis::Vertical, Some(control))?
            .with_direction_detection(vertical_amount, horizontal_amount);
        config.max_frames = 400;
        Ok(config)
    }
}

pub(crate) fn resolve_scrolling_target(
    backend: &dyn CaptureBackend,
    request: CaptureRequest,
) -> CliResult<ScrollingTarget> {
    if is_wayland() && request.target == wayland_portal_picker_target() {
        return Ok(ScrollingTarget::manual_portal(request));
    }
    if request.target.is_window() {
        return Err(CliError::Core(CoreError::InvalidRequest(
            "scrolling capture cannot treat an ordinary window id as a Wayland portal choice"
                .to_owned(),
        )));
    }
    let CaptureTarget::Display(display_id) = &request.target else {
        return Err(CliError::Core(CoreError::InvalidRequest(
            "scrolling capture requires one display or a Wayland portal window".to_owned(),
        )));
    };
    let display = backend
        .displays()?
        .into_iter()
        .find(|display| display.id == *display_id)
        .ok_or_else(|| {
            CliError::Core(CoreError::TargetGone(format!(
                "display {} vanished before scrolling capture started",
                display_id.0
            )))
        })?;
    let window = match backend.windows() {
        Ok(windows) => select_scrolling_window(windows, &display),
        Err(error) => return Err(CliError::Core(error)),
    }
    .ok_or_else(|| {
        CliError::Core(CoreError::InvalidRequest(format!(
            "no visible application window is available on display {}; focus the window to \
             capture and retry (or use --delay to switch to it after starting the command)",
            display.id.0
        )))
    })?;
    resolved_native_scrolling_target(request, display, window)
}

pub(crate) fn resolve_scrolling_region_target(
    backend: &dyn CaptureBackend,
    region: LogicalRect,
) -> CliResult<ScrollingTarget> {
    resolve_scrolling_region_target_on_display(backend, region, None)
}

pub(crate) fn resolve_scrolling_region_target_on_display(
    backend: &dyn CaptureBackend,
    region: LogicalRect,
    selected_display: Option<&scrozz_core::DisplayId>,
) -> CliResult<ScrollingTarget> {
    if region.is_empty() {
        return Err(CliError::Core(CoreError::InvalidRequest(
            "draw a scrolling area with positive width and height".to_owned(),
        )));
    }
    let displays = backend.displays()?;
    let selected_display = selected_display
        .map(|selected| {
            displays
                .iter()
                .find(|display| display.id == *selected)
                .cloned()
                .ok_or_else(|| {
                    CliError::Core(CoreError::TargetGone(format!(
                        "display {} vanished before scrolling capture started",
                        selected.0
                    )))
                })
        })
        .transpose()?;
    let windows = backend.windows()?;
    let window = select_scrolling_window_for_region_on_display(
        windows,
        &displays,
        region,
        selected_display.as_ref(),
    )
    .ok_or_else(|| {
        CliError::Core(CoreError::InvalidRequest(
            "draw the scrolling area entirely inside one visible application window".to_owned(),
        ))
    })?;
    let display = selected_display
        .or_else(|| {
            displays
                .iter()
                .find(|display| display.id == window.display)
                .cloned()
        })
        .ok_or_else(|| {
            CliError::Core(CoreError::TargetGone(format!(
                "display {} containing window {} is no longer connected",
                window.display.0, window.id.0
            )))
        })?;
    let request = CaptureRequest {
        target: CaptureTarget::Display(display.id.clone()),
        cursor: scrozz_core::CursorMode::Hidden,
        include_window_shadow: false,
    };
    resolved_native_scrolling_region_target(request, display, window, region)
}

pub(crate) fn scrolling_capture_target_with<C, F>(
    target: ScrollingTarget,
    axis: ScrollAxis,
    cancel: &mut C,
    progress: F,
) -> CliResult<Capture>
where
    C: CancelSignal,
    F: FnMut(Progress),
{
    let acquisition_cancellation = scrozz_capture::CaptureCancellation::new();
    scrolling_capture_target_with_cancellation(
        target,
        axis,
        cancel,
        &acquisition_cancellation,
        progress,
    )
}

pub(crate) fn scrolling_capture_target_with_cancellation<C, F>(
    target: ScrollingTarget,
    axis: ScrollAxis,
    cancel: &mut C,
    acquisition_cancellation: &scrozz_capture::CaptureCancellation,
    progress: F,
) -> CliResult<Capture>
where
    C: CancelSignal,
    F: FnMut(Progress),
{
    scrolling_capture_target_with_control_and_cancellation(
        target,
        axis,
        None,
        cancel,
        acquisition_cancellation,
        progress,
    )
}

pub(crate) fn scrolling_capture_target_with_control_and_cancellation<C, F>(
    target: ScrollingTarget,
    axis: ScrollAxis,
    control: Option<ScrollControl>,
    cancel: &mut C,
    acquisition_cancellation: &scrozz_capture::CaptureCancellation,
    progress: F,
) -> CliResult<Capture>
where
    C: CancelSignal,
    F: FnMut(Progress),
{
    scrolling_capture_target_with_optional_axis_and_cancellation(
        target,
        Some(axis),
        control,
        cancel,
        acquisition_cancellation,
        progress,
    )
}

pub(crate) fn scrolling_capture_target_with_detected_direction_and_cancellation<C, F>(
    target: ScrollingTarget,
    control: ScrollControl,
    cancel: &mut C,
    acquisition_cancellation: &scrozz_capture::CaptureCancellation,
    progress: F,
) -> CliResult<Capture>
where
    C: CancelSignal,
    F: FnMut(Progress),
{
    scrolling_capture_target_with_optional_axis_and_cancellation(
        target,
        None,
        Some(control),
        cancel,
        acquisition_cancellation,
        progress,
    )
}

fn scrolling_capture_target_with_optional_axis_and_cancellation<C, F>(
    target: ScrollingTarget,
    axis: Option<ScrollAxis>,
    control: Option<ScrollControl>,
    cancel: &mut C,
    acquisition_cancellation: &scrozz_capture::CaptureCancellation,
    progress: F,
) -> CliResult<Capture>
where
    C: CancelSignal,
    F: FnMut(Progress),
{
    if acquisition_cancellation.is_cancelled() {
        return Err(CliError::Core(CoreError::Cancelled));
    }
    let config = match axis {
        Some(axis) => target.session_config(axis, control)?,
        None => target.direction_detecting_session_config(control.unwrap_or_default())?,
    };
    let crop = match &target.context {
        ScrollingContext::Native { crop, .. } => *crop,
        ScrollingContext::ManualPortal => None,
    };
    let request = target.request;
    let source = CaptureFrameSource {
        session: scrozz_capture::frame_session_with_cancellation(
            request.clone(),
            acquisition_cancellation,
        )
        .map_err(CliError::Core)?,
        crop,
        acquisition_cancellation: acquisition_cancellation.clone(),
    };
    let driver = platform::scroll_driver()?;
    let output = ScrollSession::new(source, driver, ThreadPacer, config).run(cancel, progress)?;
    Ok(output.into_capture(request.target))
}

pub(crate) struct CaptureFrameSource {
    session: Box<dyn scrozz_capture::FrameSession>,
    crop: Option<FrameCrop>,
    acquisition_cancellation: scrozz_capture::CaptureCancellation,
}

impl FrameSource for CaptureFrameSource {
    fn capture_frame(&mut self) -> scrozz_core::Result<scrozz_core::Frame> {
        if self.acquisition_cancellation.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        let frame = self.session.capture_frame()?;
        if self.acquisition_cancellation.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        match self.crop {
            Some(crop) => crop.apply(frame),
            None => Ok(frame),
        }
    }

    fn name(&self) -> &str {
        self.session.name()
    }
}

pub(crate) fn resolved_native_scrolling_target(
    mut request: CaptureRequest,
    display: Display,
    window: Window,
) -> CliResult<ScrollingTarget> {
    let viewport = intersect_rect(window.bounds, display.work_area).ok_or_else(|| {
        CliError::Core(CoreError::InvalidRequest(format!(
            "the selected window is outside display {}'s usable area",
            display.id.0
        )))
    })?;
    let crop = FrameCrop::new(window.bounds, viewport)?;
    let window_id = window.id;
    request.target = CaptureTarget::Window(window_id.clone());
    // X11 enumeration reports the outer frame while a shadowless capture reads
    // only the client drawable. Capture the frame window so work-area clipping
    // and frame pixels share one coordinate space.
    request.include_window_shadow = needs_outer_frame_capture(&window_id);
    Ok(ScrollingTarget::new(request, display, viewport, window_id).with_crop(crop))
}

fn resolved_native_scrolling_region_target(
    mut request: CaptureRequest,
    display: Display,
    window: Window,
    region: LogicalRect,
) -> CliResult<ScrollingTarget> {
    let visible_window = intersect_rect(window.bounds, display.work_area).ok_or_else(|| {
        CliError::Core(CoreError::InvalidRequest(format!(
            "window {} is outside display {}'s usable area",
            window.id.0, display.id.0
        )))
    })?;
    if !contains_rect(visible_window, region) {
        return Err(CliError::Core(CoreError::InvalidRequest(
            "draw the scrolling area entirely inside the visible window content".to_owned(),
        )));
    }
    let crop = FrameCrop::new(window.bounds, region)?;
    let selection = RelativeViewport::new(window.bounds, region);
    let window_id = window.id;
    request.target = CaptureTarget::Window(window_id.clone());
    request.include_window_shadow = needs_outer_frame_capture(&window_id);
    Ok(ScrollingTarget::new(request, display, region, window_id)
        .with_crop(crop)
        .with_selection(selection))
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RelativeViewport {
    offset: LogicalPoint,
    size: LogicalSize,
}

impl RelativeViewport {
    fn new(window: LogicalRect, viewport: LogicalRect) -> Self {
        Self {
            offset: LogicalPoint::new(
                viewport.origin.x - window.origin.x,
                viewport.origin.y - window.origin.y,
            ),
            size: viewport.size,
        }
    }

    fn resolve(self, window: LogicalRect) -> LogicalRect {
        LogicalRect::new(
            LogicalPoint::new(
                window.origin.x + self.offset.x,
                window.origin.y + self.offset.y,
            ),
            self.size,
        )
    }
}

pub(crate) fn needs_outer_frame_capture(window: &WindowId) -> bool {
    cfg!(target_os = "linux") && window.0.starts_with("x11:")
}

pub(crate) fn wayland_portal_picker_target() -> CaptureTarget {
    CaptureTarget::Window(WindowId(WAYLAND_PORTAL_PICKER_WINDOW_ID.to_owned()))
}

pub(crate) fn wayland_scrolling_capture_target(
    display: &DisplaySelector,
) -> CliResult<CaptureTarget> {
    if *display != DisplaySelector::Active {
        return Err(CliError::Core(CoreError::Unsupported {
            what: "Wayland scrolling capture with an explicit display selector".to_owned(),
            why: "Wayland can safely capture one scrolling window only through the desktop \
                  portal picker; omit the selector or use `--scrolling=active`"
                .to_owned(),
        }));
    }
    Ok(wayland_portal_picker_target())
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameCrop {
    source_bounds: LogicalRect,
    viewport: LogicalRect,
}

impl FrameCrop {
    fn new(source_bounds: LogicalRect, viewport: LogicalRect) -> CliResult<Self> {
        if source_bounds.is_empty() || viewport.is_empty() {
            return Err(CliError::Core(CoreError::InvalidRequest(
                "scrolling viewport must have positive dimensions".to_owned(),
            )));
        }
        Ok(Self {
            source_bounds,
            viewport,
        })
    }

    fn apply(self, frame: Frame) -> scrozz_core::Result<Frame> {
        if frame.width() == 0 || frame.height() == 0 || !frame.is_well_formed() {
            return Err(CoreError::Platform(
                "the scrolling frame source returned malformed pixel geometry".to_owned(),
            ));
        }
        let relative_x = self.viewport.origin.x - self.source_bounds.origin.x;
        let relative_y = self.viewport.origin.y - self.source_bounds.origin.y;
        let x_scale = f64::from(frame.width()) / self.source_bounds.size.width;
        let y_scale = f64::from(frame.height()) / self.source_bounds.size.height;

        // Round inward so pixels outside the selected display/work area can
        // never enter matching or output, even at fractional scale factors.
        let left = (relative_x.max(0.0) * x_scale).ceil() as u32;
        let top = (relative_y.max(0.0) * y_scale).ceil() as u32;
        let right = ((relative_x + self.viewport.size.width).min(self.source_bounds.size.width)
            * x_scale)
            .floor() as u32;
        let bottom = ((relative_y + self.viewport.size.height).min(self.source_bounds.size.height)
            * y_scale)
            .floor() as u32;

        if left >= right || top >= bottom || right > frame.width() || bottom > frame.height() {
            return Err(CoreError::Platform(format!(
                "selected scrolling viewport maps outside the captured frame: \
                 crop=({left},{top})..({right},{bottom}), frame={}x{}",
                frame.width(),
                frame.height()
            )));
        }
        if left == 0 && top == 0 && right == frame.width() && bottom == frame.height() {
            return Ok(frame);
        }

        let bytes_per_pixel = frame.format.bytes_per_pixel();
        let width = right - left;
        let height = bottom - top;
        let row_bytes = width as usize * bytes_per_pixel;
        let mut pixels = Vec::with_capacity(row_bytes * height as usize);
        for y in top..bottom {
            let start = y as usize * frame.stride + left as usize * bytes_per_pixel;
            pixels.extend_from_slice(&frame.data[start..start + row_bytes]);
        }

        Ok(Frame {
            data: pixels,
            size: PhysicalSize::new(f64::from(width), f64::from(height)),
            stride: row_bytes,
            format: frame.format,
            color_space: frame.color_space,
            scale: frame.scale,
        })
    }
}

pub(crate) fn select_scrolling_window(windows: Vec<Window>, display: &Display) -> Option<Window> {
    windows.into_iter().find(|window| {
        window.is_visible
            && window.display == display.id
            && !window.bounds.is_empty()
            && !is_capture_control_window(window)
    })
}

fn select_scrolling_window_for_region(windows: Vec<Window>, region: LogicalRect) -> Option<Window> {
    select_scrolling_window_for_region_on_display(windows, &[], region, None)
}

fn select_scrolling_window_for_region_on_display(
    windows: Vec<Window>,
    displays: &[Display],
    region: LogicalRect,
    selected_display: Option<&Display>,
) -> Option<Window> {
    let center = LogicalPoint::new(
        region.origin.x + region.size.width * 0.5,
        region.origin.y + region.size.height * 0.5,
    );
    let mut candidates = windows.into_iter().filter(|window| {
        window.is_visible && !window.bounds.is_empty() && !is_capture_control_window(window)
    });
    let Some(selected_display) = selected_display else {
        return candidates.find(|window| {
            contains_point(window.bounds, center) && contains_rect(window.bounds, region)
        });
    };
    candidates.find(|window| {
        let declared_display = displays.iter().find(|display| display.id == window.display);
        let eligible_display = window.display == selected_display.id
            || declared_display.is_some_and(|display| {
                !contains_rect(display.bounds, window.bounds)
                    && spanning_coordinate_spaces_compatible(display, selected_display)
                    && intersect_rect(window.bounds, selected_display.bounds).is_some()
            });
        eligible_display
            && contains_point(window.bounds, center)
            && contains_rect(window.bounds, region)
    })
}

fn contains_point(rect: LogicalRect, point: LogicalPoint) -> bool {
    point.x >= rect.origin.x
        && point.y >= rect.origin.y
        && point.x <= rect.origin.x + rect.size.width
        && point.y <= rect.origin.y + rect.size.height
}

fn contains_rect(outer: LogicalRect, inner: LogicalRect) -> bool {
    const TOLERANCE: f64 = 1.0;
    inner.origin.x + TOLERANCE >= outer.origin.x
        && inner.origin.y + TOLERANCE >= outer.origin.y
        && inner.origin.x + inner.size.width <= outer.origin.x + outer.size.width + TOLERANCE
        && inner.origin.y + inner.size.height <= outer.origin.y + outer.size.height + TOLERANCE
}

fn display_for_refreshed_region(
    displays: &[Display],
    window: &Window,
    region: LogicalRect,
    previous: &scrozz_core::DisplayId,
) -> Option<Display> {
    let current = displays.iter().find(|display| display.id == window.display);
    let previous = displays.iter().find(|display| display.id == *previous);
    let spans_beyond_current =
        current.is_some_and(|display| !contains_rect(display.bounds, window.bounds));
    if spans_beyond_current
        && let Some(previous) = previous.filter(|previous| {
            current.is_some_and(|current| spanning_coordinate_spaces_compatible(current, previous))
                && contains_rect(previous.bounds, region)
        })
    {
        return Some(previous.clone());
    }
    current
        .filter(|display| contains_rect(display.bounds, region))
        .or_else(|| {
            displays.iter().find(|display| {
                current
                    .is_none_or(|current| spanning_coordinate_spaces_compatible(current, display))
                    && contains_rect(display.bounds, region)
            })
        })
        .cloned()
}

fn spanning_coordinate_spaces_compatible(declared: &Display, selected: &Display) -> bool {
    coordinate_spaces_compatible(declared.scale, selected.scale, cfg!(target_os = "windows"))
}

const fn coordinate_spaces_compatible(
    declared: ScaleFactor,
    selected: ScaleFactor,
    require_same_scale: bool,
) -> bool {
    !require_same_scale || declared.get() == selected.get()
}

fn is_capture_control_window(window: &Window) -> bool {
    window
        .application
        .as_deref()
        .is_some_and(|application| application.eq_ignore_ascii_case("scrozz"))
}

pub(crate) fn intersect_rect(left: LogicalRect, right: LogicalRect) -> Option<LogicalRect> {
    let x = left.origin.x.max(right.origin.x);
    let y = left.origin.y.max(right.origin.y);
    let right_edge = (left.origin.x + left.size.width).min(right.origin.x + right.size.width);
    let bottom = (left.origin.y + left.size.height).min(right.origin.y + right.size.height);
    (right_edge > x && bottom > y).then(|| {
        LogicalRect::new(
            LogicalPoint::new(x, y),
            LogicalSize::new(right_edge - x, bottom - y),
        )
    })
}

pub(crate) fn scroll_session_config(
    display: &Display,
    area: LogicalRect,
    axis: ScrollAxis,
    window: WindowId,
) -> CliResult<ScrollSessionConfig> {
    if area.is_empty() {
        return Err(CliError::Core(CoreError::InvalidRequest(format!(
            "display {} has no usable scrolling viewport",
            display.id.0
        ))));
    }
    let at = scrozz_core::LogicalPoint::new(
        area.origin.x + area.size.width / 2.0,
        area.origin.y + area.size.height / 2.0,
    );
    // Keeping roughly a third of the viewport as overlap gives the matcher real
    // evidence while still making useful progress on each capture.
    let gesture = match axis {
        ScrollAxis::Vertical => ScrollGesture::down(at, area.size.height * 0.65),
        ScrollAxis::Horizontal => ScrollGesture::right(at, area.size.width * 0.65),
    }
    .on_display(display.id.clone())
    .in_window(window);
    Ok(ScrollSessionConfig::new(gesture))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::{DisplayId, LogicalSize, ScaleFactor};

    fn fixture_display() -> Display {
        Display {
            id: DisplayId("display-2".to_owned()),
            name: "Fixture display".to_owned(),
            bounds: LogicalRect::new(
                LogicalPoint::new(100.0, 50.0),
                LogicalSize::new(1_200.0, 800.0),
            ),
            work_area: LogicalRect::new(
                LogicalPoint::new(100.0, 70.0),
                LogicalSize::new(1_200.0, 760.0),
            ),
            scale: ScaleFactor::new(1.5),
            is_primary: false,
        }
    }

    fn fixture_window(
        id: &str,
        application: &str,
        display: &str,
        bounds: LogicalRect,
        is_visible: bool,
    ) -> Window {
        Window {
            id: WindowId(id.to_owned()),
            title: Some(format!("{application} window")),
            application: Some(application.to_owned()),
            bounds,
            display: DisplayId(display.to_owned()),
            is_visible,
        }
    }

    #[test]
    fn scrolling_refresh_keeps_window_identity_and_recomputes_geometry() {
        let initial_display = fixture_display();
        let initial_window = fixture_window(
            "browser-window",
            "Browser",
            "display-2",
            LogicalRect::new(
                LogicalPoint::new(120.0, 80.0),
                LogicalSize::new(900.0, 700.0),
            ),
            true,
        );
        let target = resolved_native_scrolling_target(
            CaptureRequest {
                target: CaptureTarget::Display(initial_display.id.clone()),
                cursor: scrozz_core::CursorMode::Hidden,
                include_window_shadow: false,
            },
            initial_display,
            initial_window,
        )
        .expect("initial target");

        let mut moved_display = fixture_display();
        moved_display.id = DisplayId("display-3".to_owned());
        moved_display.bounds.origin.x = 1_300.0;
        moved_display.work_area.origin.x = 1_300.0;
        let moved_bounds = LogicalRect::new(
            LogicalPoint::new(1_340.0, 100.0),
            LogicalSize::new(700.0, 500.0),
        );
        let refreshed = target
            .refresh_from_snapshots(
                vec![
                    fixture_window("other", "Notes", "display-3", moved_bounds, true),
                    fixture_window("browser-window", "Browser", "display-3", moved_bounds, true),
                ],
                vec![moved_display.clone()],
            )
            .expect("same window on its current display");

        assert_eq!(
            refreshed.capture_target(),
            CaptureTarget::Window(WindowId("browser-window".to_owned()))
        );
        let ScrollingContext::Native {
            display,
            viewport,
            window,
            crop,
            ..
        } = &refreshed.context
        else {
            panic!("native target became a portal target");
        };
        assert_eq!(display.id, moved_display.id);
        assert_eq!(*viewport, moved_bounds);
        assert_eq!(*window, WindowId("browser-window".to_owned()));
        assert_eq!(crop.map(|crop| crop.source_bounds), Some(moved_bounds));

        let config = refreshed
            .session_config(ScrollAxis::Horizontal, None)
            .expect("refreshed gesture");
        assert_eq!(config.gesture.display, Some(moved_display.id));
        assert_eq!(config.gesture.at, LogicalPoint::new(1_690.0, 350.0));

        let detecting = refreshed
            .direction_detecting_session_config(ScrollControl::Automatic)
            .expect("direction-detecting gesture");
        let amounts = detecting
            .direction_detection
            .expect("GUI scrolling learns its route");
        assert_eq!(amounts.vertical, 325.0);
        assert_eq!(amounts.horizontal, 455.0);
        assert_eq!(detecting.max_frames, 400);
    }

    #[test]
    fn a_window_that_vanished_is_reported_rather_than_replaced() {
        let display = fixture_display();
        let window = fixture_window(
            "browser-window",
            "Browser",
            "display-2",
            LogicalRect::new(
                LogicalPoint::new(120.0, 80.0),
                LogicalSize::new(900.0, 700.0),
            ),
            true,
        );
        let target = resolved_native_scrolling_target(
            CaptureRequest {
                target: CaptureTarget::Display(display.id.clone()),
                cursor: scrozz_core::CursorMode::Hidden,
                include_window_shadow: false,
            },
            display.clone(),
            window,
        )
        .expect("initial target");

        let error = target
            .refresh_from_snapshots(
                vec![fixture_window(
                    "some-other-window",
                    "Notes",
                    "display-2",
                    LogicalRect::new(
                        LogicalPoint::new(120.0, 80.0),
                        LogicalSize::new(900.0, 700.0),
                    ),
                    true,
                )],
                vec![display],
            )
            .expect_err("a different window must never be substituted");
        assert!(matches!(error, CliError::Core(CoreError::TargetGone(_))));
    }

    #[test]
    fn scrozz_own_windows_are_never_chosen_as_a_scrolling_target() {
        let display = fixture_display();
        let bounds = LogicalRect::new(
            LogicalPoint::new(120.0, 80.0),
            LogicalSize::new(900.0, 700.0),
        );
        let chosen = select_scrolling_window(
            vec![
                fixture_window("scrozz-card", "Scrozz", "display-2", bounds, true),
                fixture_window("browser", "Browser", "display-2", bounds, true),
            ],
            &display,
        )
        .expect("a window behind Scrozz's own chrome");
        assert_eq!(chosen.id, WindowId("browser".to_owned()));
    }

    #[test]
    fn a_drawn_region_selects_the_frontmost_window_that_fully_contains_it() {
        let region = LogicalRect::new(
            LogicalPoint::new(240.0, 180.0),
            LogicalSize::new(500.0, 320.0),
        );
        let chosen = select_scrolling_window_for_region(
            vec![
                fixture_window(
                    "utility",
                    "Notes",
                    "display-2",
                    LogicalRect::new(
                        LogicalPoint::new(700.0, 600.0),
                        LogicalSize::new(200.0, 120.0),
                    ),
                    true,
                ),
                fixture_window(
                    "browser",
                    "Browser",
                    "display-2",
                    LogicalRect::new(
                        LogicalPoint::new(120.0, 80.0),
                        LogicalSize::new(900.0, 700.0),
                    ),
                    true,
                ),
            ],
            region,
        )
        .expect("window under the complete drawn region");
        assert_eq!(chosen.id, WindowId("browser".to_owned()));
    }

    #[test]
    fn selected_display_disambiguates_overlapping_logical_desktops() {
        let left = Display {
            id: DisplayId("left".to_owned()),
            name: "Left".to_owned(),
            bounds: LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(1_920.0, 1_080.0),
            ),
            work_area: LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(1_920.0, 1_080.0),
            ),
            scale: ScaleFactor::IDENTITY,
            is_primary: true,
        };
        let right = Display {
            id: DisplayId("right".to_owned()),
            name: "Right".to_owned(),
            bounds: LogicalRect::new(
                LogicalPoint::new(960.0, 0.0),
                LogicalSize::new(1_280.0, 720.0),
            ),
            work_area: LogicalRect::new(
                LogicalPoint::new(960.0, 0.0),
                LogicalSize::new(1_280.0, 720.0),
            ),
            scale: ScaleFactor::new(2.0),
            is_primary: false,
        };
        let region = LogicalRect::new(
            LogicalPoint::new(1_100.0, 120.0),
            LogicalSize::new(300.0, 240.0),
        );
        let chosen = select_scrolling_window_for_region_on_display(
            vec![
                fixture_window(
                    "wrong",
                    "Wrong",
                    "left",
                    LogicalRect::new(
                        LogicalPoint::new(1_000.0, 80.0),
                        LogicalSize::new(700.0, 500.0),
                    ),
                    true,
                ),
                fixture_window(
                    "right-window",
                    "Browser",
                    "right",
                    LogicalRect::new(
                        LogicalPoint::new(1_000.0, 80.0),
                        LogicalSize::new(700.0, 500.0),
                    ),
                    true,
                ),
            ],
            &[left, right.clone()],
            region,
            Some(&right),
        )
        .expect("window on the display that owned the selector");
        assert_eq!(chosen.id, WindowId("right-window".to_owned()));
        assert!(!coordinate_spaces_compatible(
            ScaleFactor::IDENTITY,
            ScaleFactor::new(2.0),
            true,
        ));
        assert!(coordinate_spaces_compatible(
            ScaleFactor::IDENTITY,
            ScaleFactor::new(2.0),
            false,
        ));
    }

    #[test]
    fn a_spanning_window_can_be_selected_on_its_non_dominant_display() {
        let left = Display {
            id: DisplayId("left".to_owned()),
            name: "Left".to_owned(),
            bounds: LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(1_000.0, 800.0),
            ),
            work_area: LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(1_000.0, 800.0),
            ),
            scale: ScaleFactor::IDENTITY,
            is_primary: true,
        };
        let right = Display {
            id: DisplayId("right".to_owned()),
            name: "Right".to_owned(),
            bounds: LogicalRect::new(
                LogicalPoint::new(1_000.0, 0.0),
                LogicalSize::new(1_000.0, 800.0),
            ),
            work_area: LogicalRect::new(
                LogicalPoint::new(1_000.0, 0.0),
                LogicalSize::new(1_000.0, 800.0),
            ),
            scale: ScaleFactor::IDENTITY,
            is_primary: false,
        };
        let chosen = select_scrolling_window_for_region_on_display(
            vec![fixture_window(
                "spanning",
                "Browser",
                "left",
                LogicalRect::new(
                    LogicalPoint::new(800.0, 80.0),
                    LogicalSize::new(600.0, 600.0),
                ),
                true,
            )],
            &[left, right.clone()],
            LogicalRect::new(
                LogicalPoint::new(1_080.0, 120.0),
                LogicalSize::new(240.0, 300.0),
            ),
            Some(&right),
        )
        .expect("spanning window on the selected display");
        assert_eq!(chosen.id, WindowId("spanning".to_owned()));
    }

    #[test]
    fn a_foreground_spanning_window_beats_a_background_exact_display_window() {
        let left = Display {
            id: DisplayId("left".to_owned()),
            name: "Left".to_owned(),
            bounds: LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(1_000.0, 800.0),
            ),
            work_area: LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(1_000.0, 800.0),
            ),
            scale: ScaleFactor::IDENTITY,
            is_primary: true,
        };
        let right = Display {
            id: DisplayId("right".to_owned()),
            name: "Right".to_owned(),
            bounds: LogicalRect::new(
                LogicalPoint::new(1_000.0, 0.0),
                LogicalSize::new(1_000.0, 800.0),
            ),
            work_area: LogicalRect::new(
                LogicalPoint::new(1_000.0, 0.0),
                LogicalSize::new(1_000.0, 800.0),
            ),
            scale: ScaleFactor::IDENTITY,
            is_primary: false,
        };
        let region = LogicalRect::new(
            LogicalPoint::new(1_080.0, 120.0),
            LogicalSize::new(240.0, 300.0),
        );
        let chosen = select_scrolling_window_for_region_on_display(
            vec![
                fixture_window(
                    "foreground-spanning",
                    "Browser",
                    "left",
                    LogicalRect::new(
                        LogicalPoint::new(800.0, 80.0),
                        LogicalSize::new(600.0, 600.0),
                    ),
                    true,
                ),
                fixture_window(
                    "background-right",
                    "Notes",
                    "right",
                    LogicalRect::new(
                        LogicalPoint::new(1_000.0, 80.0),
                        LogicalSize::new(600.0, 600.0),
                    ),
                    true,
                ),
            ],
            &[left, right.clone()],
            region,
            Some(&right),
        )
        .expect("frontmost eligible window");
        assert_eq!(chosen.id, WindowId("foreground-spanning".to_owned()));
    }

    #[test]
    fn refresh_prefers_the_windows_current_display_over_overlapping_old_bounds() {
        let left = Display {
            id: DisplayId("left".to_owned()),
            name: "Left".to_owned(),
            bounds: LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(1_920.0, 1_080.0),
            ),
            work_area: LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(1_920.0, 1_080.0),
            ),
            scale: ScaleFactor::IDENTITY,
            is_primary: true,
        };
        let right = Display {
            id: DisplayId("right".to_owned()),
            name: "Right".to_owned(),
            bounds: LogicalRect::new(
                LogicalPoint::new(960.0, 0.0),
                LogicalSize::new(1_280.0, 720.0),
            ),
            work_area: LogicalRect::new(
                LogicalPoint::new(960.0, 0.0),
                LogicalSize::new(1_280.0, 720.0),
            ),
            scale: ScaleFactor::new(2.0),
            is_primary: false,
        };
        let initial_bounds = LogicalRect::new(
            LogicalPoint::new(1_100.0, 80.0),
            LogicalSize::new(600.0, 500.0),
        );
        let target = resolved_native_scrolling_region_target(
            CaptureRequest {
                target: CaptureTarget::Display(right.id.clone()),
                cursor: scrozz_core::CursorMode::Hidden,
                include_window_shadow: false,
            },
            right.clone(),
            fixture_window("browser", "Browser", "right", initial_bounds, true),
            LogicalRect::new(
                LogicalPoint::new(1_200.0, 140.0),
                LogicalSize::new(300.0, 240.0),
            ),
        )
        .expect("initial right-display target");

        let moved_bounds = LogicalRect::new(
            LogicalPoint::new(1_000.0, 80.0),
            LogicalSize::new(600.0, 500.0),
        );
        let refreshed = target
            .refresh_from_snapshots(
                vec![fixture_window(
                    "browser",
                    "Browser",
                    "left",
                    moved_bounds,
                    true,
                )],
                vec![left.clone(), right],
            )
            .expect("window moved fully onto the left display");
        let ScrollingContext::Native {
            display, viewport, ..
        } = refreshed.context
        else {
            panic!("native target became a portal target");
        };
        assert_eq!(display.id, left.id);
        assert_eq!(display.scale, ScaleFactor::IDENTITY);
        assert_eq!(viewport.origin, LogicalPoint::new(1_100.0, 140.0));
    }

    #[test]
    fn a_drawn_region_is_preserved_relative_to_a_moved_window() {
        let display = fixture_display();
        let initial_bounds = LogicalRect::new(
            LogicalPoint::new(120.0, 80.0),
            LogicalSize::new(900.0, 700.0),
        );
        let region = LogicalRect::new(
            LogicalPoint::new(220.0, 180.0),
            LogicalSize::new(500.0, 320.0),
        );
        let target = resolved_native_scrolling_region_target(
            CaptureRequest {
                target: CaptureTarget::Display(display.id.clone()),
                cursor: scrozz_core::CursorMode::Hidden,
                include_window_shadow: false,
            },
            display.clone(),
            fixture_window("browser", "Browser", "display-2", initial_bounds, true),
            region,
        )
        .expect("selected region");

        let moved_bounds = LogicalRect::new(LogicalPoint::new(180.0, 110.0), initial_bounds.size);
        let refreshed = target
            .refresh_from_snapshots(
                vec![fixture_window(
                    "browser",
                    "Browser",
                    "display-2",
                    moved_bounds,
                    true,
                )],
                vec![display],
            )
            .expect("same moved window");
        let ScrollingContext::Native {
            viewport,
            crop,
            selection,
            ..
        } = refreshed.context
        else {
            panic!("selected region became a portal target");
        };
        assert_eq!(
            viewport,
            LogicalRect::new(
                LogicalPoint::new(280.0, 210.0),
                LogicalSize::new(500.0, 320.0)
            )
        );
        assert_eq!(crop.map(|crop| crop.viewport), Some(viewport));
        assert!(selection.is_some());
    }

    #[test]
    fn wayland_rejects_an_explicit_display_selector_before_the_picker_opens() {
        assert!(matches!(
            wayland_scrolling_capture_target(&DisplaySelector::Primary),
            Err(CliError::Core(CoreError::Unsupported { .. }))
        ));
        assert_eq!(
            wayland_scrolling_capture_target(&DisplaySelector::Active).expect("portal target"),
            wayland_portal_picker_target()
        );
    }
}
