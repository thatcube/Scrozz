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
//! fires for them. A capture that takes the first buffer it is offered will,
//! on a still desktop, reliably produce a black PNG. So [`Event::EmptyBuffer`]
//! is an explicit, expected event that keeps waiting rather than an error.

use scrozz_core::Error;

use super::format::Negotiated;

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
    /// `process` produced a buffer with pixels in it.
    FrameReady,
    /// `process` produced a buffer with no valid data; keep waiting.
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
/// better result arrived. Equal-priority events keep the first diagnosis while
/// the pixel slot itself is still updated to the newest good frame.
#[must_use]
pub fn prefer_process_event(current: Event, next: Event) -> Event {
    debug_assert!(is_process_event(&current));
    debug_assert!(is_process_event(&next));

    if process_priority(&next) > process_priority(&current) {
        next
    } else {
        current
    }
}

const fn is_process_event(event: &Event) -> bool {
    matches!(
        event,
        Event::FrameReady | Event::EmptyBuffer | Event::BufferRejected(_)
    )
}

const fn process_priority(event: &Event) -> u8 {
    match event {
        Event::BufferRejected(_) => 3,
        Event::FrameReady => 2,
        Event::EmptyBuffer => 1,
        _ => 0,
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
            Event::FrameReady => {
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
                self.phase = Phase::Captured;
                Action::Stop
            }
            Event::EmptyBuffer => Action::Wait,
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
