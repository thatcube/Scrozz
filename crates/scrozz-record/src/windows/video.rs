//! Free-threaded WGC frame delivery into a bounded GPU-texture queue.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{Sender, SyncSender, TrySendError},
};

use scrozz_core::{Error, Result};
use windows::{
    Foundation::TypedEventHandler,
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::DirectXPixelFormat,
    },
    Win32::Graphics::Direct3D11::ID3D11Texture2D,
    core::IInspectable,
};

use super::{device::Device, plan::EncoderPlan, target::Source};

/// A GPU-resident frame and its QPC-based WGC timestamp.
pub struct FramePacket {
    /// Texture whose dimensions match the sink writer's input media type.
    pub texture: ID3D11Texture2D,
    /// Absolute system-relative timestamp in 100 ns units.
    pub raw_hns: i64,
}

// ID3D11Texture2D is a multithreaded COM interface. Its originating device has
// ID3D11Multithread protection enabled before any packet can be created.
unsafe impl Send for FramePacket {}

/// Non-frame events must never be discarded by video backpressure.
#[derive(Debug)]
pub enum Signal {
    /// The capture item was closed or disconnected.
    TargetClosed,
    /// A WGC or D3D operation failed.
    Failed(String),
}

/// Owns event registrations and capture resources until orderly teardown.
pub struct Capture {
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_token: i64,
    closed_token: i64,
    _frame_handler: TypedEventHandler<Direct3D11CaptureFramePool, IInspectable>,
    _closed_handler: TypedEventHandler<GraphicsCaptureItem, IInspectable>,
}

impl Capture {
    /// Registers generated WinRT handlers and starts compositor capture.
    pub fn start(
        device: &Device,
        source: Source,
        plan: EncoderPlan,
        show_cursor: bool,
        paused: Arc<AtomicBool>,
        frames: SyncSender<FramePacket>,
        signals: Sender<Signal>,
    ) -> Result<Self> {
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            device.winrt(),
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            3,
            source.pool_size,
        )
        .map_err(|error| Error::Platform(format!("CreateFreeThreaded failed: {error}")))?;
        let session = pool
            .CreateCaptureSession(&source.item)
            .map_err(|error| Error::Platform(format!("CreateCaptureSession failed: {error}")))?;
        // Cursor control arrived in Windows 10 2004. Earlier WGC builds can
        // include the default cursor, but cannot honour explicit exclusion.
        if let Err(error) = session.SetIsCursorCaptureEnabled(show_cursor)
            && !show_cursor
        {
            return Err(Error::Unsupported {
                what: "recording without the cursor".into(),
                why: format!("cursor exclusion requires Windows 10 version 2004 or newer: {error}"),
            });
        }
        // The yellow capture border can only be disabled on Windows 11.
        let _ = session.SetIsBorderRequired(false);

        let callback_device = device.clone();
        let crop = source.crop;
        let resize_with_content = source.resize_with_content;
        let pool_size = Arc::new(Mutex::new(source.pool_size));
        let callback_size = Arc::clone(&pool_size);
        let callback_signals = signals.clone();
        let frame_handler =
            TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |sender, _| {
                let pool = match sender.ok() {
                    Ok(pool) => pool,
                    Err(error) => {
                        let _ = callback_signals.send(Signal::Failed(format!(
                            "FrameArrived had no sender: {error}"
                        )));
                        return Ok(());
                    }
                };
                // Free-threaded events may overlap. Serialising acquisition
                // guarantees Recreate never races another outstanding frame.
                let mut known = match callback_size.lock() {
                    Ok(known) => known,
                    Err(_) => {
                        let _ = callback_signals
                            .send(Signal::Failed("WGC size lock was poisoned".into()));
                        return Ok(());
                    }
                };
                let frame = match pool.TryGetNextFrame() {
                    Ok(frame) => frame,
                    Err(error) => {
                        let _ = callback_signals
                            .send(Signal::Failed(format!("TryGetNextFrame failed: {error}")));
                        return Ok(());
                    }
                };

                let result = (|| -> windows::core::Result<()> {
                    let content = frame.ContentSize()?;
                    if content.Width <= 0 || content.Height <= 0 {
                        let _ = callback_signals.send(Signal::TargetClosed);
                        return Ok(());
                    }

                    if !paused.load(Ordering::Acquire) {
                        let timestamp = frame.SystemRelativeTime()?.Duration;
                        let surface = frame.Surface()?;
                        let frame_crop = if resize_with_content {
                            super::device::Crop {
                                left: 0,
                                top: 0,
                                width: content.Width as u32,
                                height: content.Height as u32,
                            }
                        } else {
                            crop
                        };
                        let copied = callback_device.copy_frame(
                            &surface,
                            content.Width as u32,
                            content.Height as u32,
                            frame_crop,
                            plan,
                        );
                        drop(surface);
                        match copied {
                            Ok(texture) => {
                                match frames.try_send(FramePacket {
                                    texture,
                                    raw_hns: timestamp,
                                }) {
                                    Ok(()) | Err(TrySendError::Full(_)) => {}
                                    Err(TrySendError::Disconnected(_)) => return Ok(()),
                                }
                            }
                            Err(error) => {
                                let _ = callback_signals.send(Signal::Failed(error.to_string()));
                            }
                        }
                    }

                    // Recreate requires every frame and surface from the old pool
                    // to be released first.
                    frame.Close()?;
                    if *known != content {
                        pool.Recreate(
                            callback_device.winrt(),
                            DirectXPixelFormat::B8G8R8A8UIntNormalized,
                            3,
                            content,
                        )?;
                        *known = content;
                    }
                    Ok(())
                })();

                let _ = frame.Close();
                if let Err(error) = result {
                    let _ = callback_signals.send(Signal::Failed(format!(
                        "WGC frame callback failed: {error}"
                    )));
                }
                Ok(())
            });

        let closed_signals = signals;
        let closed_handler =
            TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                let _ = closed_signals.send(Signal::TargetClosed);
                Ok(())
            });

        let frame_token = pool.FrameArrived(&frame_handler).map_err(|error| {
            Error::Platform(format!("FrameArrived registration failed: {error}"))
        })?;
        let closed_token = source
            .item
            .Closed(&closed_handler)
            .map_err(|error| Error::Platform(format!("Closed registration failed: {error}")))?;

        session
            .StartCapture()
            .map_err(|error| Error::Platform(format!("StartCapture failed: {error}")))?;

        Ok(Self {
            item: source.item,
            pool,
            session,
            frame_token,
            closed_token,
            _frame_handler: frame_handler,
            _closed_handler: closed_handler,
        })
    }

    /// Stops callbacks before the encoder is finalised.
    pub fn close(self) {
        let _ = self.pool.RemoveFrameArrived(self.frame_token);
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
