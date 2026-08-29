//! Driving a PipeWire stream long enough to get one good frame out of it.
//!
//! This is the only file in the Wayland backend that contains unsafe code, and
//! it is deliberately thin: every decision it makes is delegated to
//! [`super::lifecycle`], every byte it interprets is decoded by [`super::pod`]
//! and [`super::format`], and every symbol it calls is described in
//! [`super::sys`]. What is left here is resource ownership and the shape of the
//! wait loop.
//!
//! # Shape of the thing
//!
//! ```text
//! pw_init                    once per process
//! pw_thread_loop_new         a loop on its own thread
//! pw_context_new             on that loop
//! pw_context_connect_fd      using the fd the portal handed over
//! pw_stream_new              a capture stream
//! pw_stream_add_listener     callbacks below
//! pw_stream_connect          INPUT, target = the portal's node, EnumFormat
//!   -> state_changed(Streaming)
//!   -> param_changed(Format)  the server fixates one of our offers
//!   -> process                buffers start arriving
//! pw_stream_disconnect / destroy / core disconnect / context destroy / loop destroy
//! ```
//!
//! # Threading
//!
//! `pw_thread_loop` runs the loop on its own thread and gives callers a
//! recursive lock. Callbacks fire on the loop thread *with that lock held*, and
//! the capturing thread holds it except while parked inside
//! `pw_thread_loop_timed_wait`. The two therefore never run at once, and the
//! [`Mutex`] guarding shared state is uncontended — it is there so the sharing
//! is expressible in safe Rust, not because the lock is doing real work.
//!
//! # Timeouts
//!
//! `pw_thread_loop_timed_wait` takes whole seconds, so the deadline is tracked
//! separately with [`Instant`] and the wait is re-armed with whatever time is
//! left. Without that, every signalled event — and a busy screen signals often —
//! would silently restart the timeout and a stalled capture could hang forever.

use std::ffi::{CString, c_char, c_int, c_void};
use std::os::fd::{IntoRawFd, OwnedFd};
use std::ptr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use scrozz_core::Error;

use crate::CaptureCancellation;

use super::format::{self, Negotiated};
use super::lifecycle::{
    Action, ChunkDisposition, Event, FrameTimeline, Lifecycle, StreamState, classify_chunk,
    prefer_process_event, prefer_state_event,
};
use super::sys::{self, Library, Symbols, pw_stream_events, spa_hook, spa_pod};

const MAX_PARAM_BODY_SIZE: usize = 64 * 1024;
const CHUNK_FLAG_CORRUPTED: u32 = 1 << 0;
const CHUNK_FLAG_EMPTY: u32 = 1 << 1;
const ERRNO_TIMED_OUT: c_int = 110;

/// One frame, packed and described.
#[derive(Debug, Clone)]
pub struct RawFrame {
    /// Tightly-packed pixels, `width * 4` bytes per row.
    pub pixels: Vec<u8>,
    /// The format the pixels are in.
    pub format: Negotiated,
}

/// State the callbacks write and the capturing thread reads.
#[derive(Default)]
struct Shared {
    events: Vec<Event>,
    negotiated: Option<Negotiated>,
    /// Continuous stream sequencing and delivery watermark.
    timeline: FrameTimeline,
    /// Latest state, retained after its queued callback event is drained.
    state: Option<(StreamState, Option<String>)>,
    /// Stream history must survive the lifecycle created for each frame.
    ever_streamed: bool,
    /// The newest complete frame not yet taken by the capture thread.
    ///
    /// Keeping its format beside its pixels makes renegotiation safe: a later
    /// callback can replace both atomically rather than pair new pixels with an
    /// earlier `FormatAgreed` event.
    frame: Option<SequencedFrame>,
}

#[derive(Debug, Clone)]
struct SequencedFrame {
    sequence: u64,
    frame: RawFrame,
}

impl Shared {
    fn invalidate_frame(&mut self) {
        self.frame = None;
        self.events
            .retain(|event| !matches!(event, Event::FrameReady(_) | Event::NoDamage(_)));
    }

    fn push(&mut self, event: Event) {
        if let Event::StateChanged(state, message) = &event {
            if self
                .state
                .as_ref()
                .is_none_or(|(current, _)| !current.is_terminal())
            {
                self.state = Some((*state, message.clone()));
            }
            self.ever_streamed |= *state == StreamState::Streaming;

            if let Some(position) = self
                .events
                .iter()
                .position(|queued| matches!(queued, Event::StateChanged(_, _)))
            {
                let retained = prefer_state_event(self.events[position].clone(), event);
                self.events[position] = retained;
            } else {
                self.events.push(event);
            }
            return;
        }

        match &event {
            Event::FormatRejected(_) => {
                self.negotiated = None;
                self.invalidate_frame();
            }
            Event::BufferRejected(_) => self.invalidate_frame(),
            Event::StateChanged(_, _)
            | Event::FormatAgreed(_)
            | Event::FrameReady(_)
            | Event::NoDamage(_)
            | Event::EmptyBuffer
            | Event::TimedOut => {}
        }

        if matches!(
            event,
            Event::FrameReady(_)
                | Event::NoDamage(_)
                | Event::EmptyBuffer
                | Event::BufferRejected(_)
        ) {
            if let Some(position) = self.events.iter().position(|queued| {
                matches!(
                    queued,
                    Event::FrameReady(_)
                        | Event::NoDamage(_)
                        | Event::EmptyBuffer
                        | Event::BufferRejected(_)
                )
            }) {
                let retained = prefer_process_event(self.events[position].clone(), event);
                self.events[position] = retained;
            } else {
                self.events.push(event);
            }
            return;
        }

        let kind = std::mem::discriminant(&event);
        if matches!(event, Event::FormatAgreed(_)) {
            // Only the latest nonterminal state of each kind can affect a new
            // frame request. Coalescing keeps a hostile or broken producer from
            // growing this queue while a reusable stream is idle.
            self.events
                .retain(|queued| std::mem::discriminant(queued) != kind);
        } else if self
            .events
            .iter()
            .any(|queued| std::mem::discriminant(queued) == kind)
        {
            // Preserve the first terminal diagnosis instead of letting a later
            // callback hide it, while still bounding each failure kind to one.
            return;
        }
        self.events.push(event);
    }
}

