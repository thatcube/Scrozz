//! Tests for the Windows backend's platform-independent logic.
//!
//! # Why this file looks the way it does
//!
//! The Windows backend is developed and reviewed on machines that are not
//! running Windows, so the usual safety net — run it and look at the picture —
//! is unavailable. The response is to push every decision that can be made
//! without the OS into modules that name no `windows` crate type at all
//! (`geom`, `filter`, `pixels`) and then test *those* properly, on whatever
//! platform happens to be building.
//!
//! Those three modules are included by path rather than reached through the
//! crate, because `mod windows` is `#[cfg(target_os = "windows")]` and would
//! otherwise be invisible here. They are wrapped in a `sut` module so that the
//! `use super::geom::…` inside `pixels.rs` resolves.
//!
//! What this buys is real: coordinate arithmetic across a negative-origin
//! virtual desktop, mixed-DPI scale conversion, stride handling, and the window
//! filtering rules are the parts most likely to be wrong and the parts that
//! never need an OS call to check. What it does not buy is equally real, and is
//! listed at the bottom of the file.

#![allow(clippy::float_cmp)]

// `#[path = "."]` keeps the inner paths relative to `tests/` itself; without
// it rustc would look inside a `tests/sut/` directory that does not exist.
#[path = "."]
mod sut {
    #[path = "../src/windows/geom.rs"]
    pub mod geom;

    #[path = "../src/windows/filter.rs"]
    pub mod filter;

    #[path = "../src/windows/pixels.rs"]
    pub mod pixels;
}

use scrozz_core::{LogicalRect, Point, ScaleFactor, ShadowSupport, Size, WindowSelection};
use sut::filter::{self, Rejection, WindowFacts};
use sut::geom::{self, DeviceRect};
use sut::pixels::{self, PlaneRef};

#[test]
fn window_picker_capabilities_match_each_windows_capture_path() {
    let gdi = pixels::window_picking_capability(false);
    let wgc = pixels::window_picking_capability(true);

    assert_eq!(gdi.selection, WindowSelection::InProcess);
    assert_eq!(wgc.selection, WindowSelection::InProcess);
    assert!(!gdi.native_alpha);
    assert!(wgc.native_alpha);
    assert!(matches!(gdi.shadow, ShadowSupport::AlwaysExcluded { .. }));
    assert!(matches!(wgc.shadow, ShadowSupport::AlwaysExcluded { .. }));
    assert!(!gdi.shadow.resolve(true));
    assert!(!wgc.shadow.resolve(true));
}

// ---------------------------------------------------------------------------
// Coordinate arithmetic
// ---------------------------------------------------------------------------

#[test]
fn rect_dimensions_survive_a_negative_origin() {
    // A 1920x1080 monitor placed to the left of the primary starts at -1920.
    // Getting this wrong is not a rounding error; it silently captures the
    // wrong screen.
    let left_monitor = DeviceRect::new(-1920, 0, 0, 1080);
    assert_eq!(left_monitor.width(), 1920);
    assert_eq!(left_monitor.height(), 1080);
    assert!(!left_monitor.is_empty());

    // Above the primary, too.
    let above = DeviceRect::from_origin_size(-400, -1080, 1920, 1080);
    assert_eq!(above.width(), 1920);
    assert_eq!(above.height(), 1080);
    assert_eq!(above.bottom, 0);
}

#[test]
fn area_does_not_overflow_on_a_large_desktop() {
    // Four 4K monitors is 8192 x 4320 = ~35M pixels, which fits in i32, but a
    // 32-bit multiply of two i32 dimensions does not in general. The i64 return
    // type is what makes `dominant_monitor` safe on big desktops.
    let huge = DeviceRect::new(-16384, -16384, 16384, 16384);
    assert_eq!(huge.area(), 32768i64 * 32768i64);
}

#[test]
fn intersection_and_union_behave_across_the_origin() {
    let a = DeviceRect::new(-100, -100, 100, 100);
    let b = DeviceRect::new(-50, -50, 200, 200);

    assert_eq!(a.intersection(b), Some(DeviceRect::new(-50, -50, 100, 100)));
    assert_eq!(a.union(b), DeviceRect::new(-100, -100, 200, 200));

    // Touching edges do not overlap.
    let c = DeviceRect::new(100, -100, 200, 100);
    assert_eq!(a.intersection(c), None);
}

#[test]
fn empty_rects_never_report_an_intersection() {
    let empty = DeviceRect::new(10, 10, 10, 50);
    let real = DeviceRect::new(0, 0, 100, 100);
    assert!(empty.is_empty());
    assert_eq!(real.intersection(empty), None);
}

