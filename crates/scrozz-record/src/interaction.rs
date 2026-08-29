//! Privacy-preserving recording interaction events.
//!
//! Native listeners write only display-ready events into a bounded queue. Raw
//! key codes, focused-application identity, clipboard contents, and text from a
//! secure or indeterminate input context never cross this boundary.

use std::{
    collections::VecDeque,
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, TryLockError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use scrozz_core::{Error, LogicalPoint, LogicalRect, PhysicalPoint, PhysicalSize, Result};

use crate::{
    overlay::KeystrokeKind,
    settings::{ClickSettings, KeystrokeScope, KeystrokeSettings},
};

/// Lifetime-scoped native input listener used by the recording overlay source.
pub(crate) trait InputMonitor: Send {
    /// Aligns native event time to the first observed media timestamp.
    fn sync_media_time(&mut self, _elapsed: Duration) {}

    /// Drains already-filtered click and key events without blocking.
    fn drain(&mut self) -> Vec<InteractionEvent>;

    /// Current pointer position in global logical desktop coordinates.
    fn cursor_position(&self) -> Option<LogicalPoint>;

    /// Stops accepting input while media time is paused.
    fn pause(&mut self);

    /// Resumes input with pause-free timestamp normalization.
    fn resume(&mut self);

    /// Newly dropped callback events since the previous call.
    fn take_dropped(&mut self) -> u64;

    /// One actionable native-listener warning.
    fn take_warning(&mut self) -> Option<String> {
        None
    }
}

/// Largest producer-to-renderer queue accepted by the interaction pipeline.
pub const MAX_PENDING_INTERACTIONS: usize = 2_048;
/// Largest in-memory event archive retained for an immediate editor session.
pub const MAX_RETAINED_INTERACTIONS: usize = 262_144;
/// Largest UTF-8 display label retained for one key event.
pub const MAX_KEY_LABEL_BYTES: usize = 64;
const DUPLICATE_WINDOW: Duration = Duration::from_millis(3);
static PRIVATE_SOURCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Whether the focused input context is safe to describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSecurity {
    /// The platform positively identified a non-secure input context.
    NonSecure,
    /// The platform identified secure/password input.
    Secure,
    /// The platform could not determine whether the input is secure.
    Unknown,
}

/// Mouse button represented by a click highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    /// Primary button.
    Primary,
    /// Secondary button.
    Secondary,
    /// Middle button.
    Middle,
    /// Another mouse button.
    Other(u8),
}

/// Platform input source selected before a recording starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionBackend {
    /// A listen-only CoreGraphics event tap.
    MacOsListenOnlyEventTap,
    /// Recording-thread-owned `WH_MOUSE_LL` and `WH_KEYBOARD_LL` hooks.
    WindowsLowLevelHooks,
    /// XInput2 raw button/key events.
    X11Input2,
}

/// Runtime platform facts relevant to global interaction monitoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionPlatform {
    /// macOS.
    MacOs,
    /// Windows.
    Windows,
    /// Linux with a native X11 session.
    X11,
    /// Native Wayland or XWayland session.
    Wayland,
    /// No interactive desktop was detected.
    Headless,
}

/// Selects the only allowed native input backend.
///
/// `wayland_present` wins over any debugging/backend override. This prevents an
/// X11 override from silently recording only XWayland applications.
///
/// # Errors
///
/// Returns [`Error::Unsupported`] for Wayland and headless sessions.
pub fn select_interaction_backend(
    platform: InteractionPlatform,
    wayland_present: bool,
) -> Result<InteractionBackend> {
    if wayland_present || platform == InteractionPlatform::Wayland {
        return Err(Error::Unsupported {
            what: "click and keystroke capture on Wayland".to_owned(),
            why: "Wayland exposes no portable global input-monitoring protocol; Scrozz does not read /dev/input or misuse the RemoteDesktop injection portal"
                .to_owned(),
        });
    }
    match platform {
        InteractionPlatform::MacOs => Ok(InteractionBackend::MacOsListenOnlyEventTap),
        InteractionPlatform::Windows => Ok(InteractionBackend::WindowsLowLevelHooks),
        InteractionPlatform::X11 => Ok(InteractionBackend::X11Input2),
        InteractionPlatform::Wayland => unreachable!("handled above"),
        InteractionPlatform::Headless => Err(Error::Unsupported {
            what: "click and keystroke capture".to_owned(),
            why: "no interactive desktop session was detected".to_owned(),
        }),
    }
}

/// A fixed-capacity, zeroized display label.
///
/// The type intentionally has no public byte/string field and redacts `Debug`.
/// Platform adapters should construct it only after secure-input and scope
/// filtering. This is the only key-derived content retained by Scrozz.
pub struct SensitiveLabel {
    bytes: [u8; MAX_KEY_LABEL_BYTES],
    len: u8,
}