/// The `user_data` every callback receives.
struct Listener {
    symbols: *const Symbols,
    thread_loop: *mut c_void,
    /// Set once `pw_stream_new` has returned; `process` needs it to dequeue.
    stream: Mutex<*mut c_void>,
    shared: Mutex<Shared>,
}

impl Listener {
    fn push(&self, event: Event) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.push(event);
        }
        // Wake the capturing thread. `false` means "do not wait for the other
        // side to acknowledge", which is right here: nothing the capturing
        // thread does needs to happen before this callback returns.
        unsafe { ((*self.symbols).pw_thread_loop_signal)(self.thread_loop, false) };
    }
}

/// # Safety
///
/// The pointers are owned by the [`Session`] that created the listener and
/// outlive every callback, because the stream is destroyed before the listener
/// is dropped. Callbacks only ever run on the loop thread under the loop lock.
unsafe impl Send for Listener {}
/// # Safety
///
/// See [`Send`]; all interior mutation goes through [`Mutex`].
unsafe impl Sync for Listener {}

unsafe extern "C" fn on_state_changed(
    data: *mut c_void,
    _old: c_int,
    state: c_int,
    error: *const c_char,
) {
    let listener = unsafe { &*data.cast::<Listener>() };
    let message = unsafe { sys::optional_string(error) };
    listener.push(Event::StateChanged(StreamState::from_raw(state), message));
}

unsafe extern "C" fn on_param_changed(data: *mut c_void, id: u32, param: *const spa_pod) {
    let listener = unsafe { &*data.cast::<Listener>() };

    if id != format::param::FORMAT {
        return;
    }
    // A null parameter means "cleared", which happens during renegotiation.
    // Discard the old interpretation before any buffer from the new format can
    // arrive.
    if param.is_null() {
        if let Ok(mut shared) = listener.shared.lock() {
            shared.negotiated = None;
            shared.invalidate_frame();
        }
        return;
    }

    let bytes = match unsafe { copy_spa_pod(param) } {
        Ok(bytes) => bytes,
        Err(why) => {
            listener.push(Event::FormatRejected(why));
            return;
        }
    };

    match format::parse_format(&bytes) {
        Ok(negotiated) => {
            let stream = match listener.stream.lock() {
                Ok(stream) => *stream,
                Err(_) => {
                    listener.push(Event::FormatRejected(
                        "the PipeWire stream handle became unavailable during format negotiation"
                            .into(),
                    ));
                    return;
                }
            };
            if stream.is_null() {
                listener.push(Event::FormatRejected(
                    "PipeWire agreed a format before the stream handle was installed".into(),
                ));
                return;
            }

            // Publish the format before completing buffer negotiation. PipeWire
            // normally delivers process callbacks asynchronously, but the API
            // permits re-entrant callbacks while this recursive loop lock is
            // held; those buffers must see the new layout, and the lifecycle
            // must observe FormatAgreed before FrameReady.
            if let Ok(mut shared) = listener.shared.lock() {
                if shared
                    .negotiated
                    .is_some_and(|current| current != negotiated)
                {
                    shared.invalidate_frame();
                }
                shared.negotiated = Some(negotiated);
            }
            tracing::debug!(
                width = negotiated.width,
                height = negotiated.height,
                pixel_format = ?negotiated.pixel_format,
                color_space = ?negotiated.color_space,
                modifier = "none",
                "PipeWire accepted the raw-video format offer"
            );
            listener.push(Event::FormatAgreed(negotiated));

            // A modifier-less EnumFormat is only half of shared-memory
            // negotiation. PipeWire requires the consumer to answer the fixed
            // format with a ParamBuffers dataType flags choice.
            let buffers = format::shared_memory_buffer_param();
            let mut params = [buffers.as_ptr().cast::<spa_pod>()];
            let rc = unsafe {
                ((*listener.symbols).pw_stream_update_params)(
                    stream,
                    params.as_mut_ptr(),
                    params.len() as u32,
                )
            };
            if rc < 0 {
                if let Ok(mut shared) = listener.shared.lock() {
                    shared.negotiated = None;
                }
                listener.push(Event::FormatRejected(format!(
                    "PipeWire accepted the video format but rejected the shared-memory buffer \
                     requirement ({})",
                    errno_text(rc)
                )));
            } else {
                tracing::debug!(
                    data_types = "MemFd|MemPtr",
                    "PipeWire accepted the shared-memory buffer parameters"
                );
            }
        }
        Err(why) => listener.push(Event::FormatRejected(why.to_string())),
    }
}

