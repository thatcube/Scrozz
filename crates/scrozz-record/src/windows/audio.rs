//! Event-driven WASAPI loopback and microphone capture.

use std::{
    mem::size_of,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use scrozz_core::{Error, Result};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_FAILED, WAIT_OBJECT_0},
        Media::{
            Audio::{
                AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR,
                AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_NOPERSIST, IAudioCaptureClient,
                IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVE_FORMAT_PCM,
                WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eCapture, eConsole, eRender,
            },
            KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE},
            Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT},
        },
        System::{
            Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree},
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects},
        },
    },
    core::PCWSTR,
};

use super::{
    com::Apartment,
    mix::{Packet, Source},
    timing::qpc_to_hns,
};

const PACKET_QUEUE_CAPACITY: usize = 64;
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// A native packet before pause removal and recording-origin mapping.
#[derive(Debug)]
pub struct RawPacket {
    /// Endpoint that produced the packet.
    pub source: Source,
    /// Absolute QPC-correlated timestamp in 100 ns units.
    pub qpc_hns: i64,
    /// Endpoint sample rate.
    pub sample_rate: u32,
    /// Interleaved channel count.
    pub channels: u16,
    /// Endpoint speaker positions, or zero when the format omits them.
    pub channel_mask: u32,
    /// Normalized interleaved samples.
    pub samples: Vec<f32>,
}

impl RawPacket {
    /// Maps the native packet onto the pause-free recording timeline.
    pub fn at_stream_time(self, stream_hns: i64) -> Packet {
        Packet {
            stream_hns,
            sample_rate: self.sample_rate,
            channels: self.channels,
            channel_mask: self.channel_mask,
            samples: self.samples,
        }
    }
}

/// Owns the WASAPI worker and its bounded packet queue.
pub struct AudioCapture {
    packets: Receiver<RawPacket>,
    failures: Receiver<String>,
    shutdown: Arc<Event>,
    thread: Option<JoinHandle<()>>,
}

impl AudioCapture {
    /// Opens the requested default endpoints and starts event-driven capture.
    pub fn start(system_audio: bool, microphone: bool) -> Result<Self> {
        let shutdown = Arc::new(Event::new()?);
        let (packets_tx, packets) = mpsc::sync_channel(PACKET_QUEUE_CAPACITY);
        let (failures_tx, failures) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = thread::Builder::new()
            .name("scrozz-wasapi".into())
            .spawn(move || {
                if let Err(error) = worker(
                    system_audio,
                    microphone,
                    &thread_shutdown,
                    &packets_tx,
                    &ready_tx,
                ) && ready_tx.try_send(Err(error.clone())).is_err()
                {
                    let _ = failures_tx.send(error);
                }
            })?;

        match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                packets,
                failures,
                shutdown,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = shutdown.signal();
                let _ = thread.join();
                Err(Error::Platform(error))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = shutdown.signal();
                Err(Error::Platform(format!(
                    "WASAPI worker did not become ready within {} seconds",
                    READY_TIMEOUT.as_secs()
                )))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                Err(Error::Platform(
                    "WASAPI worker exited during startup".into(),
                ))
            }
        }
    }

    /// Receives one packet without blocking the encoder.
    pub fn try_packet(&self) -> std::result::Result<RawPacket, TryRecvError> {
        self.packets.try_recv()
    }

    /// Receives one worker failure without blocking.
    pub fn try_failure(&self) -> Option<String> {
        self.failures.try_recv().ok()
    }

    /// Stops both clients and joins the apartment that owns them.
    pub fn close(mut self) -> Result<()> {
        self.stop()
    }

    /// Stops both clients while retaining queued packets for a final drain.
    pub fn shutdown(&mut self) -> Result<()> {
        self.stop()
    }

    fn stop(&mut self) -> Result<()> {
        self.shutdown.signal()?;
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| Error::Platform("WASAPI worker panicked".into()))?;
        }
        Ok(())
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::error!(%error, "could not stop abandoned WASAPI capture");
        }
    }
}

