//! Turning the overlay's window into a non-activating panel.
//!
//! # Why this lives here
//!
//! `scrozz-ui` is `#![forbid(unsafe_code)]` and deliberately does not depend on
//! `scrozz-shell`, so it cannot perform the conversion itself; it exposes a
//! [`PanelHook`] instead. This crate depends on both, so this is the only place
//! the two halves can meet.
//!
//! # Why it takes a pointer
//!
//! [`PanelHook`] is `FnOnce(&eframe::CreationContext<'_>) -> PanelReport`, and
//! the work is split in two: [`hook`] does the pointer extraction, and
//! [`convert_ns_view`] does the conversion. The split is where the platform
//! risk is — the conversion can be exercised without a window, so it is the
//! part that carries the tests.
//!
//! # What the conversion actually does
//!
//! AppKit has no "make this window non-activating" switch. The behaviour lives
//! on `NSPanel` + `NSWindowStyleMaskNonactivatingPanel`, and winit hands us an
//! `NSWindow`. `scrozz-shell` isa-swizzles the instance into a runtime-built
//! `NSPanel` subclass and then sets the mask, guarding the swizzle on the two
//! classes having identical instance sizes and refusing when they do not.
//!
//! Refusal is not breakage: the overlay still draws and still works, it just
//! takes focus when clicked. That is the whole reason [`PanelReport`] carries a
//! `detail` string rather than being a bool.

use std::ffi::c_void;

use raw_window_handle::HasWindowHandle;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use raw_window_handle::RawWindowHandle;
#[cfg(target_os = "linux")]
use scrozz_shell::OverlayWindow;
use scrozz_shell::{NativeOverlay, OverlayBehavior};
#[cfg(target_os = "linux")]
use scrozz_ui::OverlayGeometry;
use scrozz_ui::{PanelAttachment, PanelReport};

/// Converts an already-created Linux overlay window into a capture card.
///
/// The X11 and Wayland halves of this are not symmetric, and pretending they
/// were is the mistake this function exists to avoid.
///
/// On X11 the window is real, addressable and repositionable, so the handle is
/// a window ID and the backend can do everything the card needs: stack it above
/// normal windows, keep it out of the taskbar and the alt-tab cycle, anchor it
/// to `_NET_WORKAREA`, and shape its input region so clicks outside the card
/// fall through.
///
/// On Wayland the handle is deliberately opaque, because there is nothing legal
/// to do with it. winit's surface already carries the `xdg_toplevel` role, and a
/// `wl_surface` holds exactly one role for its lifetime — asking
/// `zwlr_layer_shell_v1` to promote it raises a protocol error, which on Wayland
/// is fatal and kills the whole client connection. So the backend refuses, and
/// the refusal says which of the two very different reasons applies: the
/// compositor supports layer-shell and Scrozz cannot yet reach it, or the
/// compositor refuses layer-shell outright.
#[cfg(target_os = "linux")]
#[must_use]
fn convert_linux_window(
    handle: scrozz_shell::LinuxWindowHandle,
    pixels_per_point: f32,
) -> PanelAttachment {
    let session = scrozz_shell::Session::detect();

    let mut overlay = match NativeOverlay::adopt(handle, &session) {
        Ok(overlay) => overlay,
        Err(err) => {
            return PanelAttachment::report_only(PanelReport::unsupported(err.to_string()));
        }
    };

    let mut report = match overlay.apply(&OverlayBehavior::capture_card()) {
        // Same rule as macOS: `non_activating` is the one part of the behaviour
        // D27 depends on. On X11 an override-redirect or tool window satisfies
        // it; under the GNOME fallback nothing does, and the report says so
        // rather than claiming a success the window will not deliver.
        Ok(report) if report.non_activating => PanelReport::converted(report.detail),
        Ok(report) => PanelReport::unsupported(report.detail),
        Err(err) => {
            return PanelAttachment::report_only(PanelReport::unsupported(err.to_string()));
        }
    };

    match handle {
        scrozz_shell::LinuxWindowHandle::X11 { .. } => {
            let geometry = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
                overlay.set_scale_factor(f64::from(pixels_per_point));
                overlay.work_area().and_then(|area| {
                    if let Err(err) = overlay.set_frame(area) {
                        report.detail.push_str(&format!(
                            " — X11 work-area placement failed ({err}); keeping winit's frame"
                        ));
                        None
                    } else {
                        Some(OverlayGeometry::new(egui::Rect::from_min_size(
                            egui::pos2(area.origin.x as f32, area.origin.y as f32),
                            egui::vec2(area.size.width as f32, area.size.height as f32),
                        )))
                    }
                })
            } else {
                report.detail.push_str(&format!(
                    " — eframe reported invalid scale {pixels_per_point}; keeping winit's frame"
                ));
                None
            };

            let attachment = PanelAttachment::with_input_region(
                report,
                move |surface, hits, click_through, pixels_per_point| {
                    let region =
                        scaled_input_region(surface, hits, click_through, pixels_per_point)?;
                    overlay.set_input_region(&region)
                },
            );
            match geometry {
                Some(geometry) => attachment.with_geometry(geometry),
                None => attachment,
            }
        }
        // This is winit's xdg_toplevel. Its whole-window passthrough command is
        // legal, but retaining the protocol-only layer-shell object here would
        // not make this surface a layer surface.
        scrozz_shell::LinuxWindowHandle::Wayland => PanelAttachment::report_only(report),
    }
}

