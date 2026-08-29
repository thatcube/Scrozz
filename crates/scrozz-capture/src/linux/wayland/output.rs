//! Compositor-native Wayland output identity and geometry.
//!
//! `wl_output` identifies one compositor output and reports its current pixel
//! mode. The `xdg-output` extension adds the global logical position, logical
//! size, and stable connector-style name needed to compare that output with a
//! ScreenCast portal stream. Keeping both halves on one Wayland connection also
//! gives reusable sessions a backing-output identity that changes on hotplug.

use scrozz_core::{
    Display, DisplayId, Error, LogicalPoint, LogicalRect, LogicalSize, Result, ScaleFactor,
};
use smithay_client_toolkit::{
    delegate_output, delegate_registry,
    output::{OutputHandler, OutputInfo, OutputState},
    reexports::client::{
        Connection, EventQueue, QueueHandle, globals::registry_queue_init, protocol::wl_output,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
};

/// One compositor output at one instant.
#[derive(Debug, Clone)]
pub struct OutputSnapshot {
    /// Public output facts.
    pub display: Display,
    /// Server-global identity of the backing `wl_output`.
    pub identity: OutputIdentity,
}

/// Identity retained across refreshes of one Wayland connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputIdentity {
    registry_name: u32,
    protocol_name: String,
}

impl std::fmt::Display for OutputIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}@{}", self.protocol_name, self.registry_name)
    }
}

/// A live output registry retained by a reusable frame session.
pub struct OutputMonitor {
    queue: EventQueue<OutputMonitorState>,
    state: OutputMonitorState,
}

impl std::fmt::Debug for OutputMonitor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutputMonitor")
            .finish_non_exhaustive()
    }
}

impl OutputMonitor {
    /// Connects to the compositor and waits for the initial output facts.
    pub fn connect(compositor: &str) -> Result<Self> {
        let connection = Connection::connect_to_env().map_err(|error| Error::Unsupported {
            what: "listing displays on Wayland".into(),
            why: format!(
                "Scrozz could not connect to {compositor}'s Wayland socket to read native output \
                 identity and per-output geometry: {error}"
            ),
        })?;
        let (globals, mut queue) = registry_queue_init(&connection).map_err(|error| {
            Error::Platform(format!(
                "could not read {compositor}'s Wayland global registry: {error}"
            ))
        })?;
        let handle = queue.handle();
        let registry = RegistryState::new(&globals);
        let outputs = OutputState::new(&globals, &handle);
        let mut state = OutputMonitorState { registry, outputs };
        queue.roundtrip(&mut state).map_err(|error| {
            Error::Platform(format!(
                "could not read {compositor}'s native output properties: {error}"
            ))
        })?;
        Ok(Self { queue, state })
    }

    /// Refreshes output lifecycle events and returns one complete desktop layout.
    pub fn snapshots(&mut self, compositor: &str) -> Result<Vec<OutputSnapshot>> {
        // A hotplug event can bind a new wl_output while the first sync is being
        // dispatched. The second round trip guarantees that object's initial
        // wl_output and xdg-output events are also complete.
        for _ in 0..2 {
            self.queue.roundtrip(&mut self.state).map_err(|error| {
                Error::Platform(format!(
                    "could not refresh {compositor}'s native Wayland outputs: {error}"
                ))
            })?;
        }

        let snapshots = self
            .state
            .outputs
            .outputs()
            .map(|output| {
                let info = self.state.outputs.info(&output).ok_or_else(|| {
                    incomplete_output(
                        compositor,
                        "the compositor had not completed this wl_output's properties",
                    )
                })?;
                snapshot_from_info(compositor, &info)
            })
            .collect::<Result<Vec<_>>>()?;
        if snapshots.is_empty() {
            return Err(Error::TargetGone(
                "the Wayland compositor reports no connected outputs".into(),
            ));
        }
        for (index, snapshot) in snapshots.iter().enumerate() {
            if snapshots[index + 1..]
                .iter()
                .any(|other| other.display.id == snapshot.display.id)
            {
                return Err(incomplete_output(
                    compositor,
                    &format!(
                        "several wl_output globals reported the same stable name {:?}",
                        snapshot.identity.protocol_name
                    ),
                ));
            }
        }
        Ok(snapshots)
    }
}

struct OutputMonitorState {
    registry: RegistryState,
    outputs: OutputState,
}

impl OutputHandler for OutputMonitorState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    fn new_output(
        &mut self,
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _connection: &Connection,
        _handle: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

delegate_output!(OutputMonitorState);
delegate_registry!(OutputMonitorState);

impl ProvidesRegistryState for OutputMonitorState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }

    registry_handlers!(OutputState);
}

