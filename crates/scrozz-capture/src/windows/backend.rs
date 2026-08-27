//! The [`CaptureBackend`] implementation.

use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, CaptureTarget, Display, Error, Frame, LogicalRect,
    Provenance, Result, ScaleFactor, Size, TargetEnumerator, Window,
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
            match wgc::capture_item(device, &item, request.cursor, monitor.scale) {
                Ok(frame) => return Ok(frame),
                Err(e @ Error::TargetGone(_)) => return Err(e),
                Err(_) => {}
            }
        }
        gdi::capture_rect(monitor.bounds, monitor.scale)
    }

    fn capture_window(&self, id: &scrozz_core::WindowId, request: &CaptureRequest) -> Result<Frame> {
        let (record, monitors) = enumerate::window_by_id(id)?;
        let scale = monitors
            .get(record.monitor)
            .map_or(ScaleFactor::IDENTITY, |m| m.scale);

        if let Some(device) = &self.wgc {
            let item = wgc::item_for_window(record.handle)?;
            match wgc::capture_item(device, &item, request.cursor, scale) {
                Ok(frame) => return Ok(frame),
                Err(e @ Error::TargetGone(_)) => return Err(e),
                Err(_) => {}
            }
        }
        gdi::capture_window(record.handle, record.bounds, scale)
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
            return Err(Error::TargetGone(
                "the virtual desktop has no area".into(),
            ));
        }

        let stride = pixels::min_stride(width);
        let mut data = vec![0u8; pixels::buffer_len(stride, height)];
        let origin = (canvas.origin.x, canvas.origin.y);

        for monitor in &monitors {
            let Ok(frame) = self.capture_display(monitor, request) else {
                // One unplugged monitor should not lose the other three.
                continue;
            };
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

        Ok(Frame {
            data,
            size: Size::new(f64::from(width), f64::from(height)),
            stride,
            format: scrozz_core::PixelFormat::Bgra8,
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

impl CaptureBackend for WindowsBackend {
    fn capture(&self, request: &CaptureRequest) -> Result<Capture> {
        let frame = match &request.target {
            CaptureTarget::Display(id) => {
                let monitor = enumerate::monitor_by_id(id)?;
                self.capture_display(&monitor, request)?
            }
            CaptureTarget::Window(id) => self.capture_window(id, request)?,
            CaptureTarget::Region(rect) => self.capture_region(*rect, request)?,
            CaptureTarget::AllDisplays => self.capture_all(request)?,
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

        Ok(Capture {
            frame,
            provenance,
            target: request.target.clone(),
        })
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
