//! Screenshot feedback sounds without coupling capture code to an OS API.

use std::path::PathBuf;
use std::sync::OnceLock;

use scrozz_core::{Error, Result};

#[cfg(target_os = "macos")]
thread_local! {
    static ACTIVE_MAC_SOUNDS: std::cell::RefCell<
        Vec<objc2::rc::Retained<objc2_app_kit::NSSound>>,
    > = const { std::cell::RefCell::new(Vec::new()) };
}

/// Sound played after a successful still-image capture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ScreenshotSound {
    /// A short, synthesized chiptune confirmation.
    #[default]
    EightBit,
    /// The default camera-shutter sound.
    Shutter,
    /// A quieter bundled/system alternative.
    SoftShutter,
    /// A brighter camera alternative.
    Camera,
    /// A user-selected audio file.
    Custom(PathBuf),
    /// Deliberately silent.
    Off,
}

/// Plays screenshot feedback asynchronously where the platform supports it.
///
/// # Errors
///
/// Returns an error when a requested custom file is missing or the platform
/// cannot start audio playback. Callers should keep the capture successful and
/// surface the audio problem separately.
pub fn play_screenshot_sound(sound: &ScreenshotSound) -> Result<()> {
    if *sound == ScreenshotSound::Off {
        return Ok(());
    }
    play_platform(sound)
}

#[cfg(target_os = "macos")]
fn play_platform(sound: &ScreenshotSound) -> Result<()> {
    use objc2::AnyThread;
    use objc2_app_kit::NSSound;
    use objc2_foundation::NSString;

    let _mtm = crate::macos::main_thread("playing the screenshot sound")?;
    let sound = match sound {
        ScreenshotSound::Custom(path) => {
            if !path.is_file() {
                return Err(Error::TargetGone(format!(
                    "custom screenshot sound {} is missing",
                    path.display()
                )));
            }
            let path = NSString::from_str(&path.to_string_lossy());
            NSSound::initWithContentsOfFile_byReference(NSSound::alloc(), &path, true)
        }
        ScreenshotSound::EightBit => {
            use objc2_foundation::NSData;

            let data = NSData::with_bytes(eight_bit_wav());
            NSSound::initWithData(NSSound::alloc(), &data)
        }
        ScreenshotSound::Shutter => named_sound(&["Grab", "Tink"]),
        ScreenshotSound::SoftShutter => named_sound(&["Pop", "Tink"]),
        ScreenshotSound::Camera => named_sound(&["Glass", "Tink"]),
        ScreenshotSound::Off => return Ok(()),
    }
    .ok_or_else(|| Error::Platform("macOS could not load the screenshot sound".to_owned()))?;

    if !sound.play() {
        return Err(Error::Platform(
            "macOS refused to start the screenshot sound".to_owned(),
        ));
    }
    ACTIVE_MAC_SOUNDS.with(|active| {
        let mut active = active.borrow_mut();
        active.retain(|sound| sound.isPlaying());
        active.push(sound);
    });
    Ok(())
}

#[cfg(target_os = "macos")]
fn named_sound(names: &[&str]) -> Option<objc2::rc::Retained<objc2_app_kit::NSSound>> {
    use objc2_app_kit::NSSound;
    use objc2_foundation::NSString;

    names
        .iter()
        .find_map(|name| NSSound::soundNamed(&NSString::from_str(name)))
}

