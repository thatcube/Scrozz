//! AVFoundation media clock, audio rendering, and preview audio edits.

use std::{
    ffi::c_void,
    path::Path,
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    time::Duration,
};

use block2::RcBlock;
use objc2::{MainThreadMarker, rc::Retained};
use objc2_av_foundation::{
    AVAsset, AVAudioMixInputParameters, AVMediaTypeAudio, AVMutableAudioMix,
    AVMutableAudioMixInputParameters, AVPlayer, AVPlayerActionAtItemEnd, AVPlayerItem,
    AVPlayerItemStatus, AVPlayerStatus, AVPlayerTimeControlStatus,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, kAudioFormatFlagIsBigEndian,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMItemCount, CMTime, CMTimeFlags, CMTimeRange, kCMTimeZero};
use objc2_foundation::{NSArray, NSString, NSURL};
use objc2_media_toolbox::{
    MTAudioProcessingTap, MTAudioProcessingTapCallbacks, MTAudioProcessingTapFlags,
    kMTAudioProcessingTapCallbacksVersion_0, kMTAudioProcessingTapCreationFlag_PreEffects,
};
use scrozz_core::{Error, Result};

use crate::{
    edit::{ChannelBehavior, EditPlan},
    macos::error,
};

pub(super) const BACKEND_NAME: &str = "macOS AVFoundation + MediaToolbox";
pub(super) const AVAILABLE: bool = true;
pub(super) const UNAVAILABLE_REASON: Option<&str> = None;

const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;
const MAX_PROCESSING_BUFFERS: usize = 32;
const TAP_OK: i32 = 0;
const TAP_UNSUPPORTED_FORMAT: i32 = 1;
const TAP_MALFORMED_BUFFER: i32 = 2;

pub(super) struct Observation {
    pub(super) position: Option<Duration>,
    pub(super) running: bool,
    pub(super) buffering: bool,
    pub(super) seeking: bool,
    pub(super) audio_frames_rendered: u64,
}

pub(super) struct Clock {
    player: Retained<AVPlayer>,
    item: Retained<AVPlayerItem>,
    audio: Option<AudioTap>,
    has_audio: bool,
    seek_sequence: u64,
    pending_seek: Option<(u64, Duration)>,
    seek_events: Receiver<(u64, bool)>,
    seek_sender: Sender<(u64, bool)>,
}

impl Clock {
    pub(super) fn open(path: &Path, has_audio: bool, plan: EditPlan) -> Result<Self> {
        let mtm = MainThreadMarker::new().ok_or_else(|| {
            Error::Platform(
                "AVFoundation recording playback must be opened on the main thread".to_owned(),
            )
        })?;
        let path = NSString::from_str(&path.to_string_lossy());
        let url = NSURL::fileURLWithPath(&path);
        let asset = unsafe { AVAsset::assetWithURL(&url) };
        let item = unsafe { AVPlayerItem::playerItemWithAsset(&asset, mtm) };
        let audio = if has_audio {
            Some(AudioTap::attach(&asset, &item)?)
        } else {
            None
        };
        let player = unsafe { AVPlayer::playerWithPlayerItem(Some(&item), mtm) };
        unsafe {
            player.setActionAtItemEnd(AVPlayerActionAtItemEnd::Pause);
            player.setAutomaticallyWaitsToMinimizeStalling(false);
            player.setVolume(1.0);
        }
        let (seek_sender, seek_events) = mpsc::channel();
        let clock = Self {
            player,
            item,
            audio,
            has_audio,
            seek_sequence: 0,
            pending_seek: None,
            seek_events,
            seek_sender,
        };
        clock.configure(plan)?;
        Ok(clock)
    }

    pub(super) fn configure(&self, plan: EditPlan) -> Result<()> {
        unsafe {
            self.item
                .setForwardPlaybackEndTime(time_from_duration(plan.trim.end));
        }
        if let Some(audio) = &self.audio {
            audio.configure(plan);
        }
        unsafe {
            self.player
                .setMuted(!self.has_audio || plan.audio.mute || !plan.output.supports_audio());
        }
        Ok(())
    }

    pub(super) fn play(&self, rate: f32) -> Result<()> {
        self.check_failure()?;
        unsafe {
            self.player.setDefaultRate(rate);
            self.player.playImmediatelyAtRate(rate);
        }
        Ok(())
    }

    pub(super) fn pause(&self) {
        unsafe {
            self.player.pause();
        }
    }

