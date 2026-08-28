//! Selector strategy resolution and the app-side selector bridge.

use std::{
    sync::{
        Arc, Condvar, Mutex,
        mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    },
    thread::{self, ThreadId},
    time::{Duration, Instant},
};

use scrozz_core::{
    Capture, CaptureBackend, CaptureRequest, CaptureTarget, CursorMode, Display, DisplayId, Error,
    Frame, LogicalPoint, LogicalRect, PhysicalSize, Provenance, RegionSelector, Result,
    SelectionCapabilities, SelectionHost, SelectionOptions, SelectionOutcome, Window,
};
use scrozz_shell::{SelectionIntegration, SelectionPlan, Session, resolve_selection};
use scrozz_ui::{
    FrozenDisplayFrame, SelectionDecision, SelectionUi, overlay_app::OverlayGeometry,
    select::DisplayLayout,
};

const BRIDGE_POLL: Duration = Duration::from_millis(25);

/// A selector owned by the long-running app.
///
/// The core trait ends when the user chooses a target. A desktop overlay must
/// stay excluded or hidden until that target has actually been captured, so the
/// app adds the lifecycle notifications the platform-neutral contract cannot
/// know.
pub trait CaptureSelector: RegionSelector {
    /// Reserves the app surface for a capture that needs no picker.
    ///
    /// `surface_can_remain_visible` is true only when the capture backend
    /// guarantees that current-process windows are absent from the result.
    fn begin_capture(&self, _surface_can_remain_visible: bool) -> Result<()> {
        Ok(())
    }

    /// Runs selection with the pointer policy that the following capture uses.
    fn select_for_capture(
        &self,
        options: &SelectionOptions,
        _cursor: CursorMode,
        _surface_can_remain_visible: bool,
    ) -> Result<SelectionOutcome> {
        self.select(options)
    }

    /// Takes pixels frozen before the overlay appeared when they exactly satisfy
    /// `request`.
    fn take_frozen_capture(&self, _request: &CaptureRequest) -> Option<Capture> {
        None
    }

    /// The capture attempt following the last successful selection has ended.
    ///
    /// Implementations ignore calls from a worker that does not own the active
    /// selection.
    fn capture_finished(&self) {}

    /// Cancels blocked selection calls during app shutdown.
    fn cancel(&self) {}
}

/// A selected strategy whose platform adapter cannot yet produce an outcome.
pub struct UnsupportedSelector {
    name: &'static str,
    capabilities: SelectionCapabilities,
    reason: String,
    lifecycle: Option<Arc<dyn CaptureSelector>>,
}

impl UnsupportedSelector {
    /// Builds a selector that returns the plan's exact implementation gap.
    #[must_use]
    pub fn from_plan(plan: SelectionPlan) -> Self {
        let name = match plan.host {
            SelectionHost::ClientOverlay => "client-overlay-unavailable",
            SelectionHost::LayerShell => "layer-shell-unavailable",
            SelectionHost::CompositorOwned => "gnome-screenshot-portal",
            SelectionHost::Headless => "headless",
        };
        Self {
            name,
            capabilities: plan.capabilities,
            reason: plan.detail,
            lifecycle: None,
        }
    }

    fn with_lifecycle(plan: SelectionPlan, lifecycle: Arc<dyn CaptureSelector>) -> Self {
        let mut selector = Self::from_plan(plan);
        selector.lifecycle = Some(lifecycle);
        selector
    }

    /// A selector for a host with no window.
    #[must_use]
    pub fn headless() -> Self {
        Self::from_plan(resolve_selection(SelectionIntegration::HEADLESS))
    }
}

impl RegionSelector for UnsupportedSelector {
    fn name(&self) -> &'static str {
        self.name
    }

    fn capabilities(&self) -> SelectionCapabilities {
        self.capabilities
    }

    fn select(&self, _options: &SelectionOptions) -> Result<SelectionOutcome> {
        Err(Error::Unsupported {
            what: "interactive capture selection".to_owned(),
            why: self.reason.clone(),
        })
    }
}

impl CaptureSelector for UnsupportedSelector {
    fn begin_capture(&self, surface_can_remain_visible: bool) -> Result<()> {
        self.lifecycle.as_ref().map_or(Ok(()), |lifecycle| {
            lifecycle.begin_capture(surface_can_remain_visible)
        })
    }

    fn capture_finished(&self) {
        if let Some(lifecycle) = &self.lifecycle {
            lifecycle.capture_finished();
        }
    }

    fn cancel(&self) {
        if let Some(lifecycle) = &self.lifecycle {
            lifecycle.cancel();
        }
    }
}

/// The synchronous worker-side half of the selector hosted by the existing
/// eframe loop.
pub struct ClientOverlaySelector {
    events: Sender<BridgeEvent>,
    gate: Arc<(Mutex<Gate>, Condvar)>,
    prepare: Arc<PrepareFn>,
}

#[derive(Debug)]
struct ActiveSelection {
    id: u64,
    owner: ThreadId,
    frozen: Option<FrozenCapture>,
}

#[derive(Debug)]
struct FrozenCapture {
    cursor: CursorMode,
    capture: Capture,
}

#[derive(Debug, Default)]
struct Gate {
    active: Option<ActiveSelection>,
    stopped: bool,
    next_id: u64,
}

type PrepareFn = dyn Fn(SelectionOptions, CursorMode) -> Result<PreparedSelection> + Send + Sync;

struct FrozenSource {
    display: Display,
    capture: Capture,
}

struct PreparedSelection {
    options: SelectionOptions,
    displays: Vec<Display>,
    windows: Vec<Window>,
    frozen: Vec<FrozenDisplayFrame>,
    frozen_sources: Vec<FrozenSource>,
    viewports: SelectorViewports,
}

#[derive(Debug, Clone)]
struct SelectorViewports {
    root: SelectorRootViewport,
    children: Vec<SelectorChildViewport>,
}

#[derive(Debug, Clone)]
struct SelectorRootViewport {
    display: Option<DisplayId>,
    geometry: OverlayGeometry,
}

#[derive(Debug, Clone)]
struct SelectorChildViewport {
    id: egui::ViewportId,
    display: DisplayId,
    geometry: OverlayGeometry,
}

impl SelectorViewports {
    fn release_pointer(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd_to(
            egui::ViewportId::ROOT,
            egui::ViewportCommand::MousePassthrough(true),
        );
        for child in &self.children {
            ctx.send_viewport_cmd_to(child.id, egui::ViewportCommand::MousePassthrough(true));
        }
    }

    fn hide_and_release_input(&self, ctx: &egui::Context) {
        self.release_pointer(ctx);
        ctx.send_viewport_cmd_to(
            egui::ViewportId::ROOT,
            egui::ViewportCommand::Visible(false),
        );
        for child in &self.children {
            ctx.send_viewport_cmd_to(child.id, egui::ViewportCommand::Visible(false));
        }
        ctx.request_repaint();
    }
}

enum BridgeEvent {
    BeginCapture {
        id: u64,
        hidden: Sender<()>,
        surface_can_remain_visible: bool,
    },
    Begin {
        id: u64,
        hidden: Sender<()>,
        surface_can_remain_visible: bool,
    },
    Prepared {
        id: u64,
        prepared: Box<PreparedSelection>,
        decision: Sender<Result<SelectionOutcome>>,
    },
    PreparationFailed {
        id: u64,
    },
    CaptureFinished {
        id: u64,
        restored: Sender<()>,
    },
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Completion {
    RestoreCards,
    CloseWindow,
}

/// Main-thread state machine that swaps the card surface for [`SelectionUi`].
pub struct ClientOverlayController {
    events: Receiver<BridgeEvent>,
    phase: ControllerPhase,
    cards: OverlayGeometry,
    completion: Completion,
}

enum ControllerPhase {
    Cards,
    HideBeforeCapture {
        id: u64,
        hidden: Sender<()>,
    },
    HideBeforePreparation {
        id: u64,
        hidden: Sender<()>,
    },
    WaitingForPreparation {
        id: u64,
    },
    PreparingWithCards {
        id: u64,
    },
    ReadyToSelect {
        id: u64,
        prepared: Box<PreparedSelection>,
        decision: Sender<Result<SelectionOutcome>>,
        saw_quiet_frame: bool,
    },
    Selecting {
        id: u64,
        ui: Box<SelectionUi>,
        viewports: SelectorViewports,
        decision: Sender<Result<SelectionOutcome>>,
    },
    ReleaseBeforeHide {
        id: u64,
        decision: Sender<Result<SelectionOutcome>>,
        result: Result<SelectionOutcome>,
        viewports: SelectorViewports,
    },
    HideAfterDecision {
        id: u64,
        decision: Sender<Result<SelectionOutcome>>,
        result: Result<SelectionOutcome>,
    },
    AwaitingCapture {
        id: u64,
    },
    RestoringCards {
        restored: Sender<()>,
    },
}

impl ClientOverlaySelector {
    /// Creates the worker and main-thread halves for the long-running app.
    #[must_use]
    pub fn managed(cards: OverlayGeometry) -> (Arc<Self>, ClientOverlayController) {
        Self::pair(cards, Completion::RestoreCards, Arc::new(prepare_native))
    }

