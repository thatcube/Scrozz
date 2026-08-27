//! D3D11 device shared by WGC and the Media Foundation encoder.

use std::sync::{Arc, Mutex};

use scrozz_core::{Error, Result};
use windows::{
    Graphics::DirectX::Direct3D11::{IDirect3DDevice, IDirect3DSurface},
    Win32::{
        Foundation::{HMODULE, RECT},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                D3D11_SDK_VERSION, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT,
                D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
                D3D11_VIDEO_USAGE_OPTIMAL_QUALITY, D3D11_VPIV_DIMENSION_TEXTURE2D,
                D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11CreateDevice, ID3D11Device,
                ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, ID3D11VideoContext,
                ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
                ID3D11VideoProcessorInputView,
            },
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC},
                IDXGIDevice,
            },
        },
        System::WinRT::Direct3D11::{
            CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
        },
    },
    core::Interface,
};

use super::plan::EncoderPlan;

/// A rectangle inside the WGC source surface.
#[derive(Debug, Clone, Copy)]
pub struct Crop {
    /// Left edge in source pixels.
    pub left: u32,
    /// Top edge in source pixels.
    pub top: u32,
    /// Fixed recording width.
    pub width: u32,
    /// Fixed recording height.
    pub height: u32,
}

/// The D3D11 resources shared across WGC callback and encoder threads.
#[derive(Clone)]
pub struct Device {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    winrt: IDirect3DDevice,
    scaler: Arc<Mutex<Option<FrameScaler>>>,
}

// D3D11's multithread protection is enabled during construction. The WinRT
// frame pool and the hardware MFT are specifically designed to share this
// device across their worker threads.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    /// Creates a hardware D3D11 device and enables immediate-context locking.
    pub fn new() -> Result<Self> {
        let mut device = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                None,
                None,
            )
        }
        .map_err(|error| Error::Unsupported {
            what: "Windows screen recording".into(),
            why: format!("no hardware D3D11 device is available: {error}"),
        })?;
        let device = device.ok_or_else(|| Error::Platform("D3D11 returned no device".into()))?;
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|error| Error::Platform(format!("GetImmediateContext failed: {error}")))?;
        let multithread: ID3D11Multithread = context
            .cast()
            .map_err(|error| Error::Platform(format!("ID3D11Multithread unavailable: {error}")))?;
        let _ = unsafe { multithread.SetMultithreadProtected(true) };
        let video_device = device.cast().map_err(|error| Error::Unsupported {
            what: "Windows screen recording".into(),
            why: format!("D3D11 video processing is unavailable: {error}"),
        })?;
        let video_context = context.cast().map_err(|error| Error::Unsupported {
            what: "Windows screen recording".into(),
            why: format!("D3D11 video context is unavailable: {error}"),
        })?;

        let dxgi: IDXGIDevice = device
            .cast()
            .map_err(|error| Error::Platform(format!("IDXGIDevice unavailable: {error}")))?;
        let winrt: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .map_err(|error| {
                Error::Platform(format!(
                    "CreateDirect3D11DeviceFromDXGIDevice failed: {error}"
                ))
            })?
            .cast()
            .map_err(|error| Error::Platform(format!("IDirect3DDevice unavailable: {error}")))?;

        Ok(Self {
            device,
            context,
            video_device,
            video_context,
            winrt,
            scaler: Arc::new(Mutex::new(None)),
        })
    }

    /// Native device passed to the MF DXGI device manager.
    #[must_use]
    pub const fn native(&self) -> &ID3D11Device {
        &self.device
    }

    /// WinRT projection passed to the free-threaded WGC frame pool.
    #[must_use]
    pub const fn winrt(&self) -> &IDirect3DDevice {
        &self.winrt
    }

    /// Copies the requested source rectangle into a fixed-size GPU texture.
    pub fn copy_frame(
        &self,
        surface: &IDirect3DSurface,
        content_width: u32,
        content_height: u32,
        crop: Crop,
        plan: EncoderPlan,
    ) -> Result<ID3D11Texture2D> {
        let access: IDirect3DDxgiInterfaceAccess = surface
            .cast()
            .map_err(|error| Error::Platform(format!("WGC surface has no DXGI view: {error}")))?;
        let source: ID3D11Texture2D = unsafe { access.GetInterface() }
            .map_err(|error| Error::Platform(format!("WGC texture access failed: {error}")))?;

        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&raw mut source_desc) };
        let source_width = source_desc.Width.min(content_width);
        let source_height = source_desc.Height.min(content_height);
        if crop.left >= source_width || crop.top >= source_height {
            return Err(Error::TargetGone(
                "the recorded area no longer overlaps its capture source".into(),
            ));
        }

        let copy_width = crop.width.min(source_width - crop.left);
        let copy_height = crop.height.min(source_height - crop.top);
        let mut scaler = self
            .scaler
            .lock()
            .map_err(|_| Error::Platform("D3D11 scaler lock was poisoned".into()))?;
        let rebuild = scaler.as_ref().is_none_or(|scaler| {
            !scaler.matches(
                source_desc.Width,
                source_desc.Height,
                plan.output_width,
                plan.output_height,
                plan.fps,
            )
        });
        if rebuild {
            *scaler = Some(FrameScaler::new(
                self,
                source_desc.Width,
                source_desc.Height,
                plan.output_width,
                plan.output_height,
                plan.fps,
            )?);
        }
        scaler.as_ref().expect("frame scaler was created").scale(
            self,
            &source,
            Crop {
                left: crop.left,
                top: crop.top,
                width: copy_width,
                height: copy_height,
            },
        )
    }

    fn create_texture(&self, width: u32, height: u32) -> Result<ID3D11Texture2D> {
        let description = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE)
                .0
                .cast_unsigned(),
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe {
            self.device
                .CreateTexture2D(&raw const description, None, Some(&raw mut texture))
        }
        .map_err(|error| Error::Platform(format!("frame texture creation failed: {error}")))?;
        texture.ok_or_else(|| Error::Platform("D3D11 returned no frame texture".into()))
    }
}

