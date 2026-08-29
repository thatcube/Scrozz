//! Manual Windows desktop-session smoke test for the native recording engine.

#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::{path::PathBuf, thread, time::Duration};

    use scrozz_core::{CaptureTarget, DisplayId, WindowId};
    use scrozz_record::{Quality, RecordingRequest, RecordingResolution};

    let mut output = None;
    let mut display = Some(r"\\.\DISPLAY1".to_owned());
    let mut window = None;
    let mut seconds: f64 = 4.0;
    let mut system_audio = false;
    let mut microphone = false;
    let mut show_cursor = false;
    let mut quality = Quality::Balanced;
    let mut resolution = RecordingResolution::Native;
    let mut arguments = std::env::args().skip(1);

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--output" => output = Some(PathBuf::from(value(&mut arguments, "--output")?)),
            "--display" => {
                display = Some(value(&mut arguments, "--display")?);
                window = None;
            }
            "--window" => {
                window = Some(value(&mut arguments, "--window")?);
                display = None;
            }
            "--seconds" => {
                seconds = value(&mut arguments, "--seconds")?.parse()?;
            }
            "--system-audio" => system_audio = true,
            "--microphone" => microphone = true,
            "--cursor" => show_cursor = true,
            "--quality" => {
                quality = match value(&mut arguments, "--quality")?.as_str() {
                    "low" => Quality::Low,
                    "balanced" => Quality::Balanced,
                    "high" => Quality::High,
                    other => return Err(format!("unknown quality {other:?}").into()),
                };
            }
            "--resolution" => {
                resolution = parse_resolution(&value(&mut arguments, "--resolution")?)?;
            }
            "--max-height" => {
                resolution = RecordingResolution::MaxShortestEdge(
                    value(&mut arguments, "--max-height")?.parse()?,
                );
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => return Err(format!("unknown argument {other:?}; use --help").into()),
        }
    }

    let output = output.ok_or("--output is required")?;
    if !seconds.is_finite() || seconds < 1.0 {
        return Err("--seconds must be at least 1".into());
    }
    let target = if let Some(window) = window {
        CaptureTarget::Window(WindowId(window))
    } else {
        CaptureTarget::Display(DisplayId(display.expect("one target is selected")))
    };
    let mut request = RecordingRequest::new(target);
    request.destination = Some(output);
    request.microphone = microphone;
    request.system_audio = system_audio;
    request.show_cursor = show_cursor;
    request.quality = quality;
    request.resolution = resolution;

    let mut session = scrozz_record::start(&request)?;
    let active_half = Duration::from_secs_f64(seconds / 2.0);
    thread::sleep(active_half);
    session.pause()?;
    thread::sleep(Duration::from_millis(750));
    session.resume()?;
    thread::sleep(active_half);
    let recording = session.stop()?;

    println!("recorded: {}", recording.path.display());
    println!("active duration: {:.3}s", recording.duration_secs);
    if let Some(size) = recording.metadata.size {
        println!("encoded size: {:.0}x{:.0}", size.width, size.height);
    }
    if let Some(reason) = recording.partial_reason() {
        println!("partial recording retained: {reason}");
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn value(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    arguments
        .next()
        .ok_or_else(|| format!("{option} requires a value").into())
}

#[cfg(target_os = "windows")]
fn print_help() {
    println!(
        "Usage: windows_recording_smoke --output FILE [OPTIONS]\n\
         \n\
         Options:\n\
           --display ID       WGC display id (default: \\\\.\\DISPLAY1)\n\
           --window HWND      decimal HWND instead of a display\n\
           --seconds N        active recording seconds (default: 4)\n\
           --system-audio     include WASAPI loopback\n\
           --microphone       include the default microphone\n\
           --cursor           include the pointer\n\
           --quality LEVEL    low, balanced, or high\n\
           --resolution SPEC  native, points, 50%, 1080p, or 1920x1080\n\
           --max-height PX    legacy alias for --resolution Np\n\
         \n\
         The smoke test records, pauses for 750 ms, resumes, and finalises."
    );
}

#[cfg(target_os = "windows")]
fn parse_resolution(
    value: &str,
) -> Result<scrozz_record::RecordingResolution, Box<dyn std::error::Error>> {
    if value.eq_ignore_ascii_case("native") {
        return Ok(scrozz_record::RecordingResolution::Native);
    }
    if value.eq_ignore_ascii_case("points") {
        return Ok(scrozz_record::RecordingResolution::LogicalPoints);
    }
    if let Some(percent) = value.strip_suffix('%') {
        return Ok(scrozz_record::RecordingResolution::ScalePercent(
            percent.parse()?,
        ));
    }
    if let Some(shortest) = value.strip_suffix('p') {
        return Ok(scrozz_record::RecordingResolution::MaxShortestEdge(
            shortest.parse()?,
        ));
    }
    if let Some((width, height)) = value.split_once('x') {
        return Ok(scrozz_record::RecordingResolution::Exact {
            width: width.parse()?,
            height: height.parse()?,
        });
    }
    Err(format!("unknown resolution {value:?}").into())
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("windows_recording_smoke requires a native Windows desktop session");
    std::process::exit(2);
}