    pub(super) fn seek(&mut self, position: Duration) -> Result<()> {
        self.check_failure()?;
        self.seek_sequence = self.seek_sequence.wrapping_add(1).max(1);
        let sequence = self.seek_sequence;
        self.pending_seek = Some((sequence, position));
        let events = self.seek_sender.clone();
        let completion = RcBlock::new(move |finished: objc2::runtime::Bool| {
            let _ = events.send((sequence, finished.as_bool()));
        });
        let zero = unsafe { kCMTimeZero };
        unsafe {
            self.player
                .seekToTime_toleranceBefore_toleranceAfter_completionHandler(
                    time_from_duration(position),
                    zero,
                    zero,
                    &completion,
                );
        }
        Ok(())
    }

    pub(super) fn observe(&mut self) -> Result<Observation> {
        self.check_failure()?;
        if let Some(audio) = &self.audio {
            audio.check_failure()?;
        }
        loop {
            match self.seek_events.try_recv() {
                Ok((sequence, finished)) => {
                    if self
                        .pending_seek
                        .is_some_and(|(pending, _)| pending == sequence)
                    {
                        if !finished {
                            self.pending_seek = None;
                            return Err(Error::Platform(
                                "AVFoundation interrupted the active recording preview seek"
                                    .to_owned(),
                            ));
                        }
                        self.pending_seek = None;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(Error::Platform(
                        "recording preview seek completion channel disconnected".to_owned(),
                    ));
                }
            }
        }
        let position = self
            .pending_seek
            .map(|(_, target)| target)
            .or_else(|| duration_from_time(unsafe { self.player.currentTime() }));
        let state = unsafe { self.player.timeControlStatus() };
        Ok(Observation {
            position,
            running: state == AVPlayerTimeControlStatus::Playing
                && unsafe { self.player.rate() } > 0.0,
            buffering: state == AVPlayerTimeControlStatus::WaitingToPlayAtSpecifiedRate,
            seeking: self.pending_seek.is_some(),
            audio_frames_rendered: self.audio.as_ref().map_or(0, |audio| {
                audio.state.frames_rendered.load(Ordering::Acquire)
            }),
        })
    }

    pub(super) const fn seeking(&self) -> bool {
        self.pending_seek.is_some()
    }

    fn check_failure(&self) -> Result<()> {
        if unsafe { self.player.status() } == AVPlayerStatus::Failed {
            return Err(Error::Codec(unsafe {
                self.player.error().map_or_else(
                    || "AVPlayer failed without an NSError".to_owned(),
                    |failure| error::describe(&failure, "playing recording preview"),
                )
            }));
        }
        if unsafe { self.item.status() } == AVPlayerItemStatus::Failed {
            return Err(Error::Codec(unsafe {
                self.item.error().map_or_else(
                    || "AVPlayerItem failed without an NSError".to_owned(),
                    |failure| error::describe(&failure, "loading recording preview"),
                )
            }));
        }
        Ok(())
    }
}

struct AudioTap {
    _tap: CFRetained<MTAudioProcessingTap>,
    state: Arc<AudioTapState>,
}

impl AudioTap {
    #[allow(deprecated)]
    fn attach(asset: &AVAsset, item: &AVPlayerItem) -> Result<Self> {
        let media_type = unsafe { AVMediaTypeAudio }.ok_or_else(|| Error::Unsupported {
            what: "recording preview audio".to_owned(),
            why: "AVFoundation did not expose its audio media type".to_owned(),
        })?;
        let tracks = unsafe { asset.tracksWithMediaType(media_type) };
        if tracks.count() == 0 {
            return Err(Error::Codec(
                "recording metadata reported audio but AVFoundation found no audio track"
                    .to_owned(),
            ));
        }
        let track = unsafe { tracks.objectAtIndex_unchecked(0) };

        let state = Arc::new(AudioTapState::default());
        let client = Arc::into_raw(Arc::clone(&state))
            .cast_mut()
            .cast::<c_void>();
        let mut callbacks = MTAudioProcessingTapCallbacks {
            version: kMTAudioProcessingTapCallbacksVersion_0,
            clientInfo: client,
            init: Some(tap_init),
            finalize: Some(tap_finalize),
            prepare: Some(tap_prepare),
            unprepare: None,
            process: Some(tap_process),
        };
        let mut raw = std::ptr::null();
        let status = unsafe {
            MTAudioProcessingTap::create(
                None,
                NonNull::from(&mut callbacks),
                kMTAudioProcessingTapCreationFlag_PreEffects,
                NonNull::from(&mut raw),
            )
        };
        if status != 0 {
            // Creation did not take ownership of clientInfo when no tap exists.
            unsafe {
                drop(Arc::from_raw(client.cast::<AudioTapState>()));
            }
            return Err(Error::Codec(format!(
                "creating recording preview audio processor failed with status {status}"
            )));
        }
        let Some(raw) = NonNull::new(raw.cast_mut()) else {
            unsafe {
                drop(Arc::from_raw(client.cast::<AudioTapState>()));
            }
            return Err(Error::Codec(
                "MediaToolbox created no recording preview audio processor".to_owned(),
            ));
        };
        let tap = unsafe { CFRetained::from_raw(raw) };

        let input = unsafe {
            AVMutableAudioMixInputParameters::audioMixInputParametersWithTrack(Some(track))
        };
        unsafe {
            input.setAudioTapProcessor(Some(&tap));
        }
        let input: Retained<AVAudioMixInputParameters> = input.into_super();
        let inputs = NSArray::from_retained_slice(&[input]);
        let mix = unsafe { AVMutableAudioMix::audioMix() };
        unsafe {
            mix.setInputParameters(&inputs);
            item.setAudioMix(Some(&mix));
        }
        Ok(Self { _tap: tap, state })
    }

