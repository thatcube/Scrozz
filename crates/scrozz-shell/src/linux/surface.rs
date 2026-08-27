//! Pure state and value types for Scrozz-owned Linux overlay surfaces.
//!
//! The Wayland transport uses these types, but none of them opens a display or
//! depends on a native API. Keeping sizing, scaling, frame validation and event
//! state here makes the dangerous protocol edge cases testable on every host.

use std::num::NonZeroU32;

/// The denominator used by Wayland's fractional-scale protocol.
pub const SCALE_DENOMINATOR: u32 = 120;

/// A two-dimensional pixel or logical size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SurfaceSize {
    /// Horizontal extent.
    pub width: u32,
    /// Vertical extent.
    pub height: u32,
}

impl SurfaceSize {
    /// Creates a size from its two extents.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Whether either extent is zero.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Byte stride for one tightly packed RGBA8 row.
    #[must_use]
    pub fn rgba8_stride(self) -> Option<usize> {
        usize::try_from(u64::from(self.width).checked_mul(4)?).ok()
    }

    /// Byte length for a tightly packed RGBA8 image of this size.
    #[must_use]
    pub fn rgba8_byte_len(self) -> Option<usize> {
        self.rgba8_stride()?
            .checked_mul(usize::try_from(self.height).ok()?)
    }
}

/// A buffer scale expressed exactly in Wayland's 120ths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SurfaceScale(NonZeroU32);

impl SurfaceScale {
    /// Unscaled, or 120/120.
    pub const ONE: Self = Self(NonZeroU32::new(120).expect("120 is non-zero"));

    /// Creates a scale from protocol units.
    ///
    /// Returns `None` for zero, which is not a valid Wayland scale.
    #[must_use]
    pub const fn from_120ths(units: u32) -> Option<Self> {
        match NonZeroU32::new(units) {
            Some(units) => Some(Self(units)),
            None => None,
        }
    }

    /// The exact scale in protocol units, where 120 means 1×.
    #[must_use]
    pub const fn units_120(self) -> u32 {
        self.0.get()
    }

    /// The scale as a floating-point multiplier.
    #[must_use]
    pub fn factor(self) -> f64 {
        f64::from(self.units_120()) / f64::from(SCALE_DENOMINATOR)
    }

    /// Whether this scale has a fractional component.
    #[must_use]
    pub const fn is_fractional(self) -> bool {
        !self.units_120().is_multiple_of(SCALE_DENOMINATOR)
    }

    pub(crate) const fn from_integer(scale: u32) -> Self {
        let units = match scale.checked_mul(SCALE_DENOMINATOR) {
            Some(units) if units != 0 => units,
            _ => SCALE_DENOMINATOR,
        };
        Self(NonZeroU32::new(units).expect("the fallback scale is non-zero"))
    }
}

/// Calculates the buffer extent needed to cover a logical size at `scale`.
///
/// Each dimension rounds outward. `None` means an empty size or arithmetic
/// overflow; such a size cannot back a `wl_buffer`.
#[must_use]
pub fn scaled_buffer_size(logical: SurfaceSize, scale: SurfaceScale) -> Option<SurfaceSize> {
    fn dimension(logical: u32, scale: u32) -> Option<u32> {
        if logical == 0 {
            return None;
        }
        let numerator = u64::from(logical).checked_mul(u64::from(scale))?;
        let rounded =
            numerator.checked_add(u64::from(SCALE_DENOMINATOR - 1))? / u64::from(SCALE_DENOMINATOR);
        u32::try_from(rounded).ok().filter(|value| *value != 0)
    }

    Some(SurfaceSize {
        width: dimension(logical.width, scale.units_120())?,
        height: dimension(logical.height, scale.units_120())?,
    })
}

/// Which output a layer surface should occupy.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum OutputSelector {
    /// Pass no output to layer-shell and let the compositor choose.
    #[default]
    CompositorDefault,
    /// Select the exact stable name advertised by `wl_output.name`.
    Named(String),
}

/// Information advertised for one `wl_output`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputInfo {
    /// Registry-global numeric name, useful for diagnostics only.
    pub global_name: u32,
    /// Stable connector/output name when core output version 4 is available.
    pub name: Option<String>,
    /// Human-readable description when the compositor provides one.
    pub description: Option<String>,
    /// Integer output scale, never less than one.
    pub scale: u32,
}