    /// Creates the two halves for a one-shot interactive CLI window.
    #[must_use]
    pub fn one_shot() -> (Arc<Self>, ClientOverlayController) {
        Self::pair(
            OverlayGeometry::default(),
            Completion::CloseWindow,
            Arc::new(prepare_native),
        )
    }

    fn pair(
        cards: OverlayGeometry,
        completion: Completion,
        prepare: Arc<PrepareFn>,
    ) -> (Arc<Self>, ClientOverlayController) {
        let (events, receiver) = channel();
        let selector = Arc::new(Self {
            events,
            gate: Arc::new((Mutex::new(Gate::default()), Condvar::new())),
            prepare,
        });
        let controller = ClientOverlayController {
            events: receiver,
            phase: ControllerPhase::Cards,
            cards,
            completion,
        };
        (selector, controller)
    }

    fn acquire(&self) -> Result<u64> {
        let (lock, changed) = &*self.gate;
        let mut gate = lock
            .lock()
            .map_err(|_| bridge_error("the selector lifecycle lock was poisoned"))?;
        while gate.active.is_some() && !gate.stopped {
            gate = changed
                .wait(gate)
                .map_err(|_| bridge_error("the selector lifecycle lock was poisoned"))?;
        }
        if gate.stopped {
            return Err(Error::Cancelled);
        }
        gate.next_id = gate.next_id.wrapping_add(1).max(1);
        let id = gate.next_id;
        gate.active = Some(ActiveSelection {
            id,
            owner: thread::current().id(),
            frozen: None,
        });
        Ok(id)
    }

    fn release(&self, id: u64) {
        let (lock, changed) = &*self.gate;
        if let Ok(mut gate) = lock.lock() {
            let owned = gate
                .active
                .as_ref()
                .is_some_and(|active| active.id == id && active.owner == thread::current().id());
            if owned {
                gate.active = None;
                changed.notify_all();
            }
        }
    }

    fn is_stopped(&self) -> bool {
        self.gate.0.lock().map_or(true, |gate| gate.stopped)
    }

    fn wait_delay(&self, delay: Duration) -> Result<()> {
        let deadline = Instant::now().checked_add(delay).ok_or_else(|| {
            Error::InvalidRequest("the selection delay is too large for this platform".to_owned())
        })?;
        let (lock, changed) = &*self.gate;
        let mut gate = lock
            .lock()
            .map_err(|_| bridge_error("the selector lifecycle lock was poisoned"))?;
        loop {
            if gate.stopped {
                return Err(Error::Cancelled);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            let wait = (deadline - now).min(BRIDGE_POLL);
            let (next, _) = changed
                .wait_timeout(gate, wait)
                .map_err(|_| bridge_error("the selector lifecycle lock was poisoned"))?;
            gate = next;
        }
    }

    fn receive<T>(&self, receiver: &Receiver<T>, disconnected: &'static str) -> Result<T> {
        loop {
            match receiver.recv_timeout(BRIDGE_POLL) {
                Ok(value) => return Ok(value),
                Err(RecvTimeoutError::Timeout) if self.is_stopped() => {
                    return Err(Error::Cancelled);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(bridge_error(disconnected));
                }
            }
        }
    }

    fn run_selection(
        &self,
        id: u64,
        options: &SelectionOptions,
        cursor: CursorMode,
        surface_can_remain_visible: bool,
    ) -> Result<SelectionOutcome> {
        if let Some(delay) = options.delay
            && let Err(error) = self.wait_delay(delay)
        {
            let _ = self.events.send(BridgeEvent::PreparationFailed { id });
            return Err(error);
        }

        let (hidden_tx, hidden_rx) = channel();
        self.events
            .send(BridgeEvent::Begin {
                id,
                hidden: hidden_tx,
                surface_can_remain_visible,
            })
            .map_err(|_| bridge_error("the selector window closed before it could hide"))?;
        self.receive(
            &hidden_rx,
            "the selector window closed before confirming it was hidden",
        )?;

        let mut prepared = match (self.prepare)(options.clone(), cursor) {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = self.events.send(BridgeEvent::PreparationFailed { id });
                return Err(error);
            }
        };
        let frozen_sources = std::mem::take(&mut prepared.frozen_sources);
        let (decision_tx, decision_rx) = channel();
        self.events
            .send(BridgeEvent::Prepared {
                id,
                prepared: Box::new(prepared),
                decision: decision_tx,
            })
            .map_err(|_| bridge_error("the selector window closed before it could draw"))?;
        let outcome = self.receive(
            &decision_rx,
            "the selector window closed before a target was chosen",
        )??;
        let frozen = frozen_capture_for_outcome(outcome.clone(), frozen_sources)?;
        self.store_frozen_capture(id, cursor, frozen)?;
        Ok(outcome)
    }

    fn select_internal(
        &self,
        options: &SelectionOptions,
        cursor: CursorMode,
        surface_can_remain_visible: bool,
    ) -> Result<SelectionOutcome> {
        let id = self.acquire()?;
        let result = self.run_selection(id, options, cursor, surface_can_remain_visible);
        if result.is_err() && self.is_stopped() {
            self.release(id);
        }
        result
    }

    fn store_frozen_capture(
        &self,
        id: u64,
        cursor: CursorMode,
        capture: Option<Capture>,
    ) -> Result<()> {
        let mut gate = self
            .gate
            .0
            .lock()
            .map_err(|_| bridge_error("the selector lifecycle lock was poisoned"))?;
        let Some(active) = gate
            .active
            .as_mut()
            .filter(|active| active.id == id && active.owner == thread::current().id())
        else {
            return Err(bridge_error(
                "the selector lifecycle changed before frozen pixels were stored",
            ));
        };
        active.frozen = capture.map(|capture| FrozenCapture { cursor, capture });
        Ok(())
    }
}

impl RegionSelector for ClientOverlaySelector {
    fn name(&self) -> &'static str {
        "client-overlay"
    }

    fn capabilities(&self) -> SelectionCapabilities {
        SelectionCapabilities::CLIENT_OVERLAY
    }

    fn select(&self, options: &SelectionOptions) -> Result<SelectionOutcome> {
        self.select_internal(options, CursorMode::Hidden, false)
    }
}

impl CaptureSelector for ClientOverlaySelector {
    fn begin_capture(&self, surface_can_remain_visible: bool) -> Result<()> {
        let id = self.acquire()?;
        let (hidden_tx, hidden_rx) = channel();
        if self
            .events
            .send(BridgeEvent::BeginCapture {
                id,
                hidden: hidden_tx,
                surface_can_remain_visible,
            })
            .is_err()
        {
            self.release(id);
            return Err(bridge_error(
                "the capture window closed before it could hide",
            ));
        }
        if let Err(error) = self.receive(
            &hidden_rx,
            "the capture window closed before confirming it was hidden",
        ) {
            self.release(id);
            return Err(error);
        }
        Ok(())
    }

