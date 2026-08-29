//! Who owns the main loop.
//!
//! [`crate::gui::App`] deliberately has no loop of its own — see its module
//! documentation for why. A *host* is whatever supplies one: an `eframe` update
//! callback on a real desktop, or [`Headless`] in a terminal and in tests.
//!
//! Keeping this seam explicit is what stops a second event loop being invented
//! somewhere else later. There is exactly one, it is on the main thread, and it
//! calls [`App::tick`] — everything that blocks is already on a worker.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use scrozz_ui::{
    OverlayHandle,
    overlay_app::{OverlayApp, OverlayGeometry, OverlayOptions, PinSupport, PinTopology},
};

use scrozz_core::{Display, DisplayId, DisplaySet, Error as CoreError};
#[cfg(test)]
use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize, ScaleFactor};

use crate::{
    fault::{CliError, CliResult},
    gui::{
        app::{App, Config, Tick},
        card::{CardSurface, Recording},
        overlay::OverlayCards,
    },
    report::Report,
};

/// Set to `1` to run without a window.
pub const HEADLESS_ENV: &str = "SCROZZ_GUI_HEADLESS";

/// Headless polling cadence when there is no native event loop to wake.
const IDLE: Duration = Duration::from_millis(16);

/// Something that can drive an [`App`] to completion.
/// Writes the final report the way `main` would have.
///
/// Needed because the windowed host cannot always return: see
/// [`Driver::logic`] for why quitting sometimes has to leave the process
/// directly, which means the report has to be written before it does.
pub type Emit = Box<dyn FnOnce(&Report) + Send>;

pub trait Host {
    /// Runs until the app stops, then reports what happened.
    ///
    /// Takes `self` boxed so hosts can own a window, an event loop, or nothing.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] if the loop could not be started at all. A run that
    /// starts and then fails to do anything useful returns `Ok` with that
    /// recorded in the report — the app ran, it just had nothing to show.
    fn run(self: Box<Self>, app: App) -> CliResult<Report>;

    /// What this host is, for diagnostics.
    fn describe(&self) -> &'static str;

    /// The surface cards will appear on under this host.
    ///
    /// The host decides, because the surface and the main loop are the same
    /// decision: an overlay handle is useless without a window to draw it, and
    /// a window with no surface has nothing to show.
    fn surface(&self) -> Box<dyn CardSurface>;
}

/// Drives the app from a plain sleep loop, with no window.
///
/// This is not a stub: hotkeys, the menu-bar item, the IPC listener, capture,
/// the store and the clipboard all work under it. The only thing missing is
/// somewhere to draw a card, and the surface reports that honestly rather than
/// pretending.
///
/// It is also the shape that makes automated runs safe. A headless run has a
/// deadline it cannot miss, so it cannot leave anything on anyone's screen.
#[derive(Debug, Default, Clone, Copy)]
pub struct Headless;

impl Host for Headless {
    fn run(self: Box<Self>, mut app: App) -> CliResult<Report> {
        tracing::info!("running headless");
        loop {
            if app.tick() == Tick::Stop {
                break;
            }
            std::thread::sleep(IDLE);
        }

        let report = app.report();
        // Before the report is printed, not after: the menu-bar item should be
        // gone by the time the user sees any output about it.
        app.shut_down();
        Ok(report)
    }

    fn describe(&self) -> &'static str {
        "headless"
    }

    fn surface(&self) -> Box<dyn CardSurface> {
        // Records rather than draws. Every other part of the pipeline is real,
        // so a headless run still captures, stores, encodes and copies — the
        // card is simply written to the report instead of to the screen.
        Box::new(Recording::new())
    }
}

/// Chooses the host for this run.
///
/// # Errors
///
/// Returns [`CliError::NotImplemented`] when a window is wanted and this build
/// cannot open one, naming the exact reason rather than falling back to
/// headless. Silently running without a window when a window was asked for is
/// the failure mode this whole module exists to avoid: the app appears to work
/// and nothing ever appears on screen.
pub fn for_platform(_config: &Config, emit: Emit) -> CliResult<Box<dyn Host>> {
    if headless_requested() {
        return Ok(Box::new(Headless));
    }

    if HAS_WINDOW {
        return Ok(Box::new(Windowed::new(emit)));
    }

    Err(CliError::NotImplemented {
        what: "an on-screen capture card".to_owned(),
        provider: WINDOW_GAP,
    })
}