/// Converts egui's window-local points into protocol pixels.
///
/// Kept here, at the UI/native seam, so `scrozz-shell` never depends on egui and
/// the region arithmetic itself remains shared by X11 and Wayland.
fn scaled_input_region(
    surface: egui::Rect,
    hits: &[egui::Rect],
    click_through: bool,
    pixels_per_point: f32,
) -> scrozz_core::Result<scrozz_shell::linux::region::InputRegion> {
    if !pixels_per_point.is_finite() || pixels_per_point <= 0.0 {
        return Err(scrozz_core::Error::InvalidRequest(format!(
            "overlay scale {pixels_per_point} is not a positive finite number"
        )));
    }

    let scale = f64::from(pixels_per_point);
    let scale_rect = |rect: egui::Rect| {
        scrozz_core::LogicalRect::new(
            scrozz_core::LogicalPoint::new(
                f64::from(rect.min.x) * scale,
                f64::from(rect.min.y) * scale,
            ),
            scrozz_core::LogicalSize::new(
                f64::from(rect.width()) * scale,
                f64::from(rect.height()) * scale,
            ),
        )
    };
    let surface = scale_rect(surface);
    let hits: Vec<_> = hits.iter().copied().map(scale_rect).collect();
    Ok(scrozz_shell::linux::region::input_region(
        surface,
        &hits,
        click_through,
    ))
}

