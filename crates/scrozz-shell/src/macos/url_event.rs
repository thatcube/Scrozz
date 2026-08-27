//! Delivery of URLs opened through the application's `CFBundleURLTypes`.
//!
//! LaunchServices sends custom-scheme URLs through
//! `application:openURLs:`; it does not append them to the process argument
//! vector. Winit owns `NSApplication.delegate`, so this module adds that one
//! missing selector to winit's delegate class rather than replacing the
//! delegate and breaking its lifecycle callbacks.

use std::{
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Mutex, OnceLock},
};

use objc2::{
    ffi::class_addMethod,
    runtime::{AnyClass, AnyObject, Imp, Sel},
    sel,
};
use objc2_app_kit::NSApplication;
use objc2_foundation::{NSArray, NSURL};
use scrozz_core::{Error, Result};

use crate::macos::main_thread;

const MAX_PENDING_URLS: usize = 64;
const MAX_URL_UTF16_UNITS: usize = 256;
static PENDING_URLS: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
static INSTALLED_CLASS: OnceLock<usize> = OnceLock::new();

unsafe extern "C-unwind" fn application_open_urls(
    _delegate: *mut AnyObject,
    _selector: Sel,
    _application: *mut NSApplication,
    urls: *mut NSArray<NSURL>,
) {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: AppKit supplies a non-null NSArray for this delegate method
        // and keeps it alive for the duration of the callback.
        let Some(urls) = (unsafe { urls.as_ref() }) else {
            tracing::error!("AppKit delivered a null URL collection");
            return;
        };
        let Ok(mut pending) = pending_urls().lock() else {
            tracing::error!("the URL event queue is poisoned; incoming URLs were refused");
            return;
        };
        for url in urls {
            if pending.len() == MAX_PENDING_URLS {
                tracing::warn!("the URL event queue is full; an incoming URL was refused");
                break;
            }
            let Some(value) = url.absoluteString() else {
                tracing::warn!("AppKit delivered a URL without an absolute string");
                continue;
            };
            if value.length() > MAX_URL_UTF16_UNITS {
                tracing::warn!("an oversized incoming URL was refused");
                continue;
            }
            push_bounded(&mut pending, value.to_string());
        }
    }));
    if outcome.is_err() {
        tracing::error!("a panic was contained while receiving incoming URLs");
    }
}

/// Exposes URL values queued by the application delegate.
pub struct UrlEventHandler(());

impl UrlEventHandler {
    /// Adds the URL selector after winit has installed its application delegate.
    ///
    /// # Errors
    ///
    /// Returns an error off the main thread, before winit has installed its
    /// delegate, or if another component already owns the URL selector.
    pub fn install() -> Result<Self> {
        let mtm = main_thread("URL event registration")?;
        let application = NSApplication::sharedApplication(mtm);
        let delegate = application.delegate().ok_or_else(|| {
            Error::Platform("winit did not install its macOS application delegate".into())
        })?;
        let object: &AnyObject = AsRef::<AnyObject>::as_ref(&*delegate);
        let class = object.class();
        let class_address = std::ptr::from_ref(class).addr();

        if let Some(installed) = INSTALLED_CLASS.get() {
            return if *installed == class_address {
                Ok(Self(()))
            } else {
                Err(Error::Platform(
                    "the macOS application delegate changed after URL registration".into(),
                ))
            };
        }

        let selector = sel!(application:openURLs:);
        if class.instance_method(selector).is_some() {
            return Err(Error::Platform(format!(
                "{} already implements application:openURLs:",
                class.name().to_string_lossy()
            )));
        }

        let implementation = application_open_urls_imp();
        // SAFETY: `class` is a registered Objective-C class. The implementation
        // matches `v@:@@`: void return, self and selector, then two objects.
        let added = unsafe {
            class_addMethod(
                std::ptr::from_ref::<AnyClass>(class).cast_mut(),
                selector,
                implementation,
                c"v@:@@".as_ptr(),
            )
        };
        if !added.as_bool() {
            return Err(Error::Platform(format!(
                "could not add application:openURLs: to {}",
                class.name().to_string_lossy()
            )));
        }
        INSTALLED_CLASS.set(class_address).map_err(|_| {
            Error::Platform("URL registration raced with another installation".into())
        })?;
        Ok(Self(()))
    }

    /// Removes and returns every URL delivered since the previous drain.
    ///
    /// # Errors
    ///
    /// Returns an error if the process-global queue was poisoned.
    pub fn drain(&self) -> Result<Vec<String>> {
        let mut pending = pending_urls()
            .lock()
            .map_err(|_| Error::Platform("the URL event queue is poisoned".into()))?;
        Ok(pending.drain(..).collect())
    }
}

fn pending_urls() -> &'static Mutex<VecDeque<String>> {
    PENDING_URLS.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn push_bounded(pending: &mut VecDeque<String>, url: String) -> bool {
    if pending.len() == MAX_PENDING_URLS {
        return false;
    }
    pending.push_back(url);
    true
}

fn application_open_urls_imp() -> Imp {
    // SAFETY: Objective-C erases method implementations to `Imp`; the full
    // signature is supplied to `class_addMethod` alongside this pointer.
    unsafe {
        std::mem::transmute::<
            unsafe extern "C-unwind" fn(
                *mut AnyObject,
                Sel,
                *mut NSApplication,
                *mut NSArray<NSURL>,
            ),
            Imp,
        >(application_open_urls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_url_queue_is_bounded_and_ordered() {
        let mut pending = VecDeque::new();
        for index in 0..MAX_PENDING_URLS {
            assert!(push_bounded(&mut pending, format!("scrozz://test/{index}")));
        }
        assert!(!push_bounded(
            &mut pending,
            "scrozz://test/refused".to_owned()
        ));
        assert_eq!(pending.len(), MAX_PENDING_URLS);
        assert_eq!(pending.front().unwrap(), "scrozz://test/0");
        assert_eq!(
            pending.back().unwrap(),
            &format!("scrozz://test/{}", MAX_PENDING_URLS - 1)
        );
    }
}