fn snapshot_from_info(compositor: &str, info: &OutputInfo) -> Result<OutputSnapshot> {
    let protocol_name = info.name.clone().ok_or_else(|| {
        incomplete_output(
            compositor,
            "an output omitted its stable protocol name (requires wl_output v4 or xdg-output v2)",
        )
    })?;
    if protocol_name.trim().is_empty() {
        return Err(incomplete_output(
            compositor,
            "an output reported an empty stable protocol name",
        ));
    }
    let (x, y) = info.logical_position.ok_or_else(|| {
        incomplete_output(
            compositor,
            &format!("output {protocol_name:?} omitted its xdg-output logical position"),
        )
    })?;
    let (logical_width, logical_height) = info.logical_size.ok_or_else(|| {
        incomplete_output(
            compositor,
            &format!("output {protocol_name:?} omitted its xdg-output logical size"),
        )
    })?;
    if logical_width <= 0 || logical_height <= 0 {
        return Err(incomplete_output(
            compositor,
            &format!(
                "output {protocol_name:?} reported a non-positive logical size \
                 {logical_width}x{logical_height}"
            ),
        ));
    }

    let mut current_modes = info.modes.iter().filter(|mode| mode.current);
    let current_mode = current_modes.next().ok_or_else(|| {
        incomplete_output(
            compositor,
            &format!("output {protocol_name:?} omitted its current physical pixel mode"),
        )
    })?;
    if current_modes.next().is_some() {
        return Err(incomplete_output(
            compositor,
            &format!("output {protocol_name:?} reported several current physical modes"),
        ));
    }
    let scale =
        infer_scale(current_mode.dimensions, (logical_width, logical_height)).ok_or_else(|| {
            incomplete_output(
                compositor,
                &format!(
                    "output {protocol_name:?} reported physical mode {:?} and logical size \
                 {logical_width}x{logical_height}, which do not describe one uniform per-output \
                 scale even after accounting for rotation",
                    current_mode.dimensions
                ),
            )
        })?;
    // `infer_scale` returns only finite positive factors, which is exactly
    // ScaleFactor's invariant.
    let scale = ScaleFactor::new(scale);

    let bounds = LogicalRect::new(
        LogicalPoint::new(f64::from(x), f64::from(y)),
        LogicalSize::new(f64::from(logical_width), f64::from(logical_height)),
    );
    let display_name = info
        .description
        .clone()
        .filter(|description| !description.trim().is_empty())
        .unwrap_or_else(|| protocol_name.clone());

    Ok(OutputSnapshot {
        display: Display {
            id: DisplayId(format!("wayland-output:{protocol_name}")),
            name: display_name,
            bounds,
            work_area: bounds,
            scale,
            // Wayland defines no primary-output state. False means there is no
            // compositor-native primary claim, not that another output was
            // guessed to be primary.
            is_primary: false,
        },
        identity: OutputIdentity {
            registry_name: info.id,
            protocol_name,
        },
    })
}

fn incomplete_output(compositor: &str, detail: &str) -> Error {
    Error::Unsupported {
        what: "using exact display geometry on Wayland".into(),
        why: format!(
            "{detail}. Scrozz needs compositor-native wl_output identity plus xdg-output logical \
             geometry and a current physical mode on every connected output; it refuses the whole \
             layout instead of mixing exact and guessed display facts on {compositor}"
        ),
    }
}

/// Infers a compositor's nominal fractional scale from its native logical and
/// physical output dimensions.
///
/// Fractional-scale logical dimensions are integral, so one axis can be rounded
/// by a pixel. Prefer the protocol's 1/120 scale grid when it explains both axes;
/// otherwise accept only ratios close enough to differ by that rounding.
fn infer_scale(physical: (i32, i32), logical: (i32, i32)) -> Option<f64> {
    let candidates = [physical, (physical.1, physical.0)];
    candidates
        .into_iter()
        .filter_map(|(physical_width, physical_height)| {
            if physical_width <= 0 || physical_height <= 0 || logical.0 <= 0 || logical.1 <= 0 {
                return None;
            }
            let x = f64::from(physical_width) / f64::from(logical.0);
            let y = f64::from(physical_height) / f64::from(logical.1);
            let mean = (x + y) / 2.0;
            let protocol_scale = (mean * 120.0).round() / 120.0;
            let explains_rounding =
                (f64::from(logical.0) * protocol_scale - f64::from(physical_width)).abs() <= 1.0
                    && (f64::from(logical.1) * protocol_scale - f64::from(physical_height)).abs()
                        <= 1.0;
            let mismatch = (x - y).abs() / mean;
            (mean.is_finite() && mean > 0.0 && (explains_rounding || mismatch <= 0.002)).then_some(
                (
                    if explains_rounding {
                        protocol_scale
                    } else {
                        mean
                    },
                    mismatch,
                ),
            )
        })
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(scale, _)| scale)
}

#[cfg(test)]
mod tests {
    use super::infer_scale;

    #[test]
    fn scale_inference_handles_fractional_rounding_and_rotation() {
        assert_eq!(infer_scale((2560, 1440), (1707, 960)), Some(1.5));
        assert_eq!(infer_scale((1440, 2560), (1707, 960)), Some(1.5));
        assert_eq!(infer_scale((1920, 1080), (1920, 1080)), Some(1.0));
    }

    #[test]
    fn nonuniform_output_facts_are_refused() {
        assert_eq!(infer_scale((2560, 1080), (1920, 1080)), None);
    }
}