/// Copies one callback-owned POD after bounding its peer-controlled body size.
///
/// # Safety
///
/// `param` must point to a live `spa_pod` supplied to `param_changed`.
unsafe fn copy_spa_pod(param: *const spa_pod) -> Result<Vec<u8>, String> {
    let header = unsafe { *param };
    let body = header.size as usize;
    if body > MAX_PARAM_BODY_SIZE {
        return Err(format!(
            "the PipeWire format parameter declared a {body}-byte body; the supported maximum is \
             {MAX_PARAM_BODY_SIZE} bytes"
        ));
    }
    let total = std::mem::size_of::<spa_pod>()
        .checked_add(body)
        .filter(|len| *len <= isize::MAX as usize)
        .ok_or_else(|| "the PipeWire format parameter length overflowed".to_owned())?;
    Ok(unsafe { std::slice::from_raw_parts(param.cast::<u8>(), total) }.to_vec())
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    let listener = unsafe { &*data.cast::<Listener>() };
    let symbols = unsafe { &*listener.symbols };

    let stream = match listener.stream.lock() {
        Ok(stream) => *stream,
        Err(_) => return,
    };
    if stream.is_null() {
        return;
    }

    let negotiated = match listener.shared.lock() {
        Ok(shared) => shared.negotiated,
        Err(_) => return,
    };

    // Drain the queue rather than taking the first buffer. Compositors commonly
    // deliver an empty priming buffer, and if several have piled up the newest
    // is the one the user actually asked for. Once a completed process callback
    // has published a frame, later callbacks must not overwrite it: a format
    // renegotiation could otherwise pair pixels with the wrong layout. Pixels
    // and format are stored together, so the newest valid frame may safely
    // replace an earlier callback's frame until the capture thread takes it.
    let mut outcome: Option<Event> = None;
    loop {
        let buffer = unsafe { (symbols.pw_stream_dequeue_buffer)(stream) };
        if buffer.is_null() {
            break;
        }

        let mut event = Some(unsafe { read_buffer(buffer, negotiated.as_ref(), listener) });
        let queued = unsafe { (symbols.pw_stream_queue_buffer)(stream, buffer) };
        if queued < 0 {
            event = Some(Event::BufferRejected(format!(
                "PipeWire would not take a dequeued buffer back ({})",
                errno_text(queued)
            )));
        }

        if let Some(event) = event {
            outcome = Some(match outcome {
                Some(current) => prefer_process_event(current, event),
                None => event,
            });
        }

        if queued < 0 {
            break;
        }
    }

    if let Some(event) = outcome {
        listener.push(event);
    }
}

/// Turns one dequeued buffer into an event, copying its pixels on success.
///
/// # Safety
///
/// `buffer` must be a live buffer just returned by `pw_stream_dequeue_buffer`.
unsafe fn read_buffer(
    buffer: *mut sys::pw_buffer,
    negotiated: Option<&Negotiated>,
    listener: &Listener,
) -> Event {
    let Some(negotiated) = negotiated else {
        return Event::EmptyBuffer;
    };

    let spa = unsafe { (*buffer).buffer };
    if spa.is_null() || unsafe { (*spa).n_datas } == 0 {
        return Event::EmptyBuffer;
    }
    if unsafe { (*spa).n_datas } != 1 {
        return Event::BufferRejected(format!(
            "the packed BGRx/RGBx SPA buffer reported {} data planes; exactly one is required",
            unsafe { (*spa).n_datas }
        ));
    }
    if unsafe { (*spa).datas }.is_null() {
        return Event::BufferRejected(
            "the SPA buffer reports data planes but its plane array is null".into(),
        );
    }

    let plane = unsafe { &*(*spa).datas };

    if plane.chunk.is_null() {
        return Event::EmptyBuffer;
    }
    let chunk = unsafe { *plane.chunk };
    let flags = chunk.flags.cast_unsigned();
    let disposition = classify_chunk(
        chunk.size,
        flags & CHUNK_FLAG_CORRUPTED != 0,
        flags & CHUNK_FLAG_EMPTY != 0,
    );
    match disposition {
        ChunkDisposition::Corrupted => {
            return Event::BufferRejected(
                "the compositor marked the PipeWire buffer as corrupted".into(),
            );
        }
        ChunkDisposition::Priming => return record_no_damage(listener, *negotiated),
        ChunkDisposition::Neutral => {
            // EMPTY is authoritative media-neutral content. It carries no pixel
            // bytes to validate or map; the negotiated format supplies the
            // bounded dimensions needed to synthesize black.
            tracing::debug!(
                width = negotiated.width,
                height = negotiated.height,
                "received SPA media-neutral video; synthesizing opaque black"
            );
            return match negotiated.neutral_pixels() {
                Ok(pixels) => store_frame(listener, pixels, *negotiated),
                Err(why) => Event::BufferRejected(why.to_string()),
            };
        }
        ChunkDisposition::Pixels => {}
    }

    let memory_type = match plane.type_ {
        format::data_type::MEM_PTR => "MemPtr",
        format::data_type::MEM_FD => "MemFd",
        format::data_type::DMA_BUF => {
            return Event::BufferRejected(
                "the compositor delivered a DMA-BUF frame despite the negotiated shared-memory \
                 requirement; importing it safely would require the compositor's GPU modifier and \
                 synchronization protocol"
                    .into(),
            );
        }
        other => {
            return Event::BufferRejected(format!(
                "the compositor delivered SPA memory type {other}, but the stream negotiated only \
                 MemPtr and MemFd"
            ));
        }
    };
    if plane.flags & format::data_flag::READABLE == 0 {
        return Event::BufferRejected(format!(
            "the compositor delivered a {memory_type} buffer without SPA_DATA_FLAG_READABLE"
        ));
    }
    let needed = match format::validate_chunk_geometry(
        chunk.size,
        plane.maxsize,
        chunk.stride,
        negotiated.width,
        negotiated.height,
    ) {
        Ok(needed) => needed,
        Err(why) => return Event::BufferRejected(why.to_string()),
    };
    if plane.data.is_null() {
        return Event::BufferRejected(
            "the shared-memory buffer has bytes but PipeWire did not map them".into(),
        );
    }

    let maxsize = plane.maxsize as usize;
    if maxsize == 0 {
        return Event::BufferRejected(format!(
            "the buffer carries {} bytes in a zero-length mapping",
            chunk.size
        ));
    }
    if maxsize > isize::MAX as usize {
        return Event::BufferRejected(format!(
            "the buffer's {maxsize}-byte mapping is too large to address safely"
        ));
    }

    let mapping = unsafe { std::slice::from_raw_parts(plane.data.cast::<u8>(), maxsize) };
    let source = match format::linear_chunk(mapping, chunk.offset, needed) {
        Ok(source) => source,
        Err(why) => return Event::BufferRejected(why.to_string()),
    };

    match format::pack_rows(
        source.as_ref(),
        chunk.stride,
        negotiated.width,
        negotiated.height,
        negotiated.opaque_padding,
    ) {
        Ok(pixels) => {
            tracing::debug!(
                memory_type,
                width = negotiated.width,
                height = negotiated.height,
                stride = chunk.stride,
                bytes = pixels.len(),
                "received a mapped shared-memory PipeWire frame"
            );
            store_frame(listener, pixels, *negotiated)
        }
        Err(why) => Event::BufferRejected(why.to_string()),
    }
}