    fn configure(&self, plan: EditPlan) {
        let gain = if plan.audio.mute || !plan.output.supports_audio() {
            0.0
        } else {
            plan.audio.effective_gain()
        };
        self.state
            .gain_bits
            .store(gain.to_bits(), Ordering::Release);
        self.state.mono.store(
            plan.audio.channels == ChannelBehavior::StereoToMono,
            Ordering::Release,
        );
    }

    fn check_failure(&self) -> Result<()> {
        match self.state.failure.load(Ordering::Acquire) {
            TAP_OK => Ok(()),
            TAP_UNSUPPORTED_FORMAT => Err(Error::Unsupported {
                what: "recording preview audio format".to_owned(),
                why: "MediaToolbox did not provide packed native Float32 PCM".to_owned(),
            }),
            TAP_MALFORMED_BUFFER => Err(Error::Codec(
                "MediaToolbox provided a malformed recording preview audio buffer".to_owned(),
            )),
            status => Err(Error::Codec(format!(
                "decoding recording preview audio failed with status {status}"
            ))),
        }
    }
}

#[derive(Default)]
struct AudioTapState {
    gain_bits: AtomicU32,
    mono: AtomicBool,
    format_valid: AtomicBool,
    channels: AtomicU32,
    failure: AtomicI32,
    frames_rendered: AtomicU64,
}

unsafe extern "C-unwind" fn tap_init(
    _tap: NonNull<MTAudioProcessingTap>,
    client: *mut c_void,
    storage: NonNull<*mut c_void>,
) {
    unsafe {
        storage.as_ptr().write(client);
    }
}

unsafe extern "C-unwind" fn tap_finalize(tap: NonNull<MTAudioProcessingTap>) {
    let storage = unsafe { tap.as_ref().storage() };
    unsafe {
        drop(Arc::from_raw(storage.cast::<AudioTapState>().as_ptr()));
    }
}

unsafe extern "C-unwind" fn tap_prepare(
    tap: NonNull<MTAudioProcessingTap>,
    _max_frames: CMItemCount,
    format: NonNull<AudioStreamBasicDescription>,
) {
    let state = unsafe { tap_state(tap) };
    let format = unsafe { format.as_ref() };
    let valid = processing_format_is_supported(format);
    state
        .channels
        .store(format.mChannelsPerFrame, Ordering::Release);
    state.format_valid.store(valid, Ordering::Release);
    if !valid {
        state
            .failure
            .store(TAP_UNSUPPORTED_FORMAT, Ordering::Release);
    }
}

fn processing_format_is_supported(format: &AudioStreamBasicDescription) -> bool {
    let channels = format.mChannelsPerFrame;
    let bytes_per_frame = if format.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0 {
        std::mem::size_of::<f32>() as u32
    } else {
        channels.saturating_mul(std::mem::size_of::<f32>() as u32)
    };
    format.mFormatID == kAudioFormatLinearPCM
        && format.mFormatFlags & kAudioFormatFlagIsFloat != 0
        && format.mFormatFlags & kAudioFormatFlagIsPacked != 0
        && format.mFormatFlags & kAudioFormatFlagIsBigEndian == 0
        && format.mBitsPerChannel == 32
        && (1..=MAX_PROCESSING_BUFFERS as u32).contains(&channels)
        && format.mBytesPerFrame == bytes_per_frame
}

unsafe extern "C-unwind" fn tap_process(
    tap: NonNull<MTAudioProcessingTap>,
    requested_frames: CMItemCount,
    _flags: MTAudioProcessingTapFlags,
    buffers: NonNull<AudioBufferList>,
    frames_out: NonNull<CMItemCount>,
    flags_out: NonNull<MTAudioProcessingTapFlags>,
) {
    let state = unsafe { tap_state(tap) };
    let status = unsafe {
        tap.as_ref().source_audio(
            requested_frames,
            buffers,
            flags_out.as_ptr(),
            std::ptr::null_mut::<CMTimeRange>(),
            frames_out.as_ptr(),
        )
    };
    if status != 0 {
        state.failure.store(status, Ordering::Release);
        unsafe {
            frames_out.as_ptr().write(0);
        }
        return;
    }

    let frames = unsafe { frames_out.as_ptr().read() };
    if frames <= 0 {
        return;
    }
    let frames = match usize::try_from(frames) {
        Ok(frames) => frames,
        Err(_) => {
            state.failure.store(TAP_MALFORMED_BUFFER, Ordering::Release);
            return;
        }
    };
    if !state.format_valid.load(Ordering::Acquire) {
        unsafe {
            zero_buffers(buffers);
        }
        return;
    }
    if unsafe { process_buffers(buffers, frames, state) }.is_err() {
        state.failure.store(TAP_MALFORMED_BUFFER, Ordering::Release);
        unsafe {
            zero_buffers(buffers);
        }
    } else {
        state
            .frames_rendered
            .fetch_add(frames as u64, Ordering::Relaxed);
    }
}

unsafe fn tap_state(tap: NonNull<MTAudioProcessingTap>) -> &'static AudioTapState {
    let storage = unsafe { tap.as_ref().storage() };
    unsafe { storage.cast::<AudioTapState>().as_ref() }
}