struct FrameScaler {
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input_width: u32,
    input_height: u32,
    output_width: u32,
    output_height: u32,
    fps: u32,
}

impl FrameScaler {
    fn new(
        device: &Device,
        input_width: u32,
        input_height: u32,
        output_width: u32,
        output_height: u32,
        fps: u32,
    ) -> Result<Self> {
        let frame_rate = DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        };
        let description = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: frame_rate,
            InputWidth: input_width,
            InputHeight: input_height,
            OutputFrameRate: frame_rate,
            OutputWidth: output_width,
            OutputHeight: output_height,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
        };
        let enumerator = unsafe {
            device
                .video_device
                .CreateVideoProcessorEnumerator(&raw const description)
        }
        .map_err(|error| Error::Unsupported {
            what: "Windows recording resolution".into(),
            why: format!("D3D11 cannot create a video scaler: {error}"),
        })?;
        let support = unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM) }
            .map_err(|error| {
            Error::Platform(format!("D3D11 scaler format query failed: {error}"))
        })?;
        let required = (D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0
            | D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0)
            .cast_unsigned();
        if support & required != required {
            return Err(Error::Unsupported {
                what: "Windows recording resolution".into(),
                why: "the D3D11 video processor cannot scale BGRA capture frames".into(),
            });
        }
        let processor = unsafe { device.video_device.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|error| Error::Unsupported {
                what: "Windows recording resolution".into(),
                why: format!("D3D11 cannot create a video processor: {error}"),
            })?;
        Ok(Self {
            enumerator,
            processor,
            input_width,
            input_height,
            output_width,
            output_height,
            fps,
        })
    }

    const fn matches(
        &self,
        input_width: u32,
        input_height: u32,
        output_width: u32,
        output_height: u32,
        fps: u32,
    ) -> bool {
        self.input_width == input_width
            && self.input_height == input_height
            && self.output_width == output_width
            && self.output_height == output_height
            && self.fps == fps
    }

    fn scale(
        &self,
        device: &Device,
        source: &ID3D11Texture2D,
        crop: Crop,
    ) -> Result<ID3D11Texture2D> {
        let destination = device.create_texture(self.output_width, self.output_height)?;
        let input_description = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let output_description = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut input = None;
        let mut output = None;
        let mut create_views = || -> windows::core::Result<()> {
            unsafe {
                device.video_device.CreateVideoProcessorInputView(
                    source,
                    &self.enumerator,
                    &raw const input_description,
                    Some(&raw mut input),
                )?;
                device.video_device.CreateVideoProcessorOutputView(
                    &destination,
                    &self.enumerator,
                    &raw const output_description,
                    Some(&raw mut output),
                )?;
            }
            Ok(())
        };
        create_views().map_err(|error| {
            Error::Platform(format!("D3D11 scaler view creation failed: {error}"))
        })?;
        let input =
            input.ok_or_else(|| Error::Platform("D3D11 returned no scaler input".into()))?;
        let output =
            output.ok_or_else(|| Error::Platform("D3D11 returned no scaler output".into()))?;
        let source_rect = rect(crop.left, crop.top, crop.width, crop.height)?;
        let destination_rect = rect(0, 0, self.output_width, self.output_height)?;
        unsafe {
            device.video_context.VideoProcessorSetStreamFrameFormat(
                &self.processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            device.video_context.VideoProcessorSetStreamSourceRect(
                &self.processor,
                0,
                true,
                Some(&raw const source_rect),
            );
            device.video_context.VideoProcessorSetStreamDestRect(
                &self.processor,
                0,
                true,
                Some(&raw const destination_rect),
            );
        }
        let stream = ProcessorStream::new(input);
        unsafe {
            device.video_context.VideoProcessorBlt(
                &self.processor,
                &output,
                0,
                core::slice::from_ref(&stream.0),
            )
        }
        .map_err(|error| Error::Platform(format!("D3D11 frame scaling failed: {error}")))?;
        Ok(destination)
    }
}

struct ProcessorStream(D3D11_VIDEO_PROCESSOR_STREAM);

impl ProcessorStream {
    fn new(input: ID3D11VideoProcessorInputView) -> Self {
        Self(D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: core::mem::ManuallyDrop::new(Some(input)),
            ..Default::default()
        })
    }
}

impl Drop for ProcessorStream {
    fn drop(&mut self) {
        unsafe {
            core::mem::ManuallyDrop::drop(&mut self.0.pInputSurface);
            core::mem::ManuallyDrop::drop(&mut self.0.pInputSurfaceRight);
        }
    }
}

fn rect(left: u32, top: u32, width: u32, height: u32) -> Result<RECT> {
    let right = left
        .checked_add(width)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| Error::Platform("D3D11 source rectangle overflowed".into()))?;
    let bottom = top
        .checked_add(height)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| Error::Platform("D3D11 source rectangle overflowed".into()))?;
    Ok(RECT {
        left: i32::try_from(left)
            .map_err(|_| Error::Platform("D3D11 source rectangle overflowed".into()))?,
        top: i32::try_from(top)
            .map_err(|_| Error::Platform("D3D11 source rectangle overflowed".into()))?,
        right,
        bottom,
    })
}