fn worker(
    system_audio: bool,
    microphone: bool,
    shutdown: &Event,
    packets: &SyncSender<RawPacket>,
    ready: &SyncSender<std::result::Result<(), String>>,
) -> std::result::Result<(), String> {
    let _apartment = Apartment::enter().map_err(|error| error.to_string())?;
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|error| format!("could not create MMDeviceEnumerator: {error}"))?;

    let mut clients = Vec::with_capacity(2);
    if system_audio {
        clients.push(
            EndpointCapture::open(&enumerator, Source::System)
                .map_err(|error| format!("could not open system loopback: {error}"))?,
        );
    }
    if microphone {
        clients.push(
            EndpointCapture::open(&enumerator, Source::Microphone)
                .map_err(|error| format!("could not open microphone: {error}"))?,
        );
    }
    for client in &clients {
        client
            .start()
            .map_err(|error| format!("could not start {:?}: {error}", client.source))?;
    }
    ready
        .send(Ok(()))
        .map_err(|_| "recording session closed during WASAPI startup".to_owned())?;

    let mut handles: Vec<HANDLE> = clients.iter().map(|client| client.event.handle()).collect();
    handles.push(shutdown.handle());
    loop {
        let wait = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
        if wait == WAIT_FAILED {
            return Err(format!(
                "WaitForMultipleObjects failed: {}",
                windows::core::Error::from_thread()
            ));
        }
        let index = wait.0.saturating_sub(WAIT_OBJECT_0.0) as usize;
        if index == clients.len() {
            return Ok(());
        }
        let Some(client) = clients.get_mut(index) else {
            return Err(format!("WASAPI wait returned unexpected index {index}"));
        };
        for packet in client
            .drain()
            .map_err(|error| format!("{:?} capture failed: {error}", client.source))?
        {
            match packets.try_send(packet) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => return Ok(()),
            }
        }
    }
}

struct EndpointCapture {
    source: Source,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
    format: Format,
}

impl EndpointCapture {
    fn open(enumerator: &IMMDeviceEnumerator, source: Source) -> windows::core::Result<Self> {
        let flow = match source {
            Source::System => eRender,
            Source::Microphone => eCapture,
        };
        let endpoint = unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole)? };
        let client: IAudioClient = unsafe { endpoint.Activate(CLSCTX_ALL, None)? };
        let mix_format = unsafe { client.GetMixFormat()? };
        if mix_format.is_null() {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_POINTER,
                "WASAPI returned a null mix format",
            ));
        }

        let format = match unsafe { Format::parse(mix_format) } {
            Ok(format) => format,
            Err(error) => {
                unsafe { CoTaskMemFree(Some(mix_format.cast())) };
                return Err(error);
            }
        };
        let event = match Event::new() {
            Ok(event) => event,
            Err(error) => {
                unsafe { CoTaskMemFree(Some(mix_format.cast())) };
                return Err(core_to_windows(error));
            }
        };
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_NOPERSIST
            | if source == Source::System {
                AUDCLNT_STREAMFLAGS_LOOPBACK
            } else {
                0
            };
        let initialized =
            unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, mix_format, None) };
        unsafe { CoTaskMemFree(Some(mix_format.cast())) };
        initialized?;
        unsafe {
            client.SetEventHandle(event.handle())?;
        }
        let capture = unsafe { client.GetService::<IAudioCaptureClient>()? };
        Ok(Self {
            source,
            client,
            capture,
            event,
            format,
        })
    }

    fn start(&self) -> windows::core::Result<()> {
        unsafe { self.client.Start() }
    }

    fn drain(&mut self) -> windows::core::Result<Vec<RawPacket>> {
        let mut packets = Vec::new();
        loop {
            let available = unsafe { self.capture.GetNextPacketSize()? };
            if available == 0 {
                return Ok(packets);
            }

            let mut data = core::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut qpc = 0u64;
            unsafe {
                self.capture.GetBuffer(
                    &raw mut data,
                    &raw mut frames,
                    &raw mut flags,
                    None,
                    Some(&raw mut qpc),
                )?;
            }
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0.cast_unsigned() != 0;
            let decoded = if silent {
                Ok(vec![
                    0.0;
                    frames as usize * usize::from(self.format.channels)
                ])
            } else {
                unsafe { self.format.decode(data.cast_const(), frames) }
            };
            let released = unsafe { self.capture.ReleaseBuffer(frames) };
            let samples = decoded?;
            released?;

            let timestamp_error =
                flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0.cast_unsigned() != 0;
            let qpc_hns = if timestamp_error {
                qpc_now_hns()?
            } else {
                i64::try_from(qpc).map_err(|_| {
                    windows::core::Error::new(
                        windows::Win32::Foundation::E_FAIL,
                        "WASAPI QPC timestamp overflowed",
                    )
                })?
            };
            packets.push(RawPacket {
                source: self.source,
                qpc_hns,
                sample_rate: self.format.sample_rate,
                channels: self.format.channels,
                channel_mask: self.format.channel_mask,
                samples,
            });
        }
    }
}

