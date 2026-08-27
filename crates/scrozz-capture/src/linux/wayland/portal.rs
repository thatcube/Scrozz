//! `xdg-desktop-portal` negotiation and still-image acquisition.
//!
//! # The two portal interfaces, and why both matter
//!
//! `org.freedesktop.portal.Screenshot` takes a picture and hands back a `file://`
//! URI. Version 3 can constrain its trusted picker to windows, so it is the
//! shortest correct path for a single still image: no PipeWire stream, no
//! recording loop, and no opportunity to read another client's pixels without
//! the user's explicit choice.
//!
//! `org.freedesktop.portal.ScreenCast` gives a PipeWire node the application
//! reads frames from, with the selection made once and then restored silently on
//! later captures. It is the only route to Scrozz's own selection UI, and it is
//! the route this module is built around.
//!
//! # Wire constants
//!
//! Transcribed from the portal specification and cross-checked against
//! `ashpd`'s own definitions.

#[cfg(target_os = "linux")]
use ashpd::{
    PortalError,
    desktop::{
        ResponseError,
        screenshot::{AvailableTargets, Screenshot, ScreenshotProxy},
    },
};
#[cfg(target_os = "linux")]
use image::ImageReader;

use scrozz_core::{
    Capture, CaptureRequest, CaptureTarget, Frame, Provenance, ShadowSupport,
    WindowPickingCapability, WindowSelection,
};

use super::restore::TokenKey;

/// `SourceType` bit flags from the ScreenCast specification.
pub mod source_type {
    /// A whole monitor.
    pub const MONITOR: u32 = 1;
    /// A single window, chosen in the portal's own picker.
    pub const WINDOW: u32 = 2;
    /// A virtual, compositor-synthesised source.
    pub const VIRTUAL: u32 = 4;
}

/// `CursorMode` bit flags from the ScreenCast specification.
pub mod cursor_mode {
    /// The pointer is absent from the stream.
    pub const HIDDEN: u32 = 1;
    /// The pointer is drawn into the stream's buffers.
    pub const EMBEDDED: u32 = 2;
    /// The pointer is delivered as stream metadata for the client to draw.
    pub const METADATA: u32 = 4;
}

/// `PersistMode` values from the ScreenCast specification.
pub mod persist_mode {
    /// No token is issued; the user is asked every time.
    pub const DO_NOT: u32 = 0;
    /// The grant lasts as long as the process does.
    pub const APPLICATION: u32 = 1;
    /// The grant lasts until the user revokes it in system settings.
    pub const EXPLICITLY_REVOKED: u32 = 2;
}

/// Screenshot portal interface version which introduced `target`.
#[cfg(target_os = "linux")]
const SCREENSHOT_WINDOW_TARGET_VERSION: u32 = 3;

/// A conservative capability used until portal support has been probed.
#[must_use]
pub fn unchecked_window_picking_capability(why: impl Into<String>) -> WindowPickingCapability {
    WindowPickingCapability {
        selection: WindowSelection::Unavailable { why: why.into() },
        shadow: ShadowSupport::Unchecked {
            why: "the Screenshot portal does not report whether the compositor included a \
                  window shadow"
                .to_owned(),
        },
        // The portal may return alpha, but the interface does not promise that
        // compositors preserve a window's transparent corners.
        native_alpha: false,
    }
}

/// Probes whether the installed Screenshot portal can constrain its picker to windows.
///
/// A Screenshot v1/v2 implementation ignores the `target` option. Sending the
/// request anyway would present an unconstrained picker while Scrozz claimed it
/// was choosing a window, so support is checked before it is advertised.
#[cfg(target_os = "linux")]
pub fn window_picking_capability() -> scrozz_core::Result<WindowPickingCapability> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| scrozz_core::Error::Platform(format!("portal runtime: {err}")))?;
    let selection = runtime.block_on(probe_window_selection())?;
    Ok(capability_for_selection(selection))
}