#[cfg(target_os = "windows")]
fn play_platform(sound: &ScreenshotSound) -> Result<()> {
    use windows::Win32::Media::Audio::{
        PlaySoundW, SND_ALIAS, SND_ASYNC, SND_FILENAME, SND_MEMORY, SND_NODEFAULT,
    };
    use windows::core::{HSTRING, PCWSTR};

    if *sound == ScreenshotSound::EightBit {
        let wav = eight_bit_wav();
        // SAFETY: `wav` lives in a process-wide `OnceLock`, so the asynchronous
        // player retains a valid PCM buffer until playback completes.
        return if unsafe {
            PlaySoundW(
                PCWSTR(wav.as_ptr().cast()),
                None,
                SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
            )
        }
        .as_bool()
        {
            Ok(())
        } else {
            Err(Error::Platform(
                "Windows refused to start the 8-bit screenshot sound".to_owned(),
            ))
        };
    }

    let (name, flags) = match sound {
        ScreenshotSound::EightBit => unreachable!("handled above"),
        ScreenshotSound::Custom(path) => {
            if !path.is_file() {
                return Err(Error::TargetGone(format!(
                    "custom screenshot sound {} is missing",
                    path.display()
                )));
            }
            (HSTRING::from(path.as_os_str()), SND_FILENAME | SND_ASYNC)
        }
        ScreenshotSound::Shutter => (
            HSTRING::from("SystemAsterisk"),
            SND_ALIAS | SND_ASYNC | SND_NODEFAULT,
        ),
        ScreenshotSound::SoftShutter => (
            HSTRING::from("SystemNotification"),
            SND_ALIAS | SND_ASYNC | SND_NODEFAULT,
        ),
        ScreenshotSound::Camera => (
            HSTRING::from("SystemExclamation"),
            SND_ALIAS | SND_ASYNC | SND_NODEFAULT,
        ),
        ScreenshotSound::Off => return Ok(()),
    };

    // SAFETY: `name` owns a terminated UTF-16 buffer for the duration of the
    // call, and no module handle is needed for filename or system-alias sounds.
    if unsafe { PlaySoundW(&name, None, flags) }.as_bool() {
        Ok(())
    } else {
        Err(Error::Platform(
            "Windows refused to start the screenshot sound".to_owned(),
        ))
    }
}

#[cfg(target_os = "linux")]
fn play_platform(sound: &ScreenshotSound) -> Result<()> {
    use std::process::{Command, Stdio};

    let mut command = Command::new("canberra-gtk-play");
    match sound {
        ScreenshotSound::EightBit => {
            let path = std::env::temp_dir().join("scrozz-capture-8bit-v1.wav");
            if !path.is_file() {
                std::fs::write(&path, eight_bit_wav()).map_err(|error| {
                    Error::Platform(format!(
                        "could not prepare the 8-bit screenshot sound: {error}"
                    ))
                })?;
            }
            command.arg("--file").arg(path);
        }
        ScreenshotSound::Custom(path) => {
            if !path.is_file() {
                return Err(Error::TargetGone(format!(
                    "custom screenshot sound {} is missing",
                    path.display()
                )));
            }
            command.arg("--file").arg(path);
        }
        ScreenshotSound::Shutter => {
            command.args(["--id", "camera-shutter"]);
        }
        ScreenshotSound::SoftShutter => {
            command.args(["--id", "button-pressed"]);
        }
        ScreenshotSound::Camera => {
            command.args(["--id", "camera-shutter"]);
        }
        ScreenshotSound::Off => return Ok(()),
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            Error::Platform(format!(
                "could not start the desktop sound player `canberra-gtk-play`: {error}"
            ))
        })?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn play_platform(_sound: &ScreenshotSound) -> Result<()> {
    Err(Error::Unsupported {
        what: "screenshot sound".to_owned(),
        why: "this platform has no sound adapter".to_owned(),
    })
}

fn eight_bit_wav() -> &'static [u8] {
    static WAV: OnceLock<Vec<u8>> = OnceLock::new();
    WAV.get_or_init(build_eight_bit_wav)
}

fn build_eight_bit_wav() -> Vec<u8> {
    const SAMPLE_RATE: u32 = 16_000;
    const NOTES: [(u32, u32); 3] = [(740, 240), (1_110, 288), (1_480, 352)];
    const GAP: usize = 48;

    let sample_count = NOTES
        .iter()
        .map(|(_, length)| *length as usize)
        .sum::<usize>()
        + GAP * (NOTES.len() - 1);
    let mut samples = Vec::with_capacity(sample_count);
    for (note_index, (frequency, length)) in NOTES.into_iter().enumerate() {
        let length = length as usize;
        for index in 0..length {
            let attack = index.min(24) as f32 / 24.0;
            let decay = (length - index) as f32 / length as f32;
            let amplitude = (24.0 * attack.min(1.0) * decay) as u8;
            let high = ((index as u32 * frequency * 2) / SAMPLE_RATE).is_multiple_of(2);
            samples.push(if high {
                128 + amplitude
            } else {
                128 - amplitude
            });
        }
        if note_index + 1 < NOTES.len() {
            samples.extend(std::iter::repeat_n(128, GAP));
        }
    }

    let data_len = samples.len() as u32;
    let mut wav = Vec::with_capacity(44 + samples.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&8_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&samples);
    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_bit_sound_is_a_short_mono_pcm_wave() {
        let wav = eight_bit_wav();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert!(wav.len() > 44);
        assert!(wav.len() < 16_000);
    }
}