impl SensitiveLabel {
    /// Copies a printable, non-empty UTF-8 label into fixed storage.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for empty labels or control characters.
    pub fn new(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.is_empty() {
            return Err(Error::InvalidRequest(
                "a keystroke display label cannot be empty".to_owned(),
            ));
        }

        if value.chars().any(char::is_control) {
            return Err(Error::InvalidRequest(
                "a keystroke display label cannot contain control characters".to_owned(),
            ));
        }

        let mut end = value.len().min(MAX_KEY_LABEL_BYTES);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            return Err(Error::InvalidRequest(
                "a keystroke display label cannot be represented safely".to_owned(),
            ));
        }
        let mut bytes = [0; MAX_KEY_LABEL_BYTES];
        bytes[..end].copy_from_slice(&value.as_bytes()[..end]);
        Ok(Self {
            bytes,
            len: end as u8,
        })
    }

    pub(crate) fn from_parts(parts: &[&str]) -> Result<Self> {
        let mut bytes = [0; MAX_KEY_LABEL_BYTES];
        let mut len = 0;
        for part in parts {
            if part.chars().any(char::is_control) {
                return Err(Error::InvalidRequest(
                    "a keystroke display label cannot contain control characters".to_owned(),
                ));
            }
            for ch in part.chars() {
                let mut encoded = [0; 4];
                let encoded = ch.encode_utf8(&mut encoded).as_bytes();
                if len + encoded.len() > MAX_KEY_LABEL_BYTES {
                    break;
                }
                bytes[len..len + encoded.len()].copy_from_slice(encoded);
                len += encoded.len();
            }
        }
        if len == 0 {
            return Err(Error::InvalidRequest(
                "a keystroke display label cannot be empty".to_owned(),
            ));
        }
        Ok(Self {
            bytes,
            len: len as u8,
        })
    }

    pub(crate) fn from_utf16_parts(prefixes: &[&str], value: &[u16]) -> Result<Self> {
        let mut bytes = [0; MAX_KEY_LABEL_BYTES];
        let mut len = 0;
        for part in prefixes {
            for ch in part.chars() {
                push_label_char(&mut bytes, &mut len, ch);
            }
        }
        for ch in char::decode_utf16(value.iter().copied()) {
            push_label_char(
                &mut bytes,
                &mut len,
                ch.unwrap_or(char::REPLACEMENT_CHARACTER),
            );
        }

        if len == 0 {
            return Err(Error::InvalidRequest(
                "a keystroke display label cannot be empty".to_owned(),
            ));
        }
        Ok(Self {
            bytes,
            len: len as u8,
        })
    }

    /// The display-only label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)])
            .expect("SensitiveLabel is constructed from valid UTF-8")
    }

    /// UTF-8 byte length without exposing content.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether the label is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

fn push_label_char(bytes: &mut [u8; MAX_KEY_LABEL_BYTES], len: &mut usize, ch: char) {
    if ch.is_control() {
        return;
    }
    let mut encoded = [0; 4];
    let encoded = ch.encode_utf8(&mut encoded).as_bytes();
    if *len + encoded.len() <= MAX_KEY_LABEL_BYTES {
        bytes[*len..*len + encoded.len()].copy_from_slice(encoded);
        *len += encoded.len();
    }
}

impl Clone for SensitiveLabel {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes,
            len: self.len,
        }
    }
}

impl PartialEq for SensitiveLabel {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for SensitiveLabel {}

impl PartialEq<&str> for SensitiveLabel {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl fmt::Debug for SensitiveLabel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SensitiveLabel")
            .field("content", &"<redacted>")
            .field("utf8_bytes", &self.len())
            .finish()
    }
}

impl Drop for SensitiveLabel {
    fn drop(&mut self) {
        self.bytes.fill(0);
        self.len = 0;
    }
}

/// Display-ready key event accepted from a native listener.
#[derive(Clone, PartialEq, Eq)]
pub struct CapturedKeystroke {
    label: SensitiveLabel,
    /// Privacy-relevant key classification.
    pub kind: KeystrokeKind,
    /// Whether the platform marked this as an autorepeat.
    pub repeat: bool,
    contains_text: bool,
}

impl CapturedKeystroke {
    /// Creates a display-ready key event.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when the label is not safe to retain.
    pub fn new(label: &str, kind: KeystrokeKind, repeat: bool) -> Result<Self> {
        Self::with_text_content(label, kind, repeat, kind == KeystrokeKind::Text)
    }