/// Converts the window hosting `ns_view` into a non-activating overlay panel.
///
/// `ns_view` is the `ns_view` field of a `RawWindowHandle::AppKit`, which is
/// what `eframe::CreationContext` reports on macOS. A null pointer is refused
/// rather than dereferenced.
///
/// Never panics and never fails: every outcome, including "this platform has no
/// overlay backend", comes back as a [`PanelReport`]. A hook that could fail
/// would be a hook that can take down the window it was called to configure.
///
/// # Safety
///
/// `ns_view` must be a live `NSView *` whose `WindowHandle` borrow is still
/// alive, and this must be called on the main thread. Both hold inside the
/// `eframe` app creator, which is the only caller.
#[must_use]
pub unsafe fn convert_ns_view(ns_view: *mut c_void) -> PanelReport {
    if ns_view.is_null() {
        return PanelReport::unsupported("the window handle carried a null NSView");
    }

    // The entry point is named differently per platform: macOS adopts a view or
    // a window, and the stub backends adopt an opaque handle. Both refuse
    // safely, so the non-macOS arm is a real path rather than a `todo!`.
    #[cfg(target_os = "macos")]
    // SAFETY: forwarded from this function's own contract — a live `NSView *`
    // on the main thread.
    let adopted = unsafe { NativeOverlay::from_ns_view(ns_view) };

    // Linux has a real backend, but it is not reachable through an NSView
    // pointer — there is no such thing here. Rather than inventing a conversion,
    // this says so; [`hook`] never routes Linux through this function.
    #[cfg(target_os = "linux")]
    return PanelReport::unsupported(
        "an NSView pointer has no meaning on Linux; the X11 and Wayland backends \
         are reached through the window handle instead",
    );

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    // SAFETY: as above; the stub backends do not dereference the handle.
    let adopted = unsafe { NativeOverlay::adopt(ns_view) };

    #[cfg(not(target_os = "linux"))]
    {
        let mut overlay = match adopted {
            Ok(overlay) => overlay,
            Err(err) => return PanelReport::unsupported(err.to_string()),
        };

        match overlay.apply(&OverlayBehavior::capture_card()) {
            // `non_activating` is the only part of the behaviour D27 depends on.
            // Everything else — level, collection behaviour — can be applied and
            // the card still behaves; this cannot.
            Ok(report) if report.non_activating => PanelReport::converted(report.detail),
            Ok(report) => PanelReport::unsupported(report.detail),
            Err(err) => PanelReport::unsupported(err.to_string()),
        }
    }
}