impl Drop for EndpointCapture {
    fn drop(&mut self) {
        if let Err(error) = unsafe { self.client.Stop() } {
            tracing::warn!(
                source = ?self.source,
                %error,
                "could not stop WASAPI endpoint during drop"
            );
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Format {
    sample_rate: u32,
    channels: u16,
    channel_mask: u32,
    bytes_per_frame: u16,
    encoding: Encoding,
}

#[derive(Debug, Clone, Copy)]
enum Encoding {
    Float32,
    Float64,
    Unsigned8,
    Signed16,
    Signed24,
    Signed32,
}

impl Format {
    unsafe fn parse(format: *const WAVEFORMATEX) -> windows::core::Result<Self> {
        let base = unsafe { format.read_unaligned() };
        let tag = u32::from(base.wFormatTag);
        let (encoding, channel_mask) = if tag == WAVE_FORMAT_IEEE_FLOAT {
            (
                float_encoding(base.wBitsPerSample)?,
                Self::default_channel_mask(base.nChannels),
            )
        } else if tag == WAVE_FORMAT_PCM {
            (
                pcm_encoding(base.wBitsPerSample)?,
                Self::default_channel_mask(base.nChannels),
            )
        } else if tag == WAVE_FORMAT_EXTENSIBLE {
            if usize::from(base.cbSize) + size_of::<WAVEFORMATEX>()
                < size_of::<WAVEFORMATEXTENSIBLE>()
            {
                return Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_INVALIDARG,
                    "WASAPI extensible format is truncated",
                ));
            }
            let extended = unsafe { format.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() };
            let subtype = extended.SubFormat;
            let encoding = if subtype == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                float_encoding(base.wBitsPerSample)?
            } else if subtype == KSDATAFORMAT_SUBTYPE_PCM {
                pcm_encoding(base.wBitsPerSample)?
            } else {
                return Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_NOTIMPL,
                    "WASAPI endpoint uses an unsupported sample subtype",
                ));
            };
            (encoding, extended.dwChannelMask)
        } else {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_NOTIMPL,
                "WASAPI endpoint uses an unsupported sample format",
            ));
        };

        if base.nChannels == 0
            || base.nSamplesPerSec == 0
            || base.nBlockAlign == 0
            || base.nBlockAlign % base.nChannels != 0
        {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_INVALIDARG,
                "WASAPI endpoint returned an invalid mix format",
            ));
        }
        Ok(Self {
            sample_rate: base.nSamplesPerSec,
            channels: base.nChannels,
            channel_mask,
            bytes_per_frame: base.nBlockAlign,
            encoding,
        })
    }

    const fn default_channel_mask(channels: u16) -> u32 {
        match channels {
            1 => 0x0004,
            2 => 0x0003,
            _ => 0,
        }
    }

    unsafe fn decode(self, data: *const u8, frames: u32) -> windows::core::Result<Vec<f32>> {
        if data.is_null() {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_POINTER,
                "WASAPI returned a null non-silent packet",
            ));
        }
        let sample_count = frames as usize * usize::from(self.channels);
        let packet_bytes = frames as usize * usize::from(self.bytes_per_frame);
        let bytes = unsafe { core::slice::from_raw_parts(data, packet_bytes) };
        let bytes_per_sample = usize::from(self.bytes_per_frame) / usize::from(self.channels);
        if bytes_per_sample == 0 || bytes_per_sample * sample_count > bytes.len() {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_INVALIDARG,
                "WASAPI packet does not match its block alignment",
            ));
        }

        let mut samples = Vec::with_capacity(sample_count);
        for sample in bytes.chunks_exact(bytes_per_sample).take(sample_count) {
            let normalized = match self.encoding {
                Encoding::Float32 if sample.len() >= 4 => {
                    f32::from_le_bytes(sample[..4].try_into().expect("four bytes"))
                }
                Encoding::Float64 if sample.len() >= 8 => {
                    f64::from_le_bytes(sample[..8].try_into().expect("eight bytes")) as f32
                }
                Encoding::Unsigned8 => (f32::from(sample[0]) - 128.0) / 128.0,
                Encoding::Signed16 if sample.len() >= 2 => {
                    f32::from(i16::from_le_bytes(
                        sample[..2].try_into().expect("two bytes"),
                    )) / 32_768.0
                }
                Encoding::Signed24 if sample.len() >= 3 => {
                    let value = i32::from_le_bytes([
                        sample[0],
                        sample[1],
                        sample[2],
                        if sample[2] & 0x80 == 0 { 0 } else { 0xff },
                    ]);
                    value as f32 / 8_388_608.0
                }
                Encoding::Signed32 if sample.len() >= 4 => {
                    i32::from_le_bytes(sample[..4].try_into().expect("four bytes")) as f32
                        / 2_147_483_648.0
                }
                _ => {
                    return Err(windows::core::Error::new(
                        windows::Win32::Foundation::E_INVALIDARG,
                        "WASAPI packet has an invalid sample stride",
                    ));
                }
            };
            samples.push(if normalized.is_finite() {
                normalized.clamp(-1.0, 1.0)
            } else {
                0.0
            });
        }
        Ok(samples)
    }
}

