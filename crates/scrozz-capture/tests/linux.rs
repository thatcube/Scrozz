//! Tests for the Linux capture backends' platform-independent logic.
//!
//! # Why these run everywhere
//!
//! `src/linux/` is behind `#[cfg(target_os = "linux")]`, so on any other host it
//! is not merely untested — it is not even parsed. That is the wrong trade for a
//! backend whose hard parts are *arithmetic and byte layout*: `_NET_WORKAREA`
//! indexing, RandR's variable-length monitor records, scanline padding, session
//! detection, restore-token persistence. None of that needs an X server, and all
//! of it is exactly where the bugs live.
//!
//! So every such piece was written as a module that imports only `std` and
//! `scrozz_core`, and this file pulls those modules in by path. The result is
//! that the logic compiles and runs on macOS, Windows and CI, and a Linux box is
//! needed only for the things that genuinely require one: an actual X
//! connection, a real portal, real pixels.
//!
//! The `#[path]` on each inline module block sets the directory its children
//! resolve against, and is itself relative to `tests/`.

#[path = "../src/linux/session.rs"]
mod session;

#[path = "../src/linux/x11"]
mod x11 {
    #[path = "ewmh.rs"]
    pub mod ewmh;

    #[path = "layout.rs"]
    pub mod layout;

    #[path = "pixels.rs"]
    pub mod pixels;

    #[path = "scale.rs"]
    pub mod scale;

    #[path = "wire.rs"]
    pub mod wire;
}

#[path = "../src/linux/wayland"]
mod wayland {
    #[path = "restore.rs"]
    pub mod restore;

    #[path = "portal.rs"]
    pub mod portal;

    #[path = "region.rs"]
    pub mod region;

    #[path = "pipewire"]
    pub mod pipewire {
        #[path = "pod.rs"]
        pub mod pod;

        #[path = "format.rs"]
        pub mod format;

        #[path = "lifecycle.rs"]
        pub mod lifecycle;
    }
}

// ---------------------------------------------------------------------------
// Session and compositor detection
// ---------------------------------------------------------------------------

mod session_detection {
    use super::session::{
        Compositor, SessionEnv, SessionKind, capabilities, describe, detect_compositor,
        detect_session,
    };

    fn env(wayland: Option<&str>, session_type: Option<&str>, display: Option<&str>) -> SessionEnv {
        SessionEnv {
            wayland_display: wayland.map(str::to_owned),
            xdg_session_type: session_type.map(str::to_owned),
            display: display.map(str::to_owned),
            ..SessionEnv::default()
        }
    }

    #[test]
    fn plain_x11_session() {
        assert_eq!(
            detect_session(&env(None, Some("x11"), Some(":0"))),
            SessionKind::X11
        );
    }

    #[test]
    fn wayland_without_xwayland() {
        assert_eq!(
            detect_session(&env(Some("wayland-0"), Some("wayland"), None)),
            SessionKind::Wayland
        );
    }

    /// The case that produces silently half-empty screenshots if it is got
    /// wrong: a Wayland session with XWayland running sets `DISPLAY` too.
    #[test]
    fn wayland_with_xwayland_is_not_x11() {
        assert_eq!(
            detect_session(&env(Some("wayland-0"), Some("wayland"), Some(":0"))),
            SessionKind::XWayland
        );
    }

    /// Login managers do set `XDG_SESSION_TYPE=x11` inside Wayland sessions.
    /// The socket wins.
    #[test]
    fn wayland_socket_beats_a_lying_session_type() {
        assert_eq!(
            detect_session(&env(Some("wayland-0"), Some("x11"), Some(":0"))),
            SessionKind::XWayland
        );
    }

    /// And the converse: no socket variable, but the session type says Wayland.
    #[test]
    fn session_type_alone_is_enough_for_wayland() {
        assert_eq!(
            detect_session(&env(None, Some("wayland"), None)),
            SessionKind::Wayland
        );
    }

    #[test]
    fn nothing_at_all_is_headless() {
        assert_eq!(
            detect_session(&SessionEnv::default()),
            SessionKind::Headless
        );
    }

    #[test]
    fn explicit_override_wins() {
        let mut e = env(Some("wayland-0"), Some("wayland"), Some(":0"));
        e.forced_backend = Some("X11".to_owned());
        assert_eq!(detect_session(&e), SessionKind::X11);

        let mut e = env(None, Some("x11"), Some(":0"));
        e.forced_backend = Some(" wayland ".to_owned());
        assert_eq!(detect_session(&e), SessionKind::Wayland);
    }

    #[test]
    fn unrecognised_override_is_ignored_rather_than_fatal() {
        let mut e = env(None, Some("x11"), Some(":0"));
        e.forced_backend = Some("mir".to_owned());
        assert_eq!(detect_session(&e), SessionKind::X11);
    }

    #[test]
    fn portal_is_required_exactly_for_wayland_sessions() {
        assert!(SessionKind::Wayland.requires_portal());
        assert!(SessionKind::XWayland.requires_portal());
        assert!(!SessionKind::X11.requires_portal());
        assert!(!SessionKind::Headless.requires_portal());
    }

    fn desktop(current: &str) -> SessionEnv {
        SessionEnv {
            xdg_current_desktop: Some(current.to_owned()),
            ..SessionEnv::default()
        }
    }

    #[test]
    fn vendor_prefixed_desktops_still_resolve() {
        for value in ["GNOME", "ubuntu:GNOME", "pop:GNOME", "GNOME-Classic:GNOME"] {
            assert_eq!(
                detect_compositor(&desktop(value)),
                Compositor::Mutter,
                "{value}"
            );
        }
    }

    #[test]
    fn kde_spellings() {
        for value in ["KDE", "plasma", "KDE:plasma"] {
            assert_eq!(
                detect_compositor(&desktop(value)),
                Compositor::KWin,
                "{value}"
            );
        }
    }

    #[test]
    fn wlroots_family() {
        for value in ["sway", "Hyprland", "river", "wayfire", "labwc", "niri"] {
            assert_eq!(
                detect_compositor(&desktop(value)),
                Compositor::Wlroots,
                "{value}"
            );
        }
    }

    #[test]
    fn unrecognised_desktop_is_named_not_guessed() {
        assert_eq!(
            detect_compositor(&desktop("XFCE")),
            Compositor::Other("XFCE".to_owned())
        );
    }

    #[test]
    fn session_desktop_is_the_fallback() {
        let e = SessionEnv {
            xdg_session_desktop: Some("sway".to_owned()),
            ..SessionEnv::default()
        };
        assert_eq!(detect_compositor(&e), Compositor::Wlroots);
    }

    /// A recognised name anywhere in the list beats an unrecognised one before
    /// it — `ubuntu:GNOME` must not resolve to `Other("ubuntu")`.
    #[test]
    fn recognised_component_beats_an_earlier_unknown_one() {
        assert_eq!(
            detect_compositor(&desktop("ubuntu:unity7:GNOME")),
            Compositor::Mutter
        );
    }

    #[test]
    fn empty_environment_yields_unknown() {
        assert_eq!(
            detect_compositor(&SessionEnv::default()),
            Compositor::Unknown
        );
        assert_eq!(detect_compositor(&desktop("  :  ")), Compositor::Unknown);
    }

    /// The single most important row in the matrix: no compositor on Wayland
    /// permits an application to enumerate windows.
    #[test]
    fn no_compositor_claims_window_enumeration() {
        for compositor in [
            Compositor::Mutter,
            Compositor::KWin,
            Compositor::Wlroots,
            Compositor::Other("XFCE".to_owned()),
            Compositor::Unknown,
        ] {
            assert!(
                !capabilities(&compositor).window_enumeration,
                "{compositor} must not claim window enumeration"
            );
        }
    }

    #[test]
    fn restore_tokens_on_the_three_supported_families() {
        assert!(capabilities(&Compositor::Mutter).restore_tokens);
        assert!(capabilities(&Compositor::KWin).restore_tokens);
        assert!(capabilities(&Compositor::Wlroots).restore_tokens);
    }

    /// Decision D8's documented wlroots gap.
    #[test]
    fn wlroots_has_no_global_shortcuts_portal() {
        assert!(!capabilities(&Compositor::Wlroots).global_shortcuts);
        assert!(capabilities(&Compositor::Mutter).global_shortcuts);
        assert!(capabilities(&Compositor::KWin).global_shortcuts);
    }

    /// The layer-shell finding, encoded so a future change to it is a
    /// deliberate edit with a failing test rather than a silent drift.
    #[test]
    fn mutter_does_not_implement_layer_shell() {
        assert!(!capabilities(&Compositor::Mutter).layer_shell);
        assert!(capabilities(&Compositor::KWin).layer_shell);
        assert!(capabilities(&Compositor::Wlroots).layer_shell);
    }

    #[test]
    fn unknown_compositors_are_treated_pessimistically() {
        let caps = capabilities(&Compositor::Unknown);
        assert!(!caps.restore_tokens);
        assert!(!caps.global_shortcuts);
        assert!(!caps.remote_desktop);
        assert!(!caps.layer_shell);
    }

    #[test]
    fn description_names_both_halves() {
        let e = SessionEnv {
            wayland_display: Some("wayland-0".to_owned()),
            xdg_current_desktop: Some("ubuntu:GNOME".to_owned()),
            ..SessionEnv::default()
        };
        assert_eq!(describe(&e), "Wayland on GNOME/Mutter");
    }
}

// ---------------------------------------------------------------------------
// EWMH / ICCCM property parsing
// ---------------------------------------------------------------------------

mod ewmh_properties {
    use super::x11::ewmh::{
        WireRect, application_name, apply_frame_extents, is_listable, parse_frame_extents,
        parse_i32_list, parse_latin1_name, parse_u32_list, parse_utf8_name, parse_wm_class,
        parse_wm_state, parse_work_area, stacking_to_front_first, wm_state,
    };

    /// Properties arrive in the connection's byte order, which x11rb negotiates
    /// as the host's, so fixtures are built the same way.
    fn cardinals(values: &[i32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_ne_bytes()).collect()
    }

    #[test]
    fn list_parsing_ignores_a_short_final_chunk() {
        let mut bytes = cardinals(&[1, 2, 3]);
        bytes.push(0xff);
        assert_eq!(parse_u32_list(&bytes), vec![1, 2, 3]);
        assert_eq!(parse_i32_list(&bytes), vec![1, 2, 3]);
    }

    #[test]
    fn empty_property_is_an_empty_list_not_an_error() {
        assert!(parse_u32_list(&[]).is_empty());
        assert!(parse_i32_list(&[]).is_empty());
    }

    #[test]
    fn negative_cardinals_survive_the_signed_read() {
        assert_eq!(parse_i32_list(&cardinals(&[-1920, 0])), vec![-1920, 0]);
    }

