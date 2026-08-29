//! Main-thread AVFoundation recording-preview smoke test.

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::{path::Path, time::Duration};

    use scrozz_record::{
        Recording,
        edit::{ChannelBehavior, EditPlan, TrimRange, VideoDocument},
        playback::{NativePlayback, PlaybackAudio, PlaybackPhase},
    };

    if std::env::var("SCROZZ_PLAYBACK_SMOKE").as_deref() != Ok("1") {
        println!("playback smoke skipped; set SCROZZ_PLAYBACK_SMOKE=1");
        return Ok(());
    }

    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("preview-av.mp4");
    let recording = Recording::native(&source, 2.0, "native playback smoke fixture")?;
    let document = VideoDocument::open_native(recording)?;
    let mut plan = EditPlan::video(&document)?;
    plan.trim = TrimRange::new(
        Duration::from_millis(200),
        Duration::from_millis(1_200),
        document.duration(),
    )?;
    plan.audio.volume = 1.25;
    plan.audio.channels = ChannelBehavior::StereoToMono;

    let mut playback = NativePlayback::open(&document, plan)?;
    playback.seek(plan.trim.start)?;
    wait_for(&mut playback, Duration::from_secs(5), |playback| {
        playback.frame.is_some()
    })?;

    playback.play()?;
    wait_for(&mut playback, Duration::from_secs(5), |playback| {
        playback.position >= Duration::from_millis(450)
    })?;
    playback.set_rate(1.5)?;
    wait_for(&mut playback, Duration::from_secs(5), |playback| {
        playback.position >= Duration::from_millis(700)
    })?;
    playback.pause();
    let paused = playback.poll()?.clone();
    let held = paused.position;
    pump_run_loop(Duration::from_millis(80));
    let after_pause = playback.poll()?.clone();
    if after_pause.position.abs_diff(held) > Duration::from_millis(20) {
        return Err(format!(
            "paused native clock moved from {held:?} to {:?}",
            after_pause.position
        )
        .into());
    }

    playback.seek(Duration::from_millis(300))?;
    let pinned_seek = playback.poll()?.clone();
    if pinned_seek.position != Duration::from_millis(300) {
        return Err(format!(
            "asynchronous backward seek exposed {:?} instead of its 300ms target",
            pinned_seek.position
        )
        .into());
    }
    wait_for(&mut playback, Duration::from_secs(5), |playback| {
        playback
            .frame
            .as_ref()
            .is_some_and(|frame| frame.frame.timestamp <= Duration::from_millis(300))
    })?;
    playback.play()?;
    wait_for(&mut playback, Duration::from_secs(5), |playback| {
        playback.position >= Duration::from_millis(500)
    })?;
    playback.pause();

    playback.seek(Duration::from_millis(800))?;
    playback.play()?;
    let ended = wait_for(&mut playback, Duration::from_secs(5), |playback| {
        playback.phase == PlaybackPhase::Ended
    })?;
    if ended.position != plan.trim.end {
        return Err(format!(
            "trim playback ended at {:?}, expected {:?}",
            ended.position, plan.trim.end
        )
        .into());
    }
    if ended
        .av_drift
        .is_some_and(|drift| drift > Duration::from_millis(50))
    {
        return Err(format!(
            "native playback A/V drift exceeded one frame at {:?}: {:?}",
            ended.position, ended.av_drift
        )
        .into());
    }
    if ended.audio_frames_rendered == 0 {
        return Err("native playback clock advanced without rendering captured audio".into());
    }
    playback.play()?;
    let replay = playback.poll()?.clone();
    if replay.position != plan.trim.start {
        return Err(format!(
            "replay-after-end exposed {:?} instead of trim-in {:?}",
            replay.position, plan.trim.start
        )
        .into());
    }
    wait_for(&mut playback, Duration::from_secs(5), |playback| {
        playback.position >= Duration::from_millis(350)
    })?;
    playback.shutdown()?;
    playback.shutdown()?;

    let silent_source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("preview-silent.mp4");
    let silent = Recording::native(silent_source, 1.0, "native silent playback fixture")?;
    let silent_document = VideoDocument::open_native(silent)?;
    let silent_plan = EditPlan::video(&silent_document)?;
    let mut silent_playback = NativePlayback::open(&silent_document, silent_plan)?;
    silent_playback.play()?;
    let silent_snapshot = wait_for(&mut silent_playback, Duration::from_secs(5), |playback| {
        playback.position >= Duration::from_millis(200)
    })?;
    if silent_snapshot.audio != PlaybackAudio::NoTrack || silent_snapshot.audio_frames_rendered != 0
    {
        return Err(format!(
            "silent playback did not remain explicitly video-only: {silent_snapshot:?}"
        )
        .into());
    }
    silent_playback.shutdown()?;
    println!(
        "native playback smoke passed: audio, silent media, seek, pause, 1.5x rate, trim end, decoded frames"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn wait_for(
    playback: &mut scrozz_record::playback::NativePlayback,
    timeout: std::time::Duration,
    ready: impl Fn(&scrozz_record::playback::PlaybackSnapshot) -> bool,
) -> Result<scrozz_record::playback::PlaybackSnapshot, Box<dyn std::error::Error>> {
    use std::time::{Duration, Instant};

    use scrozz_record::playback::PlaybackPhase;

    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = playback.poll()?.clone();
        if snapshot.buffered_frames > scrozz_record::playback::MAX_BUFFERED_VIDEO_FRAMES {
            return Err(format!(
                "native preview retained {} frames, above its {}-frame bound",
                snapshot.buffered_frames,
                scrozz_record::playback::MAX_BUFFERED_VIDEO_FRAMES
            )
            .into());
        }
        if snapshot.phase == PlaybackPhase::Playing
            && snapshot
                .av_drift
                .is_some_and(|drift| drift > Duration::from_millis(50))
        {
            return Err(format!(
                "native playback drifted {:?} at {:?}",
                snapshot.av_drift, snapshot.position
            )
            .into());
        }
        if ready(&snapshot) {
            return Ok(snapshot);
        }
        if snapshot.phase == PlaybackPhase::Failed {
            return Err(snapshot
                .error
                .unwrap_or_else(|| "native playback failed".to_owned())
                .into());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "native playback smoke timed out: phase={:?}, position={:?}, frame={:?}, audio_frames={}, error={:?}",
                snapshot.phase,
                snapshot.position,
                snapshot
                    .frame
                    .as_ref()
                    .map(|frame| frame.frame.timestamp),
                snapshot.audio_frames_rendered,
                snapshot.error
            )
            .into());
        }
        pump_run_loop(Duration::from_millis(10));
    }
}

#[cfg(target_os = "macos")]
fn pump_run_loop(duration: std::time::Duration) {
    use objc2_foundation::{NSDate, NSRunLoop};

    let run_loop = NSRunLoop::currentRunLoop();
    let until = NSDate::dateWithTimeIntervalSinceNow(duration.as_secs_f64());
    run_loop.runUntilDate(&until);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("playback smoke skipped; requires macOS");
}