    fn select_for_capture(
        &self,
        options: &SelectionOptions,
        cursor: CursorMode,
        surface_can_remain_visible: bool,
    ) -> Result<SelectionOutcome> {
        self.select_internal(options, cursor, surface_can_remain_visible)
    }

    fn take_frozen_capture(&self, request: &CaptureRequest) -> Option<Capture> {
        let mut gate = self.gate.0.lock().ok()?;
        let active = gate.active.as_mut()?;
        if active.owner != thread::current().id() {
            return None;
        }
        let frozen = active.frozen.as_ref()?;
        if frozen.cursor != request.cursor || frozen.capture.target != request.target {
            return None;
        }
        active.frozen.take().map(|frozen| frozen.capture)
    }

    fn capture_finished(&self) {
        let id = {
            let Ok(gate) = self.gate.0.lock() else {
                return;
            };
            let Some(active) = gate
                .active
                .as_ref()
                .filter(|active| active.owner == thread::current().id())
            else {
                return;
            };
            if gate.stopped {
                return;
            }
            active.id
        };

        let (restored_tx, restored_rx) = channel();
        if self
            .events
            .send(BridgeEvent::CaptureFinished {
                id,
                restored: restored_tx,
            })
            .is_ok()
        {
            let _ = self.receive(
                &restored_rx,
                "the selector window closed before restoring capture cards",
            );
        }
        self.release(id);
    }

    fn cancel(&self) {
        let (lock, changed) = &*self.gate;
        if let Ok(mut gate) = lock.lock() {
            gate.stopped = true;
            gate.active = None;
            changed.notify_all();
        }
        let _ = self.events.send(BridgeEvent::Cancel);
    }
}

impl ClientOverlayController {
    /// Updates the work area capture cards return to after selection.
    pub fn set_cards_geometry(&mut self, geometry: OverlayGeometry) {
        self.cards = geometry;
    }

    /// Advances lifecycle handshakes and applies viewport/native behavior.
    pub fn logic(&mut self, ctx: &egui::Context, native: &crate::gui::panel::BehaviorController) {
        self.advance(ctx, native);
        while let Ok(event) = self.events.try_recv() {
            self.handle_event(ctx, native, event);
        }
        native.refresh();
    }

    /// Draws the selector when a prepared selection owns the window.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let decision = match &mut self.phase {
            ControllerPhase::Selecting {
                ui: selector,
                viewports,
                ..
            } => {
                let mut decision = if let Some(display) = &viewports.root.display {
                    selector.update_display(ui, display)
                } else {
                    selector.update(ui)
                };
                if decision == SelectionDecision::Pending {
                    let ctx = ui.ctx().clone();
                    for child in &viewports.children {
                        let builder = child_viewport_builder(child);
                        decision =
                            ctx.show_viewport_immediate(child.id, builder, |child_ui, _class| {
                                selector.update_display(child_ui, &child.display)
                            });
                        if decision != SelectionDecision::Pending {
                            break;
                        }
                    }
                }
                decision
            }
            ControllerPhase::ReleaseBeforeHide { viewports, .. } => {
                keep_child_viewports_alive(ui.ctx(), viewports);
                return;
            }
            _ => return,
        };
        let result = match decision {
            SelectionDecision::Pending => return,
            SelectionDecision::Selected(outcome) => Ok(outcome),
            SelectionDecision::Cancelled => Err(Error::Cancelled),
        };

        let phase = std::mem::replace(&mut self.phase, ControllerPhase::Cards);
        if let ControllerPhase::Selecting {
            id,
            decision,
            viewports,
            ..
        } = phase
        {
            // The committing key may still be down, so the viewports stay alive
            // long enough to consume key-up. Pointer input is independent and
            // must be released immediately once selection has ended.
            viewports.release_pointer(ui.ctx());
            self.phase = ControllerPhase::ReleaseBeforeHide {
                id,
                decision,
                result,
                viewports,
            };
        }
    }

    /// Whether the card UI should yield the frame to selection.
    #[must_use]
    pub fn owns_surface(&self) -> bool {
        !matches!(
            self.phase,
            ControllerPhase::Cards
                | ControllerPhase::PreparingWithCards { .. }
                | ControllerPhase::ReadyToSelect { .. }
        )
    }

    fn advance(&mut self, ctx: &egui::Context, native: &crate::gui::panel::BehaviorController) {
        let phase = std::mem::replace(&mut self.phase, ControllerPhase::Cards);
        self.phase = match phase {
            ControllerPhase::HideBeforeCapture { id, hidden } => {
                let _ = hidden.send(());
                ControllerPhase::AwaitingCapture { id }
            }
            ControllerPhase::HideBeforePreparation { id, hidden } => {
                let _ = hidden.send(());
                ControllerPhase::WaitingForPreparation { id }
            }
            ControllerPhase::ReadyToSelect {
                id,
                prepared,
                decision,
                saw_quiet_frame,
            } => {
                let quiet = input_is_quiescent(ctx, egui::ViewportId::ROOT);
                if quiet && saw_quiet_frame {
                    activate_selection(ctx, native, id, prepared, decision)
                } else {
                    ctx.request_repaint();
                    ControllerPhase::ReadyToSelect {
                        id,
                        prepared,
                        decision,
                        saw_quiet_frame: quiet,
                    }
                }
            }
            ControllerPhase::ReleaseBeforeHide {
                id,
                decision,
                result,
                viewports,
            } => {
                if !selector_input_is_quiescent(ctx, &viewports) {
                    ctx.request_repaint();
                    ControllerPhase::ReleaseBeforeHide {
                        id,
                        decision,
                        result,
                        viewports,
                    }
                } else {
                    native.apply(&scrozz_shell::OverlayBehavior::hidden_surface());
                    viewports.hide_and_release_input(ctx);
                    ControllerPhase::HideAfterDecision {
                        id,
                        decision,
                        result,
                    }
                }
            }
            ControllerPhase::HideAfterDecision {
                id,
                decision,
                result,
            } => {
                let _ = decision.send(result);
                ControllerPhase::AwaitingCapture { id }
            }
            ControllerPhase::RestoringCards { restored } => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                let _ = restored.send(());
                ControllerPhase::Cards
            }
            other => other,
        };
    }

    fn handle_event(
        &mut self,
        ctx: &egui::Context,
        native: &crate::gui::panel::BehaviorController,
        event: BridgeEvent,
    ) {
        match event {
            BridgeEvent::BeginCapture {
                id,
                hidden,
                surface_can_remain_visible: true,
            } if matches!(self.phase, ControllerPhase::Cards) => {
                let _ = hidden.send(());
            }
            BridgeEvent::BeginCapture {
                id,
                hidden,
                surface_can_remain_visible: false,
            } if matches!(self.phase, ControllerPhase::Cards) => {
                native.apply(&scrozz_shell::OverlayBehavior::hidden_surface());
                ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                ctx.request_repaint();
                self.phase = ControllerPhase::HideBeforeCapture { id, hidden };
            }
            BridgeEvent::Begin {
                id,
                hidden,
                surface_can_remain_visible: true,
            } if matches!(self.phase, ControllerPhase::Cards) => {
                let _ = hidden.send(());
                self.phase = ControllerPhase::PreparingWithCards { id };
            }
            BridgeEvent::Begin {
                id,
                hidden,
                surface_can_remain_visible: false,
            } if matches!(self.phase, ControllerPhase::Cards) => {
                native.apply(&scrozz_shell::OverlayBehavior::hidden_surface());
                ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                ctx.request_repaint();
                self.phase = ControllerPhase::HideBeforePreparation { id, hidden };
            }
            BridgeEvent::Prepared {
                id,
                prepared,
                decision,
            } if matches!(
                self.phase,
                ControllerPhase::PreparingWithCards { id: waiting } if waiting == id
            ) =>
            {
                self.phase = ControllerPhase::ReadyToSelect {
                    id,
                    prepared,
                    decision,
                    saw_quiet_frame: false,
                };
            }
            BridgeEvent::Prepared {
                id,
                prepared,
                decision,
            } if matches!(
                self.phase,
                ControllerPhase::WaitingForPreparation { id: waiting } if waiting == id
            ) =>
            {
                self.phase = activate_selection(ctx, native, id, prepared, decision);
            }
            BridgeEvent::PreparationFailed { id }
                if matches!(
                    self.phase,
                    ControllerPhase::WaitingForPreparation { id: waiting } if waiting == id
                ) =>
            {
                self.phase = ControllerPhase::AwaitingCapture { id };
            }
            BridgeEvent::PreparationFailed { id }
                if matches!(
                    self.phase,
                    ControllerPhase::PreparingWithCards { id: waiting } if waiting == id
                ) =>
            {
                self.phase = ControllerPhase::Cards;
            }
            BridgeEvent::PreparationFailed { id }
                if matches!(self.phase, ControllerPhase::Cards) =>
            {
                self.phase = ControllerPhase::AwaitingCapture { id };
            }
            BridgeEvent::CaptureFinished { id, restored }
                if matches!(
                    self.phase,
                    ControllerPhase::AwaitingCapture { id: waiting } if waiting == id
                ) =>
            {
                if self.completion == Completion::CloseWindow {
                    native.apply(&scrozz_shell::OverlayBehavior::hidden_surface());
                    ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    let _ = restored.send(());
                    self.phase = ControllerPhase::Cards;
                } else {
                    configure_viewport(ctx, self.cards, false);
                    native.apply(&scrozz_shell::OverlayBehavior::hidden_surface());
                    ctx.request_repaint();
                    self.phase = ControllerPhase::RestoringCards { restored };
                }
            }
            BridgeEvent::CaptureFinished { restored, .. }
                if matches!(self.phase, ControllerPhase::Cards) =>
            {
                let _ = restored.send(());
            }
            BridgeEvent::Cancel => {
                native.apply(&scrozz_shell::OverlayBehavior::hidden_surface());
                match &self.phase {
                    ControllerPhase::Selecting { viewports, .. }
                    | ControllerPhase::ReleaseBeforeHide { viewports, .. } => {
                        viewports.hide_and_release_input(ctx);
                    }
                    _ => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    }
                }
                if self.completion == Completion::CloseWindow {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                self.phase = ControllerPhase::Cards;
            }
            _ => {
                tracing::warn!("ignored an out-of-order selector lifecycle event");
            }
        }
    }
}