    /// `_NET_WORKAREA` is `CARDINAL[4n]`, one quadruple per virtual desktop.
    /// Indexing the wrong quadruple is the classic bug and puts the overlay on
    /// the wrong workspace's geometry.
    #[test]
    fn work_area_selects_the_current_desktop() {
        let bytes = cardinals(&[0, 27, 1920, 1053, 0, 0, 1920, 1080]);

        assert_eq!(
            parse_work_area(&bytes, 0),
            Some(WireRect {
                x: 0,
                y: 27,
                width: 1920,
                height: 1053
            })
        );
        assert_eq!(
            parse_work_area(&bytes, 1),
            Some(WireRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            })
        );
    }

    #[test]
    fn work_area_out_of_range_is_none_rather_than_a_panic() {
        let bytes = cardinals(&[0, 27, 1920, 1053]);
        assert_eq!(parse_work_area(&bytes, 4), None);
        assert_eq!(parse_work_area(&[], 0), None);
        assert_eq!(parse_work_area(&bytes, u32::MAX), None);
    }

    #[test]
    fn degenerate_work_area_is_rejected() {
        assert_eq!(parse_work_area(&cardinals(&[0, 0, 0, 1080]), 0), None);
        assert_eq!(parse_work_area(&cardinals(&[0, 0, 1920, -1]), 0), None);
    }

    #[test]
    fn utf8_titles_drop_trailing_nuls() {
        assert_eq!(parse_utf8_name(b"Firefox\0\0").as_deref(), Some("Firefox"));
        assert_eq!(parse_utf8_name(b"").as_deref(), None);
        assert_eq!(parse_utf8_name(b"\0\0").as_deref(), None);
    }

    #[test]
    fn utf8_titles_survive_a_bad_encoding() {
        let title = parse_utf8_name(&[b'a', 0xff, b'b']).expect("lossy decode");
        assert!(title.starts_with('a') && title.ends_with('b'));
    }

    /// `WM_NAME` is Latin-1, not UTF-8. Decoding it as UTF-8 mangles every
    /// accented character; the widening is the whole point of the function.
    #[test]
    fn legacy_titles_are_latin1() {
        assert_eq!(
            parse_latin1_name(&[0xe9, b'c', b'h']).as_deref(),
            Some("éch")
        );
    }

    #[test]
    fn wm_class_splits_on_nuls() {
        assert_eq!(
            parse_wm_class(b"Navigator\0Firefox\0"),
            Some(("Navigator".to_owned(), "Firefox".to_owned()))
        );
        assert_eq!(
            application_name(b"Navigator\0Firefox\0").as_deref(),
            Some("Firefox")
        );
    }

    #[test]
    fn wm_class_with_only_an_instance_still_names_something() {
        assert_eq!(application_name(b"xterm\0").as_deref(), Some("xterm"));
    }

    #[test]
    fn missing_wm_class_yields_no_name() {
        assert_eq!(parse_wm_class(b""), None);
        assert_eq!(application_name(b"\0\0"), None);
    }

    /// Client geometry excludes the window manager's decorations. Reporting it
    /// unmodified crops the title bar out of every window capture.
    #[test]
    fn frame_extents_grow_the_rectangle() {
        let extents = parse_frame_extents(&cardinals(&[1, 1, 37, 1])).expect("four cardinals");
        assert_eq!(extents, (1, 1, 37, 1));

        let client = WireRect {
            x: 100,
            y: 200,
            width: 800,
            height: 600,
        };
        assert_eq!(
            apply_frame_extents(client, extents),
            WireRect {
                x: 99,
                y: 163,
                width: 802,
                height: 638
            }
        );
    }

    #[test]
    fn frame_extents_need_all_four_values() {
        assert_eq!(parse_frame_extents(&cardinals(&[1, 1, 37])), None);
    }

    /// Some compositors report negative extents for shadow insets.
    #[test]
    fn negative_frame_extents_cannot_underflow_the_size() {
        let rect = WireRect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let shrunk = apply_frame_extents(rect, (-100, -100, -100, -100));
        assert_eq!(shrunk.width, 0);
        assert_eq!(shrunk.height, 0);
    }

    #[test]
    fn wm_state_reads_the_first_word_only() {
        assert_eq!(
            parse_wm_state(&cardinals(&[1, 0x0400_0001])),
            Some(wm_state::NORMAL)
        );
        assert_eq!(parse_wm_state(&[]), None);
    }

    /// A minimised window has no pixels; `GetImage` on it fails with
    /// `BadMatch`, so offering it in the picker offers a guaranteed failure.
    #[test]
    fn iconic_windows_are_not_listable() {
        assert!(!is_listable(Some(wm_state::ICONIC), true));
        assert!(!is_listable(Some(wm_state::WITHDRAWN), true));
        assert!(is_listable(Some(wm_state::NORMAL), true));
        assert!(!is_listable(Some(wm_state::NORMAL), false));
    }

    /// Override-redirect windows — menus, tooltips, notifications — have no
    /// `WM_STATE` and are legitimately capturable when mapped.
    #[test]
    fn unmanaged_windows_are_listable_when_mapped() {
        assert!(is_listable(None, true));
        assert!(!is_listable(None, false));
    }

    /// `_NET_CLIENT_LIST_STACKING` is bottom-to-top; the enumerator's contract
    /// is front-most first. Getting this backwards offers the wallpaper as the
    /// first choice in the picker.
    #[test]
    fn stacking_order_is_reversed_for_the_contract() {
        assert_eq!(stacking_to_front_first(vec![10, 20, 30]), vec![30, 20, 10]);
        assert!(stacking_to_front_first(Vec::new()).is_empty());
    }
}

// ---------------------------------------------------------------------------
// Pixel layout, stride and repacking
// ---------------------------------------------------------------------------

mod pixel_layout {
    use super::x11::pixels::{
        ByteLayout, all_planes, byte_layout, direct_format, repack, scanline_stride,
    };
    use scrozz_core::PixelFormat;

    /// A 32-bit scanline padded to 32-bit units is already aligned; the
    /// interesting case is a width whose byte count is not a multiple of the
    /// pad, which is where an assumed `width * 4` shears the image.
    #[test]
    fn stride_rounds_up_to_the_scanline_pad() {
        assert_eq!(scanline_stride(1920, 32, 32), Some(7680));
        assert_eq!(scanline_stride(3, 8, 32), Some(4));
        assert_eq!(scanline_stride(5, 8, 32), Some(8));
        assert_eq!(scanline_stride(5, 8, 8), Some(5));
    }

    #[test]
    fn zero_width_has_zero_stride() {
        assert_eq!(scanline_stride(0, 32, 32), Some(0));
    }

    #[test]
    fn an_impossible_pad_is_refused() {
        assert_eq!(scanline_stride(100, 32, 0), None);
        assert_eq!(scanline_stride(100, 32, 12), None);
    }

    #[test]
    fn overflow_is_refused_rather_than_panicking() {
        assert_eq!(scanline_stride(u32::MAX, 32, 32), Some(0x3_FFFF_FFFC));
    }

    /// The near-universal modern X visual: 8-8-8 at depth 24 on a
    /// little-endian server, which is B, G, R, pad in memory order.
    #[test]
    fn depth_24_bgrx_has_no_alpha() {
        let layout = byte_layout(0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 24, 32, true)
            .expect("standard TrueColor visual");
        assert_eq!(
            layout,
            ByteLayout {
                red: 2,
                green: 1,
                blue: 0,
                alpha: None,
                bytes_per_pixel: 4,
            }
        );
        assert_eq!(direct_format(&layout), Some(PixelFormat::Bgra8));
    }

    /// Depth 32 is where the fourth byte actually means something.
    #[test]
    fn depth_32_bgra_has_alpha() {
        let layout =
            byte_layout(0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 32, 32, true).expect("ARGB visual");
        assert_eq!(layout.alpha, Some(3));
    }

    #[test]
    fn msb_first_servers_reverse_the_byte_order() {
        let layout = byte_layout(0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 24, 32, false)
            .expect("big-endian TrueColor visual");
        assert_eq!(
            (layout.red, layout.green, layout.blue),
            (1, 2, 3),
            "R is byte 1 when the server sends most-significant first"
        );
        assert_eq!(direct_format(&layout), None, "not one of our formats");
    }

    #[test]
    fn rgbx_visuals_are_recognised_directly() {
        let layout =
            byte_layout(0x0000_00ff, 0x0000_ff00, 0x00ff_0000, 24, 32, true).expect("RGBX visual");
        assert_eq!(direct_format(&layout), Some(PixelFormat::Rgba8));
    }

    #[test]
    fn non_32_bit_visuals_are_declined() {
        assert_eq!(
            byte_layout(0xf800, 0x07e0, 0x001f, 16, 16, true),
            None,
            "16-bit 5-6-5 has no byte-aligned channels"
        );
        assert_eq!(
            byte_layout(0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 24, 24, true),
            None
        );
    }

    #[test]
    fn masks_that_are_not_byte_aligned_are_declined() {
        assert_eq!(
            byte_layout(0x0000_0ff0, 0x0000_ff00, 0x0000_00ff, 24, 32, true),
            None
        );
        assert_eq!(byte_layout(0, 0x0000_ff00, 0x0000_00ff, 24, 32, true), None);
    }

    #[test]
    fn overlapping_channels_are_declined() {
        assert_eq!(
            byte_layout(0x0000_00ff, 0x0000_00ff, 0x0000_ff00, 24, 32, true),
            None
        );
    }

    fn bgrx_layout() -> ByteLayout {
        ByteLayout {
            red: 2,
            green: 1,
            blue: 0,
            alpha: None,
            bytes_per_pixel: 4,
        }
    }

    /// The padding drop is the point: the source stride exceeds `width * 4`,
    /// and a naive copy would smear each row into the next.
    #[test]
    fn repack_removes_scanline_padding() {
        let layout = bgrx_layout();
        let src_stride = 12; // 2 pixels of real data, 4 bytes of padding.
        let mut src = vec![0u8; src_stride * 2];
        // Row 0: blue pixel, green pixel.
        src[0..4].copy_from_slice(&[0xff, 0x00, 0x00, 0x00]);
        src[4..8].copy_from_slice(&[0x00, 0xff, 0x00, 0x00]);
        src[8..12].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]); // padding
        // Row 1: red pixel, white pixel.
        src[12..16].copy_from_slice(&[0x00, 0x00, 0xff, 0x00]);
        src[16..20].copy_from_slice(&[0xff, 0xff, 0xff, 0x00]);
        src[20..24].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]); // padding

        let (out, format) = repack(&src, src_stride, 2, 2, &layout);
        assert_eq!(format, PixelFormat::Bgra8);
        assert_eq!(out.len(), 2 * 2 * 4);
        assert_eq!(&out[0..4], &[0xff, 0x00, 0x00, 0xff]);
        assert_eq!(&out[4..8], &[0x00, 0xff, 0x00, 0xff]);
        assert_eq!(&out[8..12], &[0x00, 0x00, 0xff, 0xff]);
        assert_eq!(&out[12..16], &[0xff, 0xff, 0xff, 0xff]);
    }

    /// At depth 24 X supplies an undefined fourth byte. Servers commonly leave
    /// it zero, so copying it through yields an entirely invisible image.
    #[test]
    fn depth_24_alpha_is_forced_opaque() {
        let src = vec![0x11, 0x22, 0x33, 0x00];
        let (out, _) = repack(&src, 4, 1, 1, &bgrx_layout());
        assert_eq!(out[3], 0xff);
    }

    #[test]
    fn depth_32_alpha_is_preserved() {
        let layout = ByteLayout {
            red: 2,
            green: 1,
            blue: 0,
            alpha: Some(3),
            bytes_per_pixel: 4,
        };
        let src = vec![0x11, 0x22, 0x33, 0x80];
        let (out, _) = repack(&src, 4, 1, 1, &layout);
        assert_eq!(out[3], 0x80);
    }

    /// An exotic visual is swizzled into RGBA rather than being refused.
    #[test]
    fn unusual_channel_order_is_swizzled_to_rgba() {
        let layout = ByteLayout {
            red: 1,
            green: 2,
            blue: 3,
            alpha: None,
            bytes_per_pixel: 4,
        };
        let src = vec![0x00, 0xaa, 0xbb, 0xcc];
        let (out, format) = repack(&src, 4, 1, 1, &layout);
        assert_eq!(format, PixelFormat::Rgba8);
        assert_eq!(out, vec![0xaa, 0xbb, 0xcc, 0xff]);
    }

    /// A `GetImage` racing a window resize genuinely returns short. Losing the
    /// bottom rows beats losing the process.
    #[test]
    fn short_input_is_truncated_not_panicked_on() {
        let src = vec![0x11, 0x22, 0x33, 0x44]; // one pixel, two rows claimed
        let (out, _) = repack(&src, 4, 1, 2, &bgrx_layout());
        assert_eq!(out.len(), 8, "buffer is still fully sized");
        assert_eq!(
            &out[0..4],
            &[0x11, 0x22, 0x33, 0xff],
            "already Bgra8, so only the alpha changes"
        );
        assert_eq!(&out[4..8], &[0, 0, 0, 0], "missing row stays zeroed");
    }

    #[test]
    fn zero_sized_captures_produce_an_empty_buffer() {
        let (out, _) = repack(&[], 0, 0, 0, &bgrx_layout());
        assert!(out.is_empty());
    }

    #[test]
    fn plane_mask_is_every_plane() {
        assert_eq!(all_planes(), u32::MAX);
    }
}

// ---------------------------------------------------------------------------
// HiDPI scale resolution
// ---------------------------------------------------------------------------

mod scale_resolution {
    use super::x11::scale::{
        BASE_DPI, parse_scale_override, parse_xft_dpi, resolve_scale, scale_from_dpi,
    };

    const RESOURCES: &str = "\
! a comment
*background:\t#1d1f21
  Xft.dpi:\t192
Xft.antialias: 1
";

    #[test]
    fn xft_dpi_is_found_amongst_other_resources() {
        assert_eq!(parse_xft_dpi(RESOURCES), Some(192.0));
    }

    /// A line without a colon must not abort the scan — `xrdb` emits them, and
    /// bailing out hides a perfectly good `Xft.dpi` further down.
    #[test]
    fn a_line_without_a_colon_is_skipped() {
        assert_eq!(parse_xft_dpi("garbage\nXft.dpi: 144\n"), Some(144.0));
    }

    #[test]
    fn xft_dpi_matching_is_case_insensitive() {
        assert_eq!(parse_xft_dpi("XFT.DPI: 120"), Some(120.0));
    }

    #[test]
    fn absent_or_nonsense_dpi_is_none() {
        assert_eq!(parse_xft_dpi(""), None);
        assert_eq!(parse_xft_dpi("Xft.antialias: 1"), None);
        assert_eq!(parse_xft_dpi("Xft.dpi: 0"), None);
        assert_eq!(parse_xft_dpi("Xft.dpi: -96"), None);
    }

    #[test]
    fn dpi_converts_against_the_96_baseline() {
        assert_eq!(scale_from_dpi(BASE_DPI), Some(1.0));
        assert_eq!(scale_from_dpi(192.0), Some(2.0));
        assert_eq!(scale_from_dpi(144.0), Some(1.5));
    }

    #[test]
    fn implausible_scales_are_refused() {
        assert_eq!(scale_from_dpi(1.0), None, "0.01x");
        assert_eq!(scale_from_dpi(10_000.0), None);
        assert_eq!(parse_scale_override("0"), None);
        assert_eq!(parse_scale_override("nonsense"), None);
        assert_eq!(parse_scale_override("inf"), None);
    }