unsafe fn process_buffers(
    mut list: NonNull<AudioBufferList>,
    frames: usize,
    state: &AudioTapState,
) -> std::result::Result<(), ()> {
    let buffers = unsafe { audio_buffers(list.as_mut())? };
    let expected_channels =
        usize::try_from(state.channels.load(Ordering::Acquire)).map_err(|_| ())?;
    let actual_channels = buffers.iter().try_fold(0_usize, |total, buffer| {
        usize::try_from(buffer.mNumberChannels)
            .ok()
            .and_then(|channels| total.checked_add(channels))
            .ok_or(())
    })?;
    if actual_channels == 0 || actual_channels != expected_channels {
        return Err(());
    }
    for buffer in buffers.iter() {
        let channels = usize::try_from(buffer.mNumberChannels).map_err(|_| ())?;
        let needed = frames
            .checked_mul(channels)
            .and_then(|samples| samples.checked_mul(std::mem::size_of::<f32>()))
            .ok_or(())?;
        if buffer.mData.is_null() || usize::try_from(buffer.mDataByteSize).map_err(|_| ())? < needed
        {
            return Err(());
        }
    }

    let gain = f32::from_bits(state.gain_bits.load(Ordering::Acquire));
    if state.mono.load(Ordering::Acquire) && actual_channels > 1 {
        for frame in 0..frames {
            let mut sum = 0.0_f32;
            for buffer in buffers.iter() {
                let channels = usize::try_from(buffer.mNumberChannels).map_err(|_| ())?;
                let samples = unsafe {
                    std::slice::from_raw_parts(
                        buffer.mData.cast::<f32>(),
                        frames.checked_mul(channels).ok_or(())?,
                    )
                };
                for channel in 0..channels {
                    sum += samples[frame * channels + channel];
                }
            }
            let mono = (sum / actual_channels as f32 * gain).clamp(-1.0, 1.0);
            for buffer in buffers.iter_mut() {
                let channels = usize::try_from(buffer.mNumberChannels).map_err(|_| ())?;
                let samples = unsafe {
                    std::slice::from_raw_parts_mut(
                        buffer.mData.cast::<f32>(),
                        frames.checked_mul(channels).ok_or(())?,
                    )
                };
                for channel in 0..channels {
                    samples[frame * channels + channel] = mono;
                }
            }
        }
    } else {
        for buffer in buffers.iter_mut() {
            let channels = usize::try_from(buffer.mNumberChannels).map_err(|_| ())?;
            let samples = unsafe {
                std::slice::from_raw_parts_mut(
                    buffer.mData.cast::<f32>(),
                    frames.checked_mul(channels).ok_or(())?,
                )
            };
            for sample in samples {
                *sample = (*sample * gain).clamp(-1.0, 1.0);
            }
        }
    }
    Ok(())
}