/// Why a Scrozz-owned surface became unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceCloseReason {
    /// The layer-shell compositor sent `closed`.
    Compositor,
    /// The explicitly selected output disappeared from the registry.
    OutputRemoved {
        /// The selected output name, when it was known.
        name: Option<String>,
    },
    /// The Wayland connection stopped carrying events.
    ConnectionLost(String),
}

/// Pointer position in surface-local logical coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfacePoint {
    /// Horizontal coordinate.
    pub x: f64,
    /// Vertical coordinate.
    pub y: f64,
}

/// A Wayland pointer axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerAxis {
    /// A vertical scroll axis.
    Vertical,
    /// A horizontal scroll axis.
    Horizontal,
}

/// The physical source of a pointer-axis sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerAxisSource {
    /// A wheel.
    Wheel,
    /// Finger scrolling on a touch surface.
    Finger,
    /// A continuous source such as a trackpoint.
    Continuous,
    /// A wheel tilted sideways.
    WheelTilt,
    /// A value added by a newer protocol version.
    Unknown(u32),
}

/// Whether a pointer button was pressed or released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButtonState {
    /// The button transitioned down.
    Pressed,
    /// The button transitioned up.
    Released,
}

/// A pointer event delivered for this layer surface.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfacePointerEvent {
    /// The pointer entered the surface.
    Enter {
        /// Wayland input serial.
        serial: u32,
        /// Surface-local logical position.
        position: SurfacePoint,
    },
    /// The pointer moved while over the surface.
    Motion {
        /// Compositor timestamp in milliseconds.
        time: u32,
        /// Surface-local logical position.
        position: SurfacePoint,
    },
    /// The pointer left the surface.
    Leave {
        /// Wayland input serial.
        serial: u32,
    },
    /// A button changed state.
    Button {
        /// Wayland input serial.
        serial: u32,
        /// Compositor timestamp in milliseconds.
        time: u32,
        /// Linux input-event button code.
        button: u32,
        /// New button state.
        state: PointerButtonState,
        /// Last known surface-local pointer position.
        position: SurfacePoint,
    },
    /// Smooth axis motion.
    Axis {
        /// Compositor timestamp in milliseconds.
        time: u32,
        /// Axis that moved.
        axis: PointerAxis,
        /// Scroll distance in surface coordinate units.
        value: f64,
        /// Last known surface-local pointer position.
        position: SurfacePoint,
    },
    /// Legacy integer wheel steps.
    AxisDiscrete {
        /// Axis that moved.
        axis: PointerAxis,
        /// Number of wheel steps.
        steps: i32,
        /// Last known surface-local pointer position.
        position: SurfacePoint,
    },
    /// High-resolution wheel distance in 120ths of a step.
    AxisValue120 {
        /// Axis that moved.
        axis: PointerAxis,
        /// Distance in 120ths of a wheel step.
        value_120: i32,
        /// Last known surface-local pointer position.
        position: SurfacePoint,
    },
    /// An axis sequence stopped.
    AxisStop {
        /// Compositor timestamp in milliseconds.
        time: u32,
        /// Axis that stopped.
        axis: PointerAxis,
        /// Last known surface-local pointer position.
        position: SurfacePoint,
    },
    /// Declares the source for the current axis sequence.
    AxisSource(PointerAxisSource),
    /// Terminates one logical group of pointer events.
    Frame,
}

/// A state change or input event produced by the layer-shell transport.
#[derive(Debug, Clone, PartialEq)]
pub enum LayerSurfaceEvent {
    /// The compositor acknowledged a surface configuration.
    Configured {
        /// Resolved logical size, or `None` if both sides left an extent open.
        logical_size: Option<SurfaceSize>,
    },
    /// The preferred rendering scale changed.
    ScaleChanged {
        /// Exact new scale.
        scale: SurfaceScale,
        /// New outward-rounded buffer size when the logical size is known.
        buffer_size: Option<SurfaceSize>,
    },
    /// A pixel buffer mapped the surface.
    Mapped,
    /// A null attachment unmapped the surface.
    Unmapped,
    /// The surface can no longer be used.
    Closed {
        /// Why it closed.
        reason: SurfaceCloseReason,
    },
    /// Pointer input for the surface.
    Pointer(SurfacePointerEvent),
}

