//! Permission checks that run only for capabilities a recording requested.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};
use objc2_foundation::{NSBundle, NSString};
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
    match unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) } {
        AVAuthorizationStatus::Authorized => Ok(()),
        AVAuthorizationStatus::Denied | AVAuthorizationStatus::Restricted => {
            Err(error::microphone_permission_denied())
        }
        AVAuthorizationStatus::NotDetermined => request_microphone(media_type),
        _ => Err(error::microphone_permission_denied()),
    }
}

fn request_microphone(media_type: &objc2_av_foundation::AVMediaType) -> Result<()> {
    let key = NSString::from_str("NSMicrophoneUsageDescription");
    let has_usage_description = NSBundle::mainBundle()
        .objectForInfoDictionaryKey(&key)
        .is_some();
    if !has_usage_description {
        return Err(Error::PermissionDenied {
            capability: "microphone".to_owned(),
            remedy: "the application bundle must provide NSMicrophoneUsageDescription before \
                     macOS can safely ask for microphone access"
                .to_owned(),
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

    let (lock, ready) = &*answer;
    let (answer, _) = ready
        .wait_timeout_while(
            lock.lock().unwrap_or_else(PoisonError::into_inner),
            Duration::from_secs(120),
            |value| value.is_none(),
        )
        .unwrap_or_else(PoisonError::into_inner);
    match *answer {
        Some(true) => Ok(()),
        Some(false) => Err(error::microphone_permission_denied()),
        None => Err(Error::Platform(
            "microphone permission request did not complete in time".to_owned(),
        )),
    }
}