#[test]
fn offset_from_rebases_onto_a_monitor_origin() {
    let monitor = DeviceRect::new(-1920, -200, 0, 880);
    let window = DeviceRect::new(-1900, -100, -1500, 300);
    // Relative to the monitor's own top-left, the window is at (20, 100).
    assert_eq!(
        window.offset_from(monitor),
        DeviceRect::new(20, 100, 420, 500)
    );
}

#[test]
fn contains_is_half_open() {
    let r = DeviceRect::new(0, 0, 10, 10);
    assert!(r.contains(0, 0));
    assert!(r.contains(9, 9));
    assert!(!r.contains(10, 10));
    assert!(!r.contains(-1, 5));
}

// ---------------------------------------------------------------------------
// DPI
// ---------------------------------------------------------------------------

#[test]
fn windows_dpi_values_map_to_the_scale_factors_users_see() {
    // These are the exact values the Display settings slider produces.
    assert_eq!(geom::scale_from_dpi(96).get(), 1.0);
    assert_eq!(geom::scale_from_dpi(120).get(), 1.25);
    assert_eq!(geom::scale_from_dpi(144).get(), 1.5);
    assert_eq!(geom::scale_from_dpi(168).get(), 1.75);
    assert_eq!(geom::scale_from_dpi(192).get(), 2.0);
    assert_eq!(geom::scale_from_dpi(240).get(), 2.5);
}

#[test]
fn a_failed_dpi_query_falls_back_to_100_percent_rather_than_panicking() {
    // `GetDpiForMonitor` leaves its out-parameters untouched on failure, so a
    // zero here is not hypothetical. `ScaleFactor::new(0.0)` would be a bad
    // value to propagate and dividing by it would produce infinities.
    assert_eq!(geom::scale_from_dpi(0).get(), 1.0);
}

#[test]
fn device_and_logical_round_trip_within_one_monitor() {
    // The backend's convention is that each monitor's logical rectangle is its
    // own device rectangle divided by its own scale. This test is what makes
    // that convention worth having: the round trip is exact, so a capture is
    // never off by a pixel because of a coordinate conversion.
    for dpi in [96u32, 120, 144, 192] {
        let scale = geom::scale_from_dpi(dpi);
        let device = DeviceRect::new(-1920, -1080, 1920, 1080);
        let logical = geom::logical_from_device(device, scale);
        let back = geom::device_from_logical(logical, scale);
        assert_eq!(back, device, "round trip failed at {dpi} dpi");
    }
}

#[test]
fn mixed_dpi_desktops_have_no_single_scale() {
    // The whole reason `Display.scale` is per-display: a 1.0 laptop screen
    // beside a 1.5 external monitor is an ordinary configuration, and any code
    // that assumes one app-wide factor is wrong on it.
    let mixed = [ScaleFactor::new(1.0), ScaleFactor::new(1.5)];
    assert!(!geom::uniform_scale(&mixed));
    assert_eq!(geom::max_scale(&mixed).get(), 1.5);

    let same = [ScaleFactor::new(2.0), ScaleFactor::new(2.0)];
    assert!(geom::uniform_scale(&same));

    // An empty set is trivially uniform and scales at 1.0, so the composite
    // path cannot divide by zero on a machine mid-way through a display change.
    assert!(geom::uniform_scale(&[]));
    assert_eq!(geom::max_scale(&[]).get(), 1.0);
}

// ---------------------------------------------------------------------------
// Monitor selection
// ---------------------------------------------------------------------------

/// A two-monitor desktop with the secondary to the *left* of the primary, which
/// is what produces negative coordinates.
fn negative_origin_desktop() -> Vec<DeviceRect> {
    vec![
        DeviceRect::from_origin_size(0, 0, 2560, 1440),
        DeviceRect::from_origin_size(-1920, 0, 1920, 1080),
    ]
}

#[test]
fn a_window_belongs_to_the_monitor_showing_most_of_it() {
    let monitors = negative_origin_desktop();

    let on_primary = DeviceRect::from_origin_size(100, 100, 800, 600);
    assert_eq!(geom::dominant_monitor(on_primary, &monitors), Some(0));

    let on_secondary = DeviceRect::from_origin_size(-1800, 100, 800, 600);
    assert_eq!(geom::dominant_monitor(on_secondary, &monitors), Some(1));

    // Straddling: 700 px on the secondary, 300 on the primary.
    let straddling = DeviceRect::from_origin_size(-700, 100, 1000, 600);
    assert_eq!(geom::dominant_monitor(straddling, &monitors), Some(1));
}

