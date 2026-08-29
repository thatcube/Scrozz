//! Real-media playback and trim/export parity.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use scrozz_record::playback::native_playback_capabilities;

#[test]
fn platform_playback_contract_never_advertises_a_partial_adapter() {
    let capabilities = native_playback_capabilities();
    assert!(!capabilities.backend.is_empty());
    assert_eq!(capabilities.decoded_video, capabilities.audio_output);
    assert_eq!(capabilities.decoded_video, capabilities.transport);
    assert_eq!(
        capabilities.decoded_video,
        capabilities.unavailable_reason.is_none()
    );
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        thread,
        time::{Duration, Instant},
    };

    use scrozz_record::{
        Recording,
        edit::{ChannelBehavior, EditPlan, TrimRange, VideoDocument},
        media::{DecodedMediaSample, NativeMediaSource},
        settings::ResolutionCap,
        transcode::{NativeTranscoder, TranscodeEvent, Transcoder as _},
    };

    static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(1);

    fn fixture_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("preview-av.mp4")
    }

    fn recording(path: impl Into<PathBuf>) -> Recording {
        Recording::native(path, 2.0, "deterministic A/V fixture").unwrap()
    }

    fn silent_fixture_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("preview-silent.mp4")
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let sequence = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "scrozz-playback-{}-{label}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn deterministic_fixture_decodes_video_and_audio_on_one_timeline() {
        let source = NativeMediaSource::open(recording(fixture_path())).unwrap();
        assert_eq!(
            (source.metadata().width, source.metadata().height),
            (96, 54)
        );
        assert_eq!(source.metadata().audio_channels, 2);
        assert!((source.metadata().fps - 30.0).abs() < 0.01);

        let mut decoder = source
            .decoder(TrimRange::full(source.inspection().duration).unwrap())
            .unwrap();
        let mut video = Vec::new();
        let mut audio = Vec::new();
        while let Some(sample) = decoder.next_sample().unwrap() {
            match sample {
                DecodedMediaSample::Video(frame) => {
                    video.push((frame.timestamp, frame.timestamp + frame.duration));
                }

                DecodedMediaSample::Audio(chunk) => {
                    assert_eq!(chunk.sample_rate, 48_000);
                    assert_eq!(chunk.channels, 2);
                    assert!(chunk.samples.iter().all(|sample| sample.is_finite()));
                    audio.push((chunk.timestamp, chunk.timestamp + chunk.duration));
                }
            }
        }

        assert!((58..=61).contains(&video.len()), "{:?}", video.len());
        assert!(!audio.is_empty());
        assert!(video[0].0.abs_diff(audio[0].0) <= Duration::from_millis(25));
        assert!(
            video.last().unwrap().1.abs_diff(audio.last().unwrap().1) <= Duration::from_millis(50)
        );
        for &(timestamp, _) in &video {
            let drift = audio
                .iter()
                .map(|&(start, end)| {
                    if timestamp < start {
                        start - timestamp
                    } else {
                        timestamp.saturating_sub(end)
                    }
                })
                .min()
                .unwrap();
            assert!(
                drift <= Duration::from_millis(25),
                "video timestamp {timestamp:?} is {drift:?} from decoded audio"
            );
        }
    }

    #[test]
    fn silent_fixture_is_explicitly_video_only() {
        let recording =
            Recording::native(silent_fixture_path(), 1.0, "deterministic silent fixture").unwrap();
        let source = NativeMediaSource::open(recording).unwrap();
        assert_eq!(source.metadata().audio_channels, 0);
        let mut decoder = source
            .decoder(TrimRange::full(source.inspection().duration).unwrap())
            .unwrap();
        let mut video_frames = 0;
        while let Some(sample) = decoder.next_sample().unwrap() {
            match sample {
                DecodedMediaSample::Video(_) => video_frames += 1,
                DecodedMediaSample::Audio(_) => panic!("silent fixture decoded an audio stream"),
            }
        }
        assert!((23..=25).contains(&video_frames));
    }

    #[test]
    fn seeked_decode_and_trimmed_export_share_boundaries_without_overwriting_source() {
        let scratch = Scratch::new("trim");
        let source_path = scratch.join("source.mp4");
        fs::copy(fixture_path(), &source_path).unwrap();
        let source_bytes = fs::read(&source_path).unwrap();
        let document = VideoDocument::open_native(recording(&source_path)).unwrap();
        let trim = TrimRange::new(
            Duration::from_millis(500),
            Duration::from_millis(1_500),
            document.duration(),
        )
        .unwrap();

        let source = NativeMediaSource::open(recording(&source_path)).unwrap();
        let mut decoder = source.decoder(trim).unwrap();
        let first_video = loop {
            match decoder.next_sample().unwrap() {
                Some(DecodedMediaSample::Video(frame)) => break frame,
                Some(DecodedMediaSample::Audio(_)) => {}
                None => panic!("trimmed decode returned no video"),
            }
        };
        assert!(first_video.timestamp >= trim.start);
        assert!(first_video.timestamp < trim.start + Duration::from_millis(50));

        let mut plan = EditPlan::video(&document).unwrap();
        plan.trim = trim;
        plan.resolution = ResolutionCap::Half;
        plan.audio.volume = 0.5;
        plan.audio.channels = ChannelBehavior::StereoToMono;
        let output_path = scratch.join("trimmed.mp4");
        let mut job = NativeTranscoder::new()
            .start(&document, &plan, output_path.clone())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let output = loop {
            assert!(Instant::now() < deadline, "native trim export timed out");
            match job.poll() {
                Some(TranscodeEvent::Finished(output)) => break output,
                Some(TranscodeEvent::Progress(_)) | None => {
                    thread::sleep(Duration::from_millis(5));
                }
                Some(other) => panic!("native trim export ended as {other:?}"),
            }
        };

        assert_eq!(fs::read(&source_path).unwrap(), source_bytes);
        assert_eq!(output.path, output_path);
        let trimmed = NativeMediaSource::open(recording(&output.path)).unwrap();
        assert_eq!(
            (trimmed.metadata().width, trimmed.metadata().height),
            plan.output_dimensions(document.metadata())
        );
        assert_eq!(trimmed.metadata().audio_channels, 1);
        assert!(
            trimmed.inspection().duration.abs_diff(trim.duration()) <= Duration::from_millis(50)
        );
    }

    #[test]
    fn corrupt_source_fails_without_output_or_orphaned_staging() {
        let scratch = Scratch::new("corrupt");
        let corrupt = scratch.join("corrupt.mp4");
        let bytes = fs::read(fixture_path()).unwrap();
        fs::write(&corrupt, &bytes[..128]).unwrap();
        let recording = recording(&corrupt);
        assert!(NativeMediaSource::open(recording.clone()).is_err());

        let synthetic_document = VideoDocument::open(
            recording,
            scrozz_record::edit::SourceMetadata {
                width: 96,
                height: 54,
                fps: 30.0,
                audio_channels: 2,
            },
        )
        .unwrap();
        let plan = EditPlan::video(&synthetic_document).unwrap();
        let output = scratch.join("must-not-exist.mp4");
        assert!(
            NativeTranscoder::new()
                .start(&synthetic_document, &plan, output.clone())
                .is_err()
        );
        assert!(!output.exists());
        assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 1);
        assert_eq!(fs::read(&corrupt).unwrap(), bytes[..128]);
    }
}