/// The hook `scrozz-ui` runs while the overlay window is being created.
///
/// Extracts the platform handle from the `CreationContext` and hands it to
/// [`convert_ns_view`]. That is the whole hook: everything with platform risk
/// in it lives in the conversion, which is unit-tested without a window.
///
/// The `ns_view` / `ns_window` distinction is the subtle one. `eframe` reports
/// an **`NSView`**, and `scrozz-shell` offers `from_ns_view` and
/// `from_ns_window` — both taking `*mut c_void`, so handing a view to the
/// window entry point type-checks and then converts the wrong object. A test
/// pins the arm this reaches for.
#[must_use]
pub fn hook() -> scrozz_ui::PanelHook {
    Box::new(|cc: &eframe::CreationContext<'_>| {
        let handle = match cc.window_handle() {
            Ok(handle) => handle,
            Err(err) => {
                return PanelAttachment::report_only(PanelReport::unsupported(format!(
                    "eframe reported no window handle: {err}"
                )));
            }
        };

        #[cfg(target_os = "macos")]
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return PanelAttachment::report_only(PanelReport::unsupported(
                "the overlay window is not an AppKit window, so it has no NSView to convert",
            ));
        };

        // SAFETY: `handle` borrows the window for this scope, so the view is
        // alive; `OverlayApp::new` runs on the main thread.
        #[cfg(target_os = "macos")]
        return PanelAttachment::report_only(unsafe { convert_ns_view(appkit.ns_view.as_ptr()) });

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = handle;
            PanelAttachment::report_only(PanelReport::unsupported(
                "only the macOS and Linux overlay backends are implemented so far, so \
                 the window keeps its native activation behaviour",
            ))
        }

        // Both X11 handle flavours carry the same thing — a server-side window
        // ID — and which one winit reports depends on how it was built, not on
        // anything Scrozz chose. Accepting both is not defensive coding; it is
        // the difference between working and not on an ordinary X11 session.
        #[cfg(target_os = "linux")]
        return match handle.as_raw() {
            RawWindowHandle::Xlib(xlib) => match u32::try_from(xlib.window) {
                // An X11 window ID is 32 bits on the wire; Xlib widens it to
                // `c_ulong` purely for historical reasons. A value that does not
                // fit therefore is not a window, and passing a truncated ID to
                // the server would configure some *other* window — so this
                // refuses rather than guessing.
                Ok(window) => convert_linux_window(
                    scrozz_shell::LinuxWindowHandle::X11 { window },
                    cc.egui_ctx.pixels_per_point(),
                ),
                Err(_) => PanelAttachment::report_only(PanelReport::unsupported(format!(
                    "the X11 window ID {} does not fit in 32 bits, so it cannot be a \
                     valid window",
                    xlib.window
                ))),
            },
            RawWindowHandle::Xcb(xcb) => convert_linux_window(
                scrozz_shell::LinuxWindowHandle::X11 {
                    window: xcb.window.get(),
                },
                cc.egui_ctx.pixels_per_point(),
            ),
            RawWindowHandle::Wayland(_) => convert_linux_window(
                scrozz_shell::LinuxWindowHandle::Wayland,
                cc.egui_ctx.pixels_per_point(),
            ),
            other => PanelAttachment::report_only(PanelReport::unsupported(format!(
                "the overlay window is neither X11 nor Wayland ({other:?}), so Scrozz \
                 has no way to configure it"
            ))),
        };
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_null_view_is_refused_rather_than_dereferenced() {
        // The one input that would be undefined behaviour if it got through.
        // `scrozz-shell` checks this too; checking it here as well means the
        // guarantee does not depend on which backend the target selects.
        let report = unsafe { convert_ns_view(std::ptr::null_mut()) };
        assert!(!report.non_activating);
        assert!(report.detail.contains("null"), "{}", report.detail);
    }

    #[test]
    fn refusal_is_reported_not_raised() {
        // The property the whole hook design rests on: a hook that could fail
        // is a hook that can take down the window it was called to configure.
        // Every path returns a report, so the overlay always survives.
        let report = unsafe { convert_ns_view(std::ptr::null_mut()) };
        assert!(
            !report.detail.is_empty(),
            "a refusal with no reason is indistinguishable from a bug"
        );
    }

    #[test]
    fn the_hook_reaches_for_the_view_arm_not_the_window_arm() {
        // eframe reports `ns_view`. Reaching for `from_ns_window` with a view
        // pointer type-checks (both are *mut c_void) and converts the wrong
        // object, so this pins the source rather than trusting review.
        let source = include_str!("panel.rs");
        let body = source
            .split("pub fn hook()")
            .nth(1)
            .expect("the hook is defined in this file")
            .split("#[cfg(test)]")
            .next()
            .expect("the hook ends before the tests do");
        assert!(body.contains("ns_view"), "the hook must use the NSView arm");
        assert!(
            !body.contains("from_ns_window"),
            "the hook must not convert the window handle as if it were a view"
        );
    }

    #[test]
    fn a_hook_can_be_built_without_a_window() {
        // Building the hook must not touch AppKit — it is constructed at
        // start-up, long before `eframe` has a window to hand it.
        let hook = hook();
        drop(hook);
    }

    #[test]
    fn native_regions_are_scaled_and_keep_each_card_separate() {
        let surface = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0));
        let cards = [
            egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(30.0, 40.0)),
            egui::Rect::from_min_size(egui::pos2(10.0, 80.0), egui::vec2(30.0, 40.0)),
        ];

        let scrozz_shell::linux::region::InputRegion::Rects(rects) =
            scaled_input_region(surface, &cards, true, 2.0).expect("valid region")
        else {
            panic!("two cards must produce two rectangles");
        };
        assert_eq!(rects.len(), 2);
        assert_eq!((rects[0].x, rects[0].y), (20, 40));
        assert_eq!((rects[0].width, rects[0].height), (60, 80));
        assert_eq!(rects[1].y, 160);
    }

    #[test]
    fn native_regions_reject_an_invalid_scale() {
        let surface = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0));
        let err = scaled_input_region(surface, &[], true, f32::NAN)
            .expect_err("NaN cannot address protocol pixels");
        assert!(err.to_string().contains("scale"));
    }
}