#[test]
fn a_minimised_window_still_resolves_to_a_display() {
    // Windows parks minimised windows at (-32000, -32000). Returning `None`
    // there would drop them from enumeration entirely; the nearest-centre
    // fallback gives them a plausible display id instead.
    let monitors = negative_origin_desktop();
    let minimised = DeviceRect::from_origin_size(-32000, -32000, 160, 28);
    assert_eq!(geom::dominant_monitor(minimised, &monitors), Some(1));
}

#[test]
fn dominant_monitor_reports_nothing_when_there_are_no_monitors() {
    assert_eq!(
        geom::dominant_monitor(DeviceRect::new(0, 0, 10, 10), &[]),
        None
    );
}

#[test]
fn the_virtual_desktop_bounding_box_keeps_its_negative_origin() {
    let bounds = geom::virtual_desktop_bounds(&negative_origin_desktop());
    assert_eq!(bounds, DeviceRect::new(-1920, 0, 2560, 1440));
    assert_eq!(bounds.width(), 4480);
}

#[test]
fn logical_desktop_bounds_span_every_display() {
    let displays = vec![
        LogicalRect {
            origin: Point::new(0.0, 0.0),
            size: Size::new(2560.0, 1440.0),
        },
        LogicalRect {
            origin: Point::new(-1280.0, 100.0),
            size: Size::new(1280.0, 720.0),
        },
    ];
    let bounds = geom::logical_desktop_bounds(&displays).expect("two displays");
    assert_eq!(bounds.origin.x, -1280.0);
    assert_eq!(bounds.origin.y, 0.0);
    assert_eq!(bounds.size.width, 3840.0);
    assert_eq!(bounds.size.height, 1440.0);

    assert!(geom::logical_desktop_bounds(&[]).is_none());
}

// ---------------------------------------------------------------------------
// Region mapping
// ---------------------------------------------------------------------------

#[test]
fn a_region_maps_into_its_monitors_own_pixel_grid() {
    // The same logical rectangle is a different number of pixels on each
    // screen, which is exactly why region capture has to resolve a monitor
    // before it can crop anything.
    let monitor = LogicalRect {
        origin: Point::new(-1280.0, 0.0),
        size: Size::new(1280.0, 720.0),
    };
    let region = LogicalRect {
        origin: Point::new(-1180.0, 50.0),
        size: Size::new(200.0, 100.0),
    };

    let at_100 = geom::region_within_monitor(region, monitor, ScaleFactor::new(1.0));
    assert_eq!(at_100, DeviceRect::new(100, 50, 300, 150));

    let at_150 = geom::region_within_monitor(region, monitor, ScaleFactor::new(1.5));
    assert_eq!(at_150, DeviceRect::new(150, 75, 450, 225));
    assert_eq!(at_150.width(), 300);
}

#[test]
fn a_region_is_clipped_to_the_monitor_it_lands_on() {
    let monitor = LogicalRect {
        origin: Point::new(0.0, 0.0),
        size: Size::new(1000.0, 1000.0),
    };
    // Hangs off the right and bottom edges.
    let region = LogicalRect {
        origin: Point::new(900.0, 900.0),
        size: Size::new(500.0, 500.0),
    };
    let clipped = geom::region_within_monitor(region, monitor, ScaleFactor::new(1.0));
    assert_eq!(clipped, DeviceRect::new(900, 900, 1000, 1000));
}

#[test]
fn a_region_off_the_monitor_entirely_is_empty_not_negative() {
    let monitor = LogicalRect {
        origin: Point::new(0.0, 0.0),
        size: Size::new(1000.0, 1000.0),
    };
    let elsewhere = LogicalRect {
        origin: Point::new(-500.0, -500.0),
        size: Size::new(100.0, 100.0),
    };
    let empty = geom::region_within_monitor(elsewhere, monitor, ScaleFactor::new(1.0));
    assert!(empty.is_empty());
    // A negative width here would allocate a nonsense buffer downstream.
    assert!(empty.width() >= 0 && empty.height() >= 0);
}

#[test]
fn dominant_monitor_logical_picks_the_screen_a_selection_is_mostly_on() {
    let displays = vec![
        LogicalRect {
            origin: Point::new(0.0, 0.0),
            size: Size::new(1920.0, 1080.0),
        },
        LogicalRect {
            origin: Point::new(-1280.0, 0.0),
            size: Size::new(1280.0, 720.0),
        },
    ];
    let mostly_left = LogicalRect {
        origin: Point::new(-800.0, 10.0),
        size: Size::new(1000.0, 200.0),
    };
    assert_eq!(
        geom::dominant_monitor_logical(mostly_left, &displays),
        Some(1)
    );
}

// ---------------------------------------------------------------------------
// Composite placement
// ---------------------------------------------------------------------------