fn activate_selection(
    ctx: &egui::Context,
    native: &crate::gui::panel::BehaviorController,
    id: u64,
    prepared: Box<PreparedSelection>,
    decision: Sender<Result<SelectionOutcome>>,
) -> ControllerPhase {
    configure_viewport(ctx, prepared.viewports.root.geometry, true);
    native.apply(&scrozz_shell::OverlayBehavior::selection_overlay());
    let selector = SelectionUi::new(prepared.options, prepared.displays, prepared.frozen)
        .with_windows(prepared.windows)
        .with_capabilities(SelectionCapabilities::CLIENT_OVERLAY);
    ControllerPhase::Selecting {
        id,
        ui: Box::new(selector),
        viewports: prepared.viewports,
        decision,
    }
}

fn prepare_native(options: SelectionOptions, cursor: CursorMode) -> Result<PreparedSelection> {
    let backend = scrozz_capture::backend()?;
    let displays = backend.displays()?;
    let viewports = selector_viewports(&displays)?;
    let needs_windows = options.hud || options.mode == scrozz_core::SelectionMode::Window;
    let windows = if needs_windows {
        match backend.windows() {
            Ok(windows) => windows,
            Err(error) if options.hud && options.mode != scrozz_core::SelectionMode::Window => {
                tracing::warn!(%error, "window picking is unavailable in All-in-One");
                Vec::new()
            }
            Err(error) => return Err(error),
        }
    } else {
        Vec::new()
    };
    require_visible_window(&options, &windows)?;

    let mut frozen = Vec::new();
    let mut frozen_sources = Vec::new();
    if options.freeze || options.needs_magnifier_frame() {
        frozen.reserve(displays.len());
        if options.freeze {
            frozen_sources.reserve(displays.len());
        }
        for display in &displays {
            let capture = backend.capture(&CaptureRequest {
                target: CaptureTarget::Display(display.id.clone()),
                cursor,
                include_window_shadow: false,
            })?;
            frozen.push(FrozenDisplayFrame::from_frame(
                display.clone(),
                capture.frame.clone(),
            )?);
            if options.freeze {
                frozen_sources.push(FrozenSource {
                    display: display.clone(),
                    capture,
                });
            }
        }
    }

    Ok(PreparedSelection {
        options,
        displays,
        windows,
        frozen,
        frozen_sources,
        viewports,
    })
}

fn require_visible_window(options: &SelectionOptions, windows: &[Window]) -> Result<()> {
    if options.mode == scrozz_core::SelectionMode::Window
        && !windows.iter().any(|window| window.is_visible)
    {
        return Err(Error::Unsupported {
            what: "interactive window selection".to_owned(),
            why: "the capture backend reported no visible windows; open a window or capture a display instead"
                .to_owned(),
        });
    }
    Ok(())
}

/// Whether every display snapshot used to prepare selection omits Scrozz while
/// leaving its windows visible to the user.
pub(crate) fn display_captures_exclude_current_process(
    backend: &dyn CaptureBackend,
) -> Result<bool> {
    let displays = backend.displays()?;
    Ok(!displays.is_empty()
        && displays.iter().all(|display| {
            backend.excludes_current_process(&CaptureTarget::Display(display.id.clone()))
        }))
}

fn frozen_capture_for_outcome(
    outcome: SelectionOutcome,
    frozen_sources: Vec<FrozenSource>,
) -> Result<Option<Capture>> {
    match outcome.mode {
        scrozz_core::SelectionMode::Region => {
            let Some(display_id) = outcome.display.as_ref() else {
                return Ok(None);
            };
            let Some(source) = frozen_sources
                .into_iter()
                .find(|source| source.display.id == *display_id)
            else {
                return Ok(None);
            };
            let Some(rect) = outcome.rect else {
                return Ok(None);
            };
            crop_frozen_region(source, rect).map(Some)
        }
        scrozz_core::SelectionMode::Display => {
            let Some(display_id) = outcome.display else {
                return Ok(None);
            };
            Ok(frozen_sources
                .into_iter()
                .find(|source| source.display.id == display_id)
                .map(|source| source.capture))
        }
        scrozz_core::SelectionMode::Window | scrozz_core::SelectionMode::AllDisplays => Ok(None),
    }
}

/// Captures an interactive outcome without discarding its display ownership.
///
/// A Windows mixed-DPI desktop can contain overlapping public logical display
/// rectangles. Re-inferring a monitor from a region rectangle can therefore
/// choose a different display than the viewport the user dragged on. Capturing
/// that measured display and cropping it keeps the existing backend contract
/// while preserving the selector's unambiguous result.
pub(crate) fn capture_selected(
    backend: &dyn CaptureBackend,
    request: &CaptureRequest,
    outcome: Option<&SelectionOutcome>,
) -> Result<Capture> {
    let Some(outcome) =
        outcome.filter(|outcome| outcome.mode == scrozz_core::SelectionMode::Region)
    else {
        return backend.capture(request);
    };
    let (Some(display_id), Some(rect)) = (outcome.display.as_ref(), outcome.rect) else {
        return backend.capture(request);
    };
    let display = backend
        .displays()?
        .into_iter()
        .find(|display| display.id == *display_id)
        .ok_or_else(|| Error::TargetGone(format!("display {} disconnected", display_id.0)))?;
    let capture = backend.capture(&CaptureRequest {
        target: CaptureTarget::Display(display.id.clone()),
        cursor: request.cursor,
        include_window_shadow: false,
    })?;
    crop_display_capture(display, capture, rect)
}

fn crop_frozen_region(source: FrozenSource, rect: LogicalRect) -> Result<Capture> {
    crop_display_capture(source.display, source.capture, rect)
}