    /// Creates a chord while declaring whether its label includes typed text.
    ///
    /// Platform adapters must set `contains_text` for combinations such as
    /// `Shift+A`; secure input then suppresses the whole chord before retention.
    pub fn with_text_content(
        label: &str,
        kind: KeystrokeKind,
        repeat: bool,
        contains_text: bool,
    ) -> Result<Self> {
        Ok(Self {
            label: SensitiveLabel::new(label)?,
            kind,
            repeat,
            contains_text,
        })
    }

    pub(crate) fn from_parts(
        parts: &[&str],
        kind: KeystrokeKind,
        repeat: bool,
        contains_text: bool,
    ) -> Result<Self> {
        Ok(Self {
            label: SensitiveLabel::from_parts(parts)?,
            kind,
            repeat,
            contains_text,
        })
    }

    pub(crate) fn from_utf16_parts(
        prefixes: &[&str],
        value: &[u16],
        kind: KeystrokeKind,
        repeat: bool,
    ) -> Result<Self> {
        Ok(Self {
            label: SensitiveLabel::from_utf16_parts(prefixes, value)?,
            kind,
            repeat,
            contains_text: true,
        })
    }

    /// Display label.
    #[must_use]
    pub fn label(&self) -> &str {
        self.label.as_str()
    }

    /// Whether the display label carries text typed by the user.
    #[must_use]
    pub const fn contains_text(&self) -> bool {
        self.contains_text
    }
}

impl fmt::Debug for CapturedKeystroke {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedKeystroke")
            .field("label", &self.label)
            .field("kind", &self.kind)
            .field("repeat", &self.repeat)
            .field("contains_text", &self.contains_text)
            .finish()
    }
}

/// One normalized event accepted by the recording process.
#[derive(Clone, PartialEq)]
pub enum InteractionEvent {
    /// A pointer press in global logical desktop coordinates.
    Click {
        /// Pause-free time from recording start.
        at: Duration,
        /// Global logical desktop position.
        position: LogicalPoint,
        /// Pressed button.
        button: PointerButton,
    },
    /// A display-ready key label.
    Keystroke {
        /// Pause-free time from recording start.
        at: Duration,
        /// Already-filtered display value.
        key: CapturedKeystroke,
    },
}

impl InteractionEvent {
    /// Event timestamp.
    #[must_use]
    pub const fn at(&self) -> Duration {
        match self {
            Self::Click { at, .. } | Self::Keystroke { at, .. } => *at,
        }
    }

    fn set_at(&mut self, normalized: Duration) {
        match self {
            Self::Click { at, .. } | Self::Keystroke { at, .. } => *at = normalized,
        }
    }
}

impl fmt::Debug for InteractionEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Click {
                at,
                position,
                button,
            } => formatter
                .debug_struct("Click")
                .field("at", at)
                .field("position", position)
                .field("button", button)
                .finish(),
            Self::Keystroke { at, key } => formatter
                .debug_struct("Keystroke")
                .field("at", at)
                .field("key", key)
                .finish(),
        }
    }
}

#[derive(Debug)]
struct QueueState {
    events: VecDeque<InteractionEvent>,
    last: Option<InteractionEvent>,
}

#[derive(Debug)]
struct SharedQueue {
    state: Mutex<QueueState>,
    dropped: AtomicU64,
    capacity: usize,
    scope: KeystrokeScope,
}

/// Non-blocking producer retained by native input callbacks.
#[derive(Debug, Clone)]
pub struct InteractionProducer {
    shared: Arc<SharedQueue>,
}

/// Consumer owned by the frame renderer.
#[derive(Debug)]
pub struct InteractionConsumer {
    shared: Arc<SharedQueue>,
    reported_dropped: u64,
}

/// Creates one bounded producer/consumer pair.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] for a zero or excessive capacity.
pub fn interaction_channel(
    capacity: usize,
    scope: KeystrokeScope,
) -> Result<(InteractionProducer, InteractionConsumer)> {
    if capacity == 0 || capacity > MAX_PENDING_INTERACTIONS {
        return Err(Error::InvalidRequest(format!(
            "interaction queue capacity must be in 1..={MAX_PENDING_INTERACTIONS}"
        )));
    }
    let shared = Arc::new(SharedQueue {
        state: Mutex::new(QueueState {
            events: VecDeque::with_capacity(capacity),
            last: None,
        }),
        dropped: AtomicU64::new(0),
        capacity,
        scope,
    });
    Ok((
        InteractionProducer {
            shared: Arc::clone(&shared),
        },
        InteractionConsumer {
            shared,
            reported_dropped: 0,
        },
    ))
}

impl InteractionProducer {
    /// Tries to queue a click without blocking a native callback.
    #[must_use]
    pub fn push_click(&self, at: Duration, position: LogicalPoint, button: PointerButton) -> bool {
        if !position.x.is_finite() || !position.y.is_finite() {
            self.drop_one();
            return false;
        }
        self.push(InteractionEvent::Click {
            at,
            position,
            button,
        })
    }