#[cfg(target_os = "linux")]
async fn probe_window_selection() -> scrozz_core::Result<WindowSelection> {
    let proxy = ScreenshotProxy::new().await.map_err(map_portal_error)?;
    let version = proxy.version();
    if version < SCREENSHOT_WINDOW_TARGET_VERSION {
        return Ok(WindowSelection::Unavailable {
            why: format!(
                "the installed Screenshot portal implements interface version {version}; \
                 choosing only windows requires version {SCREENSHOT_WINDOW_TARGET_VERSION}"
            ),
        });
    }

    let targets = proxy.available_targets().await.map_err(map_portal_error)?;
    if !targets.contains(AvailableTargets::Window) {
        return Ok(WindowSelection::Unavailable {
            why: "the installed Screenshot portal does not advertise window targets".to_owned(),
        });
    }

    Ok(WindowSelection::PortalPicker {
        portal: "org.freedesktop.portal.Screenshot".to_owned(),
    })
}

fn capability_for_selection(selection: WindowSelection) -> WindowPickingCapability {
    WindowPickingCapability {
        selection,
        shadow: ShadowSupport::Unchecked {
            why: "the Screenshot portal does not report whether the compositor included a \
                  window shadow"
                .to_owned(),
        },
        native_alpha: false,
    }
}

/// The options for one `SelectSources` call.
///
/// Produced entirely from the request, with no D-Bus involved, so the decisions
/// — which source types to offer, whether to embed the pointer, which stored
/// token to present — are testable on any machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPlan {
    /// Bit mask of `SourceType` values to offer in the picker.
    pub types: u32,
    /// The `CursorMode` to request.
    pub cursor: u32,
    /// Whether the user may pick more than one source.
    pub multiple: bool,
    /// The `PersistMode` to request.
    pub persist: u32,
    /// Which stored token, if any, to present for silent restore.
    pub restore_key: TokenKey,
}

impl SessionPlan {
    /// Builds the plan for a capture request.
    ///
    /// Two choices here are deliberate:
    ///
    /// - The pointer is requested **embedded** rather than as metadata when it
    ///   is wanted. Metadata is better for a recorder, which can draw a crisp
    ///   pointer at any scale, but a still capture would have to composite it
    ///   itself and would get the hotspot subtly wrong.
    /// - Persistence is always `EXPLICITLY_REVOKED`. `APPLICATION` sounds safer
    ///   but expires when the process does, so every launch costs a permission
    ///   dialog — which is the failure this whole mechanism exists to prevent.
    #[must_use]
    pub fn for_target(target: &CaptureTarget, want_cursor: bool) -> Self {
        let (types, restore_key) = match target {
            CaptureTarget::Window(_) => (source_type::WINDOW, TokenKey::Window),
            CaptureTarget::AllDisplays => (
                source_type::MONITOR | source_type::VIRTUAL,
                TokenKey::AllDisplays,
            ),
            // A region is cropped from a monitor capture after the fact.
            // The portal has no concept of a sub-rectangle, and asking the user
            // to pick a region in the portal's UI and then again in Scrozz's
            // would be absurd.
            CaptureTarget::Display(_) | CaptureTarget::Region(_) => {
                (source_type::MONITOR, TokenKey::Monitor)
            }
        };

        Self {
            types,
            cursor: if want_cursor {
                cursor_mode::EMBEDDED
            } else {
                cursor_mode::HIDDEN
            },
            multiple: matches!(target, CaptureTarget::AllDisplays),
            persist: persist_mode::EXPLICITLY_REVOKED,
            restore_key,
        }
    }

    /// Narrows the plan to what the portal says it can actually do.
    ///
    /// Decision D8 requires capability by query rather than assumption, and the
    /// ScreenCast interface publishes `AvailableSourceTypes` and
    /// `AvailableCursorModes` for exactly this. Requesting an unavailable bit is
    /// not ignored — the portal rejects the call — so a compositor that cannot
    /// offer window sources must be met with a narrowed request and, if nothing
    /// remains, a truthful refusal.
    ///
    /// Returns `None` when the intersection is empty, which the caller turns
    /// into [`scrozz_core::Error::Unsupported`].
    #[must_use]
    pub fn narrow(mut self, available_types: u32, available_cursors: u32) -> Option<Self> {
        self.types &= available_types;
        if self.types == 0 {
            return None;
        }
        if self.cursor & available_cursors == 0 {
            // Every portal implements Hidden, so falling back to it keeps the
            // capture working and loses only the pointer.
            self.cursor = cursor_mode::HIDDEN;
        }
        Some(self)
    }
}