#[test]
fn a_low_dpi_monitor_is_scaled_up_into_a_high_dpi_composite() {
    // 1920x1080 at 100%, composited into a canvas built at 150% because a
    // sharper monitor is present. It has to occupy 150% of its logical size or
    // it lands in the wrong place and leaves a gap.
    let monitor = DeviceRect::from_origin_size(0, 0, 1920, 1080);
    let placed = geom::placement_in_composite(
        monitor,
        ScaleFactor::new(1.0),
        (0.0, 0.0),
        ScaleFactor::new(1.5),
    );
    assert_eq!(placed, DeviceRect::new(0, 0, 2880, 1620));
}

#[test]
fn composite_placement_is_exact_at_a_single_scale() {
    // The overwhelmingly common case. Any rounding here would be a visible
    // seam between monitors in an all-displays capture.
    let monitor = DeviceRect::from_origin_size(-1920, 0, 1920, 1080);
    let placed = geom::placement_in_composite(
        monitor,
        ScaleFactor::new(1.0),
        (-1920.0, 0.0),
        ScaleFactor::new(1.0),
    );
    assert_eq!(placed, DeviceRect::new(0, 0, 1920, 1080));
}

#[test]
fn composite_placement_rebases_a_negative_desktop_origin_to_zero() {
    // An image cannot have a negative origin, so the leftmost monitor must land
    // at x = 0 and everything else must shift with it.
    let secondary = DeviceRect::from_origin_size(-1920, 0, 1920, 1080);
    let primary = DeviceRect::from_origin_size(0, 0, 2560, 1440);
    let origin = (-1920.0, 0.0);
    let scale = ScaleFactor::new(1.0);

    let a = geom::placement_in_composite(secondary, scale, origin, scale);
    let b = geom::placement_in_composite(primary, scale, origin, scale);

    assert_eq!(a.left, 0);
    assert_eq!(b.left, 1920);
    assert_eq!(a.intersection(b), None, "monitors must not overlap");
}

// ---------------------------------------------------------------------------
// Window filtering
// ---------------------------------------------------------------------------

/// A window that should pass every rule, to be spoiled one field at a time.
fn good_window() -> WindowFacts {
    WindowFacts {
        visible: true,
        minimized: false,
        cloaked: false,
        ex_style: 0,
        is_root_owner: true,
        is_shell_window: false,
        class_name: "Chrome_WidgetWin_1".to_string(),
        title: "Scrozz — a screenshot tool".to_string(),
        width: 1200,
        height: 800,
    }
}

#[test]
fn an_ordinary_application_window_is_listed() {
    assert_eq!(filter::classify(&good_window()), Ok(()));
    assert!(filter::is_capturable(&good_window()));
}

#[test]
fn every_rejection_has_a_case_that_triggers_it() {
    // Written as a table so that adding a `Rejection` variant without a test is
    // obvious in review.
    let cases: Vec<(Rejection, WindowFacts)> = vec![
        (
            Rejection::NotVisible,
            WindowFacts {
                visible: false,
                ..good_window()
            },
        ),
        (
            Rejection::Minimized,
            WindowFacts {
                minimized: true,
                ..good_window()
            },
        ),
        (
            // The big one: suspended UWP apps stay in the window list forever
            // and are invisible. Without this rule the picker is mostly ghosts.
            Rejection::Cloaked,
            WindowFacts {
                cloaked: true,
                ..good_window()
            },
        ),
        (
            Rejection::ShellWindow,
            WindowFacts {
                is_shell_window: true,
                ..good_window()
            },
        ),
        (
            Rejection::IgnoredClass,
            WindowFacts {
                class_name: "Shell_TrayWnd".to_string(),
                ..good_window()
            },
        ),
        (
            Rejection::NotRootOwner,
            WindowFacts {
                is_root_owner: false,
                ..good_window()
            },
        ),
        (
            Rejection::ToolWindow,
            WindowFacts {
                ex_style: filter::WS_EX_TOOLWINDOW,
                ..good_window()
            },
        ),
        (
            Rejection::Untitled,
            WindowFacts {
                title: "   ".to_string(),
                ..good_window()
            },
        ),
        (
            Rejection::TooSmall,
            WindowFacts {
                width: 1,
                height: 1,
                ..good_window()
            },
        ),
    ];

    for (expected, facts) in cases {
        assert_eq!(
            filter::classify(&facts),
            Err(expected),
            "wrong rejection for {facts:?}"
        );
    }
}

