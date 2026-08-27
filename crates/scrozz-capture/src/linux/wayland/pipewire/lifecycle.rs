//! The state machine for taking exactly one frame off a PipeWire stream.
//!
//! # Why this is a separate, pure module
//!
//! The interesting part of a still capture is not the FFI — it is knowing *when
//! you have a frame*. PipeWire will happily hand a client buffers that are
//! empty, hand them before the format is agreed, drop the stream back to
//! `Paused` mid-flight, or report an error through a state change rather than a
//! return code. Getting that wrong produces the two worst outcomes a screenshot
//! tool has: a hang, or a black image.
//!
//! All of that is decidable from a sequence of events, with no library involved,
//! so it lives here and is tested on every platform. The unsafe code in
//! [`super::stream`] does nothing but translate callbacks into [`Event`]s and
//! obey the returned [`Action`].
//!
//! # The empty-buffer problem, specifically
//!
//! Mutter's screen-cast source emits buffers whose `chunk->size` is zero when
//! nothing on screen has changed. They are real buffers and `process` really
//! fires for them, but they do not describe a complete frame, so
//! [`Event::EmptyBuffer`] keeps waiting. That is distinct from
//! `SPA_CHUNK_FLAG_EMPTY`: SPA defines the flag as valid media-neutral content,
//! which means black for video and is synthesized as such by the stream reader.

use scrozz_core::Error;

use super::format::Negotiated;

/// Monotonic media observations and the last complete frame a caller received.
///
/// A reusable stream keeps producing while no `capture_frame` call is active.
/// Sequencing that continuous timeline lets the next call accept a complete
/// post-scroll frame already buffered during settle time instead of flushing it.
#[derive(Debug, Default)]
pub struct FrameTimeline {
    next: u64,
    delivered: u64,
}

impl FrameTimeline {
    /// Assigns the next observation sequence.
    pub fn publish(&mut self) -> u64 {
        self.next = self.next.wrapping_add(1);
        if self.next == 0 {
            self.next = 1;
        }
        self.next
    }

    /// The call boundary against which later no-damage events are judged.
    #[must_use]
    pub const fn boundary(&self) -> u64 {
        self.delivered
    }

    /// Whether a complete buffered frame has not yet been delivered.
    #[must_use]
    pub const fn is_fresh(&self, sequence: u64) -> bool {
        Self::is_after(sequence, self.delivered)
    }

    /// Records delivery without allowing an older callback to move time back.
    pub fn mark_delivered(&mut self, sequence: u64) {
        if Self::is_after(sequence, self.delivered) {
            self.delivered = sequence;
        }
    }

    /// Whether an observation occurred after a prior sequence, including wrap.
    #[must_use]
    pub const fn is_after(sequence: u64, boundary: u64) -> bool {
        let distance = sequence.wrapping_sub(boundary);
        distance != 0 && distance < (1_u64 << 63)
    }
}

/// Meaning of one SPA chunk before any pointer arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkDisposition {
    /// The producer marked the bytes unusable.
    Corrupted,
    /// No bytes were supplied yet; keep waiting for a complete frame.
    Priming,
    /// SPA explicitly represents media-neutral video, which is black.
    Neutral,
    /// Read and pack the supplied bytes.
    Pixels,
}

/// Distinguishes zero-byte priming from SPA's valid neutral-black video.
#[must_use]
pub const fn classify_chunk(
    size: u32,
    is_corrupted: bool,
    is_media_neutral: bool,
) -> ChunkDisposition {
    if is_corrupted {
        ChunkDisposition::Corrupted
    } else if is_media_neutral {
        ChunkDisposition::Neutral
    } else if size == 0 {
        ChunkDisposition::Priming
    } else {
        ChunkDisposition::Pixels
    }
}

/// `enum pw_stream_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// `PW_STREAM_STATE_ERROR`.
    Error,
    /// `PW_STREAM_STATE_UNCONNECTED`.
    Unconnected,
    /// `PW_STREAM_STATE_CONNECTING`.
    Connecting,
    /// `PW_STREAM_STATE_PAUSED`.
    Paused,
    /// `PW_STREAM_STATE_STREAMING`.
    Streaming,
}

impl StreamState {
    /// Maps the raw `enum pw_stream_state` value.
    ///
    /// Unknown values are treated as [`StreamState::Error`]: a future PipeWire
    /// that adds a state is a situation to fail loudly in, not to wait forever
    /// in.
    #[must_use]
    pub const fn from_raw(value: i32) -> Self {
        match value {
            0 => Self::Unconnected,
            1 => Self::Connecting,
            2 => Self::Paused,
            3 => Self::Streaming,
            _ => Self::Error,
        }
    }

    /// Whether this state permanently settles a stream.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Error | Self::Unconnected)
    }
}

