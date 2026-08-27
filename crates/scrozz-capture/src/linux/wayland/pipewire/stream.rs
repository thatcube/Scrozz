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

use super::format::{self, ChunkDisposition, Negotiated};
use super::lifecycle::{Action, Event, Lifecycle, StreamState, prefer_process_event};
use super::sys::{self, Library, Symbols, pw_stream_events, spa_hook, spa_pod};

const MAX_PARAM_BODY_SIZE: usize = 64 * 1024;
const ERRNO_TIMED_OUT: c_int = 110;

/// One frame, packed and described.
#[derive(Debug)]
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
    /// Monotonic publication boundary for reusable-session freshness.
    published_sequence: u64,
}

#[derive(Debug)]
struct SequencedFrame {
    sequence: u64,
    frame: RawFrame,
}

impl Shared {
    fn invalidate_frame(&mut self) {
        self.frame = None;
        self.events
            .retain(|event| !matches!(event, Event::FrameReady));
    }

    fn push(&mut self, event: Event) {
        match &event {
            Event::StateChanged(state, message) => {
                self.state = Some((*state, message.clone()));
                self.ever_streamed |= *state == StreamState::Streaming;
            }
            Event::FormatRejected(_) => {
                self.negotiated = None;
                self.invalidate_frame();
            }
            Event::BufferRejected(_) => self.invalidate_frame(),
            Event::FormatAgreed(_) | Event::FrameReady | Event::EmptyBuffer | Event::TimedOut => {}
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

        let mut event = unsafe { read_buffer(buffer, negotiated.as_ref(), listener) };
        let queued = unsafe { (symbols.pw_stream_queue_buffer)(stream, buffer) };
        if queued < 0 {
            event = Event::BufferRejected(format!(
                "PipeWire would not take a dequeued buffer back ({})",
                errno_text(queued)
            ));
        }

        outcome = Some(match outcome {
            Some(current) => prefer_process_event(current, event),
            None => event,
        });

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
    if unsafe { (*spa).datas }.is_null() {
        return Event::BufferRejected(
            "the SPA buffer reports data planes but its plane array is null".into(),
        );
    }

    let plane = unsafe { &*(*spa).datas };

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

    if plane.chunk.is_null() {
        return Event::EmptyBuffer;
    }
    let chunk = unsafe { *plane.chunk };
    let flags = chunk.flags.cast_unsigned();
    match format::chunk_disposition(chunk.size, flags) {
        ChunkDisposition::Empty => return Event::EmptyBuffer,
        ChunkDisposition::Corrupted => {
            return Event::BufferRejected(
                "the compositor marked the PipeWire buffer as corrupted".into(),
            );
        }
        ChunkDisposition::Data => {}
    }
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
    let source = format::linear_chunk(mapping, chunk.offset, chunk.size);

    match format::pack_rows(
        source.as_ref(),
        chunk.stride,
        negotiated.width,
        negotiated.height,
        negotiated.opaque_padding,
    ) {
        Ok(pixels) => match listener.shared.lock() {
            Ok(mut shared) => {
                tracing::debug!(
                    memory_type,
                    width = negotiated.width,
                    height = negotiated.height,
                    stride = chunk.stride,
                    bytes = pixels.len(),
                    "received a mapped shared-memory PipeWire frame"
                );
                shared.published_sequence = shared.published_sequence.saturating_add(1);
                shared.frame = Some(SequencedFrame {
                    sequence: shared.published_sequence,
                    frame: RawFrame {
                        pixels,
                        format: *negotiated,
                    },
                });
                Event::FrameReady
            }
            Err(_) => Event::BufferRejected(
                "the captured pixels could not be stored because shared state was poisoned".into(),
            ),
        },
        Err(why) => Event::BufferRejected(why.to_string()),
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
                (self.symbols.pw_stream_disconnect)(self.stream);
                (self.symbols.pw_thread_loop_unlock)(self.thread_loop);
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
                (self.symbols.pw_core_disconnect)(self.core);
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
    captured_once: bool,
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
        timeout: Duration,
    ) -> Result<Self, Error> {
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
            connect_locked(&mut session, &mut listener, &mut hook, fd, node_id)?;
        }

        tracing::debug!(node_id, "connected the reusable PipeWire capture stream");
        Ok(Self {
            session,
            _hook: hook,
            listener,
            timeout,
            captured_once: false,
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
        let _lock = LoopLock::new(self.session.symbols, self.session.thread_loop);
        let frame = wait_for_frame(
            &self.session,
            &self.listener,
            self.timeout,
            self.captured_once,
        )?;
        self.captured_once = true;
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
    timeout: Duration,
) -> Result<RawFrame, Error> {
    FrameStream::connect(library, fd, node_id, timeout)?.capture_frame()
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

    let properties = stream_properties(symbols);
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

    let rc = unsafe {
        (symbols.pw_stream_connect)(
            stream,
            sys::DIRECTION_INPUT,
            node_id,
            sys::STREAM_FLAG_AUTOCONNECT
                | sys::STREAM_FLAG_MAP_BUFFERS
                | sys::STREAM_FLAG_DONT_RECONNECT,
            params.as_mut_ptr(),
            1,
        )
    };
    if rc < 0 {
        return Err(Error::Platform(format!(
            "PipeWire refused to connect to node {node_id} ({}). The portal granted the session, \
             so the node most likely vanished between being offered and being opened",
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
    discard_buffered_frame: bool,
) -> Result<RawFrame, Error> {
    let symbols = session.symbols;
    let seconds = u32::try_from(timeout.as_secs().max(1)).unwrap_or(u32::MAX);
    if discard_buffered_frame {
        // `pw_stream_flush(..., false)` clears the capture queue while the
        // thread-loop lock is held. Unlike a callback-assigned sequence alone,
        // this is a producer-side boundary: a buffer queued before this request
        // cannot later publish as though it followed the user's scroll.
        let rc = unsafe { (symbols.pw_stream_flush)(session.stream, false) };
        if rc < 0 {
            return Err(Error::Platform(format!(
                "flushing stale PipeWire capture buffers failed ({})",
                errno_text(rc)
            )));
        }
    }
    let (pending, existing_format, state, ever_streamed, minimum_sequence) = {
        let mut shared = listener
            .shared
            .lock()
            .map_err(|_| Error::Platform("the PipeWire callback state was poisoned".into()))?;

        let minimum_sequence = if discard_buffered_frame {
            // The flush above is the freshness boundary. The sequence check
            // additionally proves that a callback published after that boundary.
            let minimum = shared.published_sequence.saturating_add(1);
            shared.invalidate_frame();
            minimum
        } else {
            0
        };

        let has_frame = shared.frame.is_some();
        let mut pending = std::mem::take(&mut shared.events);
        pending.retain(|event| match event {
            Event::FrameReady => !discard_buffered_frame && has_frame,
            Event::FormatRejected(_) | Event::BufferRejected(_) => true,
            Event::StateChanged(_, _)
            | Event::FormatAgreed(_)
            | Event::EmptyBuffer
            | Event::TimedOut => false,
        });

        (
            pending,
            shared
                .frame
                .as_ref()
                .map(|frame| frame.frame.format)
                .or(shared.negotiated),
            shared.state.clone(),
            shared.ever_streamed,
            minimum_sequence,
        )
    };

    let mut lifecycle = if ever_streamed {
        Lifecycle::resume(seconds, existing_format)
    } else {
        Lifecycle::new(seconds)
    };
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
        // Drain whatever the callbacks queued while the lock was released.
        let batch = listener
            .shared
            .lock()
            .map_err(|_| Error::Platform("the PipeWire callback state was poisoned".into()))
            .map(|mut shared| std::mem::take(&mut shared.events))?;

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
        let wait_secs = remaining.as_secs().saturating_add(1).min(i32::MAX as u64) as c_int;
        let rc = unsafe { (symbols.pw_thread_loop_timed_wait)(session.thread_loop, wait_secs) };
        if rc < 0 {
            if rc != -ERRNO_TIMED_OUT {
                return Err(Error::Platform(format!(
                    "waiting for the PipeWire event loop failed ({})",
                    errno_text(rc)
                )));
            }
            if Instant::now() >= deadline {
                lifecycle.observe(Event::TimedOut);
                break;
            }
        }
    }

    lifecycle.outcome()?;
    let frame = listener
        .shared
        .lock()
        .map_err(|_| Error::Platform("the PipeWire callback state was poisoned".into()))?
        .frame
        .take()
        .ok_or_else(|| {
            Error::Platform(
                "the PipeWire stream reported a complete frame but produced no pixels".into(),
            )
        })?;
    if frame.sequence < minimum_sequence {
        return Err(Error::Platform(format!(
            "the PipeWire stream returned stale frame sequence {} before freshness boundary \
             {minimum_sequence}",
            frame.sequence
        )));
    }

    Ok(frame.frame)
}

/// Builds the stream's properties.
///
/// These are advisory metadata: they put a recognisable name in `pw-top` and
/// tell the session manager this is a screen capture rather than, say, a camera.
/// A null return is survivable — `pw_stream_new` accepts it — so failure here is
/// not worth an error path.
fn stream_properties(symbols: &Symbols) -> *mut c_void {
    let pairs = [
        (c"media.type", c"Video"),
        (c"media.category", c"Capture"),
        (c"media.role", c"Screen"),
        (c"node.name", c"scrozz-capture"),
    ];

    unsafe {
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
