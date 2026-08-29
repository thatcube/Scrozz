//! Permission checks that run only for capabilities a recording requested.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureDevice, AVMediaType, AVMediaTypeAudio, AVMediaTypeVideo,
};
use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};
use objc2_foundation::{NSBundle, NSDate, NSDefaultRunLoopMode, NSRunLoop, NSString, NSThread};
use scrozz_core::{Error, Result};

use super::error;

pub(crate) fn ensure_screen() -> Result<()> {
    if CGPreflightScreenCaptureAccess() {
        return Ok(());
    }

    let _ = CGRequestScreenCaptureAccess();
    if CGPreflightScreenCaptureAccess() {
        Ok(())
    } else {
        Err(error::screen_permission_denied())
    }
}

pub(crate) fn ensure_microphone() -> Result<()> {
    // SAFETY: AVMediaTypeAudio is an immutable weak-linked framework constant.
    let media_type = unsafe { AVMediaTypeAudio }.ok_or_else(|| Error::Unsupported {
        what: "microphone recording".to_owned(),
        why: "AVFoundation did not expose the audio media type".to_owned(),
    })?;

    // SAFETY: this reads the process's current authorization state and cannot
    // itself show a prompt.
    ensure_media_permission(
        media_type,
        "NSMicrophoneUsageDescription",
        error::microphone_permission_denied,
        "microphone",
    )
}

pub(crate) fn camera_status() -> crate::CameraPermission {
    let Some(media_type) = (unsafe { AVMediaTypeVideo }) else {
        return crate::CameraPermission::Unsupported;
    };
    match unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) } {
        AVAuthorizationStatus::Authorized => crate::CameraPermission::Authorized,
        AVAuthorizationStatus::Denied => crate::CameraPermission::Denied,
        AVAuthorizationStatus::Restricted => crate::CameraPermission::Restricted,
        AVAuthorizationStatus::NotDetermined => crate::CameraPermission::NotDetermined,
        _ => crate::CameraPermission::Restricted,
    }
}

pub(crate) fn ensure_camera() -> Result<()> {
    let media_type = unsafe { AVMediaTypeVideo }.ok_or_else(|| Error::Unsupported {
        what: "camera capture".to_owned(),
        why: "AVFoundation did not expose the video media type".to_owned(),
    })?;
    ensure_media_permission(
        media_type,
        "NSCameraUsageDescription",
        error::camera_permission_denied,
        "camera",
    )
}

fn ensure_media_permission(
    media_type: &AVMediaType,
    usage_key: &str,
    denied: fn() -> Error,
    capability: &str,
) -> Result<()> {
    match unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) } {
        AVAuthorizationStatus::Authorized => Ok(()),
        AVAuthorizationStatus::Denied | AVAuthorizationStatus::Restricted => Err(denied()),
        AVAuthorizationStatus::NotDetermined => {
            request_media_permission(media_type, usage_key, denied, capability)
        }
        _ => Err(denied()),
    }
}

fn request_media_permission(
    media_type: &AVMediaType,
    usage_key: &str,
    denied: fn() -> Error,
    capability: &str,
) -> Result<()> {
    let key = NSString::from_str(usage_key);
    let has_usage_description = NSBundle::mainBundle()
        .objectForInfoDictionaryKey(&key)
        .is_some();
    if !has_usage_description {
        return Err(Error::PermissionDenied {
            capability: capability.to_owned(),
            remedy: format!(
                "the application bundle must provide {usage_key} before macOS can safely ask for {capability} access"
            ),
        });
    }

    let answer = Arc::new((Mutex::new(None), Condvar::new()));
    let handler = {
        let answer = Arc::clone(&answer);
        RcBlock::new(move |granted: Bool| {
            let (lock, ready) = &*answer;
            *lock.lock().unwrap_or_else(PoisonError::into_inner) = Some(granted.as_bool());
            ready.notify_all();
        })
    };
    // SAFETY: the media type is AVMediaTypeAudio and the copied block owns all
    // state it accesses.
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &handler);
    }

    let granted = if NSThread::isMainThread_class() {
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(granted) = *answer.0.lock().unwrap_or_else(PoisonError::into_inner) {
                break Some(granted);
            }
            if std::time::Instant::now() >= deadline {
                break None;
            }
            let until = NSDate::dateWithTimeIntervalSinceNow(0.01);
            let run_loop = NSRunLoop::currentRunLoop();
            // SAFETY: immutable weak-linked Foundation run-loop mode.
            let mode = unsafe { NSDefaultRunLoopMode };
            let _ = run_loop.runMode_beforeDate(mode, &until);
        }
    } else {
        let (lock, ready) = &*answer;
        let (answer, _) = ready
            .wait_timeout_while(
                lock.lock().unwrap_or_else(PoisonError::into_inner),
                Duration::from_secs(120),
                |value| value.is_none(),
            )
            .unwrap_or_else(PoisonError::into_inner);
        *answer
    };
    match granted {
        Some(true) => Ok(()),
        Some(false) => Err(denied()),
        None => Err(Error::Platform(format!(
            "{capability} permission request did not complete in time"
        ))),
    }
}