/// Something the stream told us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// `state_changed`, with the message PipeWire attached to an error state.
    StateChanged(StreamState, Option<String>),
    /// `param_changed` delivered a usable `Format`.
    FormatAgreed(Negotiated),
    /// `param_changed` delivered a `Format` this client cannot use.
    FormatRejected(String),
    /// `process` produced a complete frame at this media sequence.
    FrameReady(u64),
    /// A post-format zero-byte buffer says the previous complete frame is still
    /// current; only a request that predates this sequence may reuse it.
    NoDamage(u64),
    /// `process` produced a priming buffer with no reusable frame; keep waiting.
    EmptyBuffer,
    /// `process` produced a buffer that could not be read at all.
    BufferRejected(String),
    /// The wait ran out before a frame arrived.
    TimedOut,
}

/// Keeps the most significant result while one `process` callback drains every
/// queued buffer.
///
/// A malformed buffer is structural and outranks everything, a usable frame
/// outranks an empty priming buffer, and emptiness is retained only when no
/// better result arrived. Equal-priority media observations keep the newest
/// sequence, while equal-priority failures preserve the first diagnosis.
#[must_use]
pub fn prefer_process_event(current: Event, next: Event) -> Event {
    debug_assert!(is_process_event(&current));
    debug_assert!(is_process_event(&next));

    match process_priority(&next).cmp(&process_priority(&current)) {
        std::cmp::Ordering::Greater => next,
        std::cmp::Ordering::Less => current,
        std::cmp::Ordering::Equal => match (event_sequence(&current), event_sequence(&next)) {
            (Some(current_sequence), Some(next_sequence))
                if FrameTimeline::is_after(next_sequence, current_sequence) =>
            {
                next
            }
            _ => current,
        },
    }
}

/// Coalesces state callbacks without allowing a terminal state to disappear.
///
/// A reusable stream can sit idle long enough to queue several transitions.
/// Once `Error` or `Unconnected` arrives, a later nonterminal callback must not
/// turn a dead source back into an apparently live one. The first terminal
/// diagnosis is retained because it is closest to the cause.
#[must_use]
pub fn prefer_state_event(current: Event, next: Event) -> Event {
    debug_assert!(matches!(current, Event::StateChanged(_, _)));
    debug_assert!(matches!(next, Event::StateChanged(_, _)));

    match &current {
        Event::StateChanged(state, _) if state.is_terminal() => current,
        _ => next,
    }
}

const fn is_process_event(event: &Event) -> bool {
    matches!(
        event,
        Event::FrameReady(_) | Event::NoDamage(_) | Event::EmptyBuffer | Event::BufferRejected(_)
    )
}

const fn process_priority(event: &Event) -> u8 {
    match event {
        Event::BufferRejected(_) => 4,
        Event::FrameReady(_) | Event::NoDamage(_) => 3,
        Event::EmptyBuffer => 1,
        _ => 0,
    }
}

const fn event_sequence(event: &Event) -> Option<u64> {
    match event {
        Event::FrameReady(sequence) | Event::NoDamage(sequence) => Some(*sequence),
        _ => None,
    }
}

/// What the driving loop should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Keep the loop running.
    Wait,
    /// Stop: the outcome, good or bad, is now settled.
    Stop,
}

/// Where the capture has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Connected, no format agreed yet.
    Connecting,
    /// A format is agreed; waiting for pixels.
    Streaming,
    /// A frame was captured.
    Captured,
    /// The capture failed, and this is the error to report.
    Failed(Failure),
}

/// A settled failure, kept as data so the mapping to [`Error`] is testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The node went away — the window closed, or the monitor was unplugged.
    Gone(String),
    /// PipeWire reported an error state.
    Stream(String),
    /// The format could not be used.
    Format(String),
    /// A buffer could not be read.
    Buffer(String),
    /// No frame arrived in time.
    Timeout(u32),
}

impl Failure {
    /// The error a caller should see.
    #[must_use]
    pub fn into_error(self) -> Error {
        match self {
            Self::Gone(what) => Error::TargetGone(what),
            Self::Stream(why) | Self::Format(why) | Self::Buffer(why) => Error::Platform(why),
            Self::Timeout(seconds) => Error::Platform(format!(
                "the PipeWire stream produced no frame within {seconds}s. The portal granted the \
                 capture, so this is the compositor's screen-cast source failing to render — on \
                 wlroots this is usually a missing `xdg-desktop-portal-wlr` configuration, and on \
                 GNOME it is usually a stalled `gnome-shell` screen-cast session"
            )),
        }
    }
}

/// Tracks one still capture from `pw_stream_connect` to a frame or a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lifecycle {
    phase: Phase,
    format: Option<Negotiated>,
    captured_sequence: Option<u64>,
    /// Whether the stream ever reached `Streaming`, used to tell "the node was
    /// never there" from "the node went away".
    ever_streamed: bool,
    timeout_seconds: u32,
}

