//! Cross-host tests for Windows recording policy.

pub use scrozz_record::{
    EngineCapabilities, Quality, RecordingRequest, RecordingResolution, VideoCodec,
};

use scrozz_core::{CaptureTarget, DisplayId};

#[path = "../src/windows/plan.rs"]
mod plan;
#[path = "../src/windows/timing.rs"]
mod timing;

#[path = "../src/windows/mix.rs"]
mod mix;

#[path = "../src/windows/salvage.rs"]
mod salvage;

use mix::{Mixer, Packet, Source, downmix_stereo, f32_to_i16, resample_linear};
use plan::{PlanError, build, output_dimensions};
use salvage::{Outcome, classify, inspect};
use timing::{
    Backpressure, FramePacer, HNS_PER_SECOND, Timeline, audio_drain_limit, backpressure, qpc_to_hns,
};

fn windows_caps() -> EngineCapabilities {
    EngineCapabilities {
        video: true,
        system_audio: true,
        microphone: true,
        pause_resume: true,
        display: true,
        window: true,
        region: true,
        cursor: true,
        mp4: true,
        h264: true,
        quality: true,
        resolution: true,
        ..EngineCapabilities::default()
    }
}

#[test]
fn capability_matrix_matches_windows_contract() {
    let caps = windows_caps();
    let mut request =
        RecordingRequest::new(CaptureTarget::Display(DisplayId(r"\\.\DISPLAY1".into())));
    request.show_cursor = true;
    request.system_audio = true;
    request.microphone = true;
    assert!(scrozz_record::validate_capabilities(caps, &request, None).is_ok());

    let mut hevc = request.clone();
    hevc.video_codec = VideoCodec::Hevc;
    assert!(matches!(
        scrozz_record::validate_capabilities(caps, &hevc, None),
        Err(scrozz_core::Error::Unsupported { what, .. }) if what == "HEVC encoding"
    ));

    let mut av1 = request.clone();
    av1.video_codec = VideoCodec::Av1;
    assert!(matches!(
        scrozz_record::validate_capabilities(caps, &av1, None),
        Err(scrozz_core::Error::Unsupported { what, .. }) if what == "AV1 encoding"
    ));

    let all_displays = RecordingRequest::new(CaptureTarget::AllDisplays);
    assert!(matches!(
        scrozz_record::validate_capabilities(caps, &all_displays, None),
        Err(scrozz_core::Error::Unsupported { what, .. }) if what == "all-display recording"
    ));
}

#[cfg(target_os = "windows")]
#[test]
fn native_engine_advertises_exact_windows_capabilities() {
    let engine = scrozz_record::detect_native_engine().expect("Windows engine");
    assert_eq!(engine.capabilities(), windows_caps());
}

#[test]
fn encoder_plan_caps_height_preserves_aspect_and_rounds_even() {
    assert_eq!(
        output_dimensions(3840, 2160, 1.0, RecordingResolution::MaxShortestEdge(1080)),
        (1920, 1080)
    );
    assert_eq!(
        output_dimensions(1512, 945, 1.5, RecordingResolution::LogicalPoints),
        (1008, 630)
    );
    assert_eq!(
        output_dimensions(
            1920,
            1080,
            1.0,
            RecordingResolution::Exact {
                width: 1281,
                height: 721,
            }
        ),
        (1280, 720)
    );
    assert_eq!(
        output_dimensions(101, 99, 1.0, RecordingResolution::ScalePercent(1)),
        (2, 2)
    );
    assert_eq!(
        output_dimensions(1, 1, 1.0, RecordingResolution::Native),
        (0, 0)
    );
}