fn store_frame(listener: &Listener, pixels: Vec<u8>, format: Negotiated) -> Event {
    match listener.shared.lock() {
        Ok(mut shared) => {
            let sequence = shared.timeline.publish();
            shared.frame = Some(SequencedFrame {
                sequence,
                frame: RawFrame { pixels, format },
            });
            Event::FrameReady(sequence)
        }
        Err(_) => Event::BufferRejected(
            "the captured pixels could not be stored because shared state was poisoned".into(),
        ),
    }
}

fn record_no_damage(listener: &Listener, format: Negotiated) -> Event {
    match listener.shared.lock() {
        Ok(mut shared) => {
            let reusable = shared
                .frame
                .as_ref()
                .is_some_and(|frame| frame.frame.format == format);
            if reusable {
                let sequence = shared.timeline.publish();
                Event::NoDamage(sequence)
            } else {
                Event::EmptyBuffer
            }
        }
        Err(_) => Event::BufferRejected(
            "the no-damage buffer could not be sequenced because shared state was poisoned".into(),
        ),
    }
}

static STREAM_EVENTS: pw_stream_events = pw_stream_events {
    version: sys::VERSION_STREAM_EVENTS,
    destroy: None,
    state_changed: Some(on_state_changed),
    control_info: None,
    io_changed: None,
    param_changed: Some(on_param_changed),
    add_buffer: None,
    remove_buffer: None,
    process: Some(on_process),
    drained: None,
    command: None,
    trigger_done: None,
};

/// Every PipeWire resource this capture owns, torn down in the right order.
///
/// Teardown lives in [`Drop`] rather than at the end of the happy path so that
/// an early return — a failed connect, a rejected format, a timeout — cleans up
/// just as thoroughly as success does. A leaked `pw_thread_loop` keeps a thread
/// alive for the life of the process.
struct Session<'lib> {
    symbols: &'lib Symbols,
    thread_loop: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
    stream: *mut c_void,
    started: bool,
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        tracing::debug!("tearing down the PipeWire capture stream");
        unsafe {
            if !self.stream.is_null() {
                (self.symbols.pw_thread_loop_lock)(self.thread_loop);
                let disconnected = (self.symbols.pw_stream_disconnect)(self.stream);
                (self.symbols.pw_thread_loop_unlock)(self.thread_loop);
                if disconnected < 0 {
                    tracing::warn!(
                        error = %errno_text(disconnected),
                        "PipeWire stream disconnect failed during teardown"
                    );
                }
            }
            // Stop the loop before destroying anything it might be servicing;
            // this joins the loop thread, so no callback can be in flight after
            // it returns.
            if self.started {
                (self.symbols.pw_thread_loop_stop)(self.thread_loop);
            }
            if !self.stream.is_null() {
                (self.symbols.pw_stream_destroy)(self.stream);
            }
            if !self.core.is_null() {
                let disconnected = (self.symbols.pw_core_disconnect)(self.core);
                if disconnected < 0 {
                    tracing::warn!(
                        error = %errno_text(disconnected),
                        "PipeWire core disconnect failed during teardown"
                    );
                }
            }
            if !self.context.is_null() {
                (self.symbols.pw_context_destroy)(self.context);
            }
            if !self.thread_loop.is_null() {
                (self.symbols.pw_thread_loop_destroy)(self.thread_loop);
            }
        }
        tracing::debug!("PipeWire capture stream teardown completed");
    }
}

/// A connected PipeWire stream that can supply successive frames.
///
/// The field order is deliberate: Rust drops fields in declaration order, so
/// the stream session is destroyed before the callback hook and listener whose
/// addresses it retains.
pub struct FrameStream {
    session: Session<'static>,
    _hook: Box<spa_hook>,
    listener: Box<Listener>,
    timeout: Duration,
}

