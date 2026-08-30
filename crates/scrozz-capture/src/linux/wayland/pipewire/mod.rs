//! Reading pixels out of the PipeWire node a screen-cast portal hands over.
//!
//! # The gap this closes
//!
//! `org.freedesktop.portal.ScreenCast` does not return an image. It returns a
//! *node id* on a PipeWire graph, and a file descriptor for the socket that
//! graph lives on. Everything the user consented to — which monitor, whether the
//! cursor is drawn, how long the permission lasts — is settled by the time that
//! id exists, and none of it produces a single byte of pixel data. Turning the
//! node into a frame is entirely this module's problem, and until it existed
//! Wayland capture stopped at a `todo!()`.
//!
//! # Layout
//!
//! | Module | Unsafe | Tested off Linux |
//! | --- | --- | --- |
//! | [`pod`] | no | yes — byte-exact encoder assertions |
//! | [`format`] | no | yes — negotiation, parsing, stride packing |
//! | [`lifecycle`] | no | yes — every transition and failure |
//! | [`sys`] | declarations only | no — it *is* the ABI |
//! | [`stream`] | yes | no — needs a real server |
//!
//! The split is deliberate. Nearly all of the logic that can be wrong in an
//! interesting way — how a format offer is encoded, which pixel format an
//! `spa_video_format` really means, what to do when a buffer arrives empty — is
//! arithmetic over bytes, and arithmetic over bytes should be tested on the
//! laptop the code is written on, not only on a machine with a compositor.
//! `tests/linux.rs` compiles the first three modules on every host for exactly
//! that reason.
//!
//! What genuinely cannot be tested that way is the FFI and the event loop, and
//! `tools/wayland-smoke.sh` exists to be honest about it: it runs on a real
//! Wayland session or it says clearly that it did not run.
//!
//! # System requirements
//!
//! At run time: `libpipewire-0.3.so.0` (Ubuntu 24.04+
//! `libpipewire-0.3-0t64`, older Debian/Ubuntu `libpipewire-0.3-0`,
//! Fedora/Arch `pipewire`), the `pipewire` user service, and a desktop portal
//! implementing `ScreenCast` — `xdg-desktop-portal-gnome`, `-kde` or `-wlr`
//! depending on the compositor. At build time: nothing. The library is opened
//! with `dlopen` at the moment it is needed, so a machine without it still runs
//! Scrozz and still captures over X11, and the Wayland path reports
//! [`Error::Unsupported`] naming the package to install.

pub mod format;
pub mod lifecycle;
pub mod pod;
pub mod stream;
pub mod sys;

use std::os::fd::OwnedFd;
use std::time::Duration;

use scrozz_core::{Error, Frame, PhysicalSize, Result, ScaleFactor};

use crate::CaptureCancellation;

/// How long to wait for a compositor to produce the first usable frame.
///
/// Generous on purpose. The portal dialog has already been dismissed by this
/// point, so the user is watching a screenshot that appears to have hung; but
/// GNOME's screen-cast source can take a second or more to spin up on a busy
/// machine, and a too-eager timeout turns a slow capture into a failed one.
pub const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// A successfully loaded and ABI-validated PipeWire runtime.
///
/// Construct this before opening a portal picker. It is intentionally opaque so
/// stream construction cannot accidentally reload or bypass the validated
/// mapping after the user has granted access.
#[derive(Clone, Copy, Debug)]
pub struct Runtime {
    library: &'static sys::Library,
}

/// Loads and validates every PipeWire symbol and ABI assumption needed later.
///
/// # Errors
///
/// Returns an actionable unsupported error before portal permission when the
/// runtime is missing, too old, or incompatible with the handwritten ABI.
pub fn preflight() -> Result<Runtime> {
    Ok(Runtime {
        library: sys::Library::open()?,
    })
}

/// A portal-provided PipeWire stream kept open across frame requests.
#[derive(Debug)]
pub struct FrameStream {
    stream: stream::FrameStream,
    scale: ScaleFactor,
}

impl FrameStream {
    /// Connects once to a portal-provided PipeWire node.
    ///
    /// `fd` is consumed and closed when the stream is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] if PipeWire is unavailable and
    /// [`Error::Platform`] if its loop, context, remote, or stream cannot be
    /// created.
    pub fn connect(
        runtime: Runtime,
        fd: OwnedFd,
        node_id: u32,
        pipewire_serial: Option<u64>,
        scale: ScaleFactor,
    ) -> Result<Self> {
        let stream = stream::FrameStream::connect(
            runtime.library,
            fd,
            node_id,
            pipewire_serial,
            FRAME_TIMEOUT,
        )?;
        Ok(Self { stream, scale })
    }

    /// Waits for the next usable frame without reconnecting the stream.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TargetGone`] when the selected source disappears and
    /// [`Error::Platform`] for stream, format, buffer, and timeout failures.
    pub fn capture_frame(&mut self) -> Result<Frame> {
        raw_to_frame(self.stream.capture_frame()?, self.scale)
    }

    /// Waits for the next usable frame while observing acquisition cancellation.
    pub fn capture_frame_with_cancellation(
        &mut self,
        cancellation: Option<&CaptureCancellation>,
    ) -> Result<Frame> {
        raw_to_frame(
            self.stream.capture_frame_with_cancellation(cancellation)?,
            self.scale,
        )
    }
}

/// Captures one frame from a portal-provided PipeWire node.
///
/// `fd` is consumed — PipeWire closes it when the connection is torn down.
///
/// `scale` is what the caller knows about the display's scale factor; PipeWire
/// reports pixels only, so the logical-to-physical relationship has to come from
/// the geometry backend rather than from the stream.
///
/// # Errors
///
/// [`Error::Unsupported`] if PipeWire is not installed, [`Error::TargetGone`] if
/// the node disappears, and [`Error::Platform`] for compositor and stream
/// failures — each with enough text for the user to know what to do next.
pub fn acquire_frame(
    runtime: Runtime,
    fd: OwnedFd,
    node_id: u32,
    pipewire_serial: Option<u64>,
    scale: ScaleFactor,
) -> Result<Frame> {
    FrameStream::connect(runtime, fd, node_id, pipewire_serial, scale)?.capture_frame()
}

fn raw_to_frame(raw: stream::RawFrame, scale: ScaleFactor) -> Result<Frame> {
    let format = raw.format;
    let frame = Frame {
        data: raw.pixels,
        size: PhysicalSize::new(f64::from(format.width), f64::from(format.height)),
        stride: format.packed_stride(),
        format: format.pixel_format,
        color_space: format.color_space,
        scale,
    };

    if !frame.is_well_formed() {
        return Err(Error::Platform(format!(
            "the compositor delivered {} bytes for a {}x{} frame, which is short of the {} it \
             declared",
            frame.data.len(),
            format.width,
            format.height,
            format.packed_len()
        )));
    }

    Ok(frame)
}
