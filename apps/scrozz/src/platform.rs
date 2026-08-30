//! The seam between the CLI and the platform crates.
//!
//! # Why this file exists
//!
//! Every backend below this line is under construction. Several entry points are
//! literally `todo!()`, which is not an error value — it is a panic, and a panic
//! reaches the user as exit code 101 and a backtrace. For a tool whose entire
//! error philosophy (D8, D15) is *never show the user a crash for a thing we
//! already know about*, that is the worst possible failure mode.
//!
//! So every call into an unfinished backend goes through this module, and this
//! module refuses to make the call unless it has been told to. The default is a
//! clean [`CliError::NotImplemented`], exit code 12, with a message naming the
//! crate that owes the work.
//!
//! Setting `SCROZZ_UNSTABLE_BACKENDS=1` lifts the guard. That exists so the
//! wiring can be exercised the moment a backend lands, without editing code.
//!
//! # Why the calls are written out at all
//!
//! It would be simpler to return `NotImplemented` and never name the backend
//! functions. It would also be worthless: the point of building the CLI first is
//! to check that the traits below it are actually usable. Naming the real
//! signatures means the compiler tells us the day a signature drifts, rather
//! than the integration doing it months later.

use scrozz_core::{Capture, CaptureBackend, CaptureRequest, ScrollDriver, TargetEnumerator};

use crate::fault::{CliError, CliResult};

/// Lifts the guard on unfinished backends.
pub const UNSTABLE_ENV: &str = "SCROZZ_UNSTABLE_BACKENDS";

