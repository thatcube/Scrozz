//! The [`CaptureBackend`] implementation.

use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, CaptureTarget, Display, Error, Frame, LogicalRect,
    Provenance, Result, ScaleFactor, Size, SourceApp, TargetEnumerator, Window, WindowPicking,
    WindowPickingCapability,
};

use super::{
    dpi,
    enumerate::{self, MonitorRecord},
    gdi, geom, pixels, wgc,
};

/// Windows still capture.
pub struct WindowsBackend {
    /// `None` when WGC is unavailable and every capture falls back to GDI.
    wgc: Option<wgc::WgcDevice>,
}

struct WindowCapture {
    frame: Frame,
    source_app: SourceApp,
    window_shadow: bool,
}

impl WindowsBackend {
    /// Builds the backend, preferring WGC.
    ///
    /// Device creation happens once here rather than per capture, because it
    /// costs tens of milliseconds and a screenshot tool is judged on how fast
    /// it responds to the shortcut.
    ///
    /// # Errors
    ///
    /// Never fails: a machine with no WGC support still gets the GDI path, and
    /// reporting a hard error here would leave the user with no screenshot at
    /// all on exactly the old or unusual machines that most need a fallback.
    pub fn new() -> Result<Self> {
        dpi::ensure_process_dpi_aware();

        let wgc = if wgc::is_supported() {
            wgc::WgcDevice::new().ok()
        } else {
            None
        };

        Ok(Self { wgc })
    }

    fn capture_display(&self, monitor: &MonitorRecord, request: &CaptureRequest) -> Result<Frame> {
        if let Some(device) = &self.wgc {
            let item = wgc::item_for_monitor(monitor.handle)?;
            return wgc::capture_item(device, &item, request.cursor, monitor.scale);
        }
        gdi::capture_rect(monitor.bounds, monitor.scale)
    }

    fn capture_window(
        &self,
        id: &scrozz_core::WindowId,
        request: &CaptureRequest,
    ) -> Result<WindowCapture> {
        let (record, monitors) = enumerate::window_by_id(id)?;
        let scale = monitors
            .get(record.monitor)
            .map_or(ScaleFactor::IDENTITY, |m| m.scale);

        let frame = if let Some(device) = &self.wgc {
            let item = wgc::item_for_window(record.handle)?;
            // Once WGC is selected, its native-alpha guarantee is part of the
            // advertised picker capability. Propagate capture failures instead
            // of silently replacing those pixels with opaque PrintWindow output.
            wgc::capture_item(device, &item, request.cursor, scale)?
        } else {
            gdi::capture_window(record.handle, record.bounds, scale)?
        };

        Ok(WindowCapture {
            frame,
            source_app: record.source_app(),
            window_shadow: self
                .window_picking()
                .shadow
                .resolve(request.include_window_shadow),
        })
    }

    /// Captures a user-chosen rectangle.
    ///
    /// The rectangle arrives in the global logical desktop, so it has to be
    /// mapped onto whichever monitor holds most of it before it means anything
    /// in pixels — under mixed DPI the same logical rectangle is a different
    /// number of pixels on each screen.
    fn capture_region(&self, rect: LogicalRect, request: &CaptureRequest) -> Result<Frame> {
        let monitors = enumerate::monitors()?;
        let logical: Vec<LogicalRect> = monitors
            .iter()
            .map(|m| geom::logical_from_device(m.bounds, m.scale))
            .collect();

        let index = geom::dominant_monitor_logical(rect, &logical)
            .ok_or_else(|| Error::TargetGone("region does not overlap any display".into()))?;
        let monitor = &monitors[index];

        // Snap into the monitor's own pixel grid, then crop what the monitor
        // capture produced. Capturing the whole monitor and cropping costs one
        // extra copy but is the only way to get a region that spans the
        // taskbar, overlaps a fullscreen window, or sits under this app's own
        // selection overlay.
        let full = self.capture_display(monitor, request)?;
        let crop_rect = geom::region_within_monitor(rect, logical[index], monitor.scale);

        let (data, stride, width, height) = pixels::crop(
            &full.data,
            full.stride,
            full.width(),
            full.height(),
            crop_rect,
        );
        if width == 0 || height == 0 {
            return Err(Error::TargetGone(
                "region lies entirely outside the display it was mapped to".into(),
            ));
        }

        Ok(Frame {
            data,
            size: Size::new(f64::from(width), f64::from(height)),
            stride,
            format: full.format,
            color_space: full.color_space,
            scale: full.scale,
        })
    }

