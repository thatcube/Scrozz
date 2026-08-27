//! Native Linux recording smoke harness used by CI and real-compositor checks.

#[cfg(all(target_os = "linux", feature = "linux-native"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::{Duration, Instant};

    use scrozz_core::CaptureTarget;
    use scrozz_record::{Quality, RecordingRequest, RecordingResolution, SessionEvent, VideoCodec};

    let output = std::env::args_os().nth(1).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage: linux_record_smoke OUTPUT.mp4",
        )
    })?;
    let duration = env_f64("SCROZZ_SMOKE_DURATION", 2.0)?;
    let fps = env_u32("SCROZZ_SMOKE_FPS", 5)?;
    let scale = env_u16("SCROZZ_SMOKE_SCALE", 25)?;
    let codec = match std::env::var("SCROZZ_SMOKE_CODEC")
        .unwrap_or_else(|_| "av1".into())
        .as_str()
    {
        "auto" => VideoCodec::Auto,
        "h264" => VideoCodec::H264,
        "av1" => VideoCodec::Av1,
        other => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("unsupported SCROZZ_SMOKE_CODEC {other:?}"),
            )
            .into());
        }
    };

    let mut request = RecordingRequest::new(CaptureTarget::AllDisplays).with_destination(output);
    request.fps = fps;
    request.quality = Quality::Balanced;
    request.resolution = RecordingResolution::ScalePercent(scale);
    request.video_codec = codec;
    request.show_cursor = true;

    let mut session = scrozz_record::start(&request)?;
    let started = Instant::now();
    let mut first_frame = false;
    let mut paused = false;
    let mut terminal = None;
    while started.elapsed().as_secs_f64() < duration {
        if let Some(event) = session.poll() {
            match event {
                SessionEvent::FirstFrame => first_frame = true,
                SessionEvent::Warning(message) => eprintln!("warning={message}"),
                SessionEvent::Finished(recording) => {
                    terminal = Some(recording);
                    break;
                }
                SessionEvent::Failed(error) => {
                    return Err(std::io::Error::other(error.to_string()).into());
                }
            }
        }
        if !paused
            && std::env::var_os("SCROZZ_SMOKE_PAUSE").is_some()
            && started.elapsed().as_secs_f64() >= duration / 3.0
        {
            session.pause()?;
            let paused_at = session.engine_elapsed_secs().unwrap_or_default();
            std::thread::sleep(Duration::from_millis(200));
            let after = session.engine_elapsed_secs().unwrap_or_default();
            if (after - paused_at).abs() > 0.05 {
                return Err(std::io::Error::other(
                    "the Linux media timeline advanced while paused",
                )
                .into());
            }
            session.resume()?;
            paused = true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !first_frame {
        return Err(std::io::Error::other(
            "the Linux recording session emitted no FirstFrame event",
        )
        .into());
    }

    let recording = match terminal {
        Some(polled) => {
            let stopped = session.stop()?;
            if stopped != polled {
                return Err(
                    std::io::Error::other("stop disagreed with the terminal poll result").into(),
                );
            }
            stopped
        }
        None => session.stop()?,
    };
    print_report(&recording);
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "linux-native"))]
fn print_report(recording: &scrozz_record::Recording) {
    let (completion, salvageability) = match recording.completion {
        scrozz_record::RecordingCompletion::Complete => ("complete", "playable"),
        scrozz_record::RecordingCompletion::Partial { salvageability, .. } => (
            "partial",
            match salvageability {
                scrozz_record::Salvageability::InitialisationOnly => "initialisation-only",
                scrozz_record::Salvageability::Playable => "playable",
            },
        ),
    };
    println!("completion={completion}");
    println!("salvageability={salvageability}");
    println!("duration_secs={:.6}", recording.duration_secs);
    println!("path={}", recording.path.display());
}

#[cfg(all(target_os = "linux", feature = "linux-native"))]
fn env_u32(name: &str, default: u32) -> Result<u32, std::io::Error> {
    std::env::var(name).map_or(Ok(default), |value| {
        value.parse().map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{name} is not a u32: {error}"),
            )
        })
    })
}

#[cfg(all(target_os = "linux", feature = "linux-native"))]
fn env_u16(name: &str, default: u16) -> Result<u16, std::io::Error> {
    std::env::var(name).map_or(Ok(default), |value| {
        value.parse().map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{name} is not a u16: {error}"),
            )
        })
    })
}

#[cfg(all(target_os = "linux", feature = "linux-native"))]
fn env_f64(name: &str, default: f64) -> Result<f64, std::io::Error> {
    std::env::var(name).map_or(Ok(default), |value| {
        value.parse().map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{name} is not an f64: {error}"),
            )
        })
    })
}

#[cfg(not(all(target_os = "linux", feature = "linux-native")))]
fn main() {
    eprintln!("linux_record_smoke requires Linux and --features linux-native");
}