impl std::fmt::Debug for FrameStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameStream")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl FrameStream {
    /// Connects to a portal-provided PipeWire node without consuming a frame.
    ///
    /// `fd` is the remote returned by `org.freedesktop.portal.ScreenCast.
    /// OpenPipeWireRemote` and is consumed: PipeWire closes it when this stream
    /// is torn down. `node_id` is the stream's `pipe_wire_node_id`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] if the loop, context, remote, or stream
    /// cannot be created or connected.
    pub fn connect(
        library: &'static Library,
        fd: OwnedFd,
        node_id: u32,
        pipewire_serial: Option<u64>,
        timeout: Duration,
    ) -> Result<Self, Error> {
        if pipewire_serial.is_none() && node_id == sys::ID_ANY {
            return Err(Error::Platform(
                "the ScreenCast portal returned PipeWire's wildcard node id without a stable \
                 object serial; Scrozz refuses to auto-connect to an unspecified video source"
                    .into(),
            ));
        }
        let symbols = &library.symbols;

        // Declared before `session` so construction failures drop the session
        // first. Once moved into `FrameStream`, field order provides the same
        // guarantee.
        let mut listener = Box::new(Listener {
            symbols: ptr::from_ref(symbols),
            thread_loop: ptr::null_mut(),
            stream: Mutex::new(ptr::null_mut()),
            shared: Mutex::new(Shared::default()),
        });
        let mut hook = Box::new(spa_hook::zeroed());

        let name = CString::new("scrozz-capture").expect("literal has no interior NUL");
        let thread_loop = unsafe { (symbols.pw_thread_loop_new)(name.as_ptr(), ptr::null()) };
        if thread_loop.is_null() {
            return Err(Error::Platform(
                "PipeWire refused to create an event loop; the pipewire user service is probably \
                 not running (try `systemctl --user status pipewire`)"
                    .into(),
            ));
        }
        listener.thread_loop = thread_loop;

        let mut session = Session {
            symbols,
            thread_loop,
            context: ptr::null_mut(),
            core: ptr::null_mut(),
            stream: ptr::null_mut(),
            started: false,
        };

        let loop_ = unsafe { (symbols.pw_thread_loop_get_loop)(thread_loop) };
        session.context = unsafe { (symbols.pw_context_new)(loop_, ptr::null_mut(), 0) };
        if session.context.is_null() {
            return Err(Error::Platform(
                "PipeWire could not create a client context".into(),
            ));
        }

        if unsafe { (symbols.pw_thread_loop_start)(thread_loop) } < 0 {
            return Err(Error::Platform(
                "PipeWire could not start its event loop thread".into(),
            ));
        }
        session.started = true;

        {
            let _lock = LoopLock::new(symbols, thread_loop);
            connect_locked(
                &mut session,
                &mut listener,
                &mut hook,
                fd,
                node_id,
                pipewire_serial,
            )?;
        }

        tracing::debug!(
            node_id,
            pipewire_serial,
            "connected the reusable PipeWire capture stream"
        );
        Ok(Self {
            session,
            _hook: hook,
            listener,
            timeout,
        })
    }

    /// Waits for and removes the newest complete frame.
    ///
    /// The stream remains connected after success, so callers can trigger a
    /// repaint or scroll and request another frame without reopening the portal.
    ///
    /// # Errors
    ///
    /// Returns the lifecycle failure reported by the stream, including timeout,
    /// target disappearance, malformed buffers, and format rejection.
    pub fn capture_frame(&mut self) -> Result<RawFrame, Error> {
        self.capture_frame_with_cancellation(None)
    }

    /// Waits for and removes a frame while observing acquisition cancellation.
    pub fn capture_frame_with_cancellation(
        &mut self,
        cancellation: Option<&CaptureCancellation>,
    ) -> Result<RawFrame, Error> {
        let _lock = LoopLock::new(self.session.symbols, self.session.thread_loop);
        let frame = wait_for_frame(&self.session, &self.listener, self.timeout, cancellation)?;
        Ok(frame)
    }
}

/// Captures a single frame from a portal-provided PipeWire node.
///
/// # Errors
///
/// [`Error::Unsupported`] when PipeWire is not installed, and [`Error::Platform`]
/// or [`Error::TargetGone`] for the failure modes enumerated in
/// [`super::lifecycle::Failure`]. Never panics on a compositor's bad behaviour;
/// the whole point of [`super::lifecycle`] is that misbehaviour is data.
pub fn capture_one(
    library: &'static Library,
    fd: OwnedFd,
    node_id: u32,
    pipewire_serial: Option<u64>,
    timeout: Duration,
) -> Result<RawFrame, Error> {
    FrameStream::connect(library, fd, node_id, pipewire_serial, timeout)?.capture_frame()
}

/// Releases the recursive thread-loop lock on every return and unwind path.
struct LoopLock<'a> {
    symbols: &'a Symbols,
    thread_loop: *mut c_void,
}

impl<'a> LoopLock<'a> {
    fn new(symbols: &'a Symbols, thread_loop: *mut c_void) -> Self {
        unsafe { (symbols.pw_thread_loop_lock)(thread_loop) };
        Self {
            symbols,
            thread_loop,
        }
    }
}