fn crop_display_capture(display: Display, capture: Capture, rect: LogicalRect) -> Result<Capture> {
    let local = LogicalRect::new(
        LogicalPoint::new(
            rect.origin.x - display.bounds.origin.x,
            rect.origin.y - display.bounds.origin.y,
        ),
        rect.size,
    )
    .to_physical(display.scale);
    let frame = capture.frame;
    if !frame.is_well_formed() {
        return Err(Error::Codec(
            "the frozen display frame is malformed".to_owned(),
        ));
    }

    let left = local.origin.x.max(0.0) as usize;
    let top = local.origin.y.max(0.0) as usize;
    let right = (local.origin.x + local.size.width)
        .min(f64::from(frame.width()))
        .max(0.0) as usize;
    let bottom = (local.origin.y + local.size.height)
        .min(f64::from(frame.height()))
        .max(0.0) as usize;
    if right <= left || bottom <= top {
        return Err(Error::InvalidRequest(
            "the selected region does not overlap its frozen display".to_owned(),
        ));
    }

    let bytes_per_pixel = frame.format.bytes_per_pixel();
    let width = right - left;
    let height = bottom - top;
    let stride = width * bytes_per_pixel;
    let mut data = Vec::with_capacity(stride * height);
    for y in top..bottom {
        let row_start = y * frame.stride + left * bytes_per_pixel;
        data.extend_from_slice(&frame.data[row_start..row_start + stride]);
    }
    let target = CaptureTarget::Region(rect);
    Ok(Capture {
        frame: Frame {
            data,
            size: PhysicalSize::new(width as f64, height as f64),
            stride,
            format: frame.format,
            color_space: frame.color_space,
            scale: frame.scale,
        },
        provenance: Provenance::Region,
        target,
    })
}

fn desktop_geometry(displays: &[Display]) -> Result<OverlayGeometry> {
    let bounds = DisplayLayout::new(displays.to_vec())
        .desktop_bounds()
        .ok_or_else(|| bridge_error("the capture backend reported no connected displays"))?;
    Ok(OverlayGeometry::new(egui::Rect::from_min_size(
        egui::pos2(bounds.origin.x as f32, bounds.origin.y as f32),
        egui::vec2(bounds.size.width as f32, bounds.size.height as f32),
    )))
}

fn display_geometry(display: &Display, launch_scale: scrozz_core::ScaleFactor) -> OverlayGeometry {
    // eframe creates a child viewport using the root window's DPI, then Windows
    // moves it to the requested monitor. Express the target device origin in
    // that launch scale so the first HWND lands on the intended monitor; the
    // selector surface itself continues to use the target display's logical
    // coordinates and scale.
    let launch_position_scale = display.scale.get() / launch_scale.get();
    OverlayGeometry::new(egui::Rect::from_min_size(
        egui::pos2(
            (display.bounds.origin.x * launch_position_scale) as f32,
            (display.bounds.origin.y * launch_position_scale) as f32,
        ),
        egui::vec2(
            display.bounds.size.width as f32,
            display.bounds.size.height as f32,
        ),
    ))
}

fn selector_viewports(displays: &[Display]) -> Result<SelectorViewports> {
    selector_viewports_for(displays, cfg!(target_os = "windows"))
}

fn selector_viewports_for(
    displays: &[Display],
    split_by_display: bool,
) -> Result<SelectorViewports> {
    if displays.is_empty() {
        return Err(bridge_error(
            "the capture backend reported no connected displays",
        ));
    }
    if !split_by_display || displays.len() == 1 {
        return Ok(SelectorViewports {
            root: SelectorRootViewport {
                display: None,
                geometry: desktop_geometry(displays)?,
            },
            children: Vec::new(),
        });
    }

    let root_index = displays
        .iter()
        .position(|display| display.is_primary)
        .unwrap_or(0);
    let root = &displays[root_index];
    let launch_scale = root.scale;
    let children = displays
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != root_index)
        .map(|(_, display)| SelectorChildViewport {
            id: egui::ViewportId::from_hash_of(("scrozz-selector", &display.id.0)),
            display: display.id.clone(),
            geometry: display_geometry(display, launch_scale),
        })
        .collect();

    Ok(SelectorViewports {
        root: SelectorRootViewport {
            display: Some(root.id.clone()),
            geometry: display_geometry(root, launch_scale),
        },
        children,
    })
}

fn child_viewport_builder(child: &SelectorChildViewport) -> egui::ViewportBuilder {
    scrozz_ui::overlay_app::viewport(child.geometry)
        .with_title("Scrozz Selector")
        .with_app_id(format!("com.scrozz.selector.{}", child.display.0))
}

fn keep_child_viewports_alive(ctx: &egui::Context, viewports: &SelectorViewports) -> usize {
    for child in &viewports.children {
        ctx.show_viewport_immediate(
            child.id,
            child_viewport_builder(child),
            |_child_ui, _class| {},
        );
    }
    viewports.children.len()
}

fn selector_input_is_quiescent(ctx: &egui::Context, viewports: &SelectorViewports) -> bool {
    input_is_quiescent(ctx, egui::ViewportId::ROOT)
        && viewports
            .children
            .iter()
            .all(|child| input_is_quiescent(ctx, child.id))
}

fn input_is_quiescent(ctx: &egui::Context, viewport: egui::ViewportId) -> bool {
    ctx.input_for(viewport, |input| {
        input.keys_down.is_empty()
            && !input.pointer.any_down()
            && !input.modifiers.alt
            && !input.modifiers.ctrl
            && !input.modifiers.shift
            && !input.modifiers.mac_cmd
            && !input.modifiers.command
    })
}

fn configure_viewport(ctx: &egui::Context, geometry: OverlayGeometry, selection: bool) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(geometry.position()));
    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(geometry.size()));
    ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(!selection));
    // The viewport starts always-on-top. Re-queueing that level here would run
    // after the native macOS adapter raises selection to shielding level and
    // silently lower it back to an ordinary floating window.
    if selection {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }
}

fn bridge_error(detail: impl Into<String>) -> Error {
    Error::Platform(detail.into())
}

/// Resolves the current session without treating an advertised protocol as an
/// implemented rendering surface.
#[must_use]
pub fn current_plan() -> SelectionPlan {
    plan_for_session(&Session::detect())
}

/// Pure strategy resolution from the shell's session snapshot.
#[must_use]
pub fn plan_for_session(session: &Session) -> SelectionPlan {
    resolve_selection(SelectionIntegration::for_session(session))
}