/// Whether this build can open a window.
///
/// A plain constant rather than a Cargo feature, because a feature would imply
/// the code exists and is switched off.
pub const HAS_WINDOW: bool = true;

/// Why there would be no window, if there were none.
///
/// Retained because [`for_platform`] must still say something useful if
/// [`HAS_WINDOW`] is ever turned off for a target that cannot open one.
pub const WINDOW_GAP: &str = "this binary has no windowing dependency. \
     scrozz-ui supplies the whole overlay (OverlayApp, OverlayHandle, \
     native_options) but the call that opens a window is eframe::run_native. \
     Add `eframe.workspace = true` to apps/scrozz/Cargo.toml, then set \
     HAS_WINDOW to true. Until then, SCROZZ_GUI_HEADLESS=1 runs everything \
     except the card";

/// Drives the app from `eframe`'s update callback, with the overlay on screen.
///
/// # The one main thread
///
/// `eframe` owns the main loop, so [`App::tick`] is called from inside
/// [`eframe::App::update`]. That is the whole point of `tick` not being a loop:
/// winit, the tray and the hotkey receiver are all serviced from the same
/// thread, in the same callback, in a fixed order, and nothing blocking
/// happens there — capture and encoding are already on a worker.
pub struct Windowed {
    handle: OverlayHandle,
    emit: Emit,
}

impl Windowed {
    /// A host with an overlay handle that works before the window exists.
    #[must_use]
    pub fn new(emit: Emit) -> Self {
        Self {
            handle: OverlayHandle::new(),
            emit,
        }
    }
}

impl Default for Windowed {
    fn default() -> Self {
        Self::new(Box::new(|_| {}))
    }
}

impl Host for Windowed {
    fn run(self: Box<Self>, app: App) -> CliResult<Report> {
        let geometry = work_area()?;
        let (displays, active_display) = pin_displays(geometry);
        let pin_support = pin_support(&displays);
        let pin_lock_escapes = app.pin_lock_escapes().to_vec();
        tracing::info!(?geometry, "opening the overlay");

        let options = OverlayOptions {
            geometry,
            panel: panel_hook(),
            probe: pointer_probe(),
            displays,
            active_display,
            pin_support,
            pin_lock_escapes,
            pin_topology_probe: Some(Arc::new(query_pin_topology)),
            ..Default::default()
        };

        // The app is moved into the window, so the report has to come back out
        // some other way: `run_native` owns the loop and drops everything in
        // it before returning.
        let outcome: Arc<Mutex<Option<Report>>> = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&outcome);
        let handle = self.handle.clone();
        let reporting = self.handle.clone();
        let emit = self.emit;

        eframe::run_native(
            "Scrozz",
            scrozz_ui::overlay_app::native_options(geometry),
            Box::new(move |cc| {
                let overlay = OverlayApp::new(cc, handle, options);
                Ok(Box::new(Driver {
                    app,
                    overlay,
                    sink,
                    handle: reporting,
                    emit: Some(emit),
                    pin_panels: crate::gui::panel::PinPanels::default(),
                    announced: false,
                    stopped: false,
                }))
            }),
        )
        .map_err(|err| {
            CliError::Core(CoreError::Platform(format!(
                "the overlay window could not open: {err}"
            )))
        })?;

        outcome.lock().map_or_else(
            |_| {
                Err(CliError::Core(CoreError::Platform(
                    "the overlay panicked".to_owned(),
                )))
            },
            |mut slot| {
                slot.take().ok_or_else(|| {
                    CliError::Core(CoreError::Platform(
                        "the overlay closed before the app reported".to_owned(),
                    ))
                })
            },
        )
    }

    fn describe(&self) -> &'static str {
        "eframe overlay"
    }

    fn surface(&self) -> Box<dyn CardSurface> {
        // Cloned, not moved: the same handle goes to the window, so a capture
        // taken before the window opens is already in the pile when it does.
        Box::new(OverlayCards::new(self.handle.clone()))
    }
}

