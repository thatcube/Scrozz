//! Screenshot feedback sounds without coupling capture code to an OS API.

use std::path::PathBuf;

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
    /// The default camera-shutter sound.
    #[default]
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
        PlaySoundW, SND_ALIAS, SND_ASYNC, SND_FILENAME, SND_NODEFAULT,
    };
    use windows::core::HSTRING;

    let (name, flags) = match sound {
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