    /// Composites every display into one image.
    ///
    /// Under mixed DPI there is no single pixel grid the monitors share, so the
    /// canvas is built at the largest scale present and lower-DPI monitors are
    /// scaled up into it. That is a real resample and it is documented rather
    /// than hidden — the alternative, picking one monitor's scale and letting
    /// the sharper screen shrink, throws away pixels the user has.
    fn capture_all(&self, request: &CaptureRequest) -> Result<Frame> {
        let monitors = enumerate::monitors()?;
        let scales: Vec<ScaleFactor> = monitors.iter().map(|m| m.scale).collect();
        let scale = geom::max_scale(&scales);
        let logical: Vec<LogicalRect> = monitors
            .iter()
            .map(|m| geom::logical_from_device(m.bounds, m.scale))
            .collect();
        let canvas = geom::logical_desktop_bounds(&logical)
            .ok_or_else(|| Error::TargetGone("no displays are connected".into()))?;

        let width = (canvas.size.width * scale.get()).round() as u32;
        let height = (canvas.size.height * scale.get()).round() as u32;
        if width == 0 || height == 0 {
            return Err(Error::TargetGone("the virtual desktop has no area".into()));
        }

        let stride = pixels::min_stride(width);
        let mut data = vec![0u8; pixels::buffer_len(stride, height)];
        let origin = (canvas.origin.x, canvas.origin.y);
        let mut captured = 0usize;
        let mut last_error = None;

        for monitor in &monitors {
            let frame = match self.capture_display(monitor, request) {
                Ok(frame) => frame,
                Err(error) => {
                    // One unplugged monitor should not lose the other three,
                    // but returning a transparent success when every capture
                    // failed would be a lie.
                    last_error = Some(error);
                    continue;
                }
            };
            captured += 1;
            let placement =
                geom::placement_in_composite(monitor.bounds, monitor.scale, origin, scale);
            pixels::blit_nearest(
                &mut pixels::Plane {
                    data: &mut data,
                    stride,
                    width,
                    height,
                },
                placement,
                &pixels::PlaneRef {
                    data: &frame.data,
                    stride: frame.stride,
                    width: frame.width(),
                    height: frame.height(),
                },
            );
        }

        if captured == 0 {
            return Err(last_error.unwrap_or_else(|| {
                Error::Platform("all enumerated displays failed to capture".to_owned())
            }));
        }

        Ok(Frame {
            data,
            size: Size::new(f64::from(width), f64::from(height)),
            stride,
            // Matches what `capture_display` produces on the WGC path. The
            // uncovered gaps in a non-rectangular monitor arrangement stay
            // zeroed, which is transparent black under this format and so
            // reads correctly rather than as opaque black.
            format: scrozz_core::PixelFormat::BgraPremultiplied8,
            color_space: scrozz_core::ColorSpace::Srgb,
            scale,
        })
    }
}

impl TargetEnumerator for WindowsBackend {
    fn displays(&self) -> Result<Vec<Display>> {
        Ok(enumerate::monitors()?
            .iter()
            .map(MonitorRecord::to_display)
            .collect())
    }

    fn windows(&self) -> Result<Vec<Window>> {
        let monitors = enumerate::monitors()?;
        Ok(enumerate::windows()?
            .iter()
            .map(|record| enumerate::to_window(record, &monitors))
            .collect())
    }

    fn active_display(&self) -> Result<Display> {
        Ok(enumerate::monitor_under_cursor()?.to_display())
    }
}

impl WindowPicking for WindowsBackend {
    fn window_picking(&self) -> WindowPickingCapability {
        pixels::window_picking_capability(self.wgc.is_some())
    }
}

fn finish_capture(
    frame: Frame,
    provenance: Provenance,
    target: CaptureTarget,
    window: Option<(SourceApp, bool)>,
) -> Capture {
    let capture = Capture::new(frame, provenance, target);
    match window {
        Some((source, shadow)) => capture.with_source_app(source).with_window_shadow(shadow),
        None => capture,
    }
}

