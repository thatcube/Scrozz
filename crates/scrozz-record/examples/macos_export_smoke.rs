//! Native AVFoundation export probe for an existing real recording.

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::{
        path::PathBuf,
        thread,
        time::{Duration, Instant},
    };

    use scrozz_record::{
        Recording,
        edit::{EditPlan, TrimRange, VideoDocument},
        media::NativeMediaSource,
        transcode::{NativeTranscoder, TranscodeEvent, Transcoder as _},
    };

    let mut args = std::env::args_os().skip(1);
    let source = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: macos_export_smoke SOURCE.mp4 OUTPUT.mp4")?;
    let output = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: macos_export_smoke SOURCE.mp4 OUTPUT.mp4")?;
    if args.next().is_some() {
        return Err("usage: macos_export_smoke SOURCE.mp4 OUTPUT.mp4".into());
    }

    let source_before = std::fs::metadata(&source)?;
    let recording = Recording::native(&source, 1.0, "native export smoke source")?;
    let document = VideoDocument::open_native(recording)?;
    println!(
        "source: {}x{} @ {:.2} fps, {} audio channel(s), {:.3} s",
        document.metadata().width,
        document.metadata().height,
        document.metadata().fps,
        document.metadata().audio_channels,
        document.duration().as_secs_f64()
    );
    let mut plan = EditPlan::video(&document)?;
    let trim_start = std::env::var("SCROZZ_EXPORT_SMOKE_START_SECONDS")
        .ok()
        .map(|value| value.parse::<f64>())
        .transpose()?
        .map(Duration::try_from_secs_f64)
        .transpose()?
        .unwrap_or(Duration::ZERO)
        .min(document.duration());
    let trim_end = std::env::var("SCROZZ_EXPORT_SMOKE_SECONDS")
        .ok()
        .map(|value| value.parse::<f64>())
        .transpose()?
        .map(Duration::try_from_secs_f64)
        .transpose()?
        .unwrap_or(document.duration())
        .min(document.duration());
    plan.trim = TrimRange::new(trim_start, trim_end, document.duration())?;
    if std::env::var("SCROZZ_EXPORT_SMOKE_MUTE").as_deref() == Ok("1") {
        plan.audio.mute = true;
    }
    if std::env::var("SCROZZ_EXPORT_SMOKE_TRACE_SAMPLES").as_deref() == Ok("1") {
        let media = NativeMediaSource::open(Recording::native(
            &source,
            document.duration().as_secs_f64(),
            "native export sample-order probe",
        )?)?;
        let mut decoder = media.decoder(plan.trim)?;
        for index in 0..160 {
            let Some(sample) = decoder.next_sample()? else {
                break;
            };
            let timestamp = sample.timestamp();
            let (kind, duration) = match sample {
                scrozz_record::media::DecodedMediaSample::Video(frame) => ("video", frame.duration),
                scrozz_record::media::DecodedMediaSample::Audio(chunk) => ("audio", chunk.duration),
            };
            println!(
                "sample {index:03}: {kind} {:.6} +{:.6}",
                timestamp.as_secs_f64(),
                duration.as_secs_f64()
            );
        }
        decoder.cancel();
        if std::env::var("SCROZZ_EXPORT_SMOKE_TRACE_ONLY").as_deref() == Ok("1") {
            return Ok(());
        }
    }
    let mut job = NativeTranscoder::new().start(&document, &plan, output)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last_progress = 0.0_f32;
    loop {
        if Instant::now() >= deadline {
            job.cancel()?;
            return Err("native export smoke timed out".into());
        }
        match job.poll() {
            Some(TranscodeEvent::Finished(output)) => {
                let source_after = std::fs::metadata(&source)?;
                if source_before.len() != source_after.len()
                    || source_before.modified()? != source_after.modified()?
                {
                    return Err("native export modified its source recording".into());
                }
                let source_media = NativeMediaSource::open(Recording::native(
                    &source,
                    document.duration().as_secs_f64(),
                    "native export smoke source",
                )?)?;
                let output_media = NativeMediaSource::open(Recording::native(
                    &output.path,
                    plan.trim.duration().as_secs_f64(),
                    "native export smoke output",
                )?)?;
                let source_frames = count_video_frames(&source_media, plan.trim)?;
                let output_frames = count_video_frames(
                    &output_media,
                    TrimRange::full(output_media.inspection().duration)?,
                )?;
                if source_frames != output_frames {
                    return Err(format!(
                        "export frame count {output_frames} differs from preview/source count {source_frames}"
                    )
                    .into());
                }
                if output_media.metadata().audio_channels
                    != plan.output_audio_channels(document.metadata())
                {
                    return Err("export audio presence differs from the edit preview".into());
                }
                println!(
                    "native export smoke passed: {source_frames} frames, {} bytes at {}",
                    output.bytes_written,
                    output.path.display()
                );
                return Ok(());
            }
            Some(TranscodeEvent::Failed(failure)) => return Err(failure.error.to_string().into()),
            Some(TranscodeEvent::Cancelled(_)) => {
                return Err("native export smoke was cancelled".into());
            }
            Some(TranscodeEvent::Progress(progress)) => {
                if progress - last_progress >= 0.1 {
                    println!("export progress: {:.0}%", progress * 100.0);
                    last_progress = progress;
                }
            }
            None => {
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn count_video_frames(
    source: &scrozz_record::media::NativeMediaSource,
    range: scrozz_record::edit::TrimRange,
) -> Result<u64, Box<dyn std::error::Error>> {
    use scrozz_record::media::DecodedMediaSample;

    let mut decoder = source.decoder(range)?;
    let mut frames = 0_u64;
    while let Some(sample) = decoder.next_sample()? {
        if matches!(sample, DecodedMediaSample::Video(_)) {
            frames = frames.saturating_add(1);
        }
    }
    Ok(frames)
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("native export smoke skipped; requires macOS");
}