#[test]
fn ws_ex_appwindow_overrides_ws_ex_toolwindow() {
    // Electron and Qt applications routinely set both; `WS_EX_APPWINDOW` is an
    // explicit request to be treated as a real window and must win, or those
    // apps disappear from the picker.
    let facts = WindowFacts {
        ex_style: filter::WS_EX_TOOLWINDOW | filter::WS_EX_APPWINDOW,
        ..good_window()
    };
    assert_eq!(filter::classify(&facts), Ok(()));
}

#[test]
fn shell_furniture_classes_are_recognised() {
    assert!(filter::is_ignored_class("Shell_TrayWnd"));
    assert!(filter::is_ignored_class("Progman"));
    assert!(filter::is_ignored_class("WorkerW"));
    assert!(!filter::is_ignored_class("Chrome_WidgetWin_1"));
    assert!(!filter::is_ignored_class(""));
}

#[test]
fn a_no_redirection_bitmap_window_is_listed_but_flagged_for_wgc() {
    // A WinUI 3 window is perfectly capturable — by WGC. It must stay in the
    // picker, and the GDI path must know in advance that it will get black.
    let winui = WindowFacts {
        ex_style: filter::WS_EX_NOREDIRECTIONBITMAP,
        ..good_window()
    };
    assert_eq!(filter::classify(&winui), Ok(()));
    assert!(!filter::gdi_can_capture(&winui));
    assert!(filter::gdi_can_capture(&good_window()));
}

#[test]
fn rejection_order_reports_the_most_informative_reason() {
    // A window that is both invisible and tiny is reported as invisible,
    // because that is the fact that explains it; "too small" would send someone
    // looking at the wrong thing.
    let facts = WindowFacts {
        visible: false,
        width: 1,
        height: 1,
        ..good_window()
    };
    assert_eq!(filter::classify(&facts), Err(Rejection::NotVisible));
}

#[test]
fn a_monitor_is_labelled_readably_rather_than_as_a_device_path() {
    assert_eq!(
        filter::display_label(r"\\.\DISPLAY1", true),
        "Display 1 (primary)"
    );
    assert_eq!(filter::display_label(r"\\.\DISPLAY2", false), "Display 2");

    // Anything unexpected is shown verbatim instead of being mangled into a
    // confident-looking lie.
    assert_eq!(filter::display_label(r"\\.\ODD", false), "ODD");
    assert_eq!(filter::display_label("", false), "Display");
    assert_eq!(filter::display_label(r"\\.\DISPLAY", false), "DISPLAY");
}

// ---------------------------------------------------------------------------
// Stride and pixels
// ---------------------------------------------------------------------------

#[test]
fn stride_arithmetic_matches_the_frame_contract() {
    assert_eq!(pixels::min_stride(100), 400);
    assert_eq!(pixels::buffer_len(512, 10), 5120);
    assert_eq!(pixels::buffer_len(0, 10), 0);
    assert_eq!(pixels::buffer_len(512, 0), 0);
}

#[test]
fn a_padded_row_pitch_is_preserved_rather_than_repacked() {
    // This is requirement 3. D3D11 hands back a `RowPitch` that is almost never
    // `width * 4` — 256-byte alignment is typical — and the padding must travel
    // with the buffer as `Frame.stride`. Repacking would cost a full pass over
    // every frame; ignoring it produces the classic diagonal skew.
    let width = 3u32;
    let height = 4u32;
    let src_stride = 64; // far wider than 3 * 4 = 12
    let mut src = vec![0u8; src_stride * height as usize];
    for row in 0..height as usize {
        src[row * src_stride] = row as u8 + 1;
        // Poison the padding so a repack would be detectable.
        src[row * src_stride + 40] = 0xAA;
    }

    let out = pixels::copy_rows_keeping_stride(&src, src_stride, height);
    assert_eq!(out.len(), src_stride * height as usize);
    assert_eq!(out, src, "padding must be preserved verbatim");

    // And the contract `Frame::is_well_formed` enforces still holds.
    assert!(src_stride >= pixels::min_stride(width));
    assert!(out.len() >= src_stride * height as usize);
}

#[test]
fn copying_rows_is_safe_when_the_source_is_short() {
    // A truncated mapped buffer must yield a short result, never a panic.
    let src = vec![7u8; 100];
    let out = pixels::copy_rows_keeping_stride(&src, 64, 10);
    assert!(out.len() <= 640);
}

/// Builds a `width` x `height` BGRA image where each pixel's blue channel is
/// its x and green is its y, with `pad` extra bytes on each row.
fn test_image(width: u32, height: u32, pad: usize) -> (Vec<u8>, usize) {
    let stride = pixels::min_stride(width) + pad;
    let mut buf = vec![0u8; pixels::buffer_len(stride, height)];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let i = y * stride + x * 4;
            buf[i] = x as u8;
            buf[i + 1] = y as u8;
            buf[i + 2] = 0x40;
            buf[i + 3] = 0xFF;
        }
    }
    (buf, stride)
}