/// Result of trying to submit a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameCommit {
    /// The frame was attached and committed.
    Committed,
    /// Both reusable SHM buffers are still owned by the compositor.
    BuffersBusy,
    /// An empty commit requested the configure required after an unmap.
    AwaitingConfigure,
    /// Configure or scale changed while the caller was rendering this frame.
    SurfaceChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BufferLayout {
    pub(crate) size: SurfaceSize,
    pub(crate) stride: i32,
    pub(crate) byte_len: usize,
    pub(crate) pool_len: i32,
}

impl BufferLayout {
    pub(crate) fn new(size: SurfaceSize) -> Result<Self, &'static str> {
        if size.is_empty() {
            return Err("frame dimensions must both be non-zero");
        }
        if size.width > i32::MAX as u32 || size.height > i32::MAX as u32 {
            return Err("frame dimensions exceed wl_shm's signed 32-bit range");
        }

        let stride = u64::from(size.width)
            .checked_mul(4)
            .ok_or("frame stride overflow")?;
        let bytes = stride
            .checked_mul(u64::from(size.height))
            .ok_or("frame byte length overflow")?;
        if stride > i32::MAX as u64 || bytes > i32::MAX as u64 {
            return Err("frame allocation exceeds wl_shm's signed 32-bit range");
        }

        Ok(Self {
            size,
            stride: i32::try_from(stride).map_err(|_| "frame stride does not fit i32")?,
            byte_len: usize::try_from(bytes)
                .map_err(|_| "frame byte length does not fit this architecture")?,
            pool_len: i32::try_from(bytes).map_err(|_| "frame pool length does not fit i32")?,
        })
    }

    pub(crate) fn validate_pixels(
        self,
        size: SurfaceSize,
        stride: usize,
        pixels: &[u8],
    ) -> Result<(), String> {
        if size != self.size {
            return Err(format!(
                "premultiplied RGBA frame is {}x{}; the surface currently requires {}x{}",
                size.width, size.height, self.size.width, self.size.height
            ));
        }
        let row_bytes = usize::try_from(u64::from(size.width) * 4)
            .map_err(|_| "RGBA row length does not fit this architecture")?;
        if stride < row_bytes {
            return Err(format!(
                "premultiplied RGBA stride {stride} is smaller than the {row_bytes}-byte row"
            ));
        }
        let required = stride
            .checked_mul(
                usize::try_from(size.height)
                    .map_err(|_| "RGBA frame height does not fit this architecture")?,
            )
            .ok_or_else(|| "premultiplied RGBA byte length overflow".to_string())?;
        if pixels.len() != required {
            return Err(format!(
                "premultiplied RGBA frame has {} bytes; {}x{} at stride {} requires exactly {}",
                pixels.len(),
                size.width,
                size.height,
                stride,
                required
            ));
        }
        Ok(())
    }
}

pub(crate) fn write_argb8888(
    premultiplied_rgba: &[u8],
    source_stride: usize,
    size: SurfaceSize,
    destination: &mut [u8],
) {
    let row_bytes = size.width as usize * 4;
    debug_assert_eq!(destination.len(), row_bytes * size.height as usize);

    for row in 0..size.height as usize {
        let source_start = row * source_stride;
        let destination_start = row * row_bytes;
        let source = &premultiplied_rgba[source_start..source_start + row_bytes];
        let destination = &mut destination[destination_start..destination_start + row_bytes];
        let rgba_pixels = source.as_chunks::<4>().0;
        let argb_pixels = destination.as_chunks_mut::<4>().0;
        for (rgba, argb) in rgba_pixels.iter().zip(argb_pixels) {
            let pixel = u32::from(rgba[3]) << 24
                | u32::from(rgba[0]) << 16
                | u32::from(rgba[1]) << 8
                | u32::from(rgba[2]);
            argb.copy_from_slice(&pixel.to_ne_bytes());
        }
    }
}