    /// Tries to queue a key after applying privacy and control-shortcut filters.
    ///
    /// Text is accepted only when the platform positively identified a
    /// non-secure context. Unknown is intentionally treated as secure.
    #[must_use]
    pub fn push_key(
        &self,
        at: Duration,
        key: CapturedKeystroke,
        security: InputSecurity,
        scrozz_control_shortcut: bool,
    ) -> bool {
        if scrozz_control_shortcut
            || (key.contains_text() && security != InputSecurity::NonSecure)
            || (self.shared.scope == KeystrokeScope::ModifiersOnly
                && key.kind == KeystrokeKind::Text)
        {
            return false;
        }
        self.push(InteractionEvent::Keystroke { at, key })
    }

    fn push(&self, mut event: InteractionEvent) -> bool {
        let mut state = match self.shared.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                self.drop_one();
                return false;
            }
        };
        if let Some(previous) = state.last.as_ref()
            && event.at() < previous.at()
        {
            event.set_at(previous.at());
        }
        if state
            .last
            .as_ref()
            .is_some_and(|previous| duplicate(previous, &event))
        {
            return false;
        }
        if state.events.len() == self.shared.capacity {
            self.drop_one();
            return false;
        }
        state.last = Some(event.clone());
        state.events.push_back(event);
        true
    }

    fn drop_one(&self) {
        self.shared.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

impl InteractionConsumer {
    /// Drains every queued event in timestamp order.
    pub fn drain(&mut self) -> Vec<InteractionEvent> {
        let mut state = lock(&self.shared.state);
        state.events.drain(..).collect()
    }

    /// Newly dropped events since the previous call.
    pub fn take_dropped(&mut self) -> u64 {
        let total = self.shared.dropped.load(Ordering::Relaxed);
        let new = total.saturating_sub(self.reported_dropped);
        self.reported_dropped = total;
        new
    }
}

fn duplicate(previous: &InteractionEvent, incoming: &InteractionEvent) -> bool {
    let close = incoming.at().saturating_sub(previous.at()) <= DUPLICATE_WINDOW;
    close
        && match (previous, incoming) {
            (
                InteractionEvent::Click {
                    position: left,
                    button: left_button,
                    ..
                },
                InteractionEvent::Click {
                    position: right,
                    button: right_button,
                    ..
                },
            ) => left == right && left_button == right_button,
            (
                InteractionEvent::Keystroke { key: left, .. },
                InteractionEvent::Keystroke { key: right, .. },
            ) => left == right && !right.repeat,
            _ => false,
        }
}

/// A point normalized to a captured source rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalizedPoint {
    /// Horizontal fraction in `0.0..=1.0`.
    pub x: f64,
    /// Vertical fraction in `0.0..=1.0`.
    pub y: f64,
}

impl NormalizedPoint {
    fn to_physical(self, canvas: PhysicalSize) -> PhysicalPoint {
        let max_x = (canvas.width - 1.0).max(0.0);
        let max_y = (canvas.height - 1.0).max(0.0);
        PhysicalPoint::new(
            (self.x * canvas.width).clamp(0.0, max_x),
            (self.y * canvas.height).clamp(0.0, max_y),
        )
    }
}

/// Maps global logical input coordinates into a recording source.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InteractionMapper {
    source: LogicalRect,
}

impl InteractionMapper {
    /// Creates a mapper for one finite non-empty source rectangle.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for malformed source geometry.
    pub fn new(source: LogicalRect) -> Result<Self> {
        let values = [
            source.origin.x,
            source.origin.y,
            source.size.width,
            source.size.height,
        ];
        if values.iter().any(|value| !value.is_finite()) || source.is_empty() {
            return Err(Error::InvalidRequest(
                "interaction source bounds must be finite and non-empty".to_owned(),
            ));
        }
        Ok(Self { source })
    }

    /// Maps a global point, returning `None` while the pointer is outside.
    #[must_use]
    pub fn normalize(self, point: LogicalPoint) -> Option<NormalizedPoint> {
        if !point.x.is_finite() || !point.y.is_finite() {
            return None;
        }
        let x = (point.x - self.source.origin.x) / self.source.size.width;
        let y = (point.y - self.source.origin.y) / self.source.size.height;
        if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
            return None;
        }
        Some(NormalizedPoint { x, y })
    }
}

#[derive(Clone, PartialEq)]
pub(crate) enum RetainedInteraction {
    Click {
        at: Duration,
        position: NormalizedPoint,
        button: PointerButton,
    },
    Keystroke {
        at: Duration,
        key: CapturedKeystroke,
    },
    Cursor {
        at: Duration,
        position: NormalizedPoint,
    },
}