/// Turns a portal `file://` URI into a path.
///
/// The Screenshot interface answers with a URI, not a path, and the difference
/// bites: a screenshot saved while the user's locale produced a filename with a
/// space arrives as `%20`, and opening the literal `%20` path fails with "no
/// such file" on a file that plainly exists.
///
/// Rejects anything that is not `file://`; the portal may legitimately hand back
/// a `file:` URI pointing into its own document store, but a `http://` here
/// would mean something has gone very wrong.
#[must_use]
pub fn path_from_uri(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file://host/path` is legal; an empty or `localhost` authority is the
    // only form a portal produces, and both leave the path starting at the
    // first slash.
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(index) if &rest[..index] == "localhost" => &rest[index..],
        _ => return None,
    };

    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }

    Some(std::path::PathBuf::from(String::from_utf8(out).ok()?))
}

/// One video stream as `Start` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    /// The PipeWire node to connect to.
    pub node_id: u32,
    /// Position in the compositor's global space, if the portal supplied one.
    ///
    /// Optional in the specification and genuinely absent on some compositors
    /// for window streams, where the window's desktop position is not something
    /// the client is allowed to know.
    pub position: Option<(i32, i32)>,
    /// Size in pixels, if supplied.
    pub size: Option<(i32, i32)>,
}

impl StreamInfo {
    /// Whether the stream carries enough geometry to place it on the desktop.
    #[must_use]
    pub const fn is_placeable(&self) -> bool {
        self.position.is_some() && self.size.is_some()
    }

    /// Attaches only metadata that the ScreenCast portal actually disclosed.
    ///
    /// The standard stream response has no owning-application identity. In
    /// particular, the advisory [`scrozz_core::WindowId`] used to enter the portal
    /// flow is not evidence about which window the user selected.
    #[must_use]
    pub fn capture_from_frame(frame: Frame, request: &CaptureRequest) -> Capture {
        let provenance = match request.target {
            CaptureTarget::Display(_) => Provenance::Display,
            CaptureTarget::Window(_) => Provenance::Window,
            CaptureTarget::Region(_) => Provenance::Region,
            CaptureTarget::AllDisplays => Provenance::AllDisplays,
        };
        Capture::new(frame, provenance, request.target.clone())
    }
}

/// Opens the desktop's trusted window picker and decodes the selected still.
///
/// The advisory [`scrozz_core::WindowId`] in `request` is intentionally ignored:
/// Wayland does not disclose a window list, and only the portal knows which
/// surface the user selected.
///
/// # Errors
///
/// Returns [`scrozz_core::Error::Cancelled`] when the chooser is dismissed,
/// [`scrozz_core::Error::Unsupported`] when no Screenshot portal is installed
/// or its trusted picker cannot be restricted to windows, or a platform/codec
/// error when the portal response cannot be read.
#[cfg(target_os = "linux")]
pub fn capture_window(request: &CaptureRequest) -> scrozz_core::Result<Capture> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| scrozz_core::Error::Platform(format!("portal runtime: {err}")))?;

    let screenshot = runtime.block_on(async {
        if let WindowSelection::Unavailable { why } = probe_window_selection().await? {
            return Err(scrozz_core::Error::Unsupported {
                what: "capturing a chosen window on Wayland".to_owned(),
                why,
            });
        }

        Screenshot::request()
            .interactive(true)
            .modal(false)
            .target(AvailableTargets::Window)
            .send()
            .await
            .map_err(map_portal_error)?
            .response()
            .map_err(map_portal_error)
    })?;

    let path = path_from_uri(screenshot.uri().as_str()).ok_or_else(|| {
        scrozz_core::Error::Platform(format!(
            "Screenshot portal returned a non-file URI: {}",
            screenshot.uri().as_str()
        ))
    })?;
    let reader = ImageReader::open(&path)
        .map_err(scrozz_core::Error::Io)?
        .with_guessed_format()
        .map_err(|err| scrozz_core::Error::Codec(format!("portal image header: {err}")))?;
    let image = reader
        .decode()
        .map_err(|err| scrozz_core::Error::Codec(format!("portal image decode: {err}")))?
        .to_rgba8();
    let (width, height) = image.dimensions();
    let frame = Frame {
        data: image.into_raw(),
        size: scrozz_core::PhysicalSize::new(f64::from(width), f64::from(height)),
        stride: width as usize * 4,
        format: scrozz_core::PixelFormat::Rgba8,
        color_space: scrozz_core::ColorSpace::Unknown,
        scale: scrozz_core::ScaleFactor::IDENTITY,
    };

    Ok(StreamInfo::capture_from_frame(frame, request))
}