impl Drop for LoopLock<'_> {
    fn drop(&mut self) {
        unsafe { (self.symbols.pw_thread_loop_unlock)(self.thread_loop) };
    }
}

/// The part that must happen with the loop lock held.
fn connect_locked(
    session: &mut Session<'_>,
    listener: &mut Listener,
    hook: &mut spa_hook,
    fd: OwnedFd,
    node_id: u32,
    pipewire_serial: Option<u64>,
) -> Result<(), Error> {
    let symbols = session.symbols;

    // PipeWire takes the descriptor and closes it with the connection, so it
    // must be released here rather than dropped.
    let raw_fd = fd.into_raw_fd();
    session.core =
        unsafe { (symbols.pw_context_connect_fd)(session.context, raw_fd, ptr::null_mut(), 0) };
    if session.core.is_null() {
        return Err(Error::Platform(
            "could not connect to the PipeWire remote the portal provided. The screen-cast \
             session was granted but its socket was refused, which usually means the pipewire \
             service stopped between the portal dialog and the capture"
                .into(),
        ));
    }

    let properties = stream_properties(symbols, pipewire_serial)?;
    let stream_name = CString::new("scrozz").expect("literal has no interior NUL");
    let stream = unsafe { (symbols.pw_stream_new)(session.core, stream_name.as_ptr(), properties) };
    if stream.is_null() {
        return Err(Error::Platform("PipeWire could not create a stream".into()));
    }
    session.stream = stream;
    if let Ok(mut slot) = listener.stream.lock() {
        *slot = stream;
    }

    unsafe {
        (symbols.pw_stream_add_listener)(
            stream,
            ptr::from_mut(hook),
            &raw const STREAM_EVENTS,
            ptr::from_mut(listener).cast::<c_void>(),
        );
    }

    let offer = format::enum_format_param();
    let mut params: [*const spa_pod; 1] = [offer.as_ptr().cast::<spa_pod>()];

    let target_id = if pipewire_serial.is_some() {
        sys::ID_ANY
    } else {
        node_id
    };
    let rc = unsafe {
        (symbols.pw_stream_connect)(
            stream,
            sys::DIRECTION_INPUT,
            target_id,
            sys::STREAM_FLAG_AUTOCONNECT
                | sys::STREAM_FLAG_MAP_BUFFERS
                | sys::STREAM_FLAG_DONT_RECONNECT,
            params.as_mut_ptr(),
            1,
        )
    };
    if rc < 0 {
        return Err(Error::Platform(format!(
            "PipeWire refused to connect to portal stream node {node_id} (serial \
             {pipewire_serial:?}; {}). The portal granted the session, so the source most likely \
             vanished between being offered and being opened",
            errno_text(rc)
        )));
    }

    Ok(())
}

/// Blocks on the loop until the lifecycle settles or the deadline passes.
fn wait_for_frame(
    session: &Session<'_>,
    listener: &Listener,
    timeout: Duration,
    cancellation: Option<&CaptureCancellation>,
) -> Result<RawFrame, Error> {
    if let Some(cancellation) = cancellation {
        cancellation.check()?;
    }
    let symbols = session.symbols;
    let seconds = u32::try_from(timeout.as_secs().max(1)).unwrap_or(u32::MAX);
    let (pending, existing_format, state, ever_streamed) = {
        let mut shared = listener
            .shared
            .lock()
            .map_err(|_| Error::Platform("the PipeWire callback state was poisoned".into()))?;

        // Complete frames are copied continuously while a reusable stream is
        // idle. Keep any frame newer than the previously delivered sequence:
        // it may be exactly the post-scroll viewport produced during settle time.
        let queued = std::mem::take(&mut shared.events);
        let mut pending = normalize_events(&shared, queued);
        if let Some(frame) = shared
            .frame
            .as_ref()
            .filter(|frame| shared.timeline.is_fresh(frame.sequence))
            && !pending
                .iter()
                .any(|event| matches!(event, Event::FrameReady(_)))
        {
            pending.push(Event::FrameReady(frame.sequence));
        }

        (
            pending,
            shared
                .frame
                .as_ref()
                .map(|frame| frame.frame.format)
                .or(shared.negotiated),
            shared.state.clone(),
            shared.ever_streamed,
        )
    };

    let mut lifecycle = if ever_streamed {
        Lifecycle::resume(seconds, existing_format)
    } else {
        Lifecycle::new(seconds)
    };
    if let Some((state, message)) = state
        .as_ref()
        .filter(|(state, _)| state.is_terminal())
        .cloned()
    {
        lifecycle.observe(Event::StateChanged(state, message));
    }
    if !ever_streamed && let Some(format) = existing_format {
        lifecycle.observe(Event::FormatAgreed(format));
    }

    for event in pending {
        if lifecycle.observe(event) == Action::Stop {
            break;
        }
    }
    if !lifecycle.is_settled()
        && let Some((state, message)) = state
    {
        lifecycle.observe(Event::StateChanged(state, message));
    }

    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| Error::Platform("the PipeWire frame deadline overflowed".into()))?;

    loop {
        if let Some(cancellation) = cancellation {
            cancellation.check()?;
        }
        // Drain whatever the callbacks queued while the lock was released.
        let batch = {
            let mut shared = listener
                .shared
                .lock()
                .map_err(|_| Error::Platform("the PipeWire callback state was poisoned".into()))?;
            let queued = std::mem::take(&mut shared.events);
            normalize_events(&shared, queued)
        };

        let mut stop = false;
        for event in batch {
            if lifecycle.observe(event) == Action::Stop {
                stop = true;
                break;
            }
        }
        if stop || lifecycle.is_settled() {
            break;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            lifecycle.observe(Event::TimedOut);
            break;
        }

        // Whole seconds only, rounded up so a sub-second remainder still waits
        // rather than spinning.
        let wait_secs = remaining
            .as_secs()
            .saturating_add(u64::from(remaining.subsec_nanos() != 0))
            .max(1)
            .min(if cancellation.is_some() { 1 } else { u64::MAX })
            .min(i32::MAX as u64) as c_int;
        let rc = unsafe { (symbols.pw_thread_loop_timed_wait)(session.thread_loop, wait_secs) };
        if let Some(cancellation) = cancellation {
            cancellation.check()?;
        }
        // The legacy whole-second API returns positive ETIMEDOUT, unlike the
        // newer `_full` variant and most PipeWire calls, which return -errno.
        if rc == ERRNO_TIMED_OUT {
            if Instant::now() >= deadline {
                lifecycle.observe(Event::TimedOut);
                break;
            }
        } else if rc < 0 {
            return Err(Error::Platform(format!(
                "waiting for the PipeWire event loop failed ({})",
                errno_text(rc)
            )));
        }
    }

    let captured_sequence = lifecycle.captured_sequence().ok_or_else(|| {
        Error::Platform("the PipeWire lifecycle captured pixels without a media sequence".into())
    })?;
    let captured_format = lifecycle.outcome()?;
    let mut shared = listener
        .shared
        .lock()
        .map_err(|_| Error::Platform("the PipeWire callback state was poisoned".into()))?;
    let frame = shared
        .frame
        .as_ref()
        .ok_or_else(|| {
            Error::Platform(
                "the PipeWire stream reported a complete frame but produced no pixels".into(),
            )
        })?
        .clone();
    if frame.frame.format != captured_format {
        return Err(Error::Platform(
            "the PipeWire format changed after the selected frame was sequenced".into(),
        ));
    }
    shared.timeline.mark_delivered(captured_sequence);

    Ok(frame.frame)
}

