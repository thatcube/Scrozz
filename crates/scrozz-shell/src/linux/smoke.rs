//! Native Linux overlay smoke probes.
//!
//! The shell wrappers under `tools/linux-smoke/` select the correct mode and
//! explicitly skip when they are not running inside the requested desktop
//! session. Every request made after that selection is required.

use super::{overlay_plan, probe_layer_shell};
use crate::hotkey::{Compositor, DisplayServer, Session};
use crate::linux::capability::{LayerShellProbe, OverlayBackend, Placement};
use crate::linux::layer::LayerSurfaceConfig;
use crate::linux::region::{InputRegion, RegionRect};
use crate::linux::{LinuxOverlay, LinuxWindowHandle};
use crate::overlay::OverlayBehavior;
use scrozz_core::LogicalSize;
use std::io;
use x11rb::connection::Connection as _;
use x11rb::protocol::shape::{ConnectionExt as _, SK};
use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Runs the mode named by the process's first argument.
pub fn run() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("x11") => x11(),
        Some("kde-wayland") => layer_shell(Desktop::Kde),
        Some("gnome-wayland") => gnome(),
        Some("wlroots") => layer_shell(Desktop::Wlroots),
        _ => Err(failure(
            "usage: linux_overlay_smoke <x11|kde-wayland|gnome-wayland|wlroots>",
        )),
    }
}

fn x11() -> Result<()> {
    let session = Session::detect();
    require(
        session.server == DisplayServer::X11,
        format!("expected X11, detected {:?}", session.server),
    )?;

    let (conn, screen_number) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_number];
    let window = conn.generate_id()?;
    conn.create_window(
        screen.root_depth,
        window,
        screen.root,
        0,
        0,
        320,
        240,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new().override_redirect(1),
    )?
    .check()?;

    let mut overlay = LinuxOverlay::adopt(LinuxWindowHandle::X11 { window }, &session)?;
    let report = overlay.apply(&OverlayBehavior::capture_card())?;
    require(
        report.non_activating,
        format!("X11 capture card remained activating: {}", report.detail),
    )?;

    let first = InputRegion::Rects(vec![
        RegionRect {
            x: 12,
            y: 16,
            width: 80,
            height: 60,
        },
        RegionRect {
            x: 12,
            y: 96,
            width: 80,
            height: 60,
        },
    ]);
    overlay.set_input_region(&first)?;
    let first_reply = conn.shape_get_rectangles(window, SK::INPUT)?.reply()?;
    require(
        first_reply.rectangles.len() == 2,
        format!(
            "expected two card input rectangles, server returned {:?}",
            first_reply.rectangles
        ),
    )?;

    let moved = InputRegion::Rects(vec![RegionRect {
        x: 40,
        y: 24,
        width: 120,
        height: 72,
    }]);
    overlay.set_input_region(&moved)?;
    let moved_reply = conn.shape_get_rectangles(window, SK::INPUT)?.reply()?;
    require(
        moved_reply.rectangles.len() == 1
            && moved_reply.rectangles[0].x == 40
            && moved_reply.rectangles[0].y == 24
            && moved_reply.rectangles[0].width == 120
            && moved_reply.rectangles[0].height == 72,
        format!(
            "runtime SHAPE replacement did not reach the server: {:?}",
            moved_reply.rectangles
        ),
    )?;

    overlay.set_input_region(&InputRegion::Nothing)?;
    let empty = conn.shape_get_rectangles(window, SK::INPUT)?.reply()?;
    require(
        empty.rectangles.is_empty(),
        format!(
            "empty overlay still accepted input in {:?}",
            empty.rectangles
        ),
    )?;

    conn.destroy_window(window)?.check()?;
    println!("PASS: X11 retained backend replaced per-card SHAPE input regions at runtime");
    Ok(())
}

#[derive(Clone, Copy)]
enum Desktop {
    Kde,
    Wlroots,
}

fn layer_shell(expected: Desktop) -> Result<()> {
    let session = Session::detect();
    require(
        session.server == DisplayServer::Wayland,
        format!("expected Wayland, detected {:?}", session.server),
    )?;
    match expected {
        Desktop::Kde => require(
            session.compositor == Compositor::Kde,
            format!("expected KDE/KWin, detected {:?}", session.compositor),
        )?,
        Desktop::Wlroots => require(
            matches!(
                session.compositor,
                Compositor::Sway | Compositor::Hyprland | Compositor::River | Compositor::Wayfire
            ),
            format!(
                "expected a recognised wlroots compositor, detected {:?}",
                session.compositor
            ),
        )?,
    }

    let LayerShellProbe::Present { version } = probe_layer_shell() else {
        return Err(failure(
            "the compositor did not advertise zwlr_layer_shell_v1",
        ));
    };

    let plan = overlay_plan(&session);
    require(
        plan.backend == OverlayBackend::CompositorPlaced
            && plan.placement == Placement::CompositorChosen
            && !plan.is_fully_controlled(),
        format!("the active eframe surface falsely claimed layer-shell control: {plan:?}"),
    )?;

    let config = LayerSurfaceConfig::for_behavior(
        &OverlayBehavior::capture_card(),
        LogicalSize::new(232.0, 150.0),
        20.0,
    );
    let mut protocol = crate::linux::LayerShellSession::new(&config)?;
    let granted = protocol.granted_size().ok_or_else(|| {
        failure("layer-shell surface received no initial configure from the compositor")
    })?;
    protocol.set_input_region(&InputRegion::Rects(vec![RegionRect {
        x: 0,
        y: 0,
        width: 232,
        height: 150,
    }]))?;

    println!(
        "PASS: layer-shell v{version} accepted the Scrozz-owned protocol surface \
             ({granted:?}); active eframe rendering remained compositor-positioned"
    );
    Ok(())
}

fn gnome() -> Result<()> {
    let session = Session::detect();
    require(
        session.server == DisplayServer::Wayland,
        format!("expected Wayland, detected {:?}", session.server),
    )?;
    require(
        session.compositor == Compositor::Gnome,
        format!("expected GNOME/Mutter, detected {:?}", session.compositor),
    )?;

    let probe = probe_layer_shell();
    let plan = overlay_plan(&session);
    require(
        plan.backend == OverlayBackend::CompositorPlaced
            && plan.placement == Placement::CompositorChosen
            && !plan.is_fully_controlled(),
        format!("GNOME falsely claimed controlled placement: {plan:?}"),
    )?;
    match probe {
        LayerShellProbe::Absent => require(
            plan.detail.contains("Mutter") && plan.detail.contains("portal"),
            format!("stock GNOME fallback omitted its D31 explanation: {plan:?}"),
        )?,
        LayerShellProbe::Present { .. } => require(
            plan.detail
                .contains("does not yet own a layer-shell renderer"),
            format!("patched GNOME did not report the renderer gap: {plan:?}"),
        )?,
        LayerShellProbe::NotProbed => {
            return Err(failure(
                "Wayland was detected but the compositor registry could not be probed",
            ));
        }
    }

    println!("PASS: GNOME Wayland selected and explained the compositor-positioned D31 fallback");
    Ok(())
}

fn require(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(failure(message))
    }
}

fn failure(message: impl Into<String>) -> Box<dyn std::error::Error> {
    Box::new(io::Error::other(message.into()))
}