#[test]
fn cropping_repacks_tightly_and_takes_the_right_pixels() {
    let (src, stride) = test_image(16, 16, 48);
    let (out, out_stride, w, h) = pixels::crop(&src, stride, 16, 16, DeviceRect::new(4, 4, 12, 12));

    assert_eq!((w, h), (8, 8));
    assert_eq!(out_stride, 32, "a crop must be tightly packed");
    assert_eq!(out.len(), 32 * 8);

    // Top-left of the crop is source pixel (4, 4).
    assert_eq!(out[0], 4);
    assert_eq!(out[1], 4);
    // Bottom-right is (11, 11).
    let last = 7 * out_stride + 7 * 4;
    assert_eq!(out[last], 11);
    assert_eq!(out[last + 1], 11);
}

#[test]
fn a_crop_beyond_the_source_is_clamped() {
    let (src, stride) = test_image(8, 8, 0);
    let (out, out_stride, w, h) = pixels::crop(&src, stride, 8, 8, DeviceRect::new(4, 4, 100, 100));
    assert_eq!((w, h), (4, 4));
    assert_eq!(out.len(), out_stride * 4);
}

#[test]
fn a_crop_with_no_overlap_is_empty_so_the_caller_must_reject_it() {
    let (src, stride) = test_image(8, 8, 0);
    let (out, out_stride, w, h) =
        pixels::crop(&src, stride, 8, 8, DeviceRect::new(-100, -100, -50, -50));
    assert!(out.is_empty());
    assert_eq!((out_stride, w, h), (0, 0, 0));
}

#[test]
fn forcing_alpha_opaque_does_not_touch_colour_or_padding() {
    // The GDI fallback needs this because `BitBlt` leaves alpha undefined; a
    // PNG straight from `BitBlt` is otherwise fully transparent.
    let width = 4u32;
    let height = 2u32;
    let stride = pixels::min_stride(width) + 8;
    let mut buf = vec![0u8; pixels::buffer_len(stride, height)];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let before = buf.clone();

    pixels::force_opaque_alpha(&mut buf, stride, width, height);

    for y in 0..height as usize {
        for x in 0..width as usize {
            let i = y * stride + x * 4;
            assert_eq!(buf[i], before[i], "blue changed");
            assert_eq!(buf[i + 1], before[i + 1], "green changed");
            assert_eq!(buf[i + 2], before[i + 2], "red changed");
            assert_eq!(buf[i + 3], 0xFF, "alpha not opaque");
        }
        // Padding beyond the last pixel is left alone.
        let pad = y * stride + width as usize * 4;
        assert_eq!(buf[pad], before[pad]);
    }
}

#[test]
fn detecting_alpha_distinguishes_a_real_capture_from_a_blank_one() {
    // The GDI window path uses this to notice that `PrintWindow` produced
    // nothing at all, which is what happens on a window with no redirection
    // surface.
    let width = 4u32;
    let height = 2u32;
    let stride = pixels::min_stride(width);
    let blank = vec![0u8; pixels::buffer_len(stride, height)];
    assert!(!pixels::has_any_alpha(&blank, stride, width, height));

    let mut one_pixel = blank.clone();
    one_pixel[stride + 4 + 3] = 1;
    assert!(pixels::has_any_alpha(&one_pixel, stride, width, height));

    // Alpha in the row padding must not count as a real pixel.
    let mut padded = vec![0u8; pixels::buffer_len(stride + 8, height)];
    padded[width as usize * 4 + 3] = 0xFF;
    assert!(!pixels::has_any_alpha(&padded, stride + 8, width, height));
}

#[test]
fn a_one_to_one_blit_is_pixel_exact() {
    // The single-scale case, which is most desktops. Nearest-neighbour must be
    // an identity here or every all-displays capture is subtly resampled.
    let (src, src_stride) = test_image(8, 8, 16);
    let dst_stride = pixels::min_stride(8);
    let mut dst = vec![0u8; pixels::buffer_len(dst_stride, 8)];

    pixels::blit_nearest(
        &mut pixels::Plane {
            data: &mut dst,
            stride: dst_stride,
            width: 8,
            height: 8,
        },
        DeviceRect::new(0, 0, 8, 8),
        &PlaneRef {
            data: &src,
            stride: src_stride,
            width: 8,
            height: 8,
        },
    );

    for y in 0..8usize {
        for x in 0..8usize {
            let d = y * dst_stride + x * 4;
            assert_eq!(dst[d], x as u8, "blue at ({x}, {y})");
            assert_eq!(dst[d + 1], y as u8, "green at ({x}, {y})");
        }
    }
}