#[test]
fn quality_and_frame_rate_change_the_encoder_plan() {
    let low = build(
        1920,
        1080,
        1.0,
        30,
        Quality::Low,
        RecordingResolution::Native,
    )
    .unwrap();
    let high = build(
        1920,
        1080,
        1.0,
        30,
        Quality::High,
        RecordingResolution::Native,
    )
    .unwrap();
    let sixty = build(
        1920,
        1080,
        1.0,
        60,
        Quality::Balanced,
        RecordingResolution::Native,
    )
    .unwrap();
    let thirty = build(
        1920,
        1080,
        1.0,
        30,
        Quality::Balanced,
        RecordingResolution::Native,
    )
    .unwrap();

    assert!(high.bitrate > low.bitrate * 3);
    assert_eq!(sixty.bitrate, thirty.bitrate * 2);
    assert_eq!(sixty.gop, 120);
    let high_4k60 = build(
        3840,
        2160,
        1.0,
        60,
        Quality::High,
        RecordingResolution::Native,
    )
    .unwrap();
    assert_eq!(
        u64::from(high_4k60.bitrate),
        Quality::High.target_bitrate(3840, 2160, 60)
    );
    let minimum = build(2, 2, 1.0, 1, Quality::Low, RecordingResolution::Native).unwrap();
    assert_eq!(u64::from(minimum.bitrate), 64_000);
    assert_eq!(
        u64::from(minimum.bitrate),
        Quality::Low.target_bitrate(2, 2, 1)
    );
    assert_eq!(
        build(
            1920,
            1080,
            1.0,
            0,
            Quality::Balanced,
            RecordingResolution::Native
        ),
        Err(PlanError::InvalidFrameRate(0))
    );
    assert_eq!(
        build(
            1920,
            1080,
            0.0,
            30,
            Quality::Balanced,
            RecordingResolution::LogicalPoints
        ),
        Err(PlanError::InvalidScale(0.0))
    );
    assert_eq!(
        build(
            1,
            1,
            1.0,
            30,
            Quality::Balanced,
            RecordingResolution::Native
        ),
        Err(PlanError::ResolutionTooSmall {
            width: 0,
            height: 0,
        })
    );
}

#[test]
fn qpc_conversion_uses_100ns_units_without_overflow() {
    assert_eq!(qpc_to_hns(25_000_000, 10_000_000), Some(25_000_000));
    assert_eq!(qpc_to_hns(i64::MAX, i64::MAX), Some(HNS_PER_SECOND));
    assert_eq!(qpc_to_hns(1, 0), None);
}

#[test]
fn pause_time_is_removed_from_the_media_timeline() {
    let mut timeline = Timeline::default();
    timeline.start(10 * HNS_PER_SECOND);
    assert_eq!(timeline.map(11 * HNS_PER_SECOND), Some(HNS_PER_SECOND));
    timeline.pause(12 * HNS_PER_SECOND);
    assert_eq!(timeline.map(13 * HNS_PER_SECOND), None);
    timeline.resume(15 * HNS_PER_SECOND);
    assert_eq!(timeline.map(16 * HNS_PER_SECOND), Some(3 * HNS_PER_SECOND));
}

#[test]
fn independent_stream_timestamps_do_not_clamp_each_other() {
    let mut timeline = Timeline::default();
    timeline.start(10 * HNS_PER_SECOND);
    assert_eq!(timeline.map(12 * HNS_PER_SECOND), Some(2 * HNS_PER_SECOND));
    assert_eq!(timeline.map(11 * HNS_PER_SECOND), Some(HNS_PER_SECOND));
    assert_eq!(timeline.duration_hns(), 2 * HNS_PER_SECOND);
}

#[test]
fn frame_pacing_and_queue_backpressure_drop_newest_work() {
    let mut pacer = FramePacer::new(30);
    assert!(pacer.accept(0));
    assert!(!pacer.accept(100_000));
    assert!(pacer.accept(HNS_PER_SECOND / 30));
    assert_eq!(backpressure(2, 3), Backpressure::Enqueue);
    assert_eq!(backpressure(3, 3), Backpressure::DropNewest);
    assert_eq!(backpressure(0, 0), Backpressure::DropNewest);
}

#[test]
fn audio_watermark_waits_for_wasapi_but_finalisation_fills_the_tail() {
    let frame_end = 2 * HNS_PER_SECOND;
    let latest_video = frame_end - HNS_PER_SECOND / 30;
    let settle = HNS_PER_SECOND / 10;

    assert_eq!(
        audio_drain_limit(frame_end, latest_video, settle, false),
        latest_video - settle
    );
    assert_eq!(
        audio_drain_limit(frame_end, latest_video, settle, true),
        frame_end
    );
}

#[test]
fn mono_and_multichannel_audio_downmix_safely() {
    assert_eq!(
        downmix_stereo(&[0.25, -0.5], 1, 0x0004),
        vec![[0.25, 0.25], [-0.5, -0.5]]
    );
    let surround = downmix_stereo(&[0.2, -0.2, 0.1, 0.1, 0.2, -0.2], 6, 0x003f);
    assert_eq!(surround.len(), 1);
    assert!(surround[0][0] > 0.2);
    assert!(surround[0][1] < -0.1);

    let quad = downmix_stereo(&[0.0, 0.0, 1.0, 0.0], 4, 0x0033);
    assert_eq!(quad, vec![[1.0, 0.0]]);
}