/// Whether unfinished backends may be invoked.
#[must_use]
pub fn unstable_backends_enabled() -> bool {
    std::env::var(UNSTABLE_ENV).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Whether the still-capture backend is ready without an opt-in guard.
///
/// Shipping targets expose their capture backends directly. Unsupported native
/// capabilities still fail at the backend boundary with actionable errors;
/// they are not hidden behind a process-wide development opt-in.
#[must_use]
pub const fn capture_backend_is_stable() -> bool {
    // Unit tests deliberately retain the guard. Several command tests exercise
    // error paths with synthetic capture arguments; letting those reach the
    // verified backend captures the developer's real screen and writes into
    // ~/Pictures, which is both invasive and nondeterministic.
    !cfg!(test)
        && cfg!(any(
            target_os = "macos",
            target_os = "windows",
            target_os = "linux"
        ))
}

/// Whether still capture can be offered to a live UI in this process.
///
/// Unlike [`capture_backend_is_stable`], this includes the explicit unstable
/// opt-in used for native integration work.
#[must_use]
pub fn capture_backend_is_ready() -> bool {
    capture_backend_is_stable() || unstable_backends_enabled()
}

fn capture_guard(what: &str, provider: &'static str) -> CliResult<()> {
    if capture_backend_is_ready() {
        Ok(())
    } else {
        Err(CliError::not_implemented(what, provider))
    }
}

/// Refuses still-capture work before it can enumerate or freeze the desktop.
///
/// Interactive selection reads the desktop before the final capture. Keeping
/// that preparation behind the same opt-in guard prevents an unfinished backend
/// from touching screen contents only to be rejected after the user commits.
///
/// # Errors
///
/// Returns [`CliError::NotImplemented`] when the capture backend has not been
/// verified on this platform and [`UNSTABLE_ENV`] is not enabled.
pub fn ensure_capture_backend_ready() -> CliResult<()> {
    capture_guard("taking a capture", "scrozz-capture")
}

fn guard(what: &str, provider: &'static str) -> CliResult<()> {
    if unstable_backends_enabled() {
        Ok(())
    } else {
        Err(CliError::not_implemented(what, provider))
    }
}

/// The still-capture backend for this platform.
///
/// # Errors
///
/// On a platform whose backend has not been verified, returns
/// [`CliError::NotImplemented`] unless [`UNSTABLE_ENV`] is set. Otherwise returns
/// whatever [`scrozz_capture::backend`] returns — including
/// [`scrozz_core::Error::Unsupported`] on a compositor with no usable path.
pub fn capture_backend() -> CliResult<Box<dyn CaptureBackend>> {
    ensure_capture_backend_ready()?;
    Ok(scrozz_capture::backend()?)
}

/// Takes a capture whose interactive acquisition can be cancelled during shutdown.
pub fn capture_with_cancellation(
    request: &CaptureRequest,
    cancellation: &scrozz_capture::CaptureCancellation,
) -> CliResult<Capture> {
    capture_guard("taking a capture", "scrozz-capture")?;
    Ok(scrozz_capture::capture_with_cancellation(
        request,
        cancellation,
    )?)
}

/// The display and window enumerator.
///
/// # Errors
///
/// As [`capture_backend`]. Note that `windows()` on the returned enumerator
/// reports [`scrozz_core::Error::Unsupported`] under Wayland; that is a designed
/// outcome (D8), not a failure of this function.
pub fn target_enumerator() -> CliResult<Box<dyn TargetEnumerator>> {
    capture_guard("listing displays and windows", "scrozz-capture")?;
    // Enumeration is part of the capture backend rather than a separate object:
    // the two have to agree about identifiers, and splitting them is how a
    // window id starts meaning two different things. `CaptureBackend` has
    // `TargetEnumerator` as a supertrait, so this is a trait upcast.
    Ok(scrozz_capture::backend()?)
}

/// Native display geometry for window placement.
///
/// Unlike capture, querying monitor metrics neither reads pixels nor needs the
/// unstable-backend opt-in. A backend that cannot report real topology returns
/// its explicit platform/compositor error; callers must not invent a desktop.
pub fn display_topology() -> CliResult<Box<dyn TargetEnumerator>> {
    Ok(scrozz_capture::backend()?)
}

/// The input driver used by scrolling capture on this desktop.
///
/// # Errors
///
/// Returns a platform or permission error when automatic synthesis was selected
/// but cannot be prepared. Desktops without a synthesis protocol return the
/// manual driver instead.
pub fn scroll_driver() -> CliResult<Box<dyn ScrollDriver>> {
    capture_guard("starting a scrolling capture", "scrozz-capture")?;
    Ok(scrozz_capture::scroll_driver()?)
}

/// Starts a screen recording.
///
/// # Errors
///
/// Returns [`CliError::NotImplemented`] unless [`UNSTABLE_ENV`] is set.
pub fn start_recording(
    request: &scrozz_record::RecordingRequest,
) -> CliResult<Box<dyn scrozz_record::RecordingSession>> {
    guard("recording the screen", "scrozz-record")?;
    let (persisted, _) = crate::settings::stored_settings()?;
    let mut settings = crate::settings::recording_settings_from(&persisted)?;
    let camera_device = crate::settings::camera_device_from(&persisted)?;
    let mut request = request.clone();
    apply_persisted_camera(&mut request, &settings, camera_device);
    settings.cursor = if request.show_cursor {
        scrozz_core::CursorMode::Visible
    } else {
        scrozz_core::CursorMode::Hidden
    };
    if !request.show_cursor {
        settings.cursor_smoothing = false;
    }
    Ok(scrozz_record::start_with_settings(&request, &settings)?)
}

fn apply_persisted_camera(
    request: &mut scrozz_record::RecordingRequest,
    settings: &scrozz_record::RecordingSettings,
    device: Option<scrozz_record::CameraDeviceId>,
) {
    if request.camera.is_some() || !settings.camera.enabled {
        return;
    }
    let mut camera = scrozz_record::CameraRequest::new(settings.camera);
    if let Some(device) = device {
        camera = camera.with_device(device);
    }
    request.camera = Some(camera);
}

/// The capture history, at its default location.
///
/// Not guarded by [`UNSTABLE_ENV`]: the store is a SQLite database and a
/// directory of files, with no platform backend behind it that could be
/// half-finished. Opening it on a machine with no capture support still
/// works, which is what `scrozz history` on a headless box needs.
///
/// # Errors
///
/// Returns [`CliError`] if the database could not be opened or migrated — a
/// missing directory, a schema from a newer Scrozz, a disk that is full.
pub fn store() -> CliResult<scrozz_store::SqliteStore> {
    Ok(scrozz_store::SqliteStore::open_default()?)
}

/// The text recogniser.
///
/// Unlike the others this one is genuinely constructible today, so it is not
/// guarded, and its input is no longer a gap either: [`decode_image_file`]
/// turns a path into a [`scrozz_core::Frame`]. What remains is the call that
/// joins them, which lives in `commands::ocr`.
#[must_use]
pub fn ocr_engine() -> scrozz_ocr::SystemOcr {
    scrozz_ocr::SystemOcr::new()
}

/// Whether this build has a working OCR engine.
///
/// Lets `scrozz ocr` fail with a platform explanation before doing any work,
/// rather than after reading a file.
#[must_use]
pub const fn ocr_available() -> bool {
    scrozz_ocr::SystemOcr::is_available()
}

/// Decodes an image file into a frame.
///
/// Goes through `scrozz-export` rather than an image crate of its own, so the
/// binary has exactly one decode path. A second one would drift: colour space
/// and premultiplication are decided at decode time, and two answers to that
/// question is how a screenshot comes back with grey fringing.
///
/// Two properties of the returned frame are deliberate, and callers that
/// paper over them will be wrong in the harmful direction:
///
/// - [`ColorSpace::Unknown`], not `Srgb`. The bytes may carry an ICC profile
///   the decoder does not interpret, and calling a Display P3 screenshot sRGB
///   is exactly what makes wide-gamut captures come back wrong. Encoders read
///   `Unknown` as "embed nothing", never as "embed sRGB".
/// - [`ScaleFactor::IDENTITY`]. A file records no display. A caller that
///   genuinely knows better — the store reopening its own capture — should
///   override it rather than let the decoder invent one. For OCR the 1× claim
///   errs safely: the recogniser upscales small images, so it may upscale a
///   little more than it needed to instead of skipping it on a 2× shot.
///
/// [`ColorSpace::Unknown`]: scrozz_core::ColorSpace::Unknown
/// [`ScaleFactor::IDENTITY`]: scrozz_core::ScaleFactor::IDENTITY
///
/// # Errors
///
/// Returns [`CliError`] if the file is unreadable or is not an image format
/// this build understands.
pub fn decode_image_file(path: &std::path::Path) -> CliResult<scrozz_core::Frame> {
    Ok(scrozz_export::decode_file(path)?)
}

/// Puts the process into macOS's accessory activation policy.
///
/// # Why
///
/// D27: Scrozz is invisible at rest. A menu-bar item, no Dock icon, no window
/// until the user asks for one. On macOS that requires
/// `NSApplication.setActivationPolicy(.accessory)` — or `LSUIElement` in the
/// bundle's `Info.plist`, which is the same decision made statically.
///
/// # When
///
/// Before anything creates a window or a menu-bar item, and before the first
/// `NSApplication` activation. A Dock icon that appears for two hundred
/// milliseconds during start-up and then vanishes is still a Dock icon the
/// user saw, and it is still a bounce in their peripheral vision.
///
/// The AppKit call itself lives in `scrozz-shell`, which owns the platform
/// boundary and already links AppKit. This crate stays free of `objc2`: the
/// CLI must build and run on a headless machine, and linking AppKit into the
/// binary a compositor keybinding invokes is exactly backwards.
///
/// # Errors
///
/// Never. AppKit can refuse — inside a `.app` whose `Info.plist` disagrees, or
/// in a test binary with no real `NSApplication` — and the refusal is logged
/// with the remedy it carries. It is not returned, because the consequence of
/// a refusal is a Dock icon, and a Dock icon is a worse look, not a broken
/// app. Failing to be invisible must never be the reason a capture does not
/// happen.
#[allow(clippy::unnecessary_wraps)]
pub fn become_accessory_app() -> CliResult<()> {
    #[cfg(target_os = "macos")]
    match scrozz_shell::tray::use_accessory_activation_policy() {
        Ok(()) => {
            tracing::debug!("this process is now a macOS accessory app (D27: no Dock icon)");
        }
        Err(err) => {
            tracing::warn!("{err}");
        }
    }
    Ok(())
}

/// A one-line summary of what is and is not wired up.
///
/// Surfaced by `scrozz --json` diagnostics and, more usefully, by the tests:
/// when a backend lands, the corresponding assertion here fails and points at
/// the code that should now be doing real work.
#[must_use]
pub fn readiness() -> Vec<(&'static str, bool)> {
    vec![
        (
            "capture",
            capture_backend_is_stable() || unstable_backends_enabled(),
        ),
        ("record", unstable_backends_enabled()),
        (
            "enumerate",
            capture_backend_is_stable() || unstable_backends_enabled(),
        ),
        ("store", true),
        ("ocr", ocr_available()),
        ("decode", true),
        ("gui", crate::gui::available()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{exit::Exit, test_env};

    /// `unwrap_err` needs `T: Debug`, and none of these boxed traits are.
    fn err_of<T>(result: CliResult<T>) -> CliError {
        match result {
            Ok(_) => panic!("a stubbed backend must not succeed"),
            Err(e) => e,
        }
    }

    #[test]
    fn test_builds_keep_the_capture_guard_even_on_macos() {
        let _env = test_env::lock();
        test_env::clear(UNSTABLE_ENV);
        let err = err_of(capture_backend());
        assert_eq!(err.exit(), Exit::NotImplemented);
        assert!(err.to_string().contains("scrozz-capture"), "{err}");
    }

    #[test]
    fn the_guard_names_the_crate_that_owes_the_work() {
        let _env = test_env::lock();
        test_env::clear(UNSTABLE_ENV);
        let err = guard("doing a thing", "scrozz-somewhere").unwrap_err();
        let text = err.to_human();
        assert!(text.contains("scrozz-somewhere"), "{text}");
        assert!(text.contains("doing a thing"), "{text}");
    }

    #[test]
    fn cli_camera_uses_persisted_device_unless_the_request_is_explicit() {
        let persisted = scrozz_record::CameraDeviceId::new("external-camera").unwrap();
        let explicit = scrozz_record::CameraDeviceId::new("explicit-camera").unwrap();
        let mut settings = scrozz_record::RecordingSettings::shipped();
        settings.camera.enabled = true;
        let mut request =
            scrozz_record::RecordingRequest::new(scrozz_core::CaptureTarget::AllDisplays);

        apply_persisted_camera(&mut request, &settings, Some(persisted.clone()));
        assert_eq!(
            request
                .camera
                .as_ref()
                .and_then(|camera| camera.device_id.as_ref()),
            Some(&persisted)
        );

        request.camera =
            Some(scrozz_record::CameraRequest::new(settings.camera).with_device(explicit.clone()));
        apply_persisted_camera(&mut request, &settings, Some(persisted));
        assert_eq!(
            request
                .camera
                .as_ref()
                .and_then(|camera| camera.device_id.as_ref()),
            Some(&explicit),
            "an explicit --camera selection remains authoritative"
        );
    }

    #[test]
    fn the_store_opens_without_the_unstable_guard() {
        // History is not a platform backend. Gating it would make `scrozz
        // history` fail on a machine that can read its own database perfectly
        // well, which is the wrong answer for a headless box or a CI runner.
        let _env = test_env::lock();
        test_env::clear(UNSTABLE_ENV);
        if let Err(e) = store() {
            assert_ne!(
                e.exit(),
                Exit::NotImplemented,
                "the store is implemented; a failure must name a real cause"
            );
        }
    }

    #[test]
    fn decoding_a_missing_file_fails_for_a_real_reason() {
        // Not `NotImplemented` any more: `scrozz-export` decodes. What is left
        // is an ordinary I/O failure, and calling that "unimplemented" would
        // send someone looking for missing code instead of a missing file.
        let err = err_of(decode_image_file(std::path::Path::new(
            "/nonexistent/scrozz-not-here.png",
        )));
        assert_ne!(err.exit(), Exit::NotImplemented, "{err}");
    }

    #[test]
    fn a_real_png_decodes_and_keeps_the_decoder_honest() {
        // The round trip the OCR path depends on, through the encoder that
        // wrote the file. Doing it end to end is the point: a hand-written
        // fixture could agree with a decoder that both encoder and decoder had
        // got wrong.
        use scrozz_core::{ColorSpace, ScaleFactor};
        use scrozz_export::{FrameEncoder, ImageFormat, RgbaImage};

        let image = RgbaImage {
            width: 3,
            height: 2,
            // Straight alpha, deliberately not opaque: premultiplication
            // damage shows up in the partly transparent pixel and nowhere else.
            data: vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, //
                255, 255, 0, 128, 0, 0, 0, 255, 255, 255, 255, 0,
            ],
        };
        let bytes = FrameEncoder::new()
            .encode_rgba(&image, ColorSpace::Srgb, ImageFormat::Png)
            .expect("encoding a 3x2 png");

        let dir = std::env::temp_dir().join(format!("scrozz-decode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("probe.png");
        std::fs::write(&path, &bytes).expect("writing the probe");

        let frame = decode_image_file(&path).expect("decoding what we just wrote");

        assert_eq!(frame.size.width as u32, image.width);
        assert_eq!(frame.size.height as u32, image.height);
        assert_eq!(
            frame.stride,
            image.width as usize * 4,
            "the decoder promises tightly packed rows, so no stride reconciliation is needed"
        );
        assert_eq!(
            frame.data[..image.data.len()],
            image.data[..],
            "straight alpha, unchanged: the 128-alpha pixel is where premultiplication would show"
        );

        // Both of these are deliberate, and both are the safe direction.
        // Claiming sRGB for what might be Display P3 is what makes wide-gamut
        // captures look wrong; claiming 1x for a 2x file only makes the OCR
        // upscaler do slightly more work than it had to.
        assert_eq!(
            frame.color_space,
            ColorSpace::Unknown,
            "the decoder must not claim a colour space it did not interpret"
        );
        assert_eq!(
            frame.scale,
            ScaleFactor::IDENTITY,
            "a file records no display; a caller that knows better overrides"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enumeration_is_guarded_in_tests_but_no_longer_a_gap() {
        // Enumeration is now wired to the capture backend, so with the guard
        // lifted this either succeeds or fails for a *platform* reason — a
        // missing permission, or a compositor with no usable path. What it must
        // never be again is `NotImplemented`.
        let _env = test_env::lock();
        test_env::set(UNSTABLE_ENV, "1");
        let lifted = target_enumerator();
        test_env::clear(UNSTABLE_ENV);
        if let Err(e) = &lifted {
            assert_ne!(
                e.exit(),
                Exit::NotImplemented,
                "enumeration is implemented; a failure here must name a real cause"
            );
        }

        // Test builds always retain the guard, including on macOS, so a unit
        // test never enumerates the developer's real desktop accidentally.
        assert_eq!(err_of(target_enumerator()).exit(), Exit::NotImplemented);
    }

    #[test]
    fn the_unstable_flag_reads_the_obvious_values() {
        let _env = test_env::lock();
        for (value, expected) in [
            ("1", true),
            ("true", true),
            ("TRUE", true),
            ("0", false),
            ("", false),
        ] {
            test_env::set(UNSTABLE_ENV, value);
            assert_eq!(unstable_backends_enabled(), expected, "{value:?}");
        }
        test_env::clear(UNSTABLE_ENV);
        assert!(!unstable_backends_enabled());
    }

    #[test]
    fn becoming_an_accessory_app_never_fails_the_command() {
        // D27 is not yet implementable, but failing to be invisible must never
        // be the reason a capture does not happen.
        assert!(become_accessory_app().is_ok());
    }

    #[test]
    fn ocr_availability_matches_the_platforms_that_have_an_engine() {
        assert_eq!(
            ocr_available(),
            cfg!(any(target_os = "macos", target_os = "windows"))
        );
    }

    #[test]
    fn readiness_lists_every_backend_once() {
        let names: Vec<&str> = readiness().into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            [
                "capture",
                "record",
                "enumerate",
                "store",
                "ocr",
                "decode",
                "gui"
            ]
        );
    }
}