#[test]
fn a_blit_at_an_offset_lands_where_it_was_told_to() {
    let (src, src_stride) = test_image(4, 4, 0);
    let dst_stride = pixels::min_stride(16);
    let mut dst = vec![0u8; pixels::buffer_len(dst_stride, 16)];

    pixels::blit_nearest(
        &mut pixels::Plane {
            data: &mut dst,
            stride: dst_stride,
            width: 16,
            height: 16,
        },
        DeviceRect::from_origin_size(8, 8, 4, 4),
        &PlaneRef {
            data: &src,
            stride: src_stride,
            width: 4,
            height: 4,
        },
    );

    // Inside the placement.
    assert_eq!(dst[8 * dst_stride + 8 * 4 + 3], 0xFF);
    // Outside it, untouched.
    assert_eq!(dst[0], 0);
    assert_eq!(dst[7 * dst_stride + 7 * 4 + 3], 0);
}

#[test]
fn a_blit_partly_off_canvas_clips_instead_of_panicking() {
    // A monitor arrangement can put a display's placement partly outside a
    // canvas that was rounded a pixel short; an out-of-bounds write here would
    // be a crash in the middle of taking a screenshot.
    let (src, src_stride) = test_image(8, 8, 0);
    let dst_stride = pixels::min_stride(8);
    let mut dst = vec![0u8; pixels::buffer_len(dst_stride, 8)];

    pixels::blit_nearest(
        &mut pixels::Plane {
            data: &mut dst,
            stride: dst_stride,
            width: 8,
            height: 8,
        },
        DeviceRect::from_origin_size(-4, -4, 8, 8),
        &PlaneRef {
            data: &src,
            stride: src_stride,
            width: 8,
            height: 8,
        },
    );

    // The bottom-right quadrant of the source landed in the top-left.
    assert_eq!(dst[0], 4);
    assert_eq!(dst[1], 4);
}

#[test]
fn upscaling_a_low_dpi_monitor_fills_its_whole_placement() {
    // 4x4 source into an 8x8 placement, which is the 100% into 200% case.
    let (src, src_stride) = test_image(4, 4, 0);
    let dst_stride = pixels::min_stride(8);
    let mut dst = vec![0u8; pixels::buffer_len(dst_stride, 8)];

    pixels::blit_nearest(
        &mut pixels::Plane {
            data: &mut dst,
            stride: dst_stride,
            width: 8,
            height: 8,
        },
        DeviceRect::new(0, 0, 8, 8),
        &PlaneRef {
            data: &src,
            stride: src_stride,
            width: 4,
            height: 4,
        },
    );

    // Every destination pixel was written.
    for y in 0..8usize {
        for x in 0..8usize {
            assert_eq!(dst[y * dst_stride + x * 4 + 3], 0xFF, "hole at ({x}, {y})");
        }
    }
    // And each 2x2 block came from one source pixel.
    assert_eq!(dst[0], 0);
    assert_eq!(dst[4], 0);
    assert_eq!(dst[8], 1);
}

#[test]
fn a_blit_from_an_empty_source_is_a_no_op() {
    let dst_stride = pixels::min_stride(4);
    let mut dst = vec![9u8; pixels::buffer_len(dst_stride, 4)];
    let before = dst.clone();

    pixels::blit_nearest(
        &mut pixels::Plane {
            data: &mut dst,
            stride: dst_stride,
            width: 4,
            height: 4,
        },
        DeviceRect::new(0, 0, 4, 4),
        &PlaneRef {
            data: &[],
            stride: 0,
            width: 0,
            height: 0,
        },
    );
    assert_eq!(dst, before);
}

#[test]
fn flipping_reverses_row_order_and_leaves_odd_middles_alone() {
    let stride = 4;
    let mut buf = vec![1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3];
    pixels::flip_vertical(&mut buf, stride, 3);
    assert_eq!(buf, vec![3, 3, 3, 3, 2, 2, 2, 2, 1, 1, 1, 1]);

    // Degenerate inputs must not panic.
    let mut one = vec![5u8; 4];
    pixels::flip_vertical(&mut one, 4, 1);
    assert_eq!(one, vec![5u8; 4]);

    let mut short = vec![0u8; 3];
    pixels::flip_vertical(&mut short, 4, 2);
}

// ---------------------------------------------------------------------------
// Live smoke tests
// ---------------------------------------------------------------------------
//
// Everything above runs everywhere. What follows needs a real desktop, so it is
// compiled only on Windows. It is deliberately shallow — it asserts the shapes
// the rest of the application depends on, not pixel values, because a test
// machine's screen contents are not knowable.