fn normalize_events(shared: &Shared, events: Vec<Event>) -> Vec<Event> {
    let mut normalized = events
        .into_iter()
        .filter_map(|event| match event {
            Event::FrameReady(sequence) if shared.timeline.is_fresh(sequence) => {
                Some(Event::FrameReady(sequence))
            }
            Event::FrameReady(_) => None,
            Event::NoDamage(sequence)
                if shared.timeline.is_fresh(sequence)
                    && shared.frame.as_ref().is_some_and(|frame| {
                        shared
                            .negotiated
                            .is_some_and(|format| frame.frame.format == format)
                    }) =>
            {
                shared.frame.as_ref().map(|_| Event::FrameReady(sequence))
            }
            Event::NoDamage(_) => None,
            Event::EmptyBuffer | Event::TimedOut => None,
            other => Some(other),
        })
        .collect::<Vec<_>>();
    if let Some(position) = normalized
        .iter()
        .position(|event| matches!(event, Event::StateChanged(state, _) if state.is_terminal()))
    {
        normalized.swap(0, position);
    }
    normalized
}

/// Builds the stream's properties.
///
/// These are advisory metadata: they put a recognisable name in `pw-top` and
/// tell the session manager this is a screen capture rather than, say, a camera.
/// The serial-bearing path must retain `target.object`: falling back to null
/// properties while passing `PW_ID_ANY` would turn an exact portal stream into
/// an unconstrained auto-connect request.
fn stream_properties(
    symbols: &Symbols,
    pipewire_serial: Option<u64>,
) -> Result<*mut c_void, Error> {
    let pairs = [
        (c"media.type", c"Video"),
        (c"media.category", c"Capture"),
        (c"media.role", c"Screen"),
        (c"node.name", c"scrozz-capture"),
    ];

    unsafe {
        let properties = if let Some(serial) = pipewire_serial {
            let serial = CString::new(serial.to_string()).expect("an integer contains no NUL");
            (symbols.pw_properties_new)(
                pairs[0].0.as_ptr(),
                pairs[0].1.as_ptr(),
                pairs[1].0.as_ptr(),
                pairs[1].1.as_ptr(),
                pairs[2].0.as_ptr(),
                pairs[2].1.as_ptr(),
                pairs[3].0.as_ptr(),
                pairs[3].1.as_ptr(),
                c"target.object".as_ptr(),
                serial.as_ptr(),
                ptr::null::<c_char>(),
            )
        } else {
            (symbols.pw_properties_new)(
                pairs[0].0.as_ptr(),
                pairs[0].1.as_ptr(),
                pairs[1].0.as_ptr(),
                pairs[1].1.as_ptr(),
                pairs[2].0.as_ptr(),
                pairs[2].1.as_ptr(),
                pairs[3].0.as_ptr(),
                pairs[3].1.as_ptr(),
                ptr::null::<c_char>(),
            )
        };
        if properties.is_null() {
            Err(Error::Platform(
                "PipeWire could not allocate the capture stream properties; target identity was \
                 not relaxed"
                    .into(),
            ))
        } else {
            Ok(properties)
        }
    }
}

/// Renders a negative PipeWire return code as something a human can act on.
fn errno_text(rc: c_int) -> String {
    let code = rc.unsigned_abs();
    match code {
        2 => "no such node".into(),
        13 => "permission denied".into(),
        22 => "invalid argument".into(),
        32 => "the connection was closed".into(),
        110 => "timed out".into(),
        other => format!("errno {other}"),
    }
}