#[test]
fn fractional_linear_resampling_interpolates() {
    let input = [[0.0, 0.0], [1.0, 1.0]];
    let output = resample_linear(&input, 2, 4);
    assert_eq!(output.len(), 4);
    assert_eq!(output[0], [0.0, 0.0]);
    assert_eq!(output[1], [0.5, 0.5]);
    assert_eq!(output[2], [1.0, 1.0]);
    assert_eq!(output[3], [1.0, 1.0]);
}

#[test]
fn mixer_aligns_qpc_packets_and_fills_loopback_silence() {
    let mut mixer = Mixer::new(1_000, 10, true, true);
    mixer.ingest(
        Source::Microphone,
        Packet {
            stream_hns: 10 * HNS_PER_SECOND / 1_000,
            sample_rate: 1_000,
            channels: 1,
            channel_mask: 0x0004,
            samples: vec![1.0; 10],
        },
    );

    let chunks = mixer.drain_through(30 * HNS_PER_SECOND / 1_000, false);
    assert_eq!(chunks.len(), 3);
    assert!(chunks[0].pcm.iter().all(|byte| *byte == 0));
    assert!(chunks[1].pcm.iter().any(|byte| *byte != 0));
    assert!(chunks[2].pcm.iter().all(|byte| *byte == 0));
}

#[test]
fn pcm_conversion_clamps_and_rejects_nan() {
    assert_eq!(f32_to_i16(2.0), i16::MAX);
    assert_eq!(f32_to_i16(-2.0), -i16::MAX);
    assert_eq!(f32_to_i16(f32::NAN), 0);
}

#[test]
fn fragmented_output_is_verified_and_incomplete_tail_is_trimmed() {
    fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(payload.len() + 8);
        bytes.extend_from_slice(&u32::try_from(payload.len() + 8).unwrap().to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(payload);
        bytes
    }

    let mut bytes = mp4_box(b"ftyp", b"isom");
    bytes.extend(mp4_box(b"moov", b"init"));
    bytes.extend(mp4_box(b"moof", b"fragment"));
    bytes.extend(mp4_box(b"mdat", b"encoded media"));
    let complete_bytes = bytes.len() as u64;
    bytes.extend_from_slice(&64u32.to_be_bytes());
    bytes.extend_from_slice(b"moof");
    bytes.extend_from_slice(b"incomplete");
    let inspection = inspect(&mut std::io::Cursor::new(bytes)).unwrap();

    assert!(inspection.playable());
    assert_eq!(inspection.complete_fragments, 1);
    assert_eq!(inspection.nonempty_fragments, 1);
    assert_eq!(inspection.truncate_to, complete_bytes);
    assert_eq!(classify(None, complete_bytes, 10, None), Outcome::Complete);
    assert!(matches!(
        classify(
            Some("device removed"),
            inspection.file_bytes,
            10,
            Some(inspection)
        ),
        Outcome::Salvaged(_)
    ));
    assert!(matches!(
        classify(Some("device removed"), 0, 0, None),
        Outcome::Unusable(_)
    ));
}

#[test]
fn empty_mdat_does_not_count_as_playable_media() {
    fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(payload.len() + 8);
        bytes.extend_from_slice(&u32::try_from(payload.len() + 8).unwrap().to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(payload);
        bytes
    }

    let mut bytes = mp4_box(b"ftyp", b"isom");
    bytes.extend(mp4_box(b"moov", b"init"));
    bytes.extend(mp4_box(b"moof", b"fragment"));
    bytes.extend_from_slice(&8u32.to_be_bytes());
    bytes.extend_from_slice(b"mdat");
    let inspection = inspect(&mut std::io::Cursor::new(bytes)).unwrap();

    assert_eq!(inspection.complete_fragments, 1);
    assert_eq!(inspection.nonempty_fragments, 0);
    assert!(!inspection.playable());
    assert!(matches!(
        classify(
            Some("device removed"),
            inspection.file_bytes,
            1,
            Some(inspection)
        ),
        Outcome::Unusable(_)
    ));
}