/// Uses `client` only when this session permits an ordinary positioned overlay.
#[must_use]
pub fn for_current_session(
    client: Arc<dyn CaptureSelector>,
) -> (Arc<dyn CaptureSelector>, SelectionPlan) {
    let plan = current_plan();
    if plan.host == SelectionHost::ClientOverlay && plan.is_available() {
        (client, plan)
    } else {
        (
            Arc::new(UnsupportedSelector::with_lifecycle(plan.clone(), client)),
            plan,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::{
        DisplayId, LogicalPoint, LogicalRect, LogicalSize, ScaleFactor, TargetEnumerator,
    };
    use scrozz_shell::{Compositor, DisplayServer};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    fn session(server: DisplayServer, compositor: Compositor) -> Session {
        Session {
            server,
            compositor,
            desktop: String::new(),
        }
    }

    #[test]
    fn native_hosts_choose_the_client_overlay() {
        for server in [
            DisplayServer::Quartz,
            DisplayServer::Windows,
            DisplayServer::X11,
        ] {
            let plan = plan_for_session(&session(server, Compositor::Other));
            assert_eq!(plan.host, SelectionHost::ClientOverlay);
            assert!(plan.is_available());
        }
    }

    #[test]
    fn gnome_selects_the_portal_route_without_inventing_a_result() {
        let plan = plan_for_session(&session(DisplayServer::Wayland, Compositor::Gnome));
        assert_eq!(plan.host, SelectionHost::CompositorOwned);
        assert!(!plan.is_available());
        let selector = UnsupportedSelector::from_plan(plan);
        assert_eq!(selector.name(), "gnome-screenshot-portal");
        assert_eq!(selector.capabilities(), SelectionCapabilities::NONE);
        let error = selector.select(&SelectionOptions::region()).unwrap_err();
        assert!(matches!(error, Error::Unsupported { .. }));
        assert!(error.to_string().contains("image URI"), "{error}");
    }

    #[test]
    fn layer_shell_compositors_do_not_relabel_eframe_as_a_layer_surface() {
        for compositor in [Compositor::Kde, Compositor::Sway, Compositor::Hyprland] {
            let plan = plan_for_session(&session(DisplayServer::Wayland, compositor));
            assert_eq!(plan.host, SelectionHost::LayerShell);
            assert!(!plan.is_available());
            assert!(plan.detail.contains("xdg_toplevel"), "{}", plan.detail);
        }
    }

    #[test]
    fn explicit_window_selection_refuses_an_empty_enumeration() {
        let error = require_visible_window(
            &SelectionOptions::for_mode(scrozz_core::SelectionMode::Window),
            &[],
        )
        .unwrap_err();

        assert!(matches!(error, Error::Unsupported { .. }));
        assert!(error.to_string().contains("no visible windows"), "{error}");
        assert!(
            require_visible_window(&SelectionOptions::default(), &[]).is_ok(),
            "All-in-One may continue with its unavailable Window entry disabled"
        );
    }

    #[test]
    fn mixed_dpi_windows_use_one_native_viewport_per_display() {
        let display = |id: &str, bounds: LogicalRect, scale: f64, is_primary: bool| -> Display {
            Display {
                id: DisplayId(id.to_owned()),
                name: id.to_owned(),
                bounds,
                work_area: bounds,
                scale: ScaleFactor::new(scale),
                is_primary,
            }
        };
        let displays = vec![
            display(
                "primary",
                LogicalRect::new(
                    LogicalPoint::new(0.0, 0.0),
                    LogicalSize::new(1920.0, 1080.0),
                ),
                1.0,
                true,
            ),
            display(
                "hidpi",
                LogicalRect::new(
                    LogicalPoint::new(1280.0, 0.0),
                    LogicalSize::new(1706.67, 960.0),
                ),
                1.5,
                false,
            ),
        ];

        let viewports = selector_viewports_for(&displays, true).unwrap();
        assert_eq!(
            viewports.root.display,
            Some(DisplayId("primary".to_owned()))
        );
        assert_eq!(viewports.root.geometry.size(), egui::vec2(1920.0, 1080.0));
        assert_eq!(viewports.children.len(), 1);
        assert_eq!(viewports.children[0].display, DisplayId("hidpi".to_owned()));
        assert_eq!(
            viewports.children[0].geometry.position(),
            egui::pos2(1920.0, 0.0)
        );
        assert!(
            (viewports.children[0].geometry.size() - egui::vec2(1706.67, 960.0)).length() < 0.01
        );
    }

    #[test]
    fn frozen_region_output_crops_the_pre_overlay_frame_with_its_stride() {
        let bounds = LogicalRect::new(LogicalPoint::new(100.0, 50.0), LogicalSize::new(4.0, 3.0));
        let display = Display {
            id: DisplayId("retina".to_owned()),
            name: "Retina".to_owned(),
            bounds,
            work_area: bounds,
            scale: ScaleFactor::new(2.0),
            is_primary: true,
        };
        let width = 8_usize;
        let height = 6_usize;
        let stride = width * 4 + 4;
        let mut data = vec![0_u8; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                data[offset..offset + 4].copy_from_slice(&[x as u8, y as u8, 99, 255]);
            }
        }
        let source = FrozenSource {
            display: display.clone(),
            capture: Capture {
                frame: Frame {
                    data,
                    size: PhysicalSize::new(width as f64, height as f64),
                    stride,
                    format: scrozz_core::PixelFormat::Rgba8,
                    color_space: scrozz_core::ColorSpace::DisplayP3,
                    scale: display.scale,
                },
                provenance: Provenance::Display,
                target: CaptureTarget::Display(display.id.clone()),
            },
        };
        let rect = LogicalRect::new(LogicalPoint::new(101.0, 50.5), LogicalSize::new(2.0, 1.0));
        let outcome = SelectionOutcome::region(
            rect,
            Some(display.id),
            display.scale,
            scrozz_core::SelectionSource::ClientOverlay,
        );

        let capture = frozen_capture_for_outcome(outcome, vec![source])
            .expect("the frozen crop should be valid")
            .expect("a region should reuse its frozen display");

        assert_eq!(capture.target, CaptureTarget::Region(rect));
        assert_eq!(capture.provenance, Provenance::Region);
        assert_eq!(capture.frame.width(), 4);
        assert_eq!(capture.frame.height(), 2);
        assert_eq!(capture.frame.stride, 16);
        assert_eq!(
            capture.frame.color_space,
            scrozz_core::ColorSpace::DisplayP3
        );
        assert_eq!(&capture.frame.data[0..4], &[2, 1, 99, 255]);
        assert_eq!(&capture.frame.data[12..16], &[5, 1, 99, 255]);
        assert_eq!(&capture.frame.data[16..20], &[2, 2, 99, 255]);
    }

    struct RecordingBackend {
        displays: Vec<Display>,
        targets: Mutex<Vec<CaptureTarget>>,
    }

    impl TargetEnumerator for RecordingBackend {
        fn displays(&self) -> Result<Vec<Display>> {
            Ok(self.displays.clone())
        }

        fn windows(&self) -> Result<Vec<Window>> {
            Ok(Vec::new())
        }

        fn active_display(&self) -> Result<Display> {
            Ok(self.displays[0].clone())
        }
    }

    impl CaptureBackend for RecordingBackend {
        fn capture(&self, request: &CaptureRequest) -> Result<Capture> {
            self.targets.lock().unwrap().push(request.target.clone());
            let CaptureTarget::Display(display_id) = &request.target else {
                panic!("the selected display should be captured before cropping")
            };
            let display = self
                .displays
                .iter()
                .find(|display| display.id == *display_id)
                .unwrap();
            let width = (display.bounds.size.width * display.scale.get()) as usize;
            let height = (display.bounds.size.height * display.scale.get()) as usize;
            Ok(Capture {
                frame: Frame {
                    data: vec![display.scale.get() as u8; width * height * 4],
                    size: PhysicalSize::new(width as f64, height as f64),
                    stride: width * 4,
                    format: scrozz_core::PixelFormat::Rgba8,
                    color_space: scrozz_core::ColorSpace::Srgb,
                    scale: display.scale,
                },
                provenance: Provenance::Display,
                target: request.target.clone(),
            })
        }

        fn name(&self) -> &str {
            "recording"
        }
    }

    #[test]
    fn live_region_capture_uses_the_selected_display_in_ambiguous_geometry() {
        let bounds = LogicalRect::new(LogicalPoint::new(50.0, 0.0), LogicalSize::new(100.0, 80.0));
        let hidpi = Display {
            id: DisplayId("hidpi".to_owned()),
            name: "HiDPI".to_owned(),
            bounds,
            work_area: bounds,
            scale: ScaleFactor::new(2.0),
            is_primary: false,
        };
        let primary_bounds =
            LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(200.0, 100.0));
        let backend = RecordingBackend {
            displays: vec![
                Display {
                    id: DisplayId("primary".to_owned()),
                    name: "Primary".to_owned(),
                    bounds: primary_bounds,
                    work_area: primary_bounds,
                    scale: ScaleFactor::IDENTITY,
                    is_primary: true,
                },
                hidpi.clone(),
            ],
            targets: Mutex::new(Vec::new()),
        };
        let rect = LogicalRect::new(LogicalPoint::new(60.0, 10.0), LogicalSize::new(20.0, 15.0));
        let outcome = SelectionOutcome::region(
            rect,
            Some(hidpi.id.clone()),
            hidpi.scale,
            scrozz_core::SelectionSource::ClientOverlay,
        );
        let request = CaptureRequest::new(CaptureTarget::Region(rect));

        let capture = capture_selected(&backend, &request, Some(&outcome)).unwrap();

        assert_eq!(
            backend.targets.into_inner().unwrap(),
            vec![CaptureTarget::Display(hidpi.id)]
        );
        assert_eq!(capture.target, CaptureTarget::Region(rect));
        assert_eq!(capture.provenance, Provenance::Region);
        assert_eq!(capture.frame.width(), 40);
        assert_eq!(capture.frame.height(), 30);
        assert!(capture.frame.data.iter().all(|byte| *byte == 2));
    }

    #[test]
    fn bridge_hides_before_preparing_and_holds_the_gate_through_capture() {
        let preparations = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&preparations);
        let prepare: Arc<PrepareFn> = Arc::new(move |options, _cursor| {
            counted.fetch_add(1, Ordering::SeqCst);
            let bounds =
                LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(320.0, 240.0));
            let displays = vec![Display {
                id: DisplayId("main".to_owned()),
                name: "Main".to_owned(),
                bounds,
                work_area: bounds,
                scale: ScaleFactor::new(2.0),
                is_primary: true,
            }];
            Ok(PreparedSelection {
                viewports: selector_viewports_for(&displays, false)
                    .expect("test display has geometry"),
                options,
                displays,
                windows: Vec::new(),
                frozen: Vec::new(),
                frozen_sources: Vec::new(),
            })
        });
        let cards = OverlayGeometry::default();
        let (selector, mut controller) =
            ClientOverlaySelector::pair(cards, Completion::RestoreCards, prepare);
        let options = SelectionOptions {
            remembered: Some(LogicalRect::new(
                LogicalPoint::new(20.0, 30.0),
                LogicalSize::new(120.0, 80.0),
            )),
            reuse_immediately: true,
            freeze: false,
            magnifier: false,
            ..SelectionOptions::region()
        };
        let (selected_tx, selected_rx) = channel();
        let (finish_owner_tx, finish_owner_rx) = channel();
        let (owner_finished_tx, owner_finished_rx) = channel();
        let first = Arc::clone(&selector);
        let first_options = options.clone();
        let first_worker = std::thread::spawn(move || {
            let _ = selected_tx.send(first.select(&first_options));
            let _ = finish_owner_rx.recv();
            first.capture_finished();
            let _ = owner_finished_tx.send(());
        });
        let ctx = egui::Context::default();
        let (native, behavior_log) = crate::gui::panel::BehaviorController::recording();

        wait_until(|| {
            controller.logic(&ctx, &native);
            matches!(
                &controller.phase,
                ControllerPhase::HideBeforePreparation { .. }
            )
        });
        assert_eq!(
            preparations.load(Ordering::SeqCst),
            0,
            "preparation must wait until a hidden frame has elapsed"
        );

        controller.logic(&ctx, &native);
        wait_until(|| {
            controller.logic(&ctx, &native);
            matches!(&controller.phase, ControllerPhase::Selecting { .. })
        });
        assert_eq!(preparations.load(Ordering::SeqCst), 1);
        assert_eq!(
            *behavior_log.borrow(),
            vec![
                scrozz_shell::OverlayBehavior::hidden_surface(),
                scrozz_shell::OverlayBehavior::selection_overlay(),
            ],
            "the hidden preparation window must release input before selection takes it"
        );

        let mut output = ctx.run_ui(
            egui::RawInput {
                focused: true,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(320.0, 240.0),
                )),
                events: vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| controller.ui(ui),
        );
        output.textures_delta.clear();
        assert!(
            output
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .is_some_and(|viewport| {
                    viewport.commands.iter().any(|command| {
                        matches!(command, egui::ViewportCommand::MousePassthrough(true))
                    })
                }),
            "selection completion must release pointer input in the same frame"
        );
        assert!(matches!(
            &controller.phase,
            ControllerPhase::ReleaseBeforeHide { .. }
        ));
        assert_eq!(
            *behavior_log.borrow(),
            vec![
                scrozz_shell::OverlayBehavior::hidden_surface(),
                scrozz_shell::OverlayBehavior::selection_overlay(),
            ],
            "focus must remain owned until the decision handshake advances"
        );
        controller.logic(&ctx, &native);
        assert_eq!(
            *behavior_log.borrow(),
            vec![
                scrozz_shell::OverlayBehavior::hidden_surface(),
                scrozz_shell::OverlayBehavior::selection_overlay(),
            ],
            "focus must remain owned until the committing key is released"
        );
        assert!(matches!(
            &controller.phase,
            ControllerPhase::ReleaseBeforeHide { .. }
        ));
        assert!(
            selected_rx.try_recv().is_err(),
            "capture must not begin while a terminal key is still held"
        );

        let mut output = ctx.run_ui(
            egui::RawInput {
                focused: true,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(320.0, 240.0),
                )),
                events: vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |_| {},
        );
        output.textures_delta.clear();
        controller.logic(&ctx, &native);
        assert_eq!(
            *behavior_log.borrow(),
            vec![
                scrozz_shell::OverlayBehavior::hidden_surface(),
                scrozz_shell::OverlayBehavior::selection_overlay(),
                scrozz_shell::OverlayBehavior::hidden_surface(),
            ],
            "the invisible capture interval must release pointer and keyboard input"
        );
        assert!(matches!(
            &controller.phase,
            ControllerPhase::HideAfterDecision { .. }
        ));
        assert!(
            selected_rx.try_recv().is_err(),
            "capture must wait until the hidden selector frame has elapsed"
        );
        controller.logic(&ctx, &native);
        let selected = selected_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("selection worker should receive the hidden decision")
            .expect("remembered selection should commit");
        assert_eq!(
            selected.target,
            CaptureTarget::Region(options.remembered.unwrap())
        );

        let (second_tx, second_rx) = channel();
        let second = Arc::clone(&selector);
        let second_options = options;
        let second_worker = std::thread::spawn(move || {
            let _ = second_tx.send(second.select(&second_options));
        });
        std::thread::sleep(Duration::from_millis(30));
        controller.logic(&ctx, &native);
        assert!(
            second_rx.try_recv().is_err(),
            "a second selection must wait through the first capture"
        );
        assert_eq!(preparations.load(Ordering::SeqCst), 1);

        let wrong_worker = {
            let selector = Arc::clone(&selector);
            std::thread::spawn(move || selector.capture_finished())
        };
        wrong_worker.join().expect("non-owner finish worker");
        controller.logic(&ctx, &native);
        assert!(
            matches!(&controller.phase, ControllerPhase::AwaitingCapture { .. }),
            "a different worker must not complete the active selection"
        );
        assert!(
            second_rx.try_recv().is_err(),
            "a different worker must not release the selector gate"
        );

        finish_owner_tx.send(()).expect("finish owner signal");
        wait_until(|| {
            controller.logic(&ctx, &native);
            matches!(&controller.phase, ControllerPhase::RestoringCards { .. })
        });
        assert!(
            behavior_log
                .borrow()
                .last()
                .is_some_and(|behavior| behavior.click_through),
            "restoration must stay fail-open until the card renderer paints"
        );
        controller.logic(&ctx, &native);
        owner_finished_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("capture finish should wait for restored cards");
        first_worker.join().expect("first selector worker");

        selector.cancel();
        second_worker.join().expect("second selector worker");
    }

    #[test]
    fn terminal_input_barrier_waits_for_escape_and_enter_release() {
        for key in [egui::Key::Escape, egui::Key::Enter] {
            let ctx = egui::Context::default();
            let key_event = |pressed| egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            };
            let mut output = ctx.run_ui(
                egui::RawInput {
                    focused: true,
                    events: vec![key_event(true)],
                    ..Default::default()
                },
                |_| {},
            );
            output.textures_delta.clear();
            assert!(
                !input_is_quiescent(&ctx, egui::ViewportId::ROOT),
                "{key:?} press must retain selector focus"
            );

            let mut output = ctx.run_ui(
                egui::RawInput {
                    focused: true,
                    events: vec![key_event(false)],
                    ..Default::default()
                },
                |_| {},
            );
            output.textures_delta.clear();
            assert!(
                input_is_quiescent(&ctx, egui::ViewportId::ROOT),
                "{key:?} release must unlock selector focus restoration"
            );
        }
    }

    #[test]
    fn terminal_input_barrier_includes_secondary_viewports() {
        let ctx = egui::Context::default();
        let child_id = egui::ViewportId::from_hash_of("secondary");
        let viewports = SelectorViewports {
            root: SelectorRootViewport {
                display: None,
                geometry: OverlayGeometry::default(),
            },
            children: vec![SelectorChildViewport {
                id: child_id,
                display: DisplayId("secondary".to_owned()),
                geometry: OverlayGeometry::default(),
            }],
        };
        let key = |pressed| egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let run_child = |ctx: &egui::Context, event| {
            let mut input = egui::RawInput {
                viewport_id: child_id,
                events: vec![event],
                ..Default::default()
            };
            input.viewports.insert(child_id, Default::default());
            let mut output = ctx.run_ui(input, |_| {});
            output.textures_delta.clear();
        };

        run_child(&ctx, key(true));
        assert!(
            !selector_input_is_quiescent(&ctx, &viewports),
            "a held key in a child selector must retain input ownership"
        );
        run_child(&ctx, key(false));
        assert!(
            selector_input_is_quiescent(&ctx, &viewports),
            "releasing the child selector key must unlock teardown"
        );
    }

    #[test]
    fn release_barrier_reissues_every_secondary_viewport() {
        let child_id = egui::ViewportId::from_hash_of("secondary-release");
        let viewports = SelectorViewports {
            root: SelectorRootViewport {
                display: None,
                geometry: OverlayGeometry::default(),
            },
            children: vec![SelectorChildViewport {
                id: child_id,
                display: DisplayId("secondary".to_owned()),
                geometry: OverlayGeometry::default(),
            }],
        };
        let ctx = egui::Context::default();
        let mut kept = 0;
        let mut output = ctx.run_ui(egui::RawInput::default(), |_ui| {
            kept = keep_child_viewports_alive(&ctx, &viewports);
        });
        output.textures_delta.clear();

        assert_eq!(kept, 1);
    }

    #[test]
    fn fixed_capture_hides_and_reserves_the_surface_until_completion() {
        let prepare: Arc<PrepareFn> =
            Arc::new(|_, _| panic!("a fixed capture must not prepare a selector"));
        let (selector, mut controller) = ClientOverlaySelector::pair(
            OverlayGeometry::default(),
            Completion::RestoreCards,
            prepare,
        );
        let (begun_tx, begun_rx) = channel();
        let (finish_tx, finish_rx) = channel();
        let worker_selector = Arc::clone(&selector);
        let worker = std::thread::spawn(move || {
            let _ = begun_tx.send(worker_selector.begin_capture(false));
            let _ = finish_rx.recv();
            worker_selector.capture_finished();
        });
        let ctx = egui::Context::default();
        let native = crate::gui::panel::BehaviorController::default();

        wait_until(|| {
            controller.logic(&ctx, &native);
            matches!(&controller.phase, ControllerPhase::HideBeforeCapture { .. })
        });
        assert!(begun_rx.try_recv().is_err());

        controller.logic(&ctx, &native);
        begun_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("capture worker should be released after a hidden frame")
            .expect("surface should hide");
        assert!(matches!(
            &controller.phase,
            ControllerPhase::AwaitingCapture { .. }
        ));

        finish_tx.send(()).expect("finish signal");
        wait_until(|| {
            controller.logic(&ctx, &native);
            matches!(&controller.phase, ControllerPhase::RestoringCards { .. })
        });
        controller.logic(&ctx, &native);
        worker.join().expect("fixed capture worker");
        assert!(matches!(&controller.phase, ControllerPhase::Cards));
    }

    #[test]
    fn fixed_capture_keeps_cards_visible_when_the_backend_excludes_them() {
        let prepare: Arc<PrepareFn> =
            Arc::new(|_, _| panic!("a fixed capture must not prepare a selector"));
        let (selector, mut controller) = ClientOverlaySelector::pair(
            OverlayGeometry::default(),
            Completion::RestoreCards,
            prepare,
        );
        let (begun_tx, begun_rx) = channel();
        let (finish_tx, finish_rx) = channel();
        let (finished_tx, finished_rx) = channel();
        let worker_selector = Arc::clone(&selector);
        let worker = std::thread::spawn(move || {
            let _ = begun_tx.send(worker_selector.begin_capture(true));
            let _ = finish_rx.recv();
            worker_selector.capture_finished();
            let _ = finished_tx.send(());
        });
        let ctx = egui::Context::default();
        let (native, behavior_log) = crate::gui::panel::BehaviorController::recording();
        let mut begun = None;

        wait_until(|| {
            controller.logic(&ctx, &native);
            if let Ok(result) = begun_rx.try_recv() {
                begun = Some(result);
            }
            begun.is_some()
        });
        begun.expect("begin result").expect("capture should begin");
        assert!(matches!(&controller.phase, ControllerPhase::Cards));
        assert!(!controller.owns_surface());
        assert!(
            behavior_log.borrow().is_empty(),
            "native cards must not be hidden when the backend excludes them"
        );

        finish_tx.send(()).expect("finish signal");
        wait_until(|| {
            controller.logic(&ctx, &native);
            finished_rx.try_recv().is_ok()
        });
        worker.join().expect("fixed capture worker");
        assert!(matches!(&controller.phase, ControllerPhase::Cards));
        assert!(behavior_log.borrow().is_empty());
    }

    #[test]
    fn selection_prepares_behind_visible_cards_when_exclusion_is_supported() {
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let prepare_barrier = Arc::clone(&barrier);
        let prepare: Arc<PrepareFn> = Arc::new(move |options, _cursor| {
            prepare_barrier.wait();
            let bounds =
                LogicalRect::new(LogicalPoint::new(0.0, 0.0), LogicalSize::new(320.0, 240.0));
            let displays = vec![Display {
                id: DisplayId("main".to_owned()),
                name: "Main".to_owned(),
                bounds,
                work_area: bounds,
                scale: ScaleFactor::new(2.0),
                is_primary: true,
            }];
            Ok(PreparedSelection {
                viewports: selector_viewports_for(&displays, false)
                    .expect("test display has geometry"),
                options,
                displays,
                windows: Vec::new(),
                frozen: Vec::new(),
                frozen_sources: Vec::new(),
            })
        });
        let (selector, mut controller) = ClientOverlaySelector::pair(
            OverlayGeometry::default(),
            Completion::RestoreCards,
            prepare,
        );
        let worker_selector = Arc::clone(&selector);
        let worker = std::thread::spawn(move || {
            worker_selector.select_for_capture(
                &SelectionOptions {
                    freeze: false,
                    magnifier: false,
                    ..SelectionOptions::region()
                },
                CursorMode::Hidden,
                true,
            )
        });
        let ctx = egui::Context::default();
        let (native, behavior_log) = crate::gui::panel::BehaviorController::recording();

        wait_until(|| {
            controller.logic(&ctx, &native);
            matches!(
                &controller.phase,
                ControllerPhase::PreparingWithCards { .. }
            )
        });
        assert!(!controller.owns_surface());
        assert!(behavior_log.borrow().is_empty());

        let pointer = egui::pos2(60.0, 60.0);
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![
                    egui::Event::PointerMoved(pointer),
                    egui::Event::PointerButton {
                        pos: pointer,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |_| {},
        );
        output.textures_delta.clear();

        barrier.wait();
        wait_until(|| {
            controller.logic(&ctx, &native);
            matches!(&controller.phase, ControllerPhase::ReadyToSelect { .. })
        });
        assert!(
            !controller.owns_surface(),
            "the cards keep the release frame so a press cannot leak into selection"
        );
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::PointerButton {
                    pos: pointer,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |_| {},
        );
        output.textures_delta.clear();
        controller.logic(&ctx, &native);
        assert!(matches!(
            &controller.phase,
            ControllerPhase::ReadyToSelect {
                saw_quiet_frame: true,
                ..
            }
        ));
        controller.logic(&ctx, &native);
        assert!(matches!(
            &controller.phase,
            ControllerPhase::Selecting { .. }
        ));
        assert_eq!(
            *behavior_log.borrow(),
            vec![scrozz_shell::OverlayBehavior::selection_overlay()]
        );

        selector.cancel();
        controller.logic(&ctx, &native);
        assert!(worker.join().expect("selection worker").is_err());
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        for _ in 0..200 {
            if predicate() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("selector state did not advance before the test deadline");
    }
}
