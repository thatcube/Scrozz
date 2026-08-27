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
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use scrozz_core::Error;

use super::format::{self, Negotiated};
use super::lifecycle::{Action, Event, Lifecycle, StreamState};
use super::sys::{self, Library, Symbols, pw_stream_events, spa_hook, spa_pod};

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
    pixels: Option<Vec<u8>>,
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
            shared.events.push(event);
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

    // A null parameter means "cleared", which happens during renegotiation and
    // is not an error.
    if id != format::param::FORMAT || param.is_null() {
        return;
    }

    let header = unsafe { *param };
    let total = 8usize.saturating_add(header.size as usize);
    let bytes = unsafe { std::slice::from_raw_parts(param.cast::<u8>(), total) };

    match format::parse_format(bytes) {
        Ok(negotiated) => {
            if let Ok(mut shared) = listener.shared.lock() {
                shared.negotiated = Some(negotiated);
            }
            listener.push(Event::FormatAgreed(negotiated));
        }
        Err(why) => listener.push(Event::FormatRejected(why.to_string())),
    }
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
    // is the one the user actually asked for.
    let mut outcome: Option<Event> = None;
    loop {
        let buffer = unsafe { (symbols.pw_stream_dequeue_buffer)(stream) };
        if buffer.is_null() {
            break;
        }

        let event = unsafe { read_buffer(buffer, negotiated.as_ref(), listener) };
        unsafe { (symbols.pw_stream_queue_buffer)(stream, buffer) };

        // A later good frame supersedes an earlier empty one, but a failure is
        // kept: it means something is structurally wrong, not merely idle.
        match (&outcome, &event) {
            (Some(Event::BufferRejected(_)), _) => {}
            _ => outcome = Some(event),
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

    let plane = unsafe { &*(*spa).datas };

    if plane.type_ == sys::data_type::DMA_BUF {
        // Cannot happen with the parameters this client offers — no modifier is
        // advertised, so the server has no way to negotiate DMA-BUF — but if a
        // server does it anyway, saying so is far better than reading a pointer
        // that was never mapped.
        return Event::BufferRejected(
            "the compositor delivered a DMA-BUF frame, which needs GPU import that Scrozz does \
             not do; this build only accepts shared-memory frames"
                .into(),
        );
    }

    if plane.chunk.is_null() {
        return Event::EmptyBuffer;
    }
    let chunk = unsafe { *plane.chunk };

    // The idle case: a real buffer carrying nothing. Not an error.
    if chunk.size == 0 || plane.data.is_null() {
        return Event::EmptyBuffer;
    }

    let offset = chunk.offset as usize;
    let maxsize = plane.maxsize as usize;
    if offset >= maxsize {
        return Event::BufferRejected(format!(
            "the buffer's chunk starts at byte {offset} of a {maxsize}-byte mapping"
        ));
    }

    let available = (maxsize - offset).min(chunk.size as usize);
    let source =
        unsafe { std::slice::from_raw_parts(plane.data.cast::<u8>().add(offset), available) };

    match format::pack_rows(
        source,
        chunk.stride,
        negotiated.width,
        negotiated.height,
        negotiated.opaque_padding,
    ) {
        Ok(pixels) => {
            if let Ok(mut shared) = listener.shared.lock() {
                shared.pixels = Some(pixels);
            }
            Event::FrameReady
        }
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
    }
}

static PW_INIT: Once = Once::new();

/// Captures a single frame from a portal-provided PipeWire node.
///
/// `fd` is the remote returned by `org.freedesktop.portal.ScreenCast.
/// OpenPipeWireRemote` and is consumed: PipeWire closes it when the connection
/// is torn down. `node_id` is the stream's `pipe_wire_node_id`.
///
/// # Errors
///
/// [`Error::Unsupported`] when PipeWire is not installed, and [`Error::Platform`]
/// or [`Error::TargetGone`] for the failure modes enumerated in
/// [`super::lifecycle::Failure`]. Never panics on a compositor's bad behaviour;
/// the whole point of [`super::lifecycle`] is that misbehaviour is data.
pub fn capture_one(
    library: &Library,
    fd: OwnedFd,
    node_id: u32,
    timeout: Duration,
) -> Result<RawFrame, Error> {
    let symbols = &library.symbols;

    PW_INIT.call_once(|| unsafe { (symbols.pw_init)(ptr::null_mut(), ptr::null_mut()) });

    // Declared before `session` so that it is dropped *after* it: the stream
    // holds a pointer to the listener and must be destroyed first.
    let listener = Box::new(Listener {
        symbols: ptr::from_ref(symbols),
        thread_loop: ptr::null_mut(),
        stream: Mutex::new(ptr::null_mut()),
        shared: Mutex::new(Shared::default()),
    });
    let mut listener = listener;
    let mut hook = Box::new(spa_hook::zeroed());

    let name = CString::new("scrozz-capture").expect("literal has no interior NUL");
    let thread_loop = unsafe { (symbols.pw_thread_loop_new)(name.as_ptr(), ptr::null()) };
    if thread_loop.is_null() {
        return Err(Error::Platform(
            "PipeWire refused to create an event loop; the pipewire user service is probably not \
             running (try `systemctl --user status pipewire`)"
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

    unsafe { (symbols.pw_thread_loop_lock)(thread_loop) };
    let result = run_locked(&mut session, &mut listener, &mut hook, fd, node_id, timeout);
    unsafe { (symbols.pw_thread_loop_unlock)(thread_loop) };
    result
}

/// The part that must happen with the loop lock held.
fn run_locked(
    session: &mut Session<'_>,
    listener: &mut Listener,
    hook: &mut spa_hook,
    fd: OwnedFd,
    node_id: u32,
    timeout: Duration,
) -> Result<RawFrame, Error> {
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

    wait_for_frame(session, listener, timeout)
}

/// Blocks on the loop until the lifecycle settles or the deadline passes.
fn wait_for_frame(
    session: &Session<'_>,
    listener: &Listener,
    timeout: Duration,
) -> Result<RawFrame, Error> {
    let symbols = session.symbols;
    let seconds = u32::try_from(timeout.as_secs().max(1)).unwrap_or(u32::MAX);
    let mut lifecycle = Lifecycle::new(seconds);
    let deadline = Instant::now() + timeout;

    loop {
        // Drain whatever the callbacks queued while the lock was released.
        let batch = listener
            .shared
            .lock()
            .map(|mut shared| std::mem::take(&mut shared.events))
            .unwrap_or_default();

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
        if rc < 0 && Instant::now() >= deadline {
            lifecycle.observe(Event::TimedOut);
            break;
        }
    }

    let format = lifecycle.outcome()?;
    let pixels = listener
        .shared
        .lock()
        .ok()
        .and_then(|mut shared| shared.pixels.take())
        .ok_or_else(|| {
            Error::Platform(
                "the PipeWire stream reported a complete frame but produced no pixels".into(),
            )
        })?;

    Ok(RawFrame { pixels, format })
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
    let code = -rc;
    match code {
        2 => "no such node".into(),
        13 => "permission denied".into(),
        22 => "invalid argument".into(),
        32 => "the connection was closed".into(),
        110 => "timed out".into(),
        other => format!("errno {other}"),
    }
}