impl Lifecycle {
    /// Starts a lifecycle that gives up after `timeout_seconds`.
    #[must_use]
    pub const fn new(timeout_seconds: u32) -> Self {
        Self {
            phase: Phase::Connecting,
            format: None,
            captured_sequence: None,
            ever_streamed: false,
            timeout_seconds,
        }
    }

    /// Resumes capture on a stream that has already reached `Streaming`.
    ///
    /// A new lifecycle is created for every requested frame, but stream history
    /// must survive those instances: an `Unconnected` callback after the first
    /// frame means the selected source disappeared, not that the portal handed
    /// over a node that never existed.
    #[must_use]
    pub const fn resume(timeout_seconds: u32, format: Option<Negotiated>) -> Self {
        Self {
            phase: if format.is_some() {
                Phase::Streaming
            } else {
                Phase::Connecting
            },
            format,
            captured_sequence: None,
            ever_streamed: true,
            timeout_seconds,
        }
    }

    /// The current phase.
    #[must_use]
    pub const fn phase(&self) -> &Phase {
        &self.phase
    }

    /// The agreed format, once there is one.
    #[must_use]
    pub const fn format(&self) -> Option<Negotiated> {
        self.format
    }

    /// Media observation that satisfied this capture, once settled successfully.
    #[must_use]
    pub const fn captured_sequence(&self) -> Option<u64> {
        self.captured_sequence
    }

    /// Whether the loop has finished, for good or ill.
    #[must_use]
    pub const fn is_settled(&self) -> bool {
        matches!(self.phase, Phase::Captured | Phase::Failed(_))
    }

    /// Feeds in an event and says whether to keep waiting.
    ///
    /// Once settled, further events are ignored — PipeWire keeps delivering
    /// callbacks while the stream is torn down, and letting a late
    /// `state_changed(Unconnected)` overwrite a successful capture with
    /// "target gone" would be a spectacular way to lose a screenshot.
    pub fn observe(&mut self, event: Event) -> Action {
        if self.is_settled() {
            return Action::Stop;
        }

        match event {
            Event::StateChanged(state, message) => self.on_state(state, message),
            Event::FormatAgreed(format) => {
                self.format = Some(format);
                if self.phase == Phase::Connecting {
                    self.phase = Phase::Streaming;
                }
                Action::Wait
            }
            Event::FormatRejected(why) => self.fail(Failure::Format(why)),
            Event::FrameReady(sequence) => {
                // A frame without an agreed format should be impossible, and if
                // it happens the pixels cannot be interpreted, so it is a
                // failure rather than a silent success.
                if self.format.is_none() {
                    return self.fail(Failure::Format(
                        "a buffer arrived before any format was agreed, so its pixel layout is \
                         unknown"
                            .into(),
                    ));
                }
                self.captured_sequence = Some(sequence);
                self.phase = Phase::Captured;
                Action::Stop
            }
            Event::NoDamage(_) | Event::EmptyBuffer => Action::Wait,
            Event::BufferRejected(why) => self.fail(Failure::Buffer(why)),
            Event::TimedOut => self.fail(Failure::Timeout(self.timeout_seconds)),
        }
    }

    fn on_state(&mut self, state: StreamState, message: Option<String>) -> Action {
        match state {
            StreamState::Streaming => {
                self.ever_streamed = true;
                self.phase = Phase::Streaming;
                Action::Wait
            }
            StreamState::Error => self.fail(Failure::Stream(message.unwrap_or_else(|| {
                "the PipeWire stream entered its error state without saying why".into()
            }))),
            StreamState::Unconnected => {
                // Before streaming, this means the node id from the portal named
                // nothing. After, it means whatever was being captured is gone.
                // They are different diagnoses and deserve different words.
                let what = if self.ever_streamed {
                    "the captured window or monitor disappeared while the frame was being read"
                } else {
                    "the PipeWire node the portal handed over does not exist, which usually means \
                     the portal session was torn down before the stream connected"
                };
                self.fail(Failure::Gone(what.into()))
            }
            StreamState::Connecting | StreamState::Paused => Action::Wait,
        }
    }

    fn fail(&mut self, failure: Failure) -> Action {
        self.phase = Phase::Failed(failure);
        Action::Stop
    }

    /// The settled result.
    ///
    /// # Errors
    ///
    /// Returns the failure the lifecycle settled on, or a timeout error if it
    /// never settled at all — which can only happen if the caller stopped the
    /// loop for its own reasons.
    pub fn outcome(self) -> Result<Negotiated, Error> {
        match self.phase {
            Phase::Captured => self.format.ok_or_else(|| {
                Error::Platform("a frame was captured but its format was lost".into())
            }),
            Phase::Failed(failure) => Err(failure.into_error()),
            Phase::Connecting | Phase::Streaming => {
                Err(Failure::Timeout(self.timeout_seconds).into_error())
            }
        }
    }
}