#[cfg(test)]
mod tests {
    use scrozz_core::{ColorSpace, PixelFormat};

    use super::*;

    fn negotiated() -> Negotiated {
        Negotiated {
            width: 1,
            height: 1,
            pixel_format: PixelFormat::Bgra8,
            opaque_padding: true,
            color_space: ColorSpace::Unknown,
        }
    }

    fn listener() -> Listener {
        let shared = Shared {
            negotiated: Some(negotiated()),
            ..Shared::default()
        };
        Listener {
            symbols: ptr::null(),
            thread_loop: ptr::null_mut(),
            stream: Mutex::new(ptr::null_mut()),
            shared: Mutex::new(shared),
        }
    }

    #[test]
    fn packed_video_rejects_multiple_data_planes_before_dereferencing_them() {
        let listener = listener();
        let mut spa = sys::spa_buffer {
            n_metas: 0,
            n_datas: 2,
            metas: ptr::null_mut(),
            datas: ptr::null_mut(),
        };
        let mut buffer = sys::pw_buffer {
            buffer: ptr::from_mut(&mut spa),
        };

        let Event::BufferRejected(reason) =
            (unsafe { read_buffer(&mut buffer, Some(&negotiated()), &listener) })
        else {
            panic!("a packed multi-plane buffer must be rejected");
        };
        assert!(reason.contains("exactly one"));
    }

    #[test]
    fn pixel_data_must_explicitly_be_readable() {
        let listener = listener();
        let mut pixels = [0_u8; 4];
        let mut chunk = sys::spa_chunk {
            offset: 0,
            size: 4,
            stride: 4,
            flags: 0,
        };
        let mut plane = sys::spa_data {
            type_: format::data_type::MEM_PTR,
            flags: 0,
            fd: -1,
            mapoffset: 0,
            maxsize: 4,
            data: pixels.as_mut_ptr().cast(),
            chunk: ptr::from_mut(&mut chunk),
        };
        let mut spa = sys::spa_buffer {
            n_metas: 0,
            n_datas: 1,
            metas: ptr::null_mut(),
            datas: ptr::from_mut(&mut plane),
        };
        let mut buffer = sys::pw_buffer {
            buffer: ptr::from_mut(&mut spa),
        };

        let Event::BufferRejected(reason) =
            (unsafe { read_buffer(&mut buffer, Some(&negotiated()), &listener) })
        else {
            panic!("an unreadable pixel plane must be rejected");
        };
        assert!(reason.contains("SPA_DATA_FLAG_READABLE"));
    }

    #[test]
    fn media_neutral_video_is_opaque_black_without_reading_plane_bytes() {
        let listener = listener();
        let mut chunk = sys::spa_chunk {
            offset: 0,
            size: 0,
            stride: 0,
            flags: CHUNK_FLAG_EMPTY.cast_signed(),
        };
        let mut plane = sys::spa_data {
            type_: 0,
            flags: 0,
            fd: -1,
            mapoffset: 0,
            maxsize: 0,
            data: ptr::null_mut(),
            chunk: ptr::from_mut(&mut chunk),
        };
        let mut spa = sys::spa_buffer {
            n_metas: 0,
            n_datas: 1,
            metas: ptr::null_mut(),
            datas: ptr::from_mut(&mut plane),
        };
        let mut buffer = sys::pw_buffer {
            buffer: ptr::from_mut(&mut spa),
        };

        assert_eq!(
            unsafe { read_buffer(&mut buffer, Some(&negotiated()), &listener) },
            Event::FrameReady(1)
        );
        let shared = listener.shared.lock().expect("shared frame");
        assert_eq!(
            shared.frame.as_ref().expect("neutral frame").frame.pixels,
            [0, 0, 0, 0xff]
        );
    }

    #[test]
    fn queued_media_keeps_the_newest_observation_and_terminal_state() {
        let mut shared = Shared {
            negotiated: Some(negotiated()),
            ..Shared::default()
        };
        shared.push(Event::FrameReady(1));
        shared.push(Event::NoDamage(2));
        assert_eq!(shared.events, [Event::NoDamage(2)]);

        shared.push(Event::StateChanged(
            StreamState::Error,
            Some("source failed".into()),
        ));
        shared.push(Event::StateChanged(StreamState::Paused, None));
        assert!(matches!(
            shared.state,
            Some((StreamState::Error, Some(ref message))) if message == "source failed"
        ));
        let normalized = normalize_events(&shared, shared.events.clone());
        assert!(matches!(
            normalized.first(),
            Some(Event::StateChanged(StreamState::Error, _))
        ));
    }

    #[test]
    fn no_damage_reuse_advances_the_delivery_watermark() {
        let mut shared = Shared {
            negotiated: Some(negotiated()),
            ..Shared::default()
        };
        let frame_sequence = shared.timeline.publish();
        shared.frame = Some(SequencedFrame {
            sequence: frame_sequence,
            frame: RawFrame {
                pixels: vec![0, 0, 0, 0xff],
                format: negotiated(),
            },
        });
        shared.timeline.mark_delivered(frame_sequence);
        let observation = shared.timeline.publish();

        assert_eq!(
            normalize_events(&shared, vec![Event::NoDamage(observation)]),
            [Event::FrameReady(observation)]
        );
        shared.timeline.mark_delivered(observation);
        assert!(!shared.timeline.is_fresh(observation));
    }
}