impl RetainedInteraction {
    pub(crate) const fn at(&self) -> Duration {
        match self {
            Self::Click { at, .. } | Self::Keystroke { at, .. } | Self::Cursor { at, .. } => *at,
        }
    }

    fn set_at(&mut self, normalized: Duration) {
        match self {
            Self::Click { at, .. } | Self::Keystroke { at, .. } | Self::Cursor { at, .. } => {
                *at = normalized;
            }
        }
    }
}

impl fmt::Debug for RetainedInteraction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Click {
                at,
                position,
                button,
            } => formatter
                .debug_struct("Click")
                .field("at", at)
                .field("position", position)
                .field("button", button)
                .finish(),
            Self::Keystroke { at, key } => formatter
                .debug_struct("Keystroke")
                .field("at", at)
                .field("key", key)
                .finish(),
            Self::Cursor { at, position } => formatter
                .debug_struct("Cursor")
                .field("at", at)
                .field("position", position)
                .finish(),
        }
    }
}

/// Editor-visible interaction toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InteractionEdits {
    /// Render the captured pointer.
    pub cursor: bool,
    /// Apply deterministic bounded pointer smoothing.
    pub smooth_cursor: bool,
    /// Render click highlights.
    pub clicks: bool,
    /// Render keystroke chips.
    pub keystrokes: bool,
}

/// In-memory, non-serialized interaction stream retained for immediate editing.
#[derive(Clone)]
pub struct InteractionRecording {
    pub(crate) events: Vec<RetainedInteraction>,
    /// Visual click settings captured with the run.
    pub clicks: ClickSettings,
    /// Keystroke display settings captured with the run.
    pub keystrokes: KeystrokeSettings,
    /// Whether the original output displayed a cursor.
    pub cursor_visible: bool,
    /// Whether the original output smoothed cursor motion.
    pub cursor_smoothing: bool,
    truncated: bool,
    click_count: usize,
    keystroke_count: usize,
    cursor_sample_count: usize,
    reference_size: Option<PhysicalSize>,
    source: Option<PrivateRecordingSource>,
}

/// Content-free counts suitable for diagnostics and smoke-test assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractionSummary {
    /// Accepted click events.
    pub clicks: usize,
    /// Accepted display-ready keystrokes.
    pub keystrokes: usize,
    /// Retained raw cursor samples.
    pub cursor_samples: usize,
    /// Whether the archive hit its hard memory ceiling.
    pub truncated: bool,
    /// Whether an untouched source is available for non-destructive editing.
    pub editable: bool,
}

impl InteractionRecording {
    /// Empty recording stream with capture-time display settings.
    #[must_use]
    pub fn new(
        clicks: ClickSettings,
        keystrokes: KeystrokeSettings,
        cursor_visible: bool,
        cursor_smoothing: bool,
    ) -> Self {
        Self {
            events: Vec::new(),
            clicks,
            keystrokes,
            cursor_visible,
            cursor_smoothing,
            truncated: false,
            click_count: 0,
            keystroke_count: 0,
            cursor_sample_count: 0,
            reference_size: None,
            source: None,
        }
    }

    pub(crate) fn push(&mut self, mut event: RetainedInteraction) -> bool {
        if self.events.len() == MAX_RETAINED_INTERACTIONS {
            self.truncated = true;
            return false;
        }
        if let Some(previous) = self.events.last()
            && event.at() < previous.at()
        {
            event.set_at(previous.at());
        }
        if let RetainedInteraction::Cursor { position, .. } = event
            && self.events.last().is_some_and(
                |previous| matches!(previous, RetainedInteraction::Cursor { position: prior, .. } if *prior == position),
            )
        {
            return true;
        }
        match &event {
            RetainedInteraction::Click { .. } => {
                self.click_count = self.click_count.saturating_add(1);
            }
            RetainedInteraction::Keystroke { .. } => {
                self.keystroke_count = self.keystroke_count.saturating_add(1);
            }
            RetainedInteraction::Cursor { .. } => {
                self.cursor_sample_count = self.cursor_sample_count.saturating_add(1);
            }
        }
        self.events.push(event);
        true
    }

    /// Number of retained, already-filtered events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether no interaction events were retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Whether the memory ceiling discarded later events.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Capture-time custom-layer choices used for the initial rendered recording.
    ///
    /// An unsmoothed native cursor is already present in the raw source and is
    /// therefore not represented as an editable custom layer.
    #[must_use]
    pub const fn default_edits(&self) -> InteractionEdits {
        InteractionEdits {
            cursor: self.cursor_visible && self.cursor_smoothing,
            smooth_cursor: self.cursor_smoothing,
            clicks: self.clicks.enabled,
            keystrokes: self.keystrokes.enabled,
        }
    }