#[cfg(target_os = "windows")]
mod live {
    #[test]
    fn every_display_has_a_sane_scale_and_a_work_area_inside_its_bounds() {
        let backend = scrozz_capture::backend().expect("a backend");
        let displays = backend.displays().expect("at least one display");
        assert!(!displays.is_empty());

        for d in &displays {
            // A scale outside this range means `GetDpiForMonitor` was
            // misread; Windows offers 100% to 500%.
            assert!(
                d.scale.get() >= 1.0 && d.scale.get() <= 5.0,
                "implausible scale {} on {}",
                d.scale.get(),
                d.name
            );
            assert!(d.bounds.size.width > 0.0 && d.bounds.size.height > 0.0);

            // Requirement 2: the work area excludes the taskbar, so it is
            // contained by the bounds and usually strictly smaller on the
            // display that hosts it.
            assert!(d.work_area.origin.x >= d.bounds.origin.x);
            assert!(d.work_area.origin.y >= d.bounds.origin.y);
            assert!(
                d.work_area.origin.x + d.work_area.size.width
                    <= d.bounds.origin.x + d.bounds.size.width
            );
            assert!(
                d.work_area.origin.y + d.work_area.size.height
                    <= d.bounds.origin.y + d.bounds.size.height
            );
        }

        assert_eq!(
            displays.iter().filter(|d| d.is_primary).count(),
            1,
            "exactly one display is primary"
        );
    }

    #[test]
    fn enumerated_windows_are_things_a_user_could_point_at() {
        let backend = scrozz_capture::backend().expect("a backend");
        let windows = backend.windows().expect("window enumeration");
        for w in &windows {
            assert!(w.is_visible);
            assert!(w.bounds.size.width > 0.0 && w.bounds.size.height > 0.0);
            assert!(
                w.title.as_deref().is_none_or(|t| !t.trim().is_empty()),
                "a window with a blank title reached the picker"
            );
        }
    }

    #[test]
    fn a_display_capture_is_well_formed_bgra() {
        use scrozz_core::{CaptureRequest, CaptureTarget, CursorMode, PixelFormat, Provenance};

        let backend = scrozz_capture::backend().expect("a backend");
        let display = backend.active_display().expect("an active display");
        let capture = backend
            .capture(&CaptureRequest {
                target: CaptureTarget::Display(display.id.clone()),
                cursor: CursorMode::Hidden,
                include_window_shadow: false,
            })
            .expect("a capture");

        assert_eq!(capture.provenance, Provenance::Display);
        // WGC yields premultiplied BGRA; the GDI fallback yields opaque BGRA.
        // Asserting one would fail on whichever machine took the other path,
        // so this pins what actually matters: four-byte BGRA, never a silent
        // swizzle to RGBA.
        assert!(
            matches!(
                capture.frame.format,
                PixelFormat::Bgra8 | PixelFormat::BgraPremultiplied8
            ),
            "unexpected format {:?}",
            capture.frame.format
        );
        // Requirement 3 again, this time against a real `RowPitch`.
        assert!(
            capture.frame.is_well_formed(),
            "stride {} for width {}",
            capture.frame.stride,
            capture.frame.width()
        );
        assert!(capture.frame.stride >= capture.frame.width() as usize * 4);
    }
}

// ---------------------------------------------------------------------------
// What these tests cannot prove
// ---------------------------------------------------------------------------
//
// Stated explicitly so nobody mistakes a green run for a working backend:
//
// - **Pixel correctness.** Nothing here has ever seen a real frame. That the
//   stride arithmetic is right does not prove the D3D11 readback reads the
//   right texture.
// - **The two constants win32metadata omits.** `PW_RENDERFULLCONTENT` and
//   `MONITORINFOF_PRIMARY` have no generated binding and are spelled out at
//   their use sites. Both are fixed by the Win32 ABI and checkable against
//   MSDN by eye, but a wrong value would compile and misbehave silently. Every
//   COM interface and every other constant now comes from the generated
//   bindings, so their slots and values are the crate's problem, not ours.
// - **Yellow-border suppression.** `SetIsBorderRequired(false)` needs Windows
//   11; whether it takes effect is unobservable from here.
// - **Mixed-DPI behaviour in the wild.** The arithmetic is tested; that
//   `GetDpiForMonitor` returns what is expected under per-monitor-v2 awareness
//   is not.
// - **Cloaked-window filtering against a live shell.** The predicate is tested
//   against synthetic facts; whether `DWMWA_CLOAKED` reports what is assumed
//   for a suspended UWP app is not.
// - **The GDI fallback's fidelity**, including whether `PW_RENDERFULLCONTENT`
//   rescues a given application.