impl CaptureBackend for WindowsBackend {
    fn capture(&self, request: &CaptureRequest) -> Result<Capture> {
        let (frame, window) = match &request.target {
            CaptureTarget::Display(id) => {
                let monitor = enumerate::monitor_by_id(id)?;
                (self.capture_display(&monitor, request)?, None)
            }
            CaptureTarget::Window(id) => {
                let captured = self.capture_window(id, request)?;
                (
                    captured.frame,
                    Some((captured.source_app, captured.window_shadow)),
                )
            }
            CaptureTarget::Region(rect) => (self.capture_region(*rect, request)?, None),
            CaptureTarget::AllDisplays => (self.capture_all(request)?, None),
        };

        // Decision D9: a window capture is returned exactly as the compositor
        // gave it — no padding to a round size, no synthesised shadow, no
        // corner rounding. `Provenance::Window` is what tells everything
        // downstream to leave it alone.
        let provenance = match &request.target {
            CaptureTarget::Display(_) => Provenance::Display,
            CaptureTarget::Window(_) => Provenance::Window,
            CaptureTarget::Region(_) => Provenance::Region,
            CaptureTarget::AllDisplays => Provenance::AllDisplays,
        };

        Ok(finish_capture(
            frame,
            provenance,
            request.target.clone(),
            window,
        ))
    }

    fn name(&self) -> &str {
        if self.wgc.is_some() {
            "Windows.Graphics.Capture"
        } else {
            "GDI BitBlt"
        }
    }
}

/// Constructs the Windows backend.
///
/// # Errors
///
/// Returns [`Error::Platform`] only if the process cannot talk to its window
/// station at all.
pub fn backend() -> Result<Box<dyn CaptureBackend>> {
    Ok(Box::new(WindowsBackend::new()?))
}

#[cfg(test)]
mod tests {
    use scrozz_core::{
        ColorSpace, PixelFormat, ShadowSupport, SourceApp, WindowId, WindowPicking, WindowSelection,
    };

    use super::*;

    fn sample_frame() -> Frame {
        Frame {
            data: vec![1, 2, 3, 4, 5, 6, 7, 8],
            size: Size::new(2.0, 1.0),
            stride: 8,
            format: PixelFormat::BgraPremultiplied8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::IDENTITY,
        }
    }

    #[test]
    fn picker_reports_dynamic_alpha_and_fixed_shadow_capabilities() {
        let gdi = WindowsBackend { wgc: None }.window_picking();
        let wgc = pixels::window_picking_capability(true);

        assert_eq!(gdi.selection, WindowSelection::InProcess);
        assert!(!gdi.native_alpha);
        assert!(wgc.native_alpha);
        assert!(matches!(gdi.shadow, ShadowSupport::AlwaysExcluded { .. }));
        assert!(matches!(wgc.shadow, ShadowSupport::AlwaysExcluded { .. }));
        assert!(!gdi.shadow.resolve(true));
        assert!(!gdi.shadow.resolve(false));
        assert!(!wgc.shadow.resolve(true));
    }

    #[test]
    fn window_completion_preserves_pixels_and_records_source() {
        let frame = sample_frame();
        let original = frame.data.clone();
        let source = SourceApp {
            name: Some("Browser".into()),
            identifier: Some("browser.exe".into()),
            window_title: Some("Document".into()),
        };
        let capture = finish_capture(
            frame,
            Provenance::Window,
            CaptureTarget::Window(WindowId("42".into())),
            Some((source.clone(), false)),
        );

        assert_eq!(capture.frame.data, original);
        assert_eq!(capture.frame.stride, 8);
        assert_eq!(capture.frame.format, PixelFormat::BgraPremultiplied8);
        assert_eq!(capture.source_app, source);
        assert_eq!(capture.window_shadow, Some(false));
        assert!(capture.provenance.forbids_compositing());
    }

    #[test]
    fn non_window_completion_has_no_source_or_shadow_question() {
        let capture = finish_capture(
            sample_frame(),
            Provenance::AllDisplays,
            CaptureTarget::AllDisplays,
            None,
        );
        assert!(!capture.source_app.is_known());
        assert_eq!(capture.window_shadow, None);
    }
}
