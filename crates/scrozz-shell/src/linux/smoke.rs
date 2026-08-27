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
use crate::linux::{
    FrameCommit, LayerSurfaceEvent, LinuxOverlay, LinuxWindowHandle, OutputSelector, SurfaceSize,
    enumerate_outputs, scaled_buffer_size,
};
use crate::overlay::OverlayBehavior;
use scrozz_core::LogicalSize;
use std::io;
use std::time::{Duration, Instant};
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
        plan.backend == OverlayBackend::LayerShell
            && plan.placement == Placement::Anchored
            && plan.is_fully_controlled(),
        format!("the advertised owned renderer was not selected: {plan:?}"),
    )?;

    let config = LayerSurfaceConfig::for_behavior(
        &OverlayBehavior::capture_card(),
        LogicalSize::new(264.0, 962.0),
        0.0,
    );
    let selector = enumerate_outputs()?
        .into_iter()
        .find_map(|output| output.name)
        .map_or(OutputSelector::CompositorDefault, OutputSelector::Named);
    let mut protocol = crate::linux::LayerShellSession::with_output(&config, selector)?;
    let granted = protocol.granted_size().ok_or_else(|| {
        failure("layer-shell surface received no initial configure from the compositor")
    })?;
    let configured = protocol
        .configured_logical_size()
        .ok_or_else(|| failure("layer-shell surface has no configured logical size"))?;
    require(
        protocol.current_buffer_size() == scaled_buffer_size(configured, protocol.current_scale()),
        "the negotiated scale did not produce the advertised buffer size",
    )?;
    let bottom_card = smoke_card_region(configured, 0)?;
    let next_card = smoke_card_region(configured, 1)?;
    let bottom_region = InputRegion::Rects(vec![bottom_card]);

    let first_map_deadline = Instant::now() + Duration::from_secs(2);
    let committed = loop {
        let outcome = commit_card_frame(&mut protocol, Some(&bottom_region))?;
        if outcome == FrameCommit::Committed || Instant::now() >= first_map_deadline {
            break outcome;
        }
        protocol.pump_events(Duration::from_millis(50))?;
    };
    require(
        committed == FrameCommit::Committed && protocol.is_mapped(),
        format!("the first pixel buffer did not map the surface: {committed:?}"),
    )?;
    require(
        protocol
            .drain_events()
            .iter()
            .any(|event| matches!(event, LayerSurfaceEvent::Mapped)),
        "the transport did not report its mapped transition",
    )?;

    protocol.set_input_region(&InputRegion::Rects(vec![bottom_card, next_card]))?;
    protocol.set_input_region(&InputRegion::Rects(vec![bottom_card]))?;

    protocol.unmap()?;
    require(!protocol.is_mapped(), "null attachment did not unmap")?;
    require(
        commit_card_frame(&mut protocol, Some(&bottom_region))? == FrameCommit::AwaitingConfigure,
        "the first frame after unmap did not start the required remap configure",
    )?;

    let remap_deadline = Instant::now() + Duration::from_secs(2);
    let remapped = loop {
        protocol.pump_events(Duration::from_millis(50))?;
        match commit_card_frame(&mut protocol, Some(&bottom_region))? {
            FrameCommit::Committed => break true,
            FrameCommit::BuffersBusy
            | FrameCommit::AwaitingConfigure
            | FrameCommit::SurfaceChanged
                if Instant::now() < remap_deadline => {}
            FrameCommit::BuffersBusy
            | FrameCommit::AwaitingConfigure
            | FrameCommit::SurfaceChanged => break false,
        }
    };
    require(
        remapped && protocol.is_mapped(),
        "the surface did not remap a pixel buffer after becoming empty",
    )?;
    // Long enough for the human KDE/wlroots wrappers to confirm bottom-left
    // placement, while remaining a small fixed cost in headless CI.
    std::thread::sleep(Duration::from_millis(750));

    protocol.set_input_region(&InputRegion::Nothing)?;
    protocol.unmap()?;
    require(!protocol.is_mapped(), "final empty state remained mapped")?;

    println!(
        "PASS: layer-shell v{version} mapped, input-shaped, unmapped, and remapped \
         the Scrozz-owned SHM surface ({granted:?}, scale {:.2}x, output {:?})",
        protocol.current_scale().factor(),
        protocol.output_selector()
    );
    Ok(())
}

fn commit_card_frame(
    protocol: &mut crate::linux::LayerShellSession,
    input_region: Option<&InputRegion>,
) -> Result<FrameCommit> {
    let size = protocol
        .current_buffer_size()
        .ok_or_else(|| failure("layer-shell surface has no physical buffer size"))?;
    let stride = size
        .rgba8_stride()
        .ok_or_else(|| failure("layer-shell smoke frame stride overflowed"))?;
    let byte_len = size
        .rgba8_byte_len()
        .ok_or_else(|| failure("layer-shell smoke frame length overflowed"))?;
    let mut pixels = vec![0_u8; byte_len];
    let scale = protocol.current_scale().units_120();
    let logical = protocol
        .configured_logical_size()
        .ok_or_else(|| failure("layer-shell smoke frame has no logical size"))?;
    let x0 = scaled_floor(16, scale).min(size.width);
    let x1 = scaled_ceil(248, scale).min(size.width);
    let y0 = scaled_floor(logical.height.saturating_sub(161), scale).min(size.height);
    let y1 = scaled_ceil(logical.height.saturating_sub(16), scale).min(size.height);
    if x0 >= x1 || y0 >= y1 {
        return Err(failure(
            "the compositor-configured surface is too small for the smoke card",
        ));
    }
    for y in y0..y1 {
        let row = y as usize * stride;
        let start = row + x0 as usize * 4;
        let end = row + x1 as usize * 4;
        for pixel in pixels[start..end].as_chunks_mut::<4>().0 {
            *pixel = [42, 108, 180, 220];
        }
    }
    let size = SurfaceSize::new(size.width, size.height);
    match input_region {
        Some(region) => protocol.commit_pixels_with_input_region(size, stride, &pixels, region),
        None => protocol.commit_pixels(size, stride, &pixels),
    }
    .map_err(Into::into)
}

fn scaled_floor(logical: u32, scale_120: u32) -> u32 {
    let value = u64::from(logical) * u64::from(scale_120) / 120;
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn scaled_ceil(logical: u32, scale_120: u32) -> u32 {
    let value = (u64::from(logical) * u64::from(scale_120)).div_ceil(120);
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn smoke_card_region(surface: SurfaceSize, slot: u32) -> Result<RegionRect> {
    let offset = slot
        .checked_mul(157)
        .and_then(|offset| offset.checked_add(161))
        .ok_or_else(|| failure("the smoke card slot offset overflowed"))?;
    let y = surface
        .height
        .checked_sub(offset)
        .ok_or_else(|| failure("the compositor-configured surface cannot hold two smoke cards"))?;
    Ok(RegionRect {
        x: 16,
        y: i32::try_from(y).map_err(|_| failure("the smoke card position overflowed"))?,
        width: 232,
        height: 145,
    })
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
            plan.detail.contains("D31") && plan.detail.contains("portal"),
            format!("patched GNOME did not preserve the D31 fallback: {plan:?}"),
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