    /// Fractional scaling is ordinary, not exotic — 125% and 150% are the two
    /// most common HiDPI settings on Linux desktops.
    #[test]
    fn fractional_overrides_are_honoured() {
        assert_eq!(parse_scale_override(" 1.25 "), Some(1.25));
        assert_eq!(parse_scale_override("1.5"), Some(1.5));
    }

    /// An explicit `GDK_SCALE` is a decision the user's whole session is
    /// already following, so it beats anything derived from the database.
    #[test]
    fn environment_beats_the_resource_database() {
        let scale = resolve_scale(Some("2"), None, Some("Xft.dpi: 144"));
        assert!((scale.get() - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn qt_is_consulted_when_gdk_is_absent_or_unusable() {
        let scale = resolve_scale(None, Some("1.5"), Some("Xft.dpi: 192"));
        assert!((scale.get() - 1.5).abs() < f64::EPSILON);

        let scale = resolve_scale(Some(""), Some("1.5"), None);
        assert!((scale.get() - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn the_database_is_used_when_nothing_is_forced() {
        let scale = resolve_scale(None, None, Some(RESOURCES));
        assert!((scale.get() - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_default_is_1x() {
        assert!((resolve_scale(None, None, None).get() - 1.0).abs() < f64::EPSILON);
        let scale = resolve_scale(Some("bogus"), Some("bogus"), Some("nothing here"));
        assert!((scale.get() - 1.0).abs() < f64::EPSILON);
    }
}

// ---------------------------------------------------------------------------
// Monitor / window / region geometry
// ---------------------------------------------------------------------------

mod geometry {
    use super::x11::layout::{
        PixelRect, bounding_box, display_containing, display_for_window, region_to_pixels,
        to_display, work_area_for,
    };
    use scrozz_core::{DisplayId, LogicalPoint, LogicalRect, LogicalSize, ScaleFactor};

    fn id(name: &str) -> DisplayId {
        DisplayId(name.to_owned())
    }

    #[test]
    fn intersection_of_disjoint_rectangles_is_none() {
        let a = PixelRect::new(0, 0, 100, 100);
        let b = PixelRect::new(200, 0, 100, 100);
        assert_eq!(a.intersection(&b), None);
        assert_eq!(a.overlap_area(&b), 0);
    }

    /// Touching edges are not an overlap; a half-open rectangle at x=100 with
    /// width 100 occupies 100..200, so 0..100 shares no pixel with it.
    #[test]
    fn touching_edges_do_not_overlap() {
        let a = PixelRect::new(0, 0, 100, 100);
        let b = PixelRect::new(100, 0, 100, 100);
        assert_eq!(a.intersection(&b), None);
    }

    #[test]
    fn intersection_is_the_shared_area() {
        let a = PixelRect::new(0, 0, 100, 100);
        let b = PixelRect::new(50, 50, 100, 100);
        assert_eq!(a.intersection(&b), Some(PixelRect::new(50, 50, 50, 50)));
        assert_eq!(a.overlap_area(&b), 2_500);
    }

    #[test]
    fn containment_is_half_open() {
        let r = PixelRect::new(10, 10, 10, 10);
        assert!(r.contains(10, 10));
        assert!(r.contains(19, 19));
        assert!(!r.contains(20, 20));
        assert!(!r.contains(9, 10));
    }

    #[test]
    fn empty_rectangles_contain_nothing() {
        assert!(!PixelRect::new(0, 0, 0, 100).contains(0, 0));
        assert!(PixelRect::new(0, 0, 0, 100).is_empty());
    }

    /// The whole reason `_NET_WORKAREA` exists: it excludes the panel. On a
    /// dual-head desktop the property spans both monitors, so it must be
    /// intersected with each monitor rather than assigned to it.
    #[test]
    fn work_area_is_intersected_per_monitor() {
        let left = PixelRect::new(0, 0, 1920, 1080);
        let right = PixelRect::new(1920, 0, 1920, 1080);
        // A 27px top panel across a 3840-wide desktop.
        let desktop = PixelRect::new(0, 27, 3840, 1053);

        assert_eq!(
            work_area_for(left, Some(desktop)),
            PixelRect::new(0, 27, 1920, 1053)
        );
        assert_eq!(
            work_area_for(right, Some(desktop)),
            PixelRect::new(1920, 27, 1920, 1053)
        );
    }

    /// A monitor hot-plugged since the window manager last published the
    /// property is genuinely outside it. An empty work area would leave an
    /// overlay nowhere to go.
    #[test]
    fn a_monitor_outside_the_work_area_falls_back_to_its_own_bounds() {
        let monitor = PixelRect::new(4000, 0, 1920, 1080);
        let desktop = PixelRect::new(0, 27, 3840, 1053);
        assert_eq!(work_area_for(monitor, Some(desktop)), monitor);
        assert_eq!(work_area_for(monitor, None), monitor);
    }

    fn two_displays() -> Vec<(DisplayId, PixelRect, bool)> {
        vec![
            (id("left"), PixelRect::new(0, 0, 1920, 1080), false),
            (id("right"), PixelRect::new(1920, 0, 2560, 1440), true),
        ]
    }

    #[test]
    fn the_pointer_picks_its_display() {
        let displays = two_displays();
        assert_eq!(display_containing(10, 10, &displays), Some(id("left")));
        assert_eq!(display_containing(2000, 10, &displays), Some(id("right")));
    }

    /// An L-shaped arrangement has dead space the pointer can legally occupy.
    #[test]
    fn a_pointer_in_dead_space_falls_back_to_primary() {
        let displays = two_displays();
        assert_eq!(display_containing(100, 1200, &displays), Some(id("right")));
        assert_eq!(display_containing(0, 0, &[]), None);
    }

    /// "Predominantly on" means largest overlap area. A corner test gets this
    /// exactly backwards for a window dragged so only its edge crosses over.
    #[test]
    fn a_window_belongs_to_the_display_it_covers_most() {
        let displays = two_displays();
        // Top-left corner is on `left`, but 700 of its 800 columns are on
        // `right`.
        let window = PixelRect::new(1820, 100, 800, 600);
        assert_eq!(display_for_window(window, &displays), Some(id("right")));

        let window = PixelRect::new(1120, 100, 800, 600);
        assert_eq!(display_for_window(window, &displays), Some(id("left")));
    }

    #[test]
    fn a_window_on_no_display_still_gets_an_answer() {
        let displays = two_displays();
        let offscreen = PixelRect::new(-5000, -5000, 100, 100);
        assert_eq!(display_for_window(offscreen, &displays), Some(id("right")));
        assert_eq!(display_for_window(offscreen, &[]), None);
    }

    /// Monitors above and to the left of the origin are normal — the primary
    /// need not be at (0, 0).
    #[test]
    fn bounding_box_handles_negative_offsets() {
        let rects = [
            PixelRect::new(0, 0, 1920, 1080),
            PixelRect::new(-2560, -200, 2560, 1440),
        ];
        assert_eq!(
            bounding_box(&rects),
            Some(PixelRect::new(-2560, -200, 4480, 1440))
        );
    }

    #[test]
    fn bounding_box_ignores_empty_and_missing_monitors() {
        assert_eq!(bounding_box(&[]), None);
        assert_eq!(bounding_box(&[PixelRect::new(0, 0, 0, 0)]), None);
        assert_eq!(
            bounding_box(&[PixelRect::new(0, 0, 0, 0), PixelRect::new(5, 5, 10, 10)]),
            Some(PixelRect::new(5, 5, 10, 10))
        );
    }

    fn region(x: f64, y: f64, w: f64, h: f64) -> LogicalRect {
        LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(w, h))
    }

    #[test]
    fn a_region_at_1x_is_itself() {
        let root = PixelRect::new(0, 0, 1920, 1080);
        assert_eq!(
            region_to_pixels(region(10.0, 20.0, 100.0, 50.0), 1.0, root),
            Some(PixelRect::new(10, 20, 100, 50))
        );
    }

    /// Fractional scaling is the Wayland norm and increasingly common on X11.
    /// Rounding outward keeps every requested logical pixel inside the result.
    #[test]
    fn a_fractional_scale_rounds_outward() {
        let root = PixelRect::new(0, 0, 3840, 2160);
        assert_eq!(
            region_to_pixels(region(10.5, 10.5, 100.0, 100.0), 1.5, root),
            Some(PixelRect::new(15, 15, 151, 151)),
            "floor(15.75) = 15, ceil(165.75) = 166, so 151 columns cover every \
             requested logical pixel"
        );
    }

    /// Dragging a selection one pixel past the screen edge is entirely
    /// ordinary; `GetImage` outside the drawable is a `BadMatch`.
    #[test]
    fn a_region_past_the_edge_is_clamped() {
        let root = PixelRect::new(0, 0, 1920, 1080);
        assert_eq!(
            region_to_pixels(region(1900.0, 1070.0, 100.0, 100.0), 1.0, root),
            Some(PixelRect::new(1900, 1070, 20, 10))
        );
    }

    #[test]
    fn a_region_entirely_offscreen_is_none() {
        let root = PixelRect::new(0, 0, 1920, 1080);
        assert_eq!(
            region_to_pixels(region(5000.0, 5000.0, 10.0, 10.0), 1.0, root),
            None
        );
        assert_eq!(
            region_to_pixels(region(f64::NAN, 0.0, 10.0, 10.0), 1.0, root),
            None
        );
    }

    #[test]
    fn display_conversion_divides_both_rectangles_by_the_scale() {
        let display = to_display(
            id("x11:0:DP-1"),
            "DP-1".to_owned(),
            PixelRect::new(0, 0, 3840, 2160),
            PixelRect::new(0, 54, 3840, 2106),
            ScaleFactor::new(2.0),
            true,
        );
        assert!((display.bounds.size.width - 1920.0).abs() < f64::EPSILON);
        assert!((display.work_area.origin.y - 27.0).abs() < f64::EPSILON);
        assert!((display.work_area.size.height - 1053.0).abs() < f64::EPSILON);
        assert!(display.is_primary);
        assert_eq!(display.name, "DP-1");
    }
}

// ---------------------------------------------------------------------------
// Hand-rolled RandR wire protocol
// ---------------------------------------------------------------------------

mod randr_wire {
    use super::x11::wire::{
        GET_MONITORS_OPCODE, MONITORS_SINCE, QUERY_VERSION_OPCODE, RANDR_EXTENSION_NAME, Version,
        get_monitors_request, parse_monitors, parse_query_version, primary_index,
        query_version_request,
    };

    /// x11rb negotiates the connection in the host's byte order, so requests
    /// are serialised natively and the fixtures match.
    #[test]
    fn query_version_request_is_three_units_long() {
        let request = query_version_request(140, 1, 5);
        assert_eq!(request[0], 140, "major opcode");
        assert_eq!(request[1], QUERY_VERSION_OPCODE);
        assert_eq!(
            u16::from_ne_bytes([request[2], request[3]]),
            3,
            "length in 4-byte units, including the header"
        );
        assert_eq!(u32::from_ne_bytes(request[4..8].try_into().unwrap()), 1);
        assert_eq!(u32::from_ne_bytes(request[8..12].try_into().unwrap()), 5);
    }

    #[test]
    fn get_monitors_request_carries_the_window_and_the_active_flag() {
        let request = get_monitors_request(140, 0x0000_01a5, true);
        assert_eq!(request[1], GET_MONITORS_OPCODE);
        assert_eq!(
            u32::from_ne_bytes(request[4..8].try_into().unwrap()),
            0x0000_01a5
        );
        assert_eq!(request[8], 1);
        assert_eq!(&request[9..12], &[0, 0, 0], "trailing pad must be zero");

        assert_eq!(get_monitors_request(140, 1, false)[8], 0);
    }

    #[test]
    fn the_extension_is_named_as_the_server_knows_it() {
        assert_eq!(RANDR_EXTENSION_NAME, "RANDR");
    }

    fn version_reply(major: u32, minor: u32) -> Vec<u8> {
        let mut reply = vec![0u8; 32];
        reply[0] = 1; // Reply, not an error or an event.
        reply[8..12].copy_from_slice(&major.to_ne_bytes());
        reply[12..16].copy_from_slice(&minor.to_ne_bytes());
        reply
    }

    #[test]
    fn version_parses_from_the_documented_offsets() {
        assert_eq!(
            parse_query_version(&version_reply(1, 6)),
            Ok(Version { major: 1, minor: 6 })
        );
    }

    /// Calling `RRGetMonitors` on a pre-1.5 server earns a `BadRequest` that
    /// looks exactly like a bug in the client.
    #[test]
    fn monitors_require_randr_1_5() {
        assert_eq!(MONITORS_SINCE, (1, 5));
        assert!(!Version { major: 1, minor: 4 }.supports_monitors());
        assert!(Version { major: 1, minor: 5 }.supports_monitors());
        assert!(Version { major: 1, minor: 6 }.supports_monitors());
        assert!(Version { major: 2, minor: 0 }.supports_monitors());
    }

    #[test]
    fn a_truncated_or_non_reply_is_refused() {
        assert!(parse_query_version(&[]).is_err());
        assert!(parse_query_version(&[0u8; 31]).is_err());
        let mut error = version_reply(1, 5);
        error[0] = 0; // an X error, not a reply
        assert!(parse_query_version(&error).is_err());
    }

    struct MonitorFixture {
        name: u32,
        primary: bool,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        outputs: Vec<u32>,
    }

    /// Builds a byte-exact `RRGetMonitors` reply. Each `MONITORINFO` is 24
    /// fixed bytes followed by its variable-length output list, which is the
    /// off-by-one this whole module exists to get right.
    fn monitors_reply(monitors: &[MonitorFixture]) -> Vec<u8> {
        let mut reply = vec![0u8; 32];
        reply[0] = 1;
        reply[8..12].copy_from_slice(&0x1234_5678u32.to_ne_bytes()); // timestamp
        reply[12..16].copy_from_slice(&(monitors.len() as u32).to_ne_bytes());
        let total_outputs: usize = monitors.iter().map(|m| m.outputs.len()).sum();
        reply[16..20].copy_from_slice(&(total_outputs as u32).to_ne_bytes());

        for m in monitors {
            reply.extend_from_slice(&m.name.to_ne_bytes());
            reply.push(u8::from(m.primary));
            reply.push(1); // automatic
            reply.extend_from_slice(&(m.outputs.len() as u16).to_ne_bytes());
            reply.extend_from_slice(&m.x.to_ne_bytes());
            reply.extend_from_slice(&m.y.to_ne_bytes());
            reply.extend_from_slice(&m.width.to_ne_bytes());
            reply.extend_from_slice(&m.height.to_ne_bytes());
            reply.extend_from_slice(&600u32.to_ne_bytes()); // width_mm
            reply.extend_from_slice(&340u32.to_ne_bytes()); // height_mm
            for output in &m.outputs {
                reply.extend_from_slice(&output.to_ne_bytes());
            }
        }

        let body = reply.len() - 32;
        reply[4..8].copy_from_slice(&((body / 4) as u32).to_ne_bytes());
        reply
    }

    /// Two monitors with *different* output counts: the second record's offset
    /// is only correct if the first record's outputs were walked, not assumed.
    #[test]
    fn variable_length_records_are_walked_not_indexed() {
        let reply = monitors_reply(&[
            MonitorFixture {
                name: 0x0000_0141,
                primary: true,
                x: 0,
                y: 0,
                width: 3840,
                height: 2160,
                outputs: vec![0x42, 0x43, 0x44],
            },
            MonitorFixture {
                name: 0x0000_0142,
                primary: false,
                x: 3840,
                y: 120,
                width: 1920,
                height: 1080,
                outputs: vec![0x45],
            },
        ]);

        let monitors = parse_monitors(&reply).expect("well-formed reply");
        assert_eq!(monitors.len(), 2);

        assert_eq!(monitors[0].name, 0x0000_0141);
        assert!(monitors[0].primary);
        assert_eq!(monitors[0].outputs, vec![0x42, 0x43, 0x44]);
        assert_eq!(
            (
                monitors[0].x,
                monitors[0].y,
                monitors[0].width,
                monitors[0].height
            ),
            (0, 0, 3840, 2160)
        );

        assert_eq!(monitors[1].name, 0x0000_0142);
        assert!(!monitors[1].primary);
        assert_eq!(monitors[1].outputs, vec![0x45]);
        assert_eq!((monitors[1].x, monitors[1].y), (3840, 120));
        assert_eq!(monitors[1].width_mm, 600);
    }

    /// A monitor positioned above or left of the origin reports a negative
    /// `i16`, which an unsigned read turns into a 65,000-pixel offset.
    #[test]
    fn negative_positions_survive_the_signed_read() {
        let reply = monitors_reply(&[MonitorFixture {
            name: 1,
            primary: true,
            x: -1920,
            y: -200,
            width: 1920,
            height: 1080,
            outputs: vec![],
        }]);
        let monitors = parse_monitors(&reply).expect("well-formed reply");
        assert_eq!((monitors[0].x, monitors[0].y), (-1920, -200));
    }

    #[test]
    fn a_monitor_with_no_outputs_is_still_parsed() {
        let reply = monitors_reply(&[MonitorFixture {
            name: 7,
            primary: false,
            x: 0,
            y: 0,
            width: 1024,
            height: 768,
            outputs: vec![],
        }]);
        let monitors = parse_monitors(&reply).expect("well-formed reply");
        assert_eq!(monitors.len(), 1);
        assert!(monitors[0].outputs.is_empty());
    }

    #[test]
    fn zero_monitors_is_an_empty_list_not_an_error() {
        assert_eq!(parse_monitors(&monitors_reply(&[])), Ok(Vec::new()));
    }

    /// A reply claiming more monitors than it carries must be refused rather
    /// than read past its end.
    #[test]
    fn a_truncated_monitor_list_is_refused() {
        let mut reply = monitors_reply(&[MonitorFixture {
            name: 1,
            primary: true,
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            outputs: vec![0x42, 0x43],
        }]);
        reply.truncate(reply.len() - 4);
        assert!(parse_monitors(&reply).is_err(), "output list cut short");

        let mut reply = monitors_reply(&[]);
        reply[12..16].copy_from_slice(&2u32.to_ne_bytes());
        assert!(parse_monitors(&reply).is_err(), "count exceeds the body");

        assert!(parse_monitors(&[]).is_err());
    }

    /// A colossal count must not provoke a colossal allocation before the
    /// length check catches it.
    #[test]
    fn an_absurd_monitor_count_is_refused_cheaply() {
        let mut reply = monitors_reply(&[]);
        reply[12..16].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert!(parse_monitors(&reply).is_err());
    }

    fn monitor(primary: bool, x: i32, y: i32) -> super::x11::wire::Monitor {
        super::x11::wire::Monitor {
            name: 1,
            primary,
            automatic: true,
            x,
            y,
            width: 1920,
            height: 1080,
            width_mm: 600,
            height_mm: 340,
            outputs: Vec::new(),
        }
    }

    #[test]
    fn the_marked_primary_wins() {
        let monitors = [monitor(false, 0, 0), monitor(true, 1920, 0)];
        assert_eq!(primary_index(&monitors), Some(1));
    }

    /// RandR permits zero primaries, and every display reporting
    /// `is_primary: false` leaves callers with no default anchor.
    #[test]
    fn with_no_primary_the_monitor_at_the_origin_wins() {
        let monitors = [monitor(false, 1920, 0), monitor(false, 0, 0)];
        assert_eq!(primary_index(&monitors), Some(1));

        let monitors = [monitor(false, 100, 100), monitor(false, 1920, 0)];
        assert_eq!(primary_index(&monitors), Some(0), "falls back to the first");

        assert_eq!(primary_index(&[]), None);
    }
}

// ---------------------------------------------------------------------------
// Portal restore tokens
// ---------------------------------------------------------------------------

mod restore_tokens {
    use super::wayland::restore::{TokenKey, TokenStore, token_path};

    #[test]
    fn keys_round_trip_through_their_on_disk_spelling() {
        for key in [TokenKey::Monitor, TokenKey::Window, TokenKey::AllDisplays] {
            assert_eq!(TokenKey::parse(key.as_str()), Some(key));
        }
        assert_eq!(TokenKey::parse("display"), None);
        assert_eq!(TokenKey::parse(""), None);
    }

    /// A grant for one monitor does not authorise a window session; reusing
    /// the wrong token earns a fresh permission prompt at best.
    #[test]
    fn tokens_are_kept_apart_by_session_kind() {
        let mut store = TokenStore::new();
        store.set(TokenKey::Monitor, "mon-token");
        store.set(TokenKey::Window, "win-token");

        assert_eq!(store.get(TokenKey::Monitor), Some("mon-token"));
        assert_eq!(store.get(TokenKey::Window), Some("win-token"));
        assert_eq!(store.get(TokenKey::AllDisplays), None);
    }

    /// The portal returns an empty string when the user declined persistence.
    /// Storing that guarantees every later attempt sends a token the portal
    /// must reject.
    #[test]
    fn an_empty_token_removes_rather_than_stores() {
        let mut store = TokenStore::new();
        store.set(TokenKey::Monitor, "mon-token");
        store.set(TokenKey::Monitor, "");
        assert_eq!(store.get(TokenKey::Monitor), None);
        assert!(store.is_empty());
    }

    #[test]
    fn a_refused_token_can_be_discarded() {
        let mut store = TokenStore::new();
        store.set(TokenKey::AllDisplays, "stale");
        store.invalidate(TokenKey::AllDisplays);
        assert_eq!(store.get(TokenKey::AllDisplays), None);
        store.invalidate(TokenKey::AllDisplays); // idempotent
    }

    #[test]
    fn the_store_round_trips_through_its_file_format() {
        let mut store = TokenStore::new();
        store.set(TokenKey::Monitor, "3fdd2d7e-8d54-4b5f-9d0f-1f7f2f0f4f8a");
        store.set(TokenKey::AllDisplays, "kwin/42");

        let text = store.serialise();
        assert!(text.starts_with('#'), "explains itself to whoever finds it");
        assert_eq!(TokenStore::parse(&text), store);
    }

    /// A corrupt token file should cost one permission prompt, not a broken
    /// application.
    #[test]
    fn unparseable_lines_are_skipped() {
        let text = "\
# comment
monitor\tgood-token

garbage-with-no-tab
unknown-key\tvalue
window\t
   window \t  spaced-token  
";
        let store = TokenStore::parse(text);
        assert_eq!(store.get(TokenKey::Monitor), Some("good-token"));
        assert_eq!(store.get(TokenKey::Window), Some("spaced-token"));
        assert_eq!(store.get(TokenKey::AllDisplays), None);
    }

    #[test]
    fn an_empty_file_parses_to_an_empty_store() {
        assert!(TokenStore::parse("").is_empty());
        assert!(TokenStore::parse("# just a comment\n").is_empty());
    }

    /// State, not cache: a token must survive a cache clear or the prompt
    /// comes back.
    #[test]
    fn tokens_live_under_xdg_state_home() {
        assert_eq!(
            token_path(Some("/home/u/.local/state"), Some("/home/u")),
            Some("/home/u/.local/state/scrozz/portal-tokens".into())
        );
    }

    #[test]
    fn the_xdg_fallback_is_dot_local_state() {
        assert_eq!(
            token_path(None, Some("/home/u")),
            Some("/home/u/.local/state/scrozz/portal-tokens".into())
        );
    }

    /// The specification requires an absolute path; resolving a relative one
    /// against the current directory scatters token files wherever the app
    /// happened to start.
    #[test]
    fn a_relative_state_home_is_ignored() {
        assert_eq!(
            token_path(Some("relative/state"), Some("/home/u")),
            Some("/home/u/.local/state/scrozz/portal-tokens".into())
        );
    }

    /// Containers and minimal init systems set neither variable; the caller
    /// then keeps tokens in memory for the process lifetime.
    #[test]
    fn no_home_at_all_yields_no_path() {
        assert_eq!(token_path(None, None), None);
        assert_eq!(token_path(Some("  "), Some("  ")), None);
    }
}

// ---------------------------------------------------------------------------
// Portal session negotiation
// ---------------------------------------------------------------------------

mod portal_negotiation {
    use super::wayland::portal::{
        PlanFailure, SessionPlan, StreamInfo, cursor_mode, path_from_uri, persist_mode, source_type,
    };
    use super::wayland::restore::TokenKey;
    use scrozz_core::{CaptureTarget, DisplayId, LogicalPoint, LogicalRect, LogicalSize, WindowId};

    #[test]
    fn a_display_capture_asks_for_a_monitor() {
        let plan =
            SessionPlan::for_target(&CaptureTarget::Display(DisplayId("1".into())), false).unwrap();
        assert_eq!(plan.types, source_type::MONITOR);
        assert_eq!(plan.restore_key, TokenKey::Monitor);
    }

    /// The portal has no concept of a sub-rectangle, and asking the user to
    /// pick a region in the portal's UI and again in Scrozz's would be absurd.
    #[test]
    fn a_region_is_cropped_from_a_monitor_capture() {
        let region = CaptureTarget::Region(LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(100.0, 100.0),
        ));
        let plan = SessionPlan::for_target(&region, false).unwrap();
        assert_eq!(plan.types, source_type::MONITOR);
        assert_eq!(plan.restore_key, TokenKey::Monitor);
    }

    #[test]
    fn a_window_capture_asks_for_a_window() {
        let plan =
            SessionPlan::for_target(&CaptureTarget::Window(WindowId("w".into())), false).unwrap();
        assert_eq!(plan.types, source_type::WINDOW);
        assert_eq!(plan.restore_key, TokenKey::Window);
    }

    #[test]
    fn all_displays_is_refused_before_portal_negotiation() {
        assert_eq!(
            SessionPlan::for_target(&CaptureTarget::AllDisplays, false),
            Err(PlanFailure::AllDisplaysNeedsPositions)
        );
        let error = PlanFailure::AllDisplaysNeedsPositions.into_error();
        let scrozz_core::Error::Unsupported { what, why } = error else {
            panic!("all-display planning must return Unsupported");
        };
        assert!(what.contains("all displays"));
        assert!(why.contains("before opening the portal picker"));
        assert!(why.contains("Capture one display instead"));
    }

    /// A still capture cannot composite a pointer itself without getting the
    /// hotspot subtly wrong, so it asks the portal to embed one.
    #[test]
    fn the_pointer_is_requested_embedded_or_hidden() {
        let target = CaptureTarget::Display(DisplayId("1".into()));
        let with = SessionPlan::for_target(&target, true).unwrap();
        assert_eq!(with.cursor, cursor_mode::EMBEDDED);
        let without = SessionPlan::for_target(&target, false).unwrap();
        assert_eq!(without.cursor, cursor_mode::HIDDEN);
    }

    /// `APPLICATION` persistence expires when the process does, so every
    /// launch would cost a permission dialog — the failure this whole
    /// mechanism exists to prevent.
    #[test]
    fn persistence_outlives_the_process() {
        let plan =
            SessionPlan::for_target(&CaptureTarget::Display(DisplayId("1".into())), false).unwrap();
        assert_eq!(plan.persist, persist_mode::EXPLICITLY_REVOKED);
        assert_ne!(plan.persist, persist_mode::APPLICATION);
    }

    /// Decision D8: capability by query. A portal that cannot offer window
    /// sources must be met with a narrowed request, not a rejected call.
    #[test]
    fn narrowing_drops_source_types_the_portal_lacks() {
        let plan = SessionPlan::for_target(&CaptureTarget::Display(DisplayId("1".into())), false)
            .unwrap()
            .narrow(source_type::MONITOR, cursor_mode::HIDDEN)
            .expect("monitor survives");
        assert_eq!(plan.types, source_type::MONITOR);
    }

    #[test]
    fn narrowing_to_nothing_is_a_refusal() {
        assert_eq!(
            SessionPlan::for_target(&CaptureTarget::Window(WindowId("w".into())), false)
                .unwrap()
                .narrow(source_type::MONITOR, cursor_mode::EMBEDDED),
            None,
            "a portal with no window source cannot serve a window capture"
        );
    }

    /// Every portal implements `Hidden`, so losing the pointer is better than
    /// losing the capture.
    #[test]
    fn an_unavailable_cursor_mode_degrades_to_hidden() {
        let plan = SessionPlan::for_target(&CaptureTarget::Display(DisplayId("1".into())), true)
            .unwrap()
            .narrow(source_type::MONITOR, cursor_mode::HIDDEN)
            .expect("monitor survives");
        assert_eq!(plan.cursor, cursor_mode::HIDDEN);
    }

    #[test]
    fn source_type_and_cursor_flags_match_the_specification() {
        assert_eq!(source_type::MONITOR, 1);
        assert_eq!(source_type::WINDOW, 2);
        assert_eq!(source_type::VIRTUAL, 4);
        assert_eq!(cursor_mode::HIDDEN, 1);
        assert_eq!(cursor_mode::EMBEDDED, 2);
        assert_eq!(cursor_mode::METADATA, 4);
        assert_eq!(persist_mode::DO_NOT, 0);
        assert_eq!(persist_mode::APPLICATION, 1);
        assert_eq!(persist_mode::EXPLICITLY_REVOKED, 2);
    }

    /// The Screenshot interface answers with a URI. A filename containing a
    /// space arrives as `%20`, and opening the literal path fails with "no
    /// such file" on a file that plainly exists.
    #[test]
    fn percent_escapes_are_decoded() {
        assert_eq!(
            path_from_uri("file:///home/u/Pictures/Screenshot%20from%202025.png"),
            Some("/home/u/Pictures/Screenshot from 2025.png".into())
        );
    }

    #[test]
    fn a_plain_path_passes_through() {
        assert_eq!(
            path_from_uri("file:///run/user/1000/doc/abc/shot.png"),
            Some("/run/user/1000/doc/abc/shot.png".into())
        );
    }

    #[test]
    fn a_localhost_authority_is_accepted() {
        assert_eq!(
            path_from_uri("file://localhost/tmp/shot.png"),
            Some("/tmp/shot.png".into())
        );
    }

    #[test]
    fn non_file_uris_are_refused() {
        assert_eq!(path_from_uri("https://example.com/shot.png"), None);
        assert_eq!(path_from_uri("/home/u/shot.png"), None);
        assert_eq!(path_from_uri("file://elsewhere/shot.png"), None);
        assert_eq!(path_from_uri(""), None);
    }

    #[test]
    fn a_trailing_percent_is_not_a_panic() {
        assert_eq!(
            path_from_uri("file:///home/u/odd%"),
            Some("/home/u/odd%".into())
        );
        assert_eq!(
            path_from_uri("file:///home/u/odd%zz"),
            Some("/home/u/odd%zz".into())
        );
    }

    /// Some compositors decline to tell a window stream where its window is,
    /// which the caller must notice rather than place the frame at (0, 0).
    #[test]
    fn a_stream_without_geometry_is_not_placeable() {
        assert!(
            StreamInfo {
                node_id: 42,
                position: Some((0, 0)),
                size: Some((1920, 1080)),
            }
            .is_placeable()
        );
        assert!(
            !StreamInfo {
                node_id: 42,
                position: None,
                size: Some((1920, 1080)),
            }
            .is_placeable()
        );
    }
}

/// The SPA POD encoder, checked byte for byte.
///
/// These assertions are transcriptions of the layout in
/// `spa/pod/pod.h`, and they are deliberately literal. A POD is a binary
/// structure another process parses with pointer arithmetic; "it looked right in
/// the debugger" is not a standard that catches a misplaced pad byte, and a
/// misplaced pad byte produces a stream that silently never negotiates.
mod spa_pod {
    use super::wayland::pipewire::pod::{
        Choice, Object, ObjectRef, Property, Scalar, choice, kind, pad_to_8,
    };

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_ne_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
    }

    #[test]
    fn padding_rounds_up_to_the_next_multiple_of_eight() {
        assert_eq!(pad_to_8(0), 0);
        assert_eq!(pad_to_8(1), 8);
        assert_eq!(pad_to_8(8), 8);
        assert_eq!(pad_to_8(9), 16);
        assert_eq!(pad_to_8(16), 16);
    }

    /// A four-byte value still occupies sixteen bytes: an eight-byte header,
    /// four bytes of body, and four of padding. The declared size covers the
    /// body only, which is the part that is easy to get backwards.
    #[test]
    fn a_scalar_declares_its_body_size_and_pads_its_footprint() {
        let encoded = Scalar::id(7).encode();

        assert_eq!(encoded.len(), 16);
        assert_eq!(u32_at(&encoded, 0), 4, "size counts the body only");
        assert_eq!(u32_at(&encoded, 4), kind::ID);
        assert_eq!(u32_at(&encoded, 8), 7);
        assert_eq!(&encoded[12..], &[0, 0, 0, 0], "tail is zero padding");
    }

    /// Rectangles and fractions are two `u32`s, so they need no padding at all.
    #[test]
    fn eight_byte_scalars_need_no_padding() {
        let rectangle = Scalar::rectangle(1920, 1080).encode();
        assert_eq!(rectangle.len(), 16);
        assert_eq!(u32_at(&rectangle, 0), 8);
        assert_eq!(u32_at(&rectangle, 4), kind::RECTANGLE);
        assert_eq!(u32_at(&rectangle, 8), 1920);
        assert_eq!(u32_at(&rectangle, 12), 1080);

        let fraction = Scalar::fraction(60, 1).encode();
        assert_eq!(u32_at(&fraction, 4), kind::FRACTION);
        assert_eq!(u32_at(&fraction, 8), 60);
        assert_eq!(u32_at(&fraction, 12), 1);
    }

    /// The rule that makes choices different from everything else: inside a
    /// choice body the alternatives are packed contiguously at exactly the
    /// child's size, with no per-value padding. `spa_pod_builder_pad` returns
    /// early while the body flag is set, and a client that pads anyway produces
    /// a POD the server reads as garbage.
    #[test]
    fn choice_alternatives_are_packed_without_padding() {
        let values = vec![Scalar::id(8), Scalar::id(7), Scalar::id(12)];
        let encoded = Choice::enumerated(values)
            .encode()
            .expect("uniform id values encode");

        // 8 header + 16 choice body header + 3 * 4 bytes of packed ids = 36,
        // padded to 40.
        assert_eq!(encoded.len(), 40);
        assert_eq!(u32_at(&encoded, 0), 28, "body is 16 + 3 * 4");
        assert_eq!(u32_at(&encoded, 4), kind::CHOICE);
        assert_eq!(u32_at(&encoded, 8), choice::ENUM);
        assert_eq!(u32_at(&encoded, 12), 0, "no choice flags");
        assert_eq!(u32_at(&encoded, 16), 4, "child size is one id");
        assert_eq!(u32_at(&encoded, 20), kind::ID);
        assert_eq!(u32_at(&encoded, 24), 8, "default comes first");
        assert_eq!(u32_at(&encoded, 28), 7);
        assert_eq!(u32_at(&encoded, 32), 12);
    }

    /// A range is default, then minimum, then maximum. Swapping the first two —
    /// the intuitive ordering — makes every negotiation pick the minimum.
    #[test]
    fn a_range_choice_orders_default_min_max() {
        let encoded = Choice::range(Scalar::int(30), Scalar::int(1), Scalar::int(60))
            .encode()
            .expect("uniform int values encode");

        assert_eq!(u32_at(&encoded, 8), choice::RANGE);
        assert_eq!(u32_at(&encoded, 24), 30);
        assert_eq!(u32_at(&encoded, 28), 1);
        assert_eq!(u32_at(&encoded, 32), 60);
    }

    /// Flags are one mask value, not an enum whose individual bits are emitted
    /// as separate alternatives.
    #[test]
    fn a_flags_choice_encodes_one_integer_mask() {
        let encoded = Choice::flags(Scalar::int(0b110))
            .encode()
            .expect("one integer is uniform");

        assert_eq!(encoded.len(), 32);
        assert_eq!(u32_at(&encoded, 0), 20);
        assert_eq!(u32_at(&encoded, 8), choice::FLAGS);
        assert_eq!(u32_at(&encoded, 16), 4);
        assert_eq!(u32_at(&encoded, 20), kind::INT);
        assert_eq!(u32_at(&encoded, 24), 0b110);
    }

    /// A choice whose alternatives disagree about their type cannot be encoded:
    /// the body carries one child header for all of them.
    #[test]
    fn a_mixed_type_choice_is_refused_rather_than_encoded_wrongly() {
        assert!(
            Choice::enumerated(vec![Scalar::id(1), Scalar::int(2)])
                .encode()
                .is_none()
        );
        assert!(Choice::enumerated(Vec::new()).encode().is_none());
    }

    /// An object's declared size covers its `{ type, id }` body plus every
    /// property, and a parse of the result must recover exactly what went in.
    #[test]
    fn an_object_round_trips_through_the_parser() {
        let object = Object {
            object_type: 0x0004_0003,
            id: 4,
            properties: vec![
                Property::scalar(1, &Scalar::id(2)),
                Property::scalar(0x0002_0003, &Scalar::rectangle(2560, 1440)),
            ],
        };
        let encoded = object.encode();

        assert_eq!(u32_at(&encoded, 0) as usize, encoded.len() - 8);
        assert_eq!(u32_at(&encoded, 4), kind::OBJECT);

        let parsed = ObjectRef::parse(&encoded).expect("the encoder produces parseable objects");
        assert_eq!(parsed.object_type, 0x0004_0003);
        assert_eq!(parsed.id, 4);
        assert_eq!(parsed.properties().count(), 2);
        assert_eq!(
            parsed.property(1).and_then(|property| property.as_id()),
            Some(2)
        );
        assert_eq!(
            parsed
                .property(0x0002_0003)
                .and_then(|property| property.as_rectangle()),
            Some((2560, 1440))
        );
        assert!(parsed.property(0xDEAD).is_none());
    }

    /// A server is entitled to report a fixated value still wrapped in a
    /// single-alternative choice, and several do. Reading through that wrapper
    /// is the difference between negotiating and hanging.
    #[test]
    fn a_fixated_value_is_read_through_its_choice_wrapper() {
        let wrapped = Choice::enumerated(vec![Scalar::id(12)]);
        let object = Object {
            object_type: 0x0004_0003,
            id: 4,
            properties: vec![
                Property::choice(0x0002_0001, &wrapped).expect("uniform choice encodes"),
            ],
        };
        let encoded = object.encode();

        let parsed = ObjectRef::parse(&encoded).expect("parseable");
        assert_eq!(
            parsed
                .property(0x0002_0001)
                .and_then(|property| property.as_id()),
            Some(12)
        );
    }

    /// This parses memory another process wrote, so every truncation must be a
    /// `None` rather than a panic or an over-read.
    #[test]
    fn truncated_and_mistyped_pods_are_rejected() {
        let encoded = Object {
            object_type: 0x0004_0003,
            id: 4,
            properties: vec![Property::scalar(1, &Scalar::id(2))],
        }
        .encode();

        assert!(ObjectRef::parse(&[]).is_none());
        assert!(ObjectRef::parse(&encoded[..8]).is_none());
        assert!(ObjectRef::parse(&encoded[..encoded.len() - 1]).is_none());
        assert!(
            ObjectRef::parse(&Scalar::id(1).encode()).is_none(),
            "a scalar is not an object"
        );

        let mut malformed_property = encoded.clone();
        malformed_property[24..28].copy_from_slice(&12u32.to_ne_bytes());
        assert!(
            ObjectRef::parse(&malformed_property).is_none(),
            "a nested value cannot extend past its enclosing object"
        );

        let mut trailing_junk = encoded.clone();
        trailing_junk.extend_from_slice(&[0; 8]);
        let enlarged = u32_at(&trailing_junk, 0) + 8;
        trailing_junk[..4].copy_from_slice(&enlarged.to_ne_bytes());
        assert!(
            ObjectRef::parse(&trailing_junk).is_none(),
            "an incomplete trailing property makes the whole object malformed"
        );

        let mut impossible = encoded;
        impossible[..4].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert!(
            ObjectRef::parse(&impossible).is_none(),
            "a peer-controlled length cannot over-read the callback buffer"
        );
    }
}

/// Format negotiation: what is offered, and what the answer is understood to
/// mean.
mod format_negotiation {
    use scrozz_core::{ColorSpace, PixelFormat};

    use super::wayland::pipewire::format::{
        self, FormatError, MAX_DIMENSION, MEDIA_SUBTYPE_RAW, MEDIA_TYPE_VIDEO, Negotiated,
        OBJECT_FORMAT, OBJECT_PARAM_BUFFERS, PREFERRED_FORMATS, buffer_key, data_type, key, param,
        primaries, video_format,
    };
    use super::wayland::pipewire::pod::{Object, ObjectRef, Property, Scalar, choice};

    fn format_object(properties: Vec<Property>) -> Vec<u8> {
        Object {
            object_type: OBJECT_FORMAT,
            id: param::FORMAT,
            properties,
        }
        .encode()
    }

    fn raw_video(extra: Vec<Property>) -> Vec<u8> {
        let mut properties = vec![
            Property::scalar(key::MEDIA_TYPE, &Scalar::id(MEDIA_TYPE_VIDEO)),
            Property::scalar(key::MEDIA_SUBTYPE, &Scalar::id(MEDIA_SUBTYPE_RAW)),
        ];
        properties.extend(extra);
        format_object(properties)
    }

    /// The offer must announce raw video and carry a format list, a size range
    /// and a framerate range. A missing `mediaType` is not a degraded offer —
    /// the server discards it and the stream never starts.
    #[test]
    fn the_offer_is_a_well_formed_raw_video_enum_format() {
        let offered = format::enum_format_param();
        let parsed = ObjectRef::parse(&offered).expect("the offer is a valid object");

        assert_eq!(parsed.object_type, OBJECT_FORMAT);
        assert_eq!(parsed.id, param::ENUM_FORMAT);
        assert_eq!(
            parsed
                .property(key::MEDIA_TYPE)
                .and_then(|property| property.as_id()),
            Some(MEDIA_TYPE_VIDEO)
        );
        assert_eq!(
            parsed
                .property(key::MEDIA_SUBTYPE)
                .and_then(|property| property.as_id()),
            Some(MEDIA_SUBTYPE_RAW)
        );
        assert!(parsed.property(key::VIDEO_FORMAT).is_some());
        assert!(parsed.property(key::VIDEO_SIZE).is_some());
        assert!(parsed.property(key::VIDEO_FRAMERATE).is_some());
    }

    /// No modifier is offered, and that is a decision rather than an oversight.
    /// The follow-up ParamBuffers response makes the shared-memory fallback
    /// explicit rather than relying on a producer default.
    #[test]
    fn the_offer_names_no_dma_buf_modifier() {
        let offered = format::enum_format_param();
        let parsed = ObjectRef::parse(&offered).expect("valid");
        assert!(parsed.property(key::VIDEO_MODIFIER).is_none());
    }

    #[test]
    fn the_buffer_response_allows_only_shared_memory() {
        let response = format::shared_memory_buffer_param();
        let parsed = ObjectRef::parse(&response).expect("valid ParamBuffers object");
        assert_eq!(parsed.object_type, OBJECT_PARAM_BUFFERS);
        assert_eq!(parsed.id, param::BUFFERS);

        let data_types = parsed
            .property(buffer_key::DATA_TYPE)
            .expect("memory types are explicit");
        assert_eq!(data_types.choice_flavour(), Some(choice::FLAGS));
        assert_eq!(data_types.as_int(), Some(data_type::SHARED_MEMORY_MASK));
        assert_eq!(
            data_type::SHARED_MEMORY_MASK,
            (1_i32 << data_type::MEM_PTR) | (1_i32 << data_type::MEM_FD)
        );
        assert_eq!(
            data_type::SHARED_MEMORY_MASK & (1_i32 << data_type::DMA_BUF),
            0
        );
    }

    /// The framerate is a range including zero. An idle screen-cast source
    /// reports 0/1, and a client that pinned 25/1 or 60/1 would fail to
    /// intersect with it and wait forever for a stream that never starts.
    #[test]
    fn the_framerate_range_starts_at_zero() {
        let offered = format::enum_format_param();
        let parsed = ObjectRef::parse(&offered).expect("valid");
        let framerate = parsed
            .property(key::VIDEO_FRAMERATE)
            .expect("a framerate is offered");
        assert_eq!(
            framerate.as_fraction(),
            Some((0, 1)),
            "the range's default is the idle rate"
        );
    }

    /// BGRx first: it is what Mutter and KWin composite in, so matching it means
    /// the server hands over its own buffer rather than converting every frame.
    #[test]
    fn bgrx_is_offered_ahead_of_the_alternatives() {
        assert_eq!(PREFERRED_FORMATS[0], video_format::BGRX);
        assert_eq!(PREFERRED_FORMATS.len(), 4);
    }

    /// Alpha honesty. SPA has no premultiplied video format, so `BGRA` means
    /// straight alpha and must never be reported as premultiplied — doing so
    /// black-fringes every antialiased edge downstream. The `x` variants have an
    /// undefined fourth byte, which is a separate fact and is tracked
    /// separately.
    #[test]
    fn pixel_formats_map_without_inventing_premultiplication() {
        assert_eq!(
            format::pixel_format(video_format::BGRX),
            Some((PixelFormat::Bgra8, true))
        );
        assert_eq!(
            format::pixel_format(video_format::RGBX),
            Some((PixelFormat::Rgba8, true))
        );
        assert_eq!(
            format::pixel_format(video_format::BGRA),
            Some((PixelFormat::Bgra8, false))
        );
        assert_eq!(
            format::pixel_format(video_format::RGBA),
            Some((PixelFormat::Rgba8, false))
        );
        assert_eq!(format::pixel_format(0), None);

        for spa in PREFERRED_FORMATS {
            let (mapped, _) = format::pixel_format(spa).expect("every offered format maps");
            assert!(
                !mapped.is_premultiplied(),
                "SPA has no premultiplied video format, so none may be reported"
            );
        }
    }

    /// An unrecognised or absent colour space stays unknown rather than
    /// defaulting to sRGB, so an encoder can decline to embed a profile instead
    /// of embedding a wrong one on a wide-gamut display.
    #[test]
    fn colour_primaries_map_conservatively() {
        assert_eq!(
            format::color_space(Some(primaries::BT709)),
            ColorSpace::Srgb
        );
        assert_eq!(
            format::color_space(Some(primaries::BT2020)),
            ColorSpace::Rec2020
        );
        assert_eq!(
            format::color_space(Some(primaries::SMPTE_RP431)),
            ColorSpace::DisplayP3
        );
        assert_eq!(
            format::color_space(Some(primaries::SMPTE_EG432)),
            ColorSpace::DisplayP3
        );
        assert_eq!(format::color_space(None), ColorSpace::Unknown);
        assert_eq!(
            format::color_space(Some(primaries::UNKNOWN)),
            ColorSpace::Unknown
        );
        assert_eq!(format::color_space(Some(4242)), ColorSpace::Unknown);
    }

    #[test]
    fn a_fixated_format_is_understood() {
        let bytes = raw_video(vec![
            Property::scalar(key::VIDEO_FORMAT, &Scalar::id(video_format::BGRX)),
            Property::scalar(key::VIDEO_SIZE, &Scalar::rectangle(2560, 1440)),
            Property::scalar(key::VIDEO_PRIMARIES, &Scalar::id(primaries::BT709)),
        ]);

        assert_eq!(
            format::parse_format(&bytes),
            Ok(Negotiated {
                width: 2560,
                height: 1440,
                pixel_format: PixelFormat::Bgra8,
                opaque_padding: true,
                color_space: ColorSpace::Srgb,
            })
        );
    }

    #[test]
    fn a_format_without_primaries_is_still_usable() {
        let bytes = raw_video(vec![
            Property::scalar(key::VIDEO_FORMAT, &Scalar::id(video_format::RGBA)),
            Property::scalar(key::VIDEO_SIZE, &Scalar::rectangle(800, 600)),
        ]);

        let negotiated = format::parse_format(&bytes).expect("usable");
        assert_eq!(negotiated.color_space, ColorSpace::Unknown);
        assert_eq!(negotiated.pixel_format, PixelFormat::Rgba8);
        assert!(!negotiated.opaque_padding);
        assert_eq!(negotiated.packed_stride(), 3200);
        assert_eq!(negotiated.packed_len(), 3200 * 600);
    }

    /// Every way the answer can be unusable has to be distinguishable, because
    /// each one points at a different thing being wrong on the user's machine.
    #[test]
    fn unusable_formats_are_classified_rather_than_lumped_together() {
        assert_eq!(format::parse_format(&[]), Err(FormatError::NotAFormat));

        let audio = format_object(vec![
            Property::scalar(key::MEDIA_TYPE, &Scalar::id(1)),
            Property::scalar(key::MEDIA_SUBTYPE, &Scalar::id(1)),
        ]);
        assert_eq!(
            format::parse_format(&audio),
            Err(FormatError::WrongMedia {
                media_type: 1,
                media_subtype: 1,
            })
        );

        let no_format = raw_video(vec![Property::scalar(
            key::VIDEO_SIZE,
            &Scalar::rectangle(64, 64),
        )]);
        assert_eq!(
            format::parse_format(&no_format),
            Err(FormatError::MissingFormat)
        );

        let no_size = raw_video(vec![Property::scalar(
            key::VIDEO_FORMAT,
            &Scalar::id(video_format::BGRX),
        )]);
        assert_eq!(
            format::parse_format(&no_size),
            Err(FormatError::MissingSize)
        );

        let zero_size = raw_video(vec![
            Property::scalar(key::VIDEO_FORMAT, &Scalar::id(video_format::BGRX)),
            Property::scalar(key::VIDEO_SIZE, &Scalar::rectangle(1920, 0)),
        ]);
        assert_eq!(
            format::parse_format(&zero_size),
            Err(FormatError::MissingSize),
            "a zero dimension is as unusable as a missing one"
        );

        let never_offered = raw_video(vec![
            Property::scalar(key::VIDEO_FORMAT, &Scalar::id(9)),
            Property::scalar(key::VIDEO_SIZE, &Scalar::rectangle(64, 64)),
        ]);
        assert_eq!(
            format::parse_format(&never_offered),
            Err(FormatError::UnsupportedFormat(9))
        );

        let oversized = raw_video(vec![
            Property::scalar(key::VIDEO_FORMAT, &Scalar::id(video_format::BGRX)),
            Property::scalar(key::VIDEO_SIZE, &Scalar::rectangle(MAX_DIMENSION + 1, 64)),
        ]);
        assert_eq!(
            format::parse_format(&oversized),
            Err(FormatError::UnsupportedSize {
                width: MAX_DIMENSION + 1,
                height: 64,
            })
        );

        let modifier = raw_video(vec![
            Property::scalar(key::VIDEO_FORMAT, &Scalar::id(video_format::BGRX)),
            Property::scalar(key::VIDEO_SIZE, &Scalar::rectangle(64, 64)),
            Property::scalar(key::VIDEO_MODIFIER, &Scalar::long(0)),
        ]);
        assert_eq!(
            format::parse_format(&modifier),
            Err(FormatError::UnexpectedModifier)
        );
    }
}

/// Stride handling: the single most common way a screenshot comes out skewed.
mod stride_packing {
    use std::borrow::Cow;

    use super::wayland::pipewire::format::{BufferError, linear_chunk, pack_rows};

    /// A padded buffer must be read row by row at the producer's stride and
    /// written at the packed one. Reading it linearly gives the classic
    /// diagonal shear, which looks like a decoding bug and is not.
    #[test]
    fn rows_are_lifted_out_of_padded_strides() {
        // Two rows of two pixels, with sixteen bytes of padding per row.
        let mut source = Vec::new();
        for row in 0..2u8 {
            for pixel in 0..2u8 {
                source.extend_from_slice(&[row, pixel, 0x11, 0x22]);
            }
            source.extend_from_slice(&[0xEE; 16]);
        }

        let packed = pack_rows(&source, 24, 2, 2, false).expect("a padded buffer is readable");

        assert_eq!(packed.len(), 16);
        assert_eq!(
            packed,
            vec![
                0, 0, 0x11, 0x22, 0, 1, 0x11, 0x22, //
                1, 0, 0x11, 0x22, 1, 1, 0x11, 0x22,
            ],
            "no padding survives, and no row is shifted"
        );
    }

    /// The final row needs only `width * 4` bytes. A producer is entitled to end
    /// its mapping there, and demanding a full trailing stride would reject a
    /// perfectly good buffer — which shows up as an intermittent failure on
    /// exactly one compositor.
    #[test]
    fn a_buffer_ending_after_the_last_visible_row_is_accepted() {
        let stride = 24usize;
        let mut source = vec![0x40; stride + 8];
        source[stride] = 0x41;

        let packed = pack_rows(&source, stride as i32, 2, 2, false).expect("short tail is fine");
        assert_eq!(packed.len(), 16);
        assert_eq!(packed[8], 0x41);

        assert_eq!(
            pack_rows(&source[..stride + 7], stride as i32, 2, 2, false),
            Err(BufferError::Short {
                available: stride + 7,
                needed: stride + 8,
            }),
            "one byte short is short"
        );
    }

    /// In an `x` format the fourth byte is undefined, not opaque. Copying it
    /// through produces a PNG with a random alpha channel — usually fully
    /// transparent, which reads as "the screenshot is blank".
    #[test]
    fn undefined_padding_bytes_are_forced_opaque() {
        let source = vec![0x10, 0x20, 0x30, 0x00, 0x40, 0x50, 0x60, 0x07];

        let kept = pack_rows(&source, 8, 2, 1, false).expect("readable");
        assert_eq!(kept[3], 0x00);
        assert_eq!(kept[7], 0x07);

        let forced = pack_rows(&source, 8, 2, 1, true).expect("readable");
        assert_eq!(forced[3], 0xFF);
        assert_eq!(forced[7], 0xFF);
        assert_eq!(
            &forced[..3],
            &[0x10, 0x20, 0x30],
            "only the fourth byte is touched"
        );
    }

    /// A stride narrower than the row, or a negative one, describes a buffer
    /// that cannot exist. Trusting it is an out-of-bounds read.
    #[test]
    fn impossible_strides_are_refused() {
        let source = vec![0u8; 4096];

        assert_eq!(
            pack_rows(&source, 4, 2, 1, false),
            Err(BufferError::BadStride {
                stride: 4,
                needed: 8,
            })
        );
        assert_eq!(
            pack_rows(&source, -8, 2, 1, false),
            Err(BufferError::BadStride {
                stride: -8,
                needed: 8,
            })
        );
        assert_eq!(
            pack_rows(&source, 0, 2, 1, false),
            Err(BufferError::BadStride {
                stride: 0,
                needed: 8,
            })
        );
    }

    #[test]
    fn invalid_geometry_is_rejected_before_arithmetic_or_allocation() {
        assert_eq!(
            pack_rows(&[], 4, 0, 1, false),
            Err(BufferError::InvalidDimensions {
                width: 0,
                height: 1,
            })
        );
        assert!(matches!(
            pack_rows(&[], i32::MAX, u32::MAX, u32::MAX, false),
            Err(BufferError::SizeOverflow { .. })
        ));
    }

    /// SPA defines a chunk offset modulo maxsize. Rejecting an offset at or past
    /// the mapping length incorrectly drops valid ring-buffer data.
    #[test]
    fn chunk_offsets_wrap_and_wrapped_data_is_linearized() {
        let mapping = [0, 1, 2, 3, 4, 5, 6, 7];

        let contiguous = linear_chunk(&mapping, 10, 3);
        assert!(matches!(&contiguous, Cow::Borrowed(_)));
        assert_eq!(contiguous.as_ref(), &[2, 3, 4]);

        let wrapped = linear_chunk(&mapping, 6, 5);
        assert!(matches!(&wrapped, Cow::Owned(_)));
        assert_eq!(wrapped.as_ref(), &[6, 7, 0, 1, 2]);

        assert_eq!(
            linear_chunk(&mapping, 0, u32::MAX).as_ref(),
            mapping.as_slice(),
            "chunk size is clamped to maxsize"
        );
    }
}

/// The capture lifecycle, which decides when to keep waiting and when to give
/// up — and what to say when it does.
mod stream_lifecycle {
    use scrozz_core::{ColorSpace, Error, PixelFormat};

    use super::wayland::pipewire::format::Negotiated;
    use super::wayland::pipewire::lifecycle::{
        Action, Event, Failure, Lifecycle, Phase, StreamState, prefer_process_event,
    };

    fn negotiated() -> Negotiated {
        Negotiated {
            width: 64,
            height: 32,
            pixel_format: PixelFormat::Bgra8,
            opaque_padding: true,
            color_space: ColorSpace::Unknown,
        }
    }

    fn started() -> Lifecycle {
        let mut lifecycle = Lifecycle::new(10);
        assert_eq!(
            lifecycle.observe(Event::StateChanged(StreamState::Streaming, None)),
            Action::Wait
        );
        assert_eq!(
            lifecycle.observe(Event::FormatAgreed(negotiated())),
            Action::Wait
        );
        lifecycle
    }

    #[test]
    fn unknown_raw_states_are_treated_as_errors_rather_than_waited_on() {
        assert_eq!(StreamState::from_raw(0), StreamState::Unconnected);
        assert_eq!(StreamState::from_raw(3), StreamState::Streaming);
        assert_eq!(StreamState::from_raw(-1), StreamState::Error);
        assert_eq!(StreamState::from_raw(99), StreamState::Error);
    }

    #[test]
    fn a_format_then_a_frame_is_a_successful_capture() {
        let mut lifecycle = started();
        assert_eq!(lifecycle.observe(Event::FrameReady), Action::Stop);
        assert_eq!(*lifecycle.phase(), Phase::Captured);
        assert_eq!(
            lifecycle.outcome().expect("a capture succeeds"),
            negotiated()
        );
    }

    /// Mutter hands over empty buffers when nothing on screen has changed.
    /// Accepting the first buffer offered is why a naive implementation produces
    /// a black PNG on an idle desktop; an empty buffer is an ordinary event and
    /// must not settle anything.
    #[test]
    fn empty_buffers_are_expected_and_keep_the_capture_waiting() {
        let mut lifecycle = started();
        for _ in 0..5 {
            assert_eq!(lifecycle.observe(Event::EmptyBuffer), Action::Wait);
            assert!(!lifecycle.is_settled());
        }
        assert_eq!(lifecycle.observe(Event::FrameReady), Action::Stop);
        assert!(lifecycle.outcome().is_ok());
    }

    #[test]
    fn draining_buffers_preserves_frame_and_failure_precedence() {
        assert_eq!(
            prefer_process_event(Event::FrameReady, Event::EmptyBuffer),
            Event::FrameReady,
            "a later empty buffer cannot erase pixels already copied"
        );
        assert_eq!(
            prefer_process_event(Event::EmptyBuffer, Event::FrameReady),
            Event::FrameReady
        );
        assert_eq!(
            prefer_process_event(Event::FrameReady, Event::BufferRejected("bad plane".into())),
            Event::BufferRejected("bad plane".into()),
            "a structural rejection outranks a plausible frame"
        );
        assert_eq!(
            prefer_process_event(
                Event::BufferRejected("first diagnosis".into()),
                Event::BufferRejected("later diagnosis".into())
            ),
            Event::BufferRejected("first diagnosis".into())
        );
    }

    /// PipeWire keeps firing callbacks while a stream is torn down. A late
    /// `state_changed(Unconnected)` arriving after a good frame must not turn a
    /// captured screenshot into "the target disappeared".
    #[test]
    fn events_after_a_capture_cannot_undo_it() {
        let mut lifecycle = started();
        lifecycle.observe(Event::FrameReady);

        assert_eq!(
            lifecycle.observe(Event::StateChanged(StreamState::Unconnected, None)),
            Action::Stop
        );
        assert_eq!(
            lifecycle.observe(Event::StateChanged(
                StreamState::Error,
                Some("too late".into())
            )),
            Action::Stop
        );
        assert_eq!(*lifecycle.phase(), Phase::Captured);
        assert!(lifecycle.outcome().is_ok());
    }

    /// The same raw state means two different things depending on whether the
    /// stream ever ran, and the user needs to be told which.
    #[test]
    fn disconnection_reads_differently_before_and_after_streaming() {
        let mut never = Lifecycle::new(10);
        assert_eq!(
            never.observe(Event::StateChanged(StreamState::Unconnected, None)),
            Action::Stop
        );
        let Phase::Failed(Failure::Gone(before)) = never.phase().clone() else {
            panic!("an unconnected stream is a failure");
        };
        assert!(
            before.contains("does not exist"),
            "before streaming it means the node was never there: {before}"
        );

        let mut after = started();
        assert_eq!(
            after.observe(Event::StateChanged(StreamState::Unconnected, None)),
            Action::Stop
        );
        let Phase::Failed(Failure::Gone(during)) = after.phase().clone() else {
            panic!("a disconnection mid-capture is a failure");
        };
        assert!(
            during.contains("disappeared"),
            "after streaming it means the target went away: {during}"
        );

        assert!(matches!(after.outcome(), Err(Error::TargetGone(_))));
    }

    #[test]
    fn repeated_capture_retains_streaming_history_and_format() {
        let mut resumed = Lifecycle::resume(10, Some(negotiated()));
        assert_eq!(resumed.format(), Some(negotiated()));
        assert_eq!(
            resumed.observe(Event::StateChanged(StreamState::Unconnected, None)),
            Action::Stop
        );
        let Phase::Failed(Failure::Gone(during)) = resumed.phase().clone() else {
            panic!("a resumed stream disconnection is a failure");
        };
        assert!(
            during.contains("disappeared"),
            "a later lifecycle must remember that the stream previously ran: {during}"
        );
    }

    /// Pixels whose layout is unknown are worse than no pixels: they would be
    /// saved as a plausible-looking, wrong image.
    #[test]
    fn a_frame_before_any_agreed_format_is_a_failure() {
        let mut lifecycle = Lifecycle::new(10);
        lifecycle.observe(Event::StateChanged(StreamState::Streaming, None));
        assert_eq!(lifecycle.observe(Event::FrameReady), Action::Stop);
        assert!(matches!(
            lifecycle.phase(),
            Phase::Failed(Failure::Format(_))
        ));
    }

    #[test]
    fn structural_failures_settle_immediately_and_carry_their_reason() {
        let mut rejected = started();
        assert_eq!(
            rejected.observe(Event::BufferRejected("dma-buf".into())),
            Action::Stop
        );
        assert_eq!(
            rejected.phase().clone(),
            Phase::Failed(Failure::Buffer("dma-buf".into()))
        );

        let mut errored = Lifecycle::new(10);
        assert_eq!(
            errored.observe(Event::StateChanged(
                StreamState::Error,
                Some("node error".into())
            )),
            Action::Stop
        );
        assert_eq!(
            errored.phase().clone(),
            Phase::Failed(Failure::Stream("node error".into()))
        );

        let mut silent = Lifecycle::new(10);
        silent.observe(Event::StateChanged(StreamState::Error, None));
        let Phase::Failed(Failure::Stream(why)) = silent.phase().clone() else {
            panic!("an error state is a failure");
        };
        assert!(!why.is_empty(), "a silent error still needs words");
    }

    /// A timeout is reported as a compositor problem with the timeout in it,
    /// because by this point the user has already granted permission and is
    /// looking at a screenshot that appears to have hung.
    #[test]
    fn a_timeout_names_the_wait_and_the_likely_cause() {
        let mut lifecycle = started();
        assert_eq!(lifecycle.observe(Event::TimedOut), Action::Stop);
        assert_eq!(
            lifecycle.phase().clone(),
            Phase::Failed(Failure::Timeout(10))
        );

        let Err(Error::Platform(message)) = lifecycle.outcome() else {
            panic!("a timeout is a platform failure");
        };
        assert!(message.contains("10s"));
        assert!(message.contains("compositor"));
    }

    /// Connecting and paused are ordinary intermediate states; treating either
    /// as terminal would abort most captures on a cold screen-cast source.
    #[test]
    fn intermediate_states_keep_the_capture_alive() {
        let mut lifecycle = Lifecycle::new(10);
        assert_eq!(
            lifecycle.observe(Event::StateChanged(StreamState::Connecting, None)),
            Action::Wait
        );
        assert_eq!(
            lifecycle.observe(Event::StateChanged(StreamState::Paused, None)),
            Action::Wait
        );
        assert!(!lifecycle.is_settled());
        assert_eq!(*lifecycle.phase(), Phase::Connecting);
    }
}

/// Turning portal failures into things a person can act on.
mod portal_errors {
    use scrozz_core::Error;

    use super::wayland::portal::{
        PortalFailure, describe_sources, is_restore_token_rejection, source_type,
    };

    /// The single most important mapping in this backend. Pressing Escape in the
    /// portal's picker is not a fault, and a screenshot tool that shows an error
    /// dialog when someone changes their mind is one people stop using.
    #[test]
    fn dismissing_the_picker_is_cancellation_and_nothing_else() {
        assert!(matches!(
            PortalFailure::Cancelled.into_error("GNOME"),
            Error::Cancelled
        ));
    }

    /// A missing portal must name the package to install, per-compositor. "Screen
    /// capture failed" leaves the user with nothing to do next.
    #[test]
    fn a_missing_portal_names_the_packages_to_install() {
        let Error::Unsupported { what, why } =
            PortalFailure::Missing("no name owner".into()).into_error("sway")
        else {
            panic!("a missing portal is an unsupported capability");
        };

        assert!(what.contains("Wayland"));
        assert!(why.contains("sway"), "the compositor is named: {why}");
        assert!(why.contains("xdg-desktop-portal-wlr"));
        assert!(why.contains("xdg-desktop-portal-gnome"));
        assert!(why.contains("xdg-desktop-portal-kde"));
        assert!(why.contains("no name owner"), "the detail survives: {why}");
    }

    /// A portal that offers monitors but not windows is a limitation of that
    /// compositor's backend, and saying so is more useful than a generic
    /// refusal.
    #[test]
    fn an_unavailable_source_type_explains_which_side_is_missing() {
        let Error::Unsupported { what, why } = (PortalFailure::NoSources {
            wanted: source_type::WINDOW,
            available: source_type::MONITOR,
        })
        .into_error("wlroots") else {
            panic!("an unavailable source is an unsupported capability");
        };

        assert!(what.contains("individual windows"));
        assert!(why.contains("whole monitors"));
        assert!(why.contains("wlroots"));
    }

    #[test]
    fn portal_and_bus_faults_stay_platform_errors() {
        assert!(matches!(
            PortalFailure::NoStreams.into_error("KDE"),
            Error::Platform(_)
        ));

        let Error::Platform(message) = PortalFailure::Bus("timed out".into()).into_error("KDE")
        else {
            panic!("a bus fault is a platform error");
        };
        assert!(message.contains("KDE"));
        assert!(message.contains("timed out"));
    }

    #[test]
    fn only_an_identified_restore_rejection_is_retryable() {
        let rejected = PortalFailure::RestoreRejected("Restore token is not valid".into());
        assert!(rejected.should_retry_without_restore());

        for failure in [
            PortalFailure::Cancelled,
            PortalFailure::Missing("portal absent".into()),
            PortalFailure::NoSources {
                wanted: source_type::WINDOW,
                available: source_type::MONITOR,
            },
            PortalFailure::NoStreams,
            PortalFailure::Bus("connection closed".into()),
        ] {
            assert!(
                !failure.should_retry_without_restore(),
                "{failure:?} cannot be repaired by deleting a token"
            );
        }

        assert!(is_restore_token_rejection("Restore token is not valid"));
        assert!(!is_restore_token_rejection("Source type is not supported"));
    }

    #[test]
    fn source_masks_are_described_in_readable_english() {
        assert_eq!(describe_sources(0), "no capture sources");
        assert_eq!(describe_sources(source_type::MONITOR), "whole monitors");
        assert_eq!(
            describe_sources(source_type::MONITOR | source_type::WINDOW),
            "whole monitors and individual windows"
        );
        assert_eq!(
            describe_sources(source_type::MONITOR | source_type::WINDOW | source_type::VIRTUAL),
            "whole monitors, individual windows and virtual sources"
        );
    }
}

/// Placing a user-drawn region inside a portal-supplied monitor stream.
mod region_cropping {
    use scrozz_core::{
        ColorSpace, Error, Frame, LogicalPoint, LogicalRect, LogicalSize, PhysicalSize,
        PixelFormat, ScaleFactor,
    };

    use super::wayland::portal::StreamInfo;
    use super::wayland::region::{self, CropError, CropRect};

    fn stream(position: Option<(i32, i32)>, size: Option<(i32, i32)>) -> StreamInfo {
        StreamInfo {
            node_id: 7,
            position,
            size,
        }
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> LogicalRect {
        LogicalRect {
            origin: LogicalPoint::new(x, y),
            size: LogicalSize::new(width, height),
        }
    }

    /// A frame whose pixels encode their own coordinates, so a crop that is off
    /// by one row or one column is visible in the assertion rather than hidden
    /// behind a length check.
    fn coordinate_frame(width: u32, height: u32, stride_padding: usize) -> Frame {
        let stride = width as usize * 4 + stride_padding;
        let mut data = vec![0xEE; stride * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let at = y * stride + x * 4;
                data[at] = x as u8;
                data[at + 1] = y as u8;
                data[at + 2] = 0x33;
                data[at + 3] = 0xFF;
            }
        }
        Frame {
            data,
            size: PhysicalSize::new(f64::from(width), f64::from(height)),
            stride,
            format: PixelFormat::Bgra8,
            color_space: ColorSpace::Unknown,
            scale: ScaleFactor::IDENTITY,
        }
    }

    #[test]
    fn a_region_on_an_unscaled_monitor_maps_one_to_one() {
        let plan = region::plan_crop(
            rect(100.0, 50.0, 200.0, 100.0),
            &stream(Some((0, 0)), Some((1920, 1080))),
            (1920, 1080),
        );

        assert_eq!(
            plan,
            Ok(CropRect {
                x: 100,
                y: 50,
                width: 200,
                height: 100,
            })
        );
    }

    /// The monitor's own origin has to come out of the region's coordinates, or
    /// a selection on a second monitor is cropped from the wrong place entirely
    /// — usually producing a picture of the first monitor's left edge.
    #[test]
    fn the_monitors_desktop_origin_is_subtracted() {
        let plan = region::plan_crop(
            rect(2020.0, 100.0, 100.0, 100.0),
            &stream(Some((1920, 0)), Some((1920, 1080))),
            (1920, 1080),
        );

        assert_eq!(
            plan,
            Ok(CropRect {
                x: 100,
                y: 100,
                width: 100,
                height: 100,
            })
        );
    }

    /// The scale is derived from the frame the compositor actually delivered
    /// against the logical size the portal reported, rather than from a display
    /// scale factor. Under fractional scaling those disagree, and only the
    /// former is a fact.
    #[test]
    fn the_scale_is_derived_from_the_delivered_frame() {
        let plan = region::plan_crop(
            rect(100.0, 100.0, 50.0, 50.0),
            &stream(Some((0, 0)), Some((1920, 1080))),
            (2400, 1350),
        );

        // 1.25x: 100 logical is pixel 125, and 50 logical is 62.5 pixels, which
        // rounds outward to cover the whole selection.
        assert_eq!(
            plan,
            Ok(CropRect {
                x: 125,
                y: 125,
                width: 63,
                height: 63,
            })
        );
    }

    /// A selection dragged past the edge of the screen is clamped to what exists
    /// rather than becoming a negative origin or an over-long read.
    #[test]
    fn a_region_overhanging_an_edge_is_clamped() {
        let plan = region::plan_crop(
            rect(-40.0, -40.0, 100.0, 100.0),
            &stream(Some((0, 0)), Some((800, 600))),
            (800, 600),
        );
        assert_eq!(
            plan,
            Ok(CropRect {
                x: 0,
                y: 0,
                width: 60,
                height: 60,
            })
        );

        let bottom_right = region::plan_crop(
            rect(760.0, 560.0, 200.0, 200.0),
            &stream(Some((0, 0)), Some((800, 600))),
            (800, 600),
        );
        assert_eq!(
            bottom_right,
            Ok(CropRect {
                x: 760,
                y: 560,
                width: 40,
                height: 40,
            })
        );
    }

    #[test]
    fn a_region_that_misses_the_monitor_is_an_invalid_request() {
        let plan = region::plan_crop(
            rect(3000.0, 100.0, 100.0, 100.0),
            &stream(Some((0, 0)), Some((1920, 1080))),
            (1920, 1080),
        );
        assert_eq!(plan, Err(CropError::Outside));
        assert!(matches!(
            CropError::Outside.into_error("GNOME"),
            Error::InvalidRequest(_)
        ));
    }

    /// Some portals decline to say where a stream is. Returning the whole
    /// monitor would hand back an image the user did not ask for and had no way
    /// to notice was wrong, so the refusal is explicit and suggests what to do.
    #[test]
    fn a_stream_without_geometry_refuses_rather_than_guesses() {
        assert_eq!(
            region::plan_crop(
                rect(0.0, 0.0, 10.0, 10.0),
                &stream(None, Some((1920, 1080))),
                (1920, 1080)
            ),
            Err(CropError::NoGeometry)
        );
        assert_eq!(
            region::plan_crop(
                rect(0.0, 0.0, 10.0, 10.0),
                &stream(Some((0, 0)), None),
                (1920, 1080)
            ),
            Err(CropError::NoGeometry)
        );

        let Error::Unsupported { what, why } = CropError::NoGeometry.into_error("Hyprland") else {
            panic!("a placeless stream is an unsupported capability");
        };
        assert!(what.contains("region"));
        assert!(why.contains("Hyprland"));
        assert!(why.contains("whole display"), "it says what to do: {why}");
    }

    #[test]
    fn a_degenerate_stream_is_a_platform_fault() {
        assert_eq!(
            region::plan_crop(
                rect(0.0, 0.0, 10.0, 10.0),
                &stream(Some((0, 0)), Some((0, 1080))),
                (1920, 1080)
            ),
            Err(CropError::DegenerateStream)
        );
        assert!(matches!(
            CropError::DegenerateStream.into_error("KDE"),
            Error::Platform(_)
        ));
    }

    /// The crop reads from a padded source and writes a packed destination, so
    /// both strides are exercised at once. Checking the corner pixels is what
    /// catches an off-by-one in either axis.
    #[test]
    fn cropping_copies_the_right_pixels_out_of_a_padded_frame() {
        let frame = coordinate_frame(16, 8, 12);
        let cropped = region::crop(
            &frame,
            CropRect {
                x: 3,
                y: 2,
                width: 4,
                height: 3,
            },
        )
        .expect("an in-bounds crop succeeds");

        assert_eq!(cropped.width(), 4);
        assert_eq!(cropped.height(), 3);
        assert_eq!(cropped.stride, 16, "the result is tightly packed");
        assert_eq!(cropped.data.len(), 48);
        assert!(cropped.is_well_formed());
        assert_eq!(cropped.format, frame.format);
        assert_eq!(cropped.color_space, frame.color_space);

        assert_eq!(&cropped.data[0..2], &[3, 2], "top-left is (3, 2)");
        assert_eq!(&cropped.data[12..14], &[6, 2], "top-right is (6, 2)");
        assert_eq!(&cropped.data[32..34], &[3, 4], "bottom-left is (3, 4)");
        assert_eq!(&cropped.data[44..46], &[6, 4], "bottom-right is (6, 4)");
    }

    /// The last line where a mistake could become an out-of-bounds read, so it
    /// is checked rather than assumed even though `plan_crop` guarantees it.
    #[test]
    fn an_out_of_bounds_crop_is_refused_rather_than_read() {
        let frame = coordinate_frame(8, 8, 0);

        for bad in [
            CropRect {
                x: 6,
                y: 0,
                width: 4,
                height: 1,
            },
            CropRect {
                x: 0,
                y: 6,
                width: 1,
                height: 4,
            },
            CropRect {
                x: 0,
                y: 0,
                width: 0,
                height: 4,
            },
            CropRect {
                x: u32::MAX,
                y: 0,
                width: 4,
                height: 1,
            },
        ] {
            assert_eq!(
                region::crop(&frame, bad).err(),
                Some(CropError::Outside),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_recovered_scale_is_the_ratio_the_compositor_used() {
        assert_eq!(
            region::resolve_scale(
                &stream(Some((0, 0)), Some((1920, 1080))),
                (3840, 2160),
                ScaleFactor::IDENTITY
            )
            .get(),
            2.0
        );
        assert_eq!(
            region::resolve_scale(
                &stream(Some((0, 0)), Some((1920, 1080))),
                (1920, 1080),
                ScaleFactor::new(2.0)
            )
            .get(),
            1.0,
            "a delivered 1:1 frame overrides a guessed scale"
        );
    }

    /// The only case with nothing to divide. Falling back is right; panicking in
    /// `ScaleFactor::new` on a zero would not be.
    #[test]
    fn an_unmeasurable_stream_falls_back_to_the_displays_scale() {
        let fallback = ScaleFactor::new(1.5);
        assert_eq!(
            region::resolve_scale(&stream(Some((0, 0)), None), (1920, 1080), fallback),
            fallback
        );
        assert_eq!(
            region::resolve_scale(&stream(Some((0, 0)), Some((0, 0))), (1920, 1080), fallback),
            fallback
        );
        assert_eq!(
            region::resolve_scale(&stream(Some((0, 0)), Some((1920, 1080))), (0, 0), fallback),
            fallback
        );
    }
}