#[cfg(target_os = "linux")]
fn map_portal_error(err: ashpd::Error) -> scrozz_core::Error {
    match err {
        ashpd::Error::Response(ResponseError::Cancelled)
        | ashpd::Error::Portal(PortalError::Cancelled(_)) => scrozz_core::Error::Cancelled,
        ashpd::Error::PortalNotFound(interface) => scrozz_core::Error::Unsupported {
            what: "capturing windows on Wayland".to_owned(),
            why: format!("no desktop portal implements {interface}"),
        },
        other => scrozz_core::Error::Platform(format!("Screenshot portal: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SessionPlan, StreamInfo, capability_for_selection, source_type,
        unchecked_window_picking_capability,
    };
    use scrozz_core::{
        CaptureRequest, CaptureTarget, ColorSpace, Frame, PhysicalSize, PixelFormat, Provenance,
        ScaleFactor, ShadowSupport, WindowId, WindowSelection,
    };

    #[test]
    fn portal_picker_names_the_real_interface_and_is_out_of_process() {
        let capability = capability_for_selection(WindowSelection::PortalPicker {
            portal: "org.freedesktop.portal.Screenshot".to_owned(),
        });
        assert_eq!(
            capability.selection,
            WindowSelection::PortalPicker {
                portal: "org.freedesktop.portal.Screenshot".to_owned()
            }
        );
        assert!(matches!(capability.shadow, ShadowSupport::Unchecked { .. }));
        assert!(!capability.native_alpha);
    }

    #[test]
    fn unprobed_portal_support_is_not_advertised() {
        let capability = unchecked_window_picking_capability("probe failed");
        assert_eq!(
            capability.selection,
            WindowSelection::Unavailable {
                why: "probe failed".to_owned()
            }
        );
        assert!(matches!(capability.shadow, ShadowSupport::Unchecked { .. }));
    }

    #[test]
    fn interactive_window_flow_selects_only_a_window_source() {
        let plan = SessionPlan::for_target(
            &CaptureTarget::Window(WindowId("portal-picker".into())),
            false,
        );
        assert_eq!(plan.types, source_type::WINDOW);
        assert!(!plan.multiple);
    }

    #[test]
    fn portal_window_metadata_does_not_fabricate_an_owner_or_double_composite() {
        let frame = Frame {
            data: vec![1, 2, 3, 0xff],
            size: PhysicalSize::new(1.0, 1.0),
            stride: 4,
            format: PixelFormat::Bgra8,
            color_space: ColorSpace::Unknown,
            scale: ScaleFactor::IDENTITY,
        };
        let request = CaptureRequest::new(CaptureTarget::Window(WindowId(
            "advisory-id-not-selected-owner".into(),
        )));

        let capture = StreamInfo::capture_from_frame(frame, &request);

        assert!(!capture.source_app.is_known());
        assert_eq!(
            capture.window_shadow, None,
            "the portal never disclosed whether a compositor shadow is present"
        );
        assert_eq!(capture.provenance, Provenance::Window);
        assert!(capture.provenance.forbids_compositing());
        assert_eq!(capture.frame.stride, 4);
        assert_eq!(capture.frame.data, [1, 2, 3, 0xff]);
    }
}