pub(crate) fn first_reusable_buffer(
    buffers: impl IntoIterator<Item = (BufferLayout, bool)>,
    expected: BufferLayout,
) -> Option<usize> {
    buffers
        .into_iter()
        .enumerate()
        .find_map(|(index, (layout, busy))| (layout == expected && !busy).then_some(index))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameSignature {
    logical_size: Option<SurfaceSize>,
    scale: SurfaceScale,
    buffer_size: Option<SurfaceSize>,
}

pub(crate) fn submitted_frame_is_stale(
    before: FrameSignature,
    after: FrameSignature,
    submitted: SurfaceSize,
) -> bool {
    before.buffer_size == Some(submitted) && before != after
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemapState {
    Ready,
    NeedsConfigure,
    ConfigurePending,
}

impl RemapState {
    pub(crate) fn after_unmap(&mut self) {
        *self = Self::NeedsConfigure;
    }

    pub(crate) fn request_configure(&mut self) -> bool {
        if *self != Self::NeedsConfigure {
            return false;
        }
        *self = Self::ConfigurePending;
        true
    }

    pub(crate) fn configured(&mut self) {
        *self = Self::Ready;
    }

    pub(crate) fn is_ready(self) -> bool {
        self == Self::Ready
    }

    pub(crate) fn is_configure_pending(self) -> bool {
        self == Self::ConfigurePending
    }
}

#[derive(Debug)]
pub(crate) struct TransportState {
    requested: SurfaceSize,
    granted: Option<SurfaceSize>,
    pending_configure_serial: Option<u32>,
    integer_scale: u32,
    fractional_enabled: bool,
    fractional_scale_120: Option<u32>,
    mapped: bool,
    close_reason: Option<SurfaceCloseReason>,
    events: Vec<LayerSurfaceEvent>,
}

impl TransportState {
    pub(crate) fn new(requested: SurfaceSize, fractional_enabled: bool) -> Self {
        Self {
            requested,
            granted: None,
            pending_configure_serial: None,
            integer_scale: 1,
            fractional_enabled,
            fractional_scale_120: None,
            mapped: false,
            close_reason: None,
            events: Vec::new(),
        }
    }

    pub(crate) fn requested(&self) -> SurfaceSize {
        self.requested
    }

    pub(crate) fn begin_reconfigure(&mut self, requested: SurfaceSize) {
        self.requested = requested;
        self.granted = None;
    }

    pub(crate) fn configure(&mut self, serial: u32, granted: SurfaceSize) {
        if self.is_closed() {
            return;
        }
        self.pending_configure_serial = Some(serial);
        self.granted = Some(granted);
        self.events.push(LayerSurfaceEvent::Configured {
            logical_size: self.logical_size(),
        });
    }

    pub(crate) fn take_pending_configure_serial(&mut self) -> Option<u32> {
        self.pending_configure_serial.take()
    }

    pub(crate) fn granted(&self) -> Option<SurfaceSize> {
        self.granted
    }

    pub(crate) fn logical_size(&self) -> Option<SurfaceSize> {
        let granted = self.granted?;
        let width = if granted.width == 0 {
            self.requested.width
        } else {
            granted.width
        };
        let height = if granted.height == 0 {
            self.requested.height
        } else {
            granted.height
        };
        let size = SurfaceSize::new(width, height);
        (!size.is_empty()).then_some(size)
    }

    pub(crate) fn scale(&self) -> SurfaceScale {
        if self.fractional_enabled
            && let Some(scale) = self.fractional_scale_120
        {
            return SurfaceScale::from_120ths(scale).unwrap_or(SurfaceScale::ONE);
        }
        SurfaceScale::from_integer(self.integer_scale)
    }

    pub(crate) fn buffer_size(&self) -> Option<SurfaceSize> {
        scaled_buffer_size(self.logical_size()?, self.scale())
    }

    pub(crate) fn frame_signature(&self) -> FrameSignature {
        FrameSignature {
            logical_size: self.logical_size(),
            scale: self.scale(),
            buffer_size: self.buffer_size(),
        }
    }

    pub(crate) fn set_integer_scale(&mut self, scale: u32) {
        let old = self.scale();
        self.integer_scale = scale.max(1);
        self.push_scale_if_changed(old);
    }

    pub(crate) fn set_fractional_scale(&mut self, scale_120: u32) {
        if !self.fractional_enabled || scale_120 == 0 {
            return;
        }
        let old = self.scale();
        self.fractional_scale_120 = Some(scale_120);
        self.push_scale_if_changed(old);
    }

    fn push_scale_if_changed(&mut self, old: SurfaceScale) {
        let scale = self.scale();
        if old != scale && !self.is_closed() {
            self.events.push(LayerSurfaceEvent::ScaleChanged {
                scale,
                buffer_size: self.buffer_size(),
            });
        }
    }

    pub(crate) fn set_mapped(&mut self, mapped: bool) {
        if self.mapped == mapped || self.is_closed() {
            return;
        }
        self.mapped = mapped;
        self.events.push(if mapped {
            LayerSurfaceEvent::Mapped
        } else {
            LayerSurfaceEvent::Unmapped
        });
    }

    pub(crate) fn is_mapped(&self) -> bool {
        self.mapped
    }

    pub(crate) fn close(&mut self, reason: SurfaceCloseReason) {
        if self.close_reason.is_some() {
            return;
        }
        self.mapped = false;
        self.close_reason = Some(reason.clone());
        self.events.push(LayerSurfaceEvent::Closed { reason });
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.close_reason.is_some()
    }

    pub(crate) fn close_reason(&self) -> Option<&SurfaceCloseReason> {
        self.close_reason.as_ref()
    }

    pub(crate) fn push_pointer(&mut self, event: SurfacePointerEvent) {
        if !self.is_closed() {
            self.events.push(LayerSurfaceEvent::Pointer(event));
        }
    }

    pub(crate) fn take_events(&mut self) -> Vec<LayerSurfaceEvent> {
        std::mem::take(&mut self.events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_buffer_extents_round_outward() {
        let scale = SurfaceScale::from_120ths(150).unwrap();
        assert_eq!(
            scaled_buffer_size(SurfaceSize::new(101, 51), scale),
            Some(SurfaceSize::new(127, 64))
        );
        assert_eq!(scale.factor(), 1.25);
        assert!(scale.is_fractional());
        assert_eq!(SurfaceSize::new(127, 64).rgba8_stride(), Some(508));
        assert_eq!(SurfaceSize::new(127, 64).rgba8_byte_len(), Some(32_512));
        assert_eq!(SurfaceScale::from_120ths(0), None);
        assert_eq!(
            scaled_buffer_size(
                SurfaceSize::new(u32::MAX, u32::MAX),
                SurfaceScale::from_120ths(u32::MAX).unwrap()
            ),
            None
        );
        assert_eq!(SurfaceSize::new(u32::MAX, u32::MAX).rgba8_byte_len(), None);
    }

    #[test]
    fn buffer_layout_rejects_empty_overflowing_and_wrong_length_frames() {
        assert!(BufferLayout::new(SurfaceSize::new(0, 10)).is_err());
        assert!(BufferLayout::new(SurfaceSize::new(u32::MAX, 1)).is_err());
        assert!(BufferLayout::new(SurfaceSize::new(32_768, 32_768)).is_err());

        let layout = BufferLayout::new(SurfaceSize::new(2, 3)).unwrap();
        assert_eq!(layout.stride, 8);
        assert_eq!(layout.byte_len, 24);
        assert!(
            layout
                .validate_pixels(SurfaceSize::new(2, 3), 8, &[0; 23])
                .is_err()
        );
        assert!(
            layout
                .validate_pixels(SurfaceSize::new(2, 3), 8, &[0; 24])
                .is_ok()
        );
        assert!(
            layout
                .validate_pixels(SurfaceSize::new(2, 3), 7, &[0; 21])
                .is_err()
        );
        assert!(
            layout
                .validate_pixels(SurfaceSize::new(3, 2), 12, &[0; 24])
                .is_err()
        );
    }

    #[test]
    fn premultiplied_rgba_becomes_native_endian_argb8888() {
        let mut destination = [0; 8];
        write_argb8888(
            &[
                0x11, 0x22, 0x33, 0x44, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xee, 0xee, 0xee,
            ],
            12,
            SurfaceSize::new(2, 1),
            &mut destination,
        );
        assert_eq!(
            u32::from_ne_bytes(destination[0..4].try_into().unwrap()),
            0x4411_2233
        );
        assert_eq!(
            u32::from_ne_bytes(destination[4..8].try_into().unwrap()),
            0xddaa_bbcc
        );
    }

    #[test]
    fn buffer_selection_never_reuses_a_busy_slot() {
        let expected = BufferLayout::new(SurfaceSize::new(2, 2)).unwrap();
        let stale = BufferLayout::new(SurfaceSize::new(1, 1)).unwrap();
        assert_eq!(
            first_reusable_buffer(
                [(expected, true), (stale, false), (expected, false)],
                expected
            ),
            Some(2)
        );
        assert_eq!(
            first_reusable_buffer([(expected, true), (expected, true)], expected),
            None
        );
    }

    #[test]
    fn unmapping_requires_one_fresh_configure_before_remapping() {
        let mut state = RemapState::Ready;
        state.after_unmap();
        assert!(!state.is_ready());
        assert!(state.request_configure());
        assert!(!state.request_configure());
        assert!(!state.is_ready());
        state.configured();
        assert!(state.is_ready());
    }

    #[test]
    fn configure_resolves_zero_grants_against_requested_size() {
        let mut state = TransportState::new(SurfaceSize::new(232, 150), false);
        assert_eq!(state.logical_size(), None);
        state.configure(1, SurfaceSize::new(0, 0));
        assert_eq!(state.logical_size(), Some(SurfaceSize::new(232, 150)));
        assert_eq!(
            state.take_events(),
            vec![LayerSurfaceEvent::Configured {
                logical_size: Some(SurfaceSize::new(232, 150))
            }]
        );
    }

    #[test]
    fn configure_batch_acknowledges_only_the_latest_serial() {
        let mut state = TransportState::new(SurfaceSize::new(264, 720), false);
        state.configure(4, SurfaceSize::new(264, 720));
        state.configure(5, SurfaceSize::new(1280, 720));

        assert_eq!(state.granted(), Some(SurfaceSize::new(1280, 720)));
        assert_eq!(state.take_pending_configure_serial(), Some(5));
        assert_eq!(state.take_pending_configure_serial(), None);
    }

    #[test]
    fn fractional_preference_supersedes_integer_scale_and_resizes_buffer() {
        let mut state = TransportState::new(SurfaceSize::new(101, 51), true);
        state.configure(1, SurfaceSize::new(101, 51));
        state.take_events();

        state.set_integer_scale(2);
        assert_eq!(state.scale().units_120(), 240);
        state.take_events();

        state.set_fractional_scale(150);
        assert_eq!(state.scale().units_120(), 150);
        assert_eq!(state.buffer_size(), Some(SurfaceSize::new(127, 64)));
        assert_eq!(
            state.take_events(),
            vec![LayerSurfaceEvent::ScaleChanged {
                scale: SurfaceScale::from_120ths(150).unwrap(),
                buffer_size: Some(SurfaceSize::new(127, 64))
            }]
        );
    }

    #[test]
    fn a_frame_becomes_stale_when_scale_changes_during_rendering() {
        let mut state = TransportState::new(SurfaceSize::new(100, 50), true);
        state.configure(1, SurfaceSize::new(100, 50));
        let before = state.frame_signature();
        let submitted = state.buffer_size().unwrap();

        state.set_fractional_scale(150);

        assert!(submitted_frame_is_stale(
            before,
            state.frame_signature(),
            submitted
        ));
        assert!(!submitted_frame_is_stale(
            state.frame_signature(),
            state.frame_signature(),
            submitted
        ));
    }

    #[test]
    fn mapping_and_close_events_are_idempotent() {
        let mut state = TransportState::new(SurfaceSize::new(10, 10), false);
        state.set_mapped(true);
        state.set_mapped(true);
        state.set_mapped(false);
        state.close(SurfaceCloseReason::Compositor);
        state.close(SurfaceCloseReason::Compositor);
        state.configure(1, SurfaceSize::new(20, 20));
        state.set_integer_scale(2);
        assert_eq!(
            state.take_events(),
            vec![
                LayerSurfaceEvent::Mapped,
                LayerSurfaceEvent::Unmapped,
                LayerSurfaceEvent::Closed {
                    reason: SurfaceCloseReason::Compositor
                }
            ]
        );
    }
}
