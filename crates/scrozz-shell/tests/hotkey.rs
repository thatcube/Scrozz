//! Hotkey and tray tests that run with no display server and grab no keys.
//!
//! Every test here uses [`GlobalHotkeys::detached`] or
//! [`GlobalHotkeys::detached_in`], which do all the bookkeeping, conflict
//! detection and config generation without touching the OS. Nothing in this file
//! registers a system-wide hotkey or creates a tray item: a test suite that
//! grabbed `Cmd+Shift+4` off the machine running it, or left a menu-bar item
//! behind after a failure, would be a worse bug than the ones it was catching.

use scrozz_core::Error;
use scrozz_shell::hotkey::{
    Accelerator, Compositor, Conflict, DisplayServer, GlobalHotkeys, Session, reserved_shortcuts,
};
use scrozz_shell::tray::{TrayAction, TrayEntry, default_icon_rgba, menu_model, recording_label};
use scrozz_shell::{Hotkey, HotkeyManager};

fn hotkey(accelerator: &str) -> Hotkey {
    Hotkey {
        accelerator: accelerator.to_owned(),
    }
}

fn parse(accelerator: &str) -> Accelerator {
    Accelerator::parse(accelerator).expect("accelerator should parse")
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

#[test]
fn cmd_super_and_meta_are_the_same_modifier() {
    let cmd = parse("Cmd+Shift+4");
    assert_eq!(cmd, parse("Super+Shift+4"));
    assert_eq!(cmd, parse("Meta+Shift+4"));
    assert_eq!(cmd, parse("Command+Shift+4"));
    assert_eq!(cmd, parse("CmdOrCtrl+Shift+4"));
}

#[test]
fn parsing_ignores_case_and_whitespace() {
    let canonical = parse("Cmd+Shift+4");
    assert_eq!(canonical, parse("cmd+shift+4"));
    assert_eq!(canonical, parse("CMD + SHIFT + 4"));
    assert_eq!(canonical, parse("  Cmd  +Shift+  4 "));
}

#[test]
fn modifier_order_does_not_matter_and_the_key_may_come_first() {
    let canonical = parse("Cmd+Shift+4");
    assert_eq!(canonical, parse("Shift+Cmd+4"));
    assert_eq!(canonical, parse("4+Shift+Cmd"));
}

#[test]
fn repeating_a_modifier_is_harmless() {
    assert_eq!(parse("Cmd+Cmd+Shift+4"), parse("Cmd+Shift+4"));
}

#[test]
fn cmd_maps_to_control_off_macos_and_to_command_on_it() {
    let cmd = parse("Cmd+Shift+4");
    let ctrl = parse("Ctrl+Shift+4");

    if cfg!(target_os = "macos") {
        assert_ne!(cmd, ctrl, "Cmd and Ctrl are distinct keys on macOS");
        assert_eq!(cmd.to_string(), "Shift+Cmd+4");
    } else {
        assert_eq!(
            cmd, ctrl,
            "off macOS a cross-platform Cmd binding must land on Control, \
             not on the desktop environment's Super key"
        );
        assert_eq!(cmd.to_string(), "Ctrl+Shift+4");
    }
}

#[test]
fn the_physical_super_key_is_still_reachable_as_win() {
    let win = parse("Win+Shift+S");
    assert_eq!(win, parse("Logo+Shift+S"));
    // On macOS `Win` and `Cmd` are the same physical modifier; elsewhere `Win`
    // is Super and `Cmd` has been remapped to Control, so they must differ.
    if cfg!(target_os = "macos") {
        assert_eq!(win, parse("Cmd+Shift+S"));
    } else {
        assert_ne!(win, parse("Cmd+Shift+S"));
    }
}

#[test]
fn a_wide_range_of_keys_parses() {
    for accelerator in [
        "Cmd+Shift+4",
        "Cmd+A",
        "F13",
        "Ctrl+Alt+Up",
        "Shift+PrintScreen",
        "Cmd+Space",
        "Alt+Escape",
        "Cmd+,",
        "Cmd+/",
        "Cmd+[",
        "Cmd+`",
        "Cmd+Enter",
        "Cmd+Digit4",
        "Cmd+KeyA",
    ] {
        assert!(
            Accelerator::parse(accelerator).is_ok(),
            "{accelerator} should parse"
        );
    }
}

#[test]
fn a_key_alone_is_a_valid_hotkey() {
    let bare = parse("F13");
    assert_eq!(bare.to_string(), "F13");
}

#[test]
fn bad_accelerators_are_rejected_with_an_explanation() {
    for (input, expected_fragment) in [
        ("Cmd+", "empty component"),
        ("+4", "empty component"),
        ("Cmd+Shift", "only modifiers"),
        ("Cmd+Shift+4+5", "more than one non-modifier key"),
        ("Cmd+Splines", "unrecognised key"),
    ] {
        let error = Accelerator::parse(input).expect_err("should be rejected");
        let message = error.to_string();
        assert!(
            message.contains(expected_fragment),
            "{input}: expected the error to mention {expected_fragment:?}, got {message:?}"
        );
        assert!(
            matches!(error, Error::InvalidRequest(_)),
            "{input}: expected InvalidRequest, got {error:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Conflict detection
// ---------------------------------------------------------------------------

#[test]
fn every_reserved_shortcut_parses_and_is_detected() {
    let reserved = reserved_shortcuts();
    assert!(
        !reserved.is_empty(),
        "every supported platform has combinations the OS keeps for itself"
    );

    for entry in reserved {
        let accelerator = Accelerator::parse(entry.accelerator).unwrap_or_else(|err| {
            panic!(
                "reserved shortcut {:?} does not parse: {err}",
                entry.accelerator
            )
        });
        assert_eq!(
            accelerator.system_owner().map(|it| it.accelerator),
            Some(entry.accelerator),
            "{} should report itself as system-reserved",
            entry.accelerator
        );
        assert!(!entry.owner.is_empty());
        assert!(
            !entry.remedy.is_empty(),
            "a conflict without a remedy is a dead end"
        );
    }
}

#[test]
fn an_ordinary_combination_is_not_reserved() {
    assert!(parse("Cmd+Shift+F13").system_owner().is_none());
}

#[test]
fn binding_a_system_shortcut_fails_loudly_instead_of_silently() {
    // The whole point: on macOS `RegisterEventHotKey` returns success for this
    // and then the system keeps the key. Registering must fail here instead.
    let reserved = reserved_shortcuts()[0];
    let mut manager = GlobalHotkeys::detached();

    let error = manager
        .register(&hotkey(reserved.accelerator), "capture.region")
        .expect_err("a system-owned combination must be refused");

    let message = error.to_string();
    assert!(
        message.contains(reserved.owner),
        "the error must name what holds the key; got {message:?}"
    );
    assert!(
        message.contains(reserved.remedy),
        "the error must carry the remedy; got {message:?}"
    );
    assert!(
        manager.action_for(&parse(reserved.accelerator)).is_none(),
        "a refused binding must not be recorded"
    );
}

#[test]
fn the_same_combination_cannot_be_bound_twice() {
    let mut manager = GlobalHotkeys::detached();
    manager
        .register(&hotkey("Cmd+Shift+F13"), "capture.region")
        .expect("first binding should succeed");

    let error = manager
        .register(&hotkey("Cmd+Shift+F13"), "capture.window")
        .expect_err("the second binding should be refused");
    assert!(
        error.to_string().contains("capture.region"),
        "the conflict must name the action already holding it; got {error}"
    );

    // Still bound to the original action, not clobbered.
    assert_eq!(
        manager.action_for(&parse("Cmd+Shift+F13")),
        Some("capture.region")
    );
}

#[test]
fn a_conflict_can_be_tested_before_committing_to_it() {
    let mut manager = GlobalHotkeys::detached();
    assert_eq!(
        manager.check(&hotkey("Cmd+Shift+F13")).expect("valid"),
        None
    );

    manager
        .register(&hotkey("Cmd+Shift+F13"), "capture.region")
        .expect("binding should succeed");

    assert_eq!(
        manager.check(&hotkey("Cmd+Shift+F13")).expect("valid"),
        Some(Conflict::AlreadyBound {
            action: "capture.region".to_owned()
        })
    );

    let reserved = reserved_shortcuts()[0];
    assert_eq!(
        manager.check(&hotkey(reserved.accelerator)).expect("valid"),
        Some(Conflict::SystemReserved { reserved })
    );
}

// ---------------------------------------------------------------------------
// Registration bookkeeping
// ---------------------------------------------------------------------------

#[test]
fn a_detached_manager_never_touches_the_operating_system() {
    let manager = GlobalHotkeys::detached();
    assert!(
        !manager.is_bound_to_os(),
        "tests must never grab a key off the machine running them"
    );
}

#[test]
fn distinct_combinations_bind_to_distinct_actions() {
    let mut manager = GlobalHotkeys::detached();
    manager
        .register(&hotkey("Cmd+Shift+F13"), "capture.region")
        .expect("region");
    manager
        .register(&hotkey("Cmd+Shift+F14"), "capture.window")
        .expect("window");

    assert_eq!(
        manager.action_for(&parse("Cmd+Shift+F13")),
        Some("capture.region")
    );
    assert_eq!(
        manager.action_for(&parse("Cmd+Shift+F14")),
        Some("capture.window")
    );
    assert_eq!(manager.bindings().count(), 2);
}

#[test]
fn unregistering_frees_the_combination() {
    let mut manager = GlobalHotkeys::detached();
    manager
        .register(&hotkey("Cmd+Shift+F13"), "capture.region")
        .expect("first binding");
    manager
        .unregister(&hotkey("Cmd+Shift+F13"))
        .expect("release");

    assert_eq!(manager.action_for(&parse("Cmd+Shift+F13")), None);
    manager
        .register(&hotkey("Cmd+Shift+F13"), "capture.window")
        .expect("rebinding a freed combination should succeed");
}

#[test]
fn unregistering_something_unbound_is_an_error() {
    let mut manager = GlobalHotkeys::detached();
    let error = manager
        .unregister(&hotkey("Cmd+Shift+F13"))
        .expect_err("nothing was bound");
    assert!(error.to_string().contains("not bound"), "got {error}");
}

#[test]
fn unregister_all_clears_every_binding() {
    let mut manager = GlobalHotkeys::detached();
    manager
        .register(&hotkey("Cmd+Shift+F13"), "capture.region")
        .expect("region");
    manager
        .register(&hotkey("Cmd+Shift+F14"), "capture.window")
        .expect("window");

    manager.unregister_all();
    assert_eq!(manager.bindings().count(), 0);
}

// ---------------------------------------------------------------------------
// Session detection
// ---------------------------------------------------------------------------

#[test]
fn wlroots_compositors_are_recognised_and_have_no_portal() {
    for (desktop, expected) in [
        ("sway", Compositor::Sway),
        ("Hyprland", Compositor::Hyprland),
        ("river", Compositor::River),
        ("niri", Compositor::Niri),
        ("wayfire", Compositor::Wayfire),
    ] {
        let session = Session::from_env(Some(desktop), Some("wayland-1"), Some("wayland"), None);
        assert_eq!(session.compositor, expected, "for {desktop}");
        assert_eq!(session.server, DisplayServer::Wayland, "for {desktop}");
        assert!(
            !session.compositor.has_global_shortcut_portal(),
            "{desktop} implements no GlobalShortcuts portal — this is decision D11's premise"
        );
        assert!(!session.supports_global_hotkeys(), "for {desktop}");
    }
}

#[test]
fn gnome_and_kde_are_recognised_and_do_have_a_portal() {
    for (desktop, expected) in [
        ("GNOME", Compositor::Gnome),
        ("ubuntu:GNOME", Compositor::Gnome),
        ("KDE", Compositor::Kde),
    ] {
        let session = Session::from_env(Some(desktop), Some("wayland-0"), Some("wayland"), None);
        assert_eq!(session.compositor, expected, "for {desktop}");
        assert!(
            session.compositor.has_global_shortcut_portal(),
            "for {desktop}"
        );
        assert!(
            !session.supports_global_hotkeys(),
            "the portal exists but is not wired up, so a binding here would never fire"
        );
    }
}

#[test]
fn an_x11_session_supports_hotkeys_natively() {
    let session = Session::from_env(Some("GNOME"), None, Some("x11"), Some(":0"));
    assert_eq!(session.server, DisplayServer::X11);
    assert!(session.supports_global_hotkeys());

    let bare = Session::from_env(None, None, None, Some(":0"));
    assert_eq!(bare.server, DisplayServer::X11);
}

#[test]
fn a_session_with_nothing_set_is_headless() {
    let session = Session::from_env(None, None, None, None);
    assert_eq!(session.server, DisplayServer::Headless);
    assert!(session.supports_global_hotkeys());
}

#[test]
fn an_empty_wayland_display_does_not_count_as_wayland() {
    let session = Session::from_env(Some("sway"), Some(""), None, None);
    assert_ne!(session.server, DisplayServer::Wayland);
}

// ---------------------------------------------------------------------------
// Wayland remedy — the config line a user actually pastes
// ---------------------------------------------------------------------------

#[test]
fn compositor_config_lines_are_exact() {
    let accelerator = parse("Win+Shift+4");
    let command = "scrozz capture region";

    assert_eq!(
        Compositor::Sway
            .binding_for(&accelerator, command)
            .as_deref(),
        Some("bindsym Shift+Mod4+4 exec scrozz capture region")
    );
    assert_eq!(
        Compositor::Hyprland
            .binding_for(&accelerator, command)
            .as_deref(),
        Some("bind = SHIFT_SUPER, 4, exec, scrozz capture region")
    );
    assert_eq!(
        Compositor::River
            .binding_for(&accelerator, command)
            .as_deref(),
        Some("riverctl map normal Shift+Super 4 spawn 'scrozz capture region'")
    );
    assert_eq!(
        Compositor::Niri
            .binding_for(&accelerator, command)
            .as_deref(),
        Some("Shift+Mod+4 { spawn-sh \"scrozz capture region\"; }")
    );
}

#[test]
fn compositor_config_maps_keys_to_x11_keysyms() {
    // Sway and river read X11 keysym names, which are not the same strings
    // Scrozz displays: `Esc` is `Escape`, `PageUp` is `Prior`, letters are
    // lowercase. A wrong name here is a config line that silently does nothing.
    assert_eq!(parse("Esc").keysym(), Some("Escape"));
    assert_eq!(parse("PageUp").keysym(), Some("Prior"));
    assert_eq!(parse("A").keysym(), Some("a"));
    assert_eq!(parse("Enter").keysym(), Some("Return"));
    assert_eq!(parse("PrintScreen").keysym(), Some("Print"));
    assert_eq!(parse("Space").keysym(), Some("space"));
}

#[test]
fn every_wlroots_compositor_offers_a_config_path_to_paste_into() {
    for compositor in [
        Compositor::Sway,
        Compositor::Hyprland,
        Compositor::River,
        Compositor::Niri,
    ] {
        assert!(
            compositor.config_path().is_some(),
            "{compositor:?} must tell the user which file to edit"
        );
    }
}

#[test]
fn registering_on_wlroots_returns_unsupported_carrying_the_config_line() {
    let session = Session::from_env(Some("sway"), Some("wayland-1"), Some("wayland"), None);
    let mut manager = GlobalHotkeys::detached_in(session);
    manager.set_command("scrozz capture region");

    let error = manager
        .register(&hotkey("Win+Shift+4"), "capture.region")
        .expect_err("wlroots has no global shortcut portal");

    let Error::Unsupported { what, why } = &error else {
        panic!("expected Unsupported, got {error:?}");
    };

    assert!(what.contains("global hotkey"), "got {what:?}");
    assert!(
        why.contains("bindsym Shift+Mod4+4 exec scrozz capture region"),
        "the remedy must be the exact line to paste, not a description of it; got {why:?}"
    );
    assert!(
        why.contains("~/.config/sway/config"),
        "the remedy must say which file; got {why:?}"
    );
    assert!(
        !manager.bindings().any(|_| true),
        "an unsupported binding must not be recorded as bound"
    );
}

#[test]
fn registering_on_hyprland_returns_its_own_syntax() {
    let session = Session::from_env(Some("Hyprland"), Some("wayland-1"), Some("wayland"), None);
    let mut manager = GlobalHotkeys::detached_in(session);
    manager.set_command("scrozz capture region");

    let error = manager
        .register(&hotkey("Win+Shift+4"), "capture.region")
        .expect_err("wlroots has no global shortcut portal");
    let why = error.to_string();
    assert!(
        why.contains("bind = SHIFT_SUPER, 4, exec, scrozz capture region"),
        "got {why:?}"
    );
    assert!(why.contains("~/.config/hypr/hyprland.conf"), "got {why:?}");
}

#[test]
fn registering_on_gnome_wayland_explains_the_portal_gap_instead() {
    let session = Session::from_env(Some("GNOME"), Some("wayland-0"), Some("wayland"), None);
    let mut manager = GlobalHotkeys::detached_in(session);

    let error = manager
        .register(&hotkey("Win+Shift+4"), "capture.region")
        .expect_err("the portal is not wired up yet");
    let why = error.to_string();
    assert!(
        why.contains("keyboard settings"),
        "GNOME users bind it in Settings, not in a config file; got {why:?}"
    );
}

#[test]
fn onboarding_can_generate_the_whole_config_block_at_once() {
    let session = Session::from_env(Some("sway"), Some("wayland-1"), Some("wayland"), None);
    let manager = GlobalHotkeys::detached_in(session);

    // Nothing can be registered here — that is the point of D11 — so the block
    // is generated from what Scrozz *wanted* to bind.
    let region = parse("Win+Shift+4");
    let record = parse("Win+Shift+5");
    let intended = [(&region, "capture region"), (&record, "record start")];

    let lines = manager.compositor_config(intended, |action| format!("scrozz {action}"));
    assert_eq!(
        lines,
        vec![
            "bindsym Shift+Mod4+4 exec scrozz capture region".to_owned(),
            "bindsym Shift+Mod4+5 exec scrozz record start".to_owned(),
        ],
        "onboarding needs the complete block, not one line at a time"
    );
}

#[test]
fn there_is_no_config_block_to_generate_on_a_platform_with_real_hotkeys() {
    let session = Session::from_env(Some("GNOME"), None, Some("x11"), Some(":0"));
    let manager = GlobalHotkeys::detached_in(session);
    let region = parse("Ctrl+Shift+F13");

    assert!(
        manager
            .compositor_config([(&region, "capture region")], ToOwned::to_owned)
            .is_empty(),
        "X11 binds keys directly; offering a config file to edit would be noise"
    );
}

#[test]
fn a_wlroots_session_generates_one_config_line_per_binding() {
    let session = Session::from_env(Some("sway"), Some("wayland-1"), Some("wayland"), None);
    let accelerators = [
        ("Win+Shift+4", "capture.region"),
        ("Win+Shift+5", "record.toggle"),
    ];

    let lines: Vec<String> = accelerators
        .iter()
        .filter_map(|(accelerator, action)| {
            session
                .compositor
                .binding_for(&parse(accelerator), &format!("scrozz {action}"))
        })
        .collect();

    assert_eq!(
        lines,
        vec![
            "bindsym Shift+Mod4+4 exec scrozz capture.region".to_owned(),
            "bindsym Shift+Mod4+5 exec scrozz record.toggle".to_owned(),
        ]
    );
}

// ---------------------------------------------------------------------------
// The tray menu
// ---------------------------------------------------------------------------

#[test]
fn the_tray_menu_reaches_every_action() {
    let items: Vec<TrayAction> = menu_model()
        .iter()
        .filter_map(|entry| match entry {
            TrayEntry::Item(action) => Some(*action),
            TrayEntry::Separator => None,
        })
        .collect();

    for action in TrayAction::ALL {
        assert!(
            items.contains(&action),
            "{action:?} is unreachable: with no window, the tray menu is the only \
             pointer-driven way in"
        );
    }
    assert_eq!(items.len(), TrayAction::ALL.len(), "no duplicates");
}

#[test]
fn the_capture_actions_come_first_and_quit_comes_last() {
    let items: Vec<TrayAction> = menu_model()
        .iter()
        .filter_map(|entry| match entry {
            TrayEntry::Item(action) => Some(*action),
            TrayEntry::Separator => None,
        })
        .collect();

    assert_eq!(items[0], TrayAction::CaptureRegion);
    assert_eq!(items[1], TrayAction::CaptureWindow);
    assert_eq!(items[2], TrayAction::CaptureFullscreen);
    assert_eq!(*items.last().expect("non-empty"), TrayAction::Quit);
}

#[test]
fn the_menu_never_opens_or_closes_on_a_separator() {
    let model = menu_model();
    assert!(!matches!(model.first(), Some(TrayEntry::Separator)));
    assert!(!matches!(model.last(), Some(TrayEntry::Separator)));

    for pair in model.windows(2) {
        assert!(
            !matches!(pair, [TrayEntry::Separator, TrayEntry::Separator]),
            "two separators in a row render as one thick line"
        );
    }
}

#[test]
fn menu_ids_are_unique_and_round_trip() {
    let mut ids: Vec<&str> = TrayAction::ALL.iter().map(|action| action.id()).collect();
    ids.sort_unstable();
    let count = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), count, "menu ids must be unique");

    for action in TrayAction::ALL {
        assert_eq!(TrayAction::from_id(action.id()), Some(action));
        assert!(!action.label().is_empty());
    }
    assert_eq!(TrayAction::from_id("nonsense"), None);
}

#[test]
fn enabled_menu_items_are_never_clickable_no_ops() {
    assert!(TrayAction::CaptureFullscreen.is_available());
    assert!(TrayAction::Quit.is_available());
    assert!(!TrayAction::ToggleRecording.is_available_with(false));
    assert!(TrayAction::ToggleRecording.is_available_with(true));

    for unfinished in [
        TrayAction::CaptureRegion,
        TrayAction::CaptureWindow,
        TrayAction::OpenHistory,
        TrayAction::OpenSettings,
    ] {
        assert!(
            !unfinished.is_available(),
            "{unfinished:?} has no end-to-end implementation and must look \
             disabled rather than accept a click that appears to do nothing"
        );
    }
}

#[test]
fn the_recording_entry_says_what_the_click_will_do() {
    assert_eq!(
        recording_label(false, std::time::Duration::from_secs(99)),
        "Start Recording"
    );
    assert_eq!(
        recording_label(true, std::time::Duration::from_secs(65)),
        "Stop Recording • 01:05"
    );
    assert_eq!(
        TrayAction::ToggleRecording.label(),
        recording_label(false, std::time::Duration::ZERO)
    );
}

#[test]
fn the_generated_menu_bar_icon_is_a_well_formed_template_image() {
    let rgba = default_icon_rgba();
    assert_eq!(rgba.len(), 36 * 36 * 4, "36x36 RGBA, displayed at 18pt");

    let opaque = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[3] > 0)
        .count();
    assert!(opaque > 0, "the icon must have some ink or nothing shows");
    assert!(
        opaque < 36 * 36,
        "a fully opaque square is a solid block, not a glyph"
    );

    // A macOS template image carries its shape in alpha alone; any colour in
    // the RGB channels is discarded, and relying on it produces an icon that
    // vanishes on a dark menu bar.
    assert!(
        rgba.as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| pixel[..3] == [0, 0, 0]),
        "template images must be black with a shaped alpha channel"
    );
}