    /// Whether an untouched source file is retained for non-destructive edits.
    #[must_use]
    pub fn is_editable(&self) -> bool {
        self.source.is_some()
    }

    /// Returns counts without exposing key labels or file paths.
    #[must_use]
    pub fn summary(&self) -> InteractionSummary {
        InteractionSummary {
            clicks: self.click_count,
            keystrokes: self.keystroke_count,
            cursor_samples: self.cursor_sample_count,
            truncated: self.truncated,
            editable: self.is_editable(),
        }
    }

    pub(crate) fn source_path(&self) -> Option<&Path> {
        self.source.as_ref().map(PrivateRecordingSource::path)
    }

    pub(crate) fn attach_source(&mut self, source: PrivateRecordingSource) {
        self.source = Some(source);
    }

    pub(crate) fn set_reference_size(&mut self, size: PhysicalSize) {
        if self.reference_size.is_none()
            && size.width.is_finite()
            && size.height.is_finite()
            && size.width > 0.0
            && size.height > 0.0
        {
            self.reference_size = Some(size);
        }
    }

    pub(crate) fn visual_scale(&self, canvas: PhysicalSize) -> f32 {
        let scale = self.reference_size.map_or_else(
            || canvas.height / 1_080.0,
            |reference| (canvas.width / reference.width).min(canvas.height / reference.height),
        );
        scale.clamp(0.2, 8.0) as f32
    }

    pub(crate) fn cursor_at(
        &self,
        at: Duration,
        canvas: PhysicalSize,
        smooth: bool,
    ) -> Option<PhysicalPoint> {
        let end = self.events.partition_point(|event| event.at() <= at);
        if !smooth {
            return self.events[..end]
                .iter()
                .rev()
                .find_map(|event| match event {
                    RetainedInteraction::Cursor { position, .. } => {
                        Some(position.to_physical(canvas))
                    }
                    _ => None,
                });
        }
        let window_start = at.saturating_sub(Duration::from_millis(90));
        let start = self.events[..end].partition_point(|event| event.at() < window_start);
        let samples: Vec<(Duration, NormalizedPoint)> = self.events[start..end]
            .iter()
            .filter_map(|event| match event {
                RetainedInteraction::Cursor { at, position } => Some((*at, *position)),
                _ => None,
            })
            .collect();
        cursor_sample(&samples, at, smooth).map(|point| point.to_physical(canvas))
    }