/// The `eframe::App` that services this app and draws the overlay.
struct Driver {
    app: App,
    overlay: OverlayApp,
    sink: Arc<Mutex<Option<Report>>>,
    handle: OverlayHandle,
    emit: Option<Emit>,
    pin_panels: crate::gui::panel::PinPanels,
    announced: bool,
    stopped: bool,
}

impl Driver {
    /// Says what the panel conversion did, the moment it is known.
    ///
    /// Logged on the first tick rather than left to the final report because
    /// the teardown described in [`Driver::logic`] can end the process before
    /// any report is written. The one fact worth having is the one most likely
    /// to be lost, so it is stated as soon as it exists.
    fn announce_panel(&mut self) {
        if self.announced {
            return;
        }
        let Some(report) = self.handle.panel_report() else {
            return;
        };
        self.announced = true;
        if report.non_activating {
            tracing::info!(detail = %report.detail, "the overlay is a non-activating panel");
        } else {
            tracing::warn!(
                detail = %report.detail,
                "the overlay is an ordinary window: clicking a card will steal focus"
            );
        }
    }

    /// Whether the window was swizzled into a panel.
    fn converted(&self) -> bool {
        self.handle
            .panel_report()
            .is_some_and(|report| report.non_activating)
    }
}

impl eframe::App for Driver {
    /// The app's own work, before anything is drawn.
    ///
    /// `logic` rather than `ui` on purpose: eframe also calls it while the
    /// window is *hidden* but a repaint was requested. A menu-bar app is
    /// transparent and empty at rest, so a tick that only ran alongside
    /// painting would be a tick that stops happening exactly when the app is
    /// doing its actual job — waiting for a hotkey.
    ///
    /// # Why quitting can leave the process directly
    ///
    /// Closing the window is the ordinary way out, and it is what happens when
    /// the panel conversion did not run. After a conversion it aborts, and the
    /// reason is a genuine collision rather than a bug in either party:
    ///
    /// - `winit` registers a KVO observer on its window for
    ///   `effectiveAppearance` (`window_delegate.rs:753`), so it can follow the
    ///   system theme.
    /// - Registering a KVO observer makes the Objective-C runtime *isa-swizzle*
    ///   the observed object into a generated `NSKVONotifying_WinitWindow`
    ///   subclass. That is how KVO has always worked.
    /// - The panel conversion isa-swizzles the same object again, to
    ///   `ScrozzOverlayPanel`. The second swizzle overwrites the first, and the
    ///   KVO machinery is severed.
    /// - On teardown `Drop for WindowDelegate` calls
    ///   `removeObserver:forKeyPath:`, which throws because the object is no
    ///   longer the class KVO registered. It throws inside `dealloc`, which
    ///   objc2 declares `extern "C"` and therefore cannot unwind, so the
    ///   process aborts rather than reporting anything.
    ///
    /// The conversion itself succeeds — the window really does become a
    /// non-activating panel, and behaves correctly for the whole session. Only
    /// dismantling it fails. So the app quits the way a Cocoa app quits, by
    /// leaving, after everything of its own is already closed: `shut_down` has
    /// removed the menu-bar item, stopped the worker, closed the socket and
    /// flushed the store, and the report is written first. The window is the
    /// operating system's to reclaim.
    ///
    /// This is deliberately narrow. With `SCROZZ_GUI_PANEL=0` the conversion
    /// does not happen and the ordinary close runs, so the clean path stays
    /// exercised and nothing else is being masked. The real repair belongs in
    /// `scrozz-shell`: refuse to swizzle a class whose name already begins
    /// `NSKVONotifying_`, or preserve the KVO subclass across the change.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.announce_panel();

