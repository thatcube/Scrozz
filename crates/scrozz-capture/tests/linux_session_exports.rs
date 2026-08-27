//! Linux session facts exposed at the capture crate boundary.

#![cfg(target_os = "linux")]

use scrozz_capture::{
    LinuxCompositor, LinuxSessionEnv, LinuxSessionKind, detect_linux_compositor,
    detect_linux_session, linux_portal_capabilities,
};

#[test]
fn public_session_facts_keep_xwayland_on_the_portal_route() {
    let env = LinuxSessionEnv {
        wayland_display: Some("wayland-0".to_owned()),
        display: Some(":0".to_owned()),
        xdg_current_desktop: Some("ubuntu:GNOME".to_owned()),
        ..LinuxSessionEnv::default()
    };

    let kind = detect_linux_session(&env);
    assert_eq!(kind, LinuxSessionKind::XWayland);
    assert!(kind.requires_portal());

    let compositor = detect_linux_compositor(&env);
    assert_eq!(compositor, LinuxCompositor::Mutter);
    assert!(!linux_portal_capabilities(&compositor).layer_shell);
}