    pub(crate) fn clicks_at(
        &self,
        since: Duration,
        at: Duration,
        canvas: PhysicalSize,
    ) -> Vec<(Duration, PhysicalPoint, PointerButton)> {
        let end = self.events.partition_point(|event| event.at() <= at);
        let start = self.events[..end].partition_point(|event| event.at() < since);
        self.events[start..end]
            .iter()
            .filter_map(move |event| match event {
                RetainedInteraction::Click {
                    at: event_at,
                    position,
                    button,
                } => Some((*event_at, position.to_physical(canvas), *button)),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn keys_at(
        &self,
        since: Duration,
        at: Duration,
    ) -> Vec<(Duration, &CapturedKeystroke)> {
        let end = self.events.partition_point(|event| event.at() <= at);
        let start = self.events[..end].partition_point(|event| event.at() < since);
        self.events[start..end]
            .iter()
            .filter_map(move |event| match event {
                RetainedInteraction::Keystroke { at: event_at, key } => Some((*event_at, key)),
                _ => None,
            })
            .collect()
    }
}

impl PartialEq for InteractionRecording {
    fn eq(&self, other: &Self) -> bool {
        self.events == other.events
            && self.clicks == other.clicks
            && self.keystrokes == other.keystrokes
            && self.cursor_visible == other.cursor_visible
            && self.cursor_smoothing == other.cursor_smoothing
            && self.truncated == other.truncated
            && self.click_count == other.click_count
            && self.keystroke_count == other.keystroke_count
            && self.cursor_sample_count == other.cursor_sample_count
            && self.reference_size == other.reference_size
            && self.source.as_ref().map(PrivateRecordingSource::path)
                == other.source.as_ref().map(PrivateRecordingSource::path)
    }
}

impl fmt::Debug for InteractionRecording {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let summary = self.summary();
        formatter
            .debug_struct("InteractionRecording")
            .field("clicks", &summary.clicks)
            .field("keystrokes", &summary.keystrokes)
            .field("cursor_samples", &summary.cursor_samples)
            .field("truncated", &summary.truncated)
            .field("editable_source", &summary.editable)
            .finish()
    }
}

/// Private raw recording retained only while an editor-capable value is alive.
#[derive(Clone)]
pub(crate) struct PrivateRecordingSource {
    inner: Arc<PrivateRecordingSourceInner>,
}

struct PrivateRecordingSourceInner {
    directory: PathBuf,
    path: PathBuf,
}

impl PrivateRecordingSource {
    pub(crate) fn create() -> Result<Self> {
        for _ in 0..100 {
            let sequence = PRIVATE_SOURCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                ".scrozz-interactions-{}-{sequence}",
                std::process::id()
            ));
            match create_private_directory(&directory) {
                Ok(()) => {
                    let path = directory.join("source.mp4");
                    return Ok(Self {
                        inner: Arc::new(PrivateRecordingSourceInner { directory, path }),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(Error::Io(error)),
            }
        }
        Err(Error::Storage(
            "could not allocate a private interaction source directory".to_owned(),
        ))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.inner.path
    }
}

impl fmt::Debug for PrivateRecordingSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateRecordingSource")
            .field("retained", &self.inner.path.exists())
            .finish()
    }
}

impl Drop for PrivateRecordingSourceInner {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

fn cursor_sample(
    samples: &[(Duration, NormalizedPoint)],
    at: Duration,
    smooth: bool,
) -> Option<NormalizedPoint> {
    let end = samples.partition_point(|(sample_at, _)| *sample_at <= at);
    let latest = samples.get(end.checked_sub(1)?)?.1;
    if !smooth {
        return Some(latest);
    }

    let window_start = at.saturating_sub(Duration::from_millis(90));
    let start = samples.partition_point(|(sample_at, _)| *sample_at < window_start);
    let window = &samples[start..end];
    if window.len() < 2 {
        return Some(latest);
    }
    let mut total = 0.0;
    let mut x = 0.0;
    let mut y = 0.0;
    for (sample_at, position) in window {
        let age = at.saturating_sub(*sample_at).as_secs_f64();
        let weight = (1.0 - age / 0.09).clamp(0.05, 1.0);
        total += weight;
        x += position.x * weight;
        y += position.y * weight;
    }
    Some(NormalizedPoint {
        x: (x / total).clamp(0.0, 1.0),
        y: (y / total).clamp(0.0, 1.0),
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(label: &str, kind: KeystrokeKind) -> CapturedKeystroke {
        CapturedKeystroke::new(label, kind, false).unwrap()
    }

    #[test]
    fn debug_never_reveals_key_content() {
        let key = key("correct horse battery staple", KeystrokeKind::Text);
        let rendered = format!("{key:?}");
        assert!(!rendered.contains("correct"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn secure_and_unknown_text_fail_closed_before_queueing() {
        let (producer, mut consumer) = interaction_channel(8, KeystrokeScope::All).unwrap();
        assert!(!producer.push_key(
            Duration::ZERO,
            key("s", KeystrokeKind::Text),
            InputSecurity::Secure,
            false,
        ));
        assert!(!producer.push_key(
            Duration::ZERO,
            key("u", KeystrokeKind::Text),
            InputSecurity::Unknown,
            false,
        ));
        assert!(consumer.drain().is_empty());
    }

    #[test]
    fn modifiers_are_safe_while_text_and_control_shortcuts_are_filtered() {
        let (producer, mut consumer) =
            interaction_channel(8, KeystrokeScope::ModifiersOnly).unwrap();
        assert!(!producer.push_key(
            Duration::ZERO,
            key("A", KeystrokeKind::Text),
            InputSecurity::NonSecure,
            false,
        ));
        assert!(!producer.push_key(
            Duration::ZERO,
            key("⌘⇧R", KeystrokeKind::Modifier),
            InputSecurity::Unknown,
            true,
        ));
        assert!(producer.push_key(
            Duration::ZERO,
            key("⌘K", KeystrokeKind::Modifier),
            InputSecurity::Unknown,
            false,
        ));
        assert_eq!(consumer.drain().len(), 1);
    }

    #[test]
    fn local_and_global_duplicates_collapse_and_queue_overflow_is_counted() {
        let (producer, mut consumer) = interaction_channel(1, KeystrokeScope::All).unwrap();
        let click = || {
            producer.push_click(
                Duration::from_millis(10),
                LogicalPoint::new(4.0, 5.0),
                PointerButton::Primary,
            )
        };
        assert!(click());
        assert!(!click());
        assert!(!producer.push_click(
            Duration::from_millis(20),
            LogicalPoint::new(8.0, 9.0),
            PointerButton::Primary,
        ));
        assert_eq!(consumer.take_dropped(), 1);
        assert_eq!(consumer.drain().len(), 1);
        assert_eq!(consumer.take_dropped(), 0);
    }

    #[test]
    fn timestamps_from_multiple_monitors_are_normalized_monotonically() {
        let (producer, mut consumer) =
            interaction_channel(4, KeystrokeScope::ModifiersOnly).unwrap();
        assert!(producer.push_click(
            Duration::from_millis(20),
            LogicalPoint::new(1.0, 1.0),
            PointerButton::Primary,
        ));
        assert!(producer.push_click(
            Duration::from_millis(10),
            LogicalPoint::new(2.0, 2.0),
            PointerButton::Primary,
        ));
        let events = consumer.drain();
        assert_eq!(events[0].at(), Duration::from_millis(20));
        assert_eq!(events[1].at(), Duration::from_millis(20));
    }

    #[test]
    fn mixed_dpi_and_zoom_are_resolved_by_source_to_output_mapping() {
        let mapper = InteractionMapper::new(LogicalRect::new(
            LogicalPoint::new(-1_920.0, 0.0),
            scrozz_core::LogicalSize::new(3_840.0, 1_080.0),
        ))
        .unwrap();
        let normalized = mapper
            .normalize(LogicalPoint::new(0.0, 540.0))
            .expect("point is inside the composite desktop");
        let physical = normalized.to_physical(PhysicalSize::new(2_560.0, 720.0));
        assert_eq!(physical, PhysicalPoint::new(1_280.0, 360.0));
        assert!(mapper.normalize(LogicalPoint::new(2_000.0, 10.0)).is_none());
    }

    #[test]
    fn smoothing_is_deterministic_bounded_and_preserves_raw_samples() {
        let samples = [
            (Duration::from_millis(0), NormalizedPoint { x: 0.1, y: 0.2 }),
            (
                Duration::from_millis(40),
                NormalizedPoint { x: 0.8, y: 0.7 },
            ),
            (
                Duration::from_millis(80),
                NormalizedPoint { x: 0.4, y: 0.3 },
            ),
        ];
        assert_eq!(
            cursor_sample(&samples, Duration::from_millis(80), false),
            Some(samples[2].1)
        );
        let smoothed = cursor_sample(&samples, Duration::from_millis(80), true).unwrap();
        assert!((0.1..=0.8).contains(&smoothed.x));
        assert!((0.2..=0.7).contains(&smoothed.y));
        assert_eq!(
            cursor_sample(&samples, Duration::from_millis(80), true),
            Some(smoothed)
        );
        assert_eq!(samples[2].1, NormalizedPoint { x: 0.4, y: 0.3 });
    }

    #[test]
    fn retained_archive_has_a_hard_ceiling() {
        let mut archive = InteractionRecording::new(
            ClickSettings::default(),
            KeystrokeSettings::default(),
            true,
            false,
        );
        archive.events.reserve(MAX_RETAINED_INTERACTIONS);
        for index in 0..=MAX_RETAINED_INTERACTIONS {
            archive.push(RetainedInteraction::Cursor {
                at: Duration::from_nanos(index as u64),
                position: NormalizedPoint {
                    x: (index % 2) as f64,
                    y: 0.5,
                },
            });
        }
        assert_eq!(archive.len(), MAX_RETAINED_INTERACTIONS);
        assert!(archive.is_truncated());
    }

    #[test]
    fn platform_contract_names_native_hooks_and_wayland_fails_closed() {
        assert_eq!(
            select_interaction_backend(InteractionPlatform::Windows, false).unwrap(),
            InteractionBackend::WindowsLowLevelHooks
        );
        assert_eq!(
            select_interaction_backend(InteractionPlatform::X11, false).unwrap(),
            InteractionBackend::X11Input2
        );
        let forced_x11_inside_wayland =
            select_interaction_backend(InteractionPlatform::X11, true).unwrap_err();
        assert!(forced_x11_inside_wayland.to_string().contains("Wayland"));
    }

    #[test]
    fn private_edit_source_is_removed_with_the_last_owner() {
        let source = PrivateRecordingSource::create().unwrap();
        let path = source.path().to_path_buf();
        let directory = path.parent().unwrap().to_path_buf();
        std::fs::write(&path, b"private source").unwrap();
        let second_owner = source.clone();
        drop(source);
        assert!(path.exists());
        drop(second_owner);
        assert!(!path.exists());
        assert!(!directory.exists());
    }

    #[test]
    fn native_cursor_is_not_double_rendered_without_smoothing() {
        let recording = InteractionRecording::new(
            ClickSettings::default(),
            KeystrokeSettings::default(),
            true,
            false,
        );
        assert!(!recording.default_edits().cursor);
        let smoothed = InteractionRecording::new(
            ClickSettings::default(),
            KeystrokeSettings::default(),
            true,
            true,
        );
        assert!(smoothed.default_edits().cursor);
    }
}