unsafe fn audio_buffers(list: &mut AudioBufferList) -> std::result::Result<&mut [AudioBuffer], ()> {
    let count = usize::try_from(list.mNumberBuffers).map_err(|_| ())?;
    if count == 0 || count > MAX_PROCESSING_BUFFERS {
        return Err(());
    }
    let first = std::ptr::addr_of_mut!(list.mBuffers).cast::<AudioBuffer>();
    Ok(unsafe { std::slice::from_raw_parts_mut(first, count) })
}

unsafe fn zero_buffers(mut list: NonNull<AudioBufferList>) {
    let Ok(buffers) = (unsafe { audio_buffers(list.as_mut()) }) else {
        return;
    };
    for buffer in buffers {
        if !buffer.mData.is_null() {
            unsafe {
                std::ptr::write_bytes(
                    buffer.mData.cast::<u8>(),
                    0,
                    usize::try_from(buffer.mDataByteSize).unwrap_or(0),
                );
            }
        }
    }
}

fn time_from_duration(duration: Duration) -> CMTime {
    let nanos = i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX);
    unsafe { CMTime::new(nanos, NANOSECONDS_PER_SECOND as i32) }
}

fn duration_from_time(time: CMTime) -> Option<Duration> {
    if !time.flags.contains(CMTimeFlags::Valid)
        || time.flags.intersects(CMTimeFlags::ImpliedValueFlagsMask)
        || time.timescale <= 0
        || time.value < 0
    {
        return None;
    }
    let nanos = i128::from(time.value).checked_mul(i128::from(NANOSECONDS_PER_SECOND))?
        / i128::from(time.timescale);
    u64::try_from(nanos).ok().map(Duration::from_nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_processor_matches_export_gain_and_mono_mix() {
        let mut samples = [0.5_f32, -0.25, 1.0, -1.0];
        let mut list = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: 2,
                mDataByteSize: u32::try_from(std::mem::size_of_val(&samples)).unwrap(),
                mData: samples.as_mut_ptr().cast(),
            }],
        };
        let state = AudioTapState::default();
        state.channels.store(2, Ordering::Relaxed);
        state.gain_bits.store(1.5_f32.to_bits(), Ordering::Relaxed);
        state.mono.store(true, Ordering::Relaxed);

        unsafe {
            process_buffers(NonNull::from(&mut list), 2, &state).unwrap();
        }

        assert_eq!(samples, [0.1875, 0.1875, 0.0, 0.0]);
    }

    #[test]
    fn audio_processor_clips_amplified_samples() {
        let mut samples = [0.75_f32, -0.75];
        let mut list = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: 1,
                mDataByteSize: u32::try_from(std::mem::size_of_val(&samples)).unwrap(),
                mData: samples.as_mut_ptr().cast(),
            }],
        };
        let state = AudioTapState::default();
        state.channels.store(1, Ordering::Relaxed);
        state.gain_bits.store(2.0_f32.to_bits(), Ordering::Relaxed);

        unsafe {
            process_buffers(NonNull::from(&mut list), 2, &state).unwrap();
        }

        assert_eq!(samples, [1.0, -1.0]);
    }

    #[test]
    fn audio_processor_rejects_padded_or_big_endian_pcm() {
        let packed = AudioStreamBasicDescription {
            mSampleRate: 48_000.0,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 8,
            mFramesPerPacket: 1,
            mBytesPerFrame: 8,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        assert!(processing_format_is_supported(&packed));
        assert!(!processing_format_is_supported(
            &AudioStreamBasicDescription {
                mBytesPerFrame: 16,
                ..packed
            }
        ));
        assert!(!processing_format_is_supported(
            &AudioStreamBasicDescription {
                mFormatFlags: packed.mFormatFlags | kAudioFormatFlagIsBigEndian,
                ..packed
            }
        ));
    }
}