fn pcm_encoding(bits: u16) -> windows::core::Result<Encoding> {
    match bits {
        8 => Ok(Encoding::Unsigned8),
        16 => Ok(Encoding::Signed16),
        24 => Ok(Encoding::Signed24),
        32 => Ok(Encoding::Signed32),
        _ => Err(windows::core::Error::new(
            windows::Win32::Foundation::E_NOTIMPL,
            format!("unsupported WASAPI PCM depth: {bits}"),
        )),
    }
}

fn float_encoding(bits: u16) -> windows::core::Result<Encoding> {
    match bits {
        32 => Ok(Encoding::Float32),
        64 => Ok(Encoding::Float64),
        _ => Err(windows::core::Error::new(
            windows::Win32::Foundation::E_NOTIMPL,
            format!("unsupported WASAPI float depth: {bits}"),
        )),
    }
}

/// Current QueryPerformanceCounter time in the same units WGC and WASAPI use.
pub fn qpc_now_hns() -> windows::core::Result<i64> {
    let mut counter = 0i64;
    let mut frequency = 0i64;
    unsafe {
        QueryPerformanceCounter(&raw mut counter)?;
        QueryPerformanceFrequency(&raw mut frequency)?;
    }
    qpc_to_hns(counter, frequency).ok_or_else(|| {
        windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            "QueryPerformanceCounter conversion overflowed",
        )
    })
}

struct Event(HANDLE);

// Kernel event handles can be waited and signalled concurrently.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    fn new() -> Result<Self> {
        let handle = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|error| Error::Platform(format!("CreateEventW failed: {error}")))?;
        Ok(Self(handle))
    }

    const fn handle(&self) -> HANDLE {
        self.0
    }

    fn signal(&self) -> Result<()> {
        unsafe { SetEvent(self.0) }
            .map_err(|error| Error::Platform(format!("SetEvent failed: {error}")))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        if let Err(error) = unsafe { CloseHandle(self.0) } {
            tracing::error!(%error, "could not close WASAPI event handle");
        }
    }
}

fn core_to_windows(error: Error) -> windows::core::Error {
    windows::core::Error::new(windows::Win32::Foundation::E_FAIL, error.to_string())
}