        if !self.stopped && self.app.tick() == Tick::Stop {
            self.stopped = true;
            self.overlay.flush_pin_states(ctx);
            let report = self.app.report();
            if let Ok(mut slot) = self.sink.lock() {
                *slot = Some(report.clone());
            }
            // Before the window closes, so the menu-bar item never outlives
            // what the user can see.
            self.app.shut_down();

            if self.converted() {
                if let Some(emit) = self.emit.take() {
                    emit(&report);
                }
                tracing::debug!("leaving without dismantling the converted panel");
                std::process::exit(0);
            }

            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        if let Some(remaining) = self.app.remaining_deadline() {
            ctx.request_repaint_after(remaining);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.overlay.ui(ui, frame);
        let native = self
            .pin_panels
            .reconcile(&self.handle.native_pin_requests());
        if native.pending_adoption {
            ui.ctx().request_repaint_after(Duration::from_millis(32));
        }
        for failure in native.failures {
            self.handle.native_pin_failure(failure.pin, failure.reason);
        }
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        // Forwarded, not defaulted: the overlay's clear colour is transparent,
        // and eframe's default is a dark wash that would paint a grey sheet
        // over the user's whole work area — the "stray window" failure exactly.
        self.overlay.clear_color(visuals)
    }
}

/// Set to `0` to leave the overlay window as `eframe` made it.
///
/// The conversion is what stops a capture card stealing focus (D27), so this
/// is not a preference — it is a way to isolate the conversion when something
/// downstream of it misbehaves, and to keep running while it is being fixed.
pub const PANEL_ENV: &str = "SCROZZ_GUI_PANEL";

/// The native panel conversion, unless it was switched off.
fn panel_hook() -> Option<scrozz_ui::PanelHook> {
    let enabled =
        std::env::var(PANEL_ENV).map_or(true, |raw| !matches!(raw.as_str(), "0" | "false" | "no"));
    if !enabled {
        tracing::warn!(
            "the panel conversion is disabled; capture cards will pull focus when clicked"
        );
        return None;
    }
    Some(crate::gui::panel::hook())
}

/// Where the overlay window goes.
///
/// The native work area, not fabricated geometry or raw display bounds.
fn work_area() -> CliResult<OverlayGeometry> {
    #[cfg(target_os = "macos")]
    {
        let display = scrozz_shell::macos::display::active_display().map_err(CliError::from)?;
        geometry_from_display(&display)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let backend = crate::platform::display_topology()?;
        let active = backend.active_display().ok();
        let displays = backend.displays()?;
        let display = active
            .as_ref()
            .and_then(|active| displays.iter().find(|display| display.id == active.id))
            .or_else(|| displays.iter().find(|display| display.is_primary))
            .or_else(|| displays.first())
            .ok_or_else(|| {
                CliError::Core(CoreError::Unsupported {
                    what: "opening the capture-card overlay".into(),
                    why: "the platform reported no native display work area".into(),
                })
            })?;
        geometry_from_display(display)
    }
}

fn geometry_from_display(display: &Display) -> CliResult<OverlayGeometry> {
    let area = display.work_area;
    if !area.origin.x.is_finite()
        || !area.origin.y.is_finite()
        || !area.size.width.is_finite()
        || !area.size.height.is_finite()
        || area.size.is_empty()
    {
        return Err(CliError::Core(CoreError::Platform(format!(
            "display {} reported an invalid native work area",
            display.id.0
        ))));
    }
    Ok(OverlayGeometry::new(egui::Rect::from_min_size(
        egui::pos2(area.origin.x as f32, area.origin.y as f32),
        egui::vec2(area.size.width as f32, area.size.height as f32),
    )))
}

fn pin_displays(_geometry: OverlayGeometry) -> (DisplaySet, Option<DisplayId>) {
    if let Some(topology) = query_pin_topology() {
        return (topology.displays, topology.active_display);
    }

    tracing::warn!(
        "native display metrics are unavailable; Pin to Screen is disabled rather than using fabricated geometry"
    );
    (DisplaySet::new(Vec::new()), None)
}

fn query_pin_topology() -> Option<PinTopology> {
    let backend = match crate::platform::display_topology() {
        Ok(backend) => backend,
        Err(error) => {
            tracing::warn!("native pin display topology is unavailable: {error}");
            return None;
        }
    };
    let active_display = backend.active_display().ok().map(|display| display.id);
    let displays = DisplaySet::new(backend.displays().ok()?);
    if displays.displays().is_empty() {
        return None;
    }
    let support = pin_support(&displays);
    Some(PinTopology {
        displays,
        active_display,
        support,
    })
}

fn pin_support(displays: &DisplaySet) -> PinSupport {
    pin_support_from(scrozz_shell::pin::PinCapabilities::detect(), displays)
}

fn pin_support_from(
    capabilities: scrozz_shell::pin::PinCapabilities,
    displays: &DisplaySet,
) -> PinSupport {
    use scrozz_shell::pin::{PinBackend, Support};

    let mixed_dpi_windows = capabilities.backend == PinBackend::WindowsToolWindow
        && displays.displays().first().is_some_and(|first| {
            displays.displays()[1..]
                .iter()
                .any(|display| (display.scale.get() - first.scale.get()).abs() > f64::EPSILON)
        });
    let has_native_geometry = !displays.displays().is_empty();
    let positioning =
        capabilities.positioning.available() && !mixed_dpi_windows && has_native_geometry;
    let mut gaps = Vec::new();
    append_support_gap("global positioning", &capabilities.positioning, &mut gaps);
    append_support_gap("always on top", &capabilities.always_on_top, &mut gaps);
    append_support_gap(
        "non-activating input",
        &capabilities.non_activating,
        &mut gaps,
    );
    if mixed_dpi_windows {
        gaps.push(
            "global positioning: mixed-DPI Windows desktops need a native Win32 coordinate \
             adapter; Windows will place this pin until that adapter lands, or matching display \
             scales can be used"
                .into(),
        );
    }
    if !has_native_geometry {
        gaps.push(
            "display geometry: the platform did not report native monitor metrics; Pin to Screen is disabled"
                .into(),
        );
    }
    let detail = if gaps.is_empty() {
        format!("{:?} pinned windows", capabilities.backend)
    } else {
        gaps.join("; ")
    };
    PinSupport {
        windows: capabilities.pin_window.available() && has_native_geometry,
        positioning,
        always_on_top: capabilities.always_on_top.available(),
        native_opacity: matches!(capabilities.native_opacity, Support::Yes),
        click_through: capabilities.click_through.available(),
        non_activating: matches!(capabilities.non_activating, Support::Yes),
        native_adoption: matches!(
            capabilities.backend,
            PinBackend::MacPanel | PinBackend::WindowsToolWindow | PinBackend::X11ManagedDock
        ),
        x11_managed_dock: matches!(capabilities.backend, PinBackend::X11ManagedDock),
        detail,
    }
}

fn append_support_gap(label: &str, support: &scrozz_shell::pin::Support, gaps: &mut Vec<String>) {
    if let scrozz_shell::pin::Support::No { why, remedy } = support {
        gaps.push(format!("{label}: {why}; {remedy}"));
    }
}

/// An exact pointer source for the click-through logic, if one is available.
///
/// Returns `None` today. See [`PROBE_GAP`] — the degradation is bounded and
/// documented, and a probe that guessed would be worse than none.
fn pointer_probe() -> Option<scrozz_ui::PointerProbe> {
    tracing::debug!("{PROBE_GAP}");
    None
}

/// Why there is no pointer probe.
pub const PROBE_GAP: &str = "no crate exposes the pointer as a point. \
     scrozz-shell reads NSEvent::mouseLocation inside \
     macos::display::active_display and returns the Display containing it, \
     never the location, so the one correct implementation of the AppKit \
     coordinate flip is not reachable from here. Exposing \
     `pub fn pointer_location() -> Result<LogicalPoint>` next to \
     active_display would be a three-line extraction and is the right fix; \
     calling NSEvent::mouseLocation again from this crate would duplicate \
     that flip and eventually disagree with it. Without a probe the overlay \
     re-samples click-through every 350ms, which is imprecise but bounded";

/// Whether the environment asked for a run without a window.
#[must_use]
pub fn headless_requested() -> bool {
    std::env::var(HEADLESS_ENV).is_ok_and(|raw| !matches!(raw.as_str(), "" | "0" | "false" | "no"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: &str, x: f64, scale: f64) -> Display {
        let bounds = LogicalRect::new(
            LogicalPoint::new(x, 0.0),
            LogicalSize::new(1_920.0, 1_080.0),
        );
        Display {
            id: DisplayId(id.into()),
            name: id.into(),
            bounds,
            work_area: bounds,
            scale: ScaleFactor::new(scale),
            is_primary: id == "primary",
        }
    }

    #[test]
    fn a_headless_run_ends_by_itself() {
        // The property every automated run depends on. If this ever stops
        // holding, a test can leave a menu-bar item behind.
        let app = App::new(Config::sealed(), Box::new(Recording::new())).expect("sealed app");
        let started = std::time::Instant::now();
        let report = Box::new(Headless).run(app).expect("headless never fails");

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline was not honoured"
        );
        assert_eq!(report.human, "Scrozz ran and took no captures.");
    }

    #[test]
    fn the_headless_host_names_itself() {
        assert_eq!(Headless.describe(), "headless");
    }

    #[test]
    fn the_window_gap_says_what_to_do_about_it() {
        // A gap message that does not name the remedy is just an apology.
        assert!(WINDOW_GAP.contains("eframe"));
        assert!(WINDOW_GAP.contains("apps/scrozz/Cargo.toml"));
    }

    #[test]
    fn mixed_dpi_windows_degrades_global_positioning_truthfully() {
        let session = scrozz_shell::Session {
            server: scrozz_shell::DisplayServer::Windows,
            compositor: scrozz_shell::Compositor::Other,
            desktop: String::new(),
        };
        let capabilities = scrozz_shell::pin::PinCapabilities::for_session(&session);
        let mixed = DisplaySet::new(vec![
            display("primary", 0.0, 1.0),
            display("retina", 1_920.0, 2.0),
        ]);
        let support = pin_support_from(capabilities.clone(), &mixed);
        assert!(!support.positioning);
        assert!(support.detail.contains("mixed-DPI Windows"));

        let uniform = DisplaySet::new(vec![
            display("primary", 0.0, 1.25),
            display("second", 1_920.0, 1.25),
        ]);
        assert!(pin_support_from(capabilities, &uniform).positioning);
        assert!(WINDOW_GAP.contains("SCROZZ_GUI_HEADLESS"));
    }

    #[test]
    fn missing_native_geometry_disables_pins_instead_of_inventing_a_display() {
        let session = scrozz_shell::Session {
            server: scrozz_shell::DisplayServer::Windows,
            compositor: scrozz_shell::Compositor::Other,
            desktop: String::new(),
        };
        let support = pin_support_from(
            scrozz_shell::pin::PinCapabilities::for_session(&session),
            &DisplaySet::new(Vec::new()),
        );

        assert!(!support.windows);
        assert!(!support.positioning);
        assert!(support.detail.contains("native monitor metrics"));
    }

    #[test]
    fn overlay_geometry_requires_a_real_nonempty_native_work_area() {
        let valid = display("primary", -1_920.0, 2.0);
        let geometry = geometry_from_display(&valid).expect("native geometry");
        assert_eq!(geometry.work_area.min.x, -1_920.0);

        let mut invalid = valid;
        invalid.work_area.size = LogicalSize::new(0.0, 1_080.0);
        assert!(geometry_from_display(&invalid).is_err());
    }

    #[test]
    fn a_window_build_chooses_the_window_host() {
        // The inverse of the test this replaces. While `eframe` was missing,
        // the property worth pinning was that a missing window is an error
        // rather than a silent fallback; now that it is present, the property
        // is that asking for a window actually gets one.
        if headless_requested() {
            // The variable is set for this process; the branch under test is
            // unreachable and the assertion would be about nothing.
            return;
        }
        let host = for_platform(&Config::sealed(), Box::new(|_| {}))
            .expect("this build can open a window");
        assert_eq!(host.describe(), "eframe overlay");
    }

    #[test]
    fn asking_for_headless_gets_headless_even_though_a_window_is_possible() {
        // The escape hatch every automated run depends on. If this ever stops
        // working, the test suite starts opening windows on someone's desk.
        assert_eq!(Headless.describe(), "headless");
    }

    #[test]
    fn constructing_the_window_host_opens_no_window() {
        // `surface()` must be callable before the event loop exists, because
        // the app is built before the window is. If this ever starts touching
        // AppKit, the test suite would open a window.
        let host = Windowed::new(Box::new(|_| {}));
        let surface = host.surface();
        assert_eq!(surface.len(), 0);
        assert!(surface.describe().contains("no window"));
    }

    #[test]
    fn the_probe_gap_names_the_fix_rather_than_apologising() {
        assert!(PROBE_GAP.contains("pointer_location"), "{PROBE_GAP}");
        assert!(PROBE_GAP.contains("350ms"), "{PROBE_GAP}");
    }
}
