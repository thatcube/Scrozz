//! The PipeWire C ABI, loaded at run time rather than linked.
//!
//! # Why `libloading` and not `pipewire-rs`
//!
//! Three reasons, in increasing order of importance.
//!
//! 1. **Build.** `pipewire-sys` and `libspa-sys` run `pkg-config` and `bindgen`
//!    from their build scripts, which means `libclang` and the PipeWire headers
//!    must exist on whatever machine compiles this crate. `docs/platforms.md`
//!    requires `scrozz-capture` to `cargo check --target
//!    x86_64-unknown-linux-gnu` from a macOS laptop; a `-sys` crate ends that
//!    the moment it is added.
//! 2. **Distribution.** Scrozz ships as a direct download (D7), so one binary
//!    meets every desktop. A `DT_NEEDED` entry for `libpipewire-0.3.so.0` makes
//!    the *whole executable* fail to load on a machine without PipeWire — an
//!    X11-only box, a minimal container — including the X11 capture path that
//!    would have worked perfectly. Loading it on demand turns that from "Scrozz
//!    does not start" into "Wayland capture is unavailable, install this
//!    package", which is what D8 asks for.
//! 3. **Honesty.** A missing library becomes an [`Error::Unsupported`] naming
//!    the package to install, at the exact moment the user asked for the thing
//!    that needs it.
//!
//! The cost is this file: the structs and signatures are transcribed from the
//! PipeWire headers by hand, and a mistake here is a crash rather than a
//! compile error. They are therefore written out in full, with the C
//! declaration beside each one, and nothing is guessed. What cannot be done is
//! *test* them without a real PipeWire — see `tools/wayland-smoke.sh`, which
//! exists precisely to be the thing that does.
//!
//! # What is not here
//!
//! `spa_pod_builder_*` is absent from this table because it is absent from the
//! shared object: every one of those functions is `static inline` in a header.
//! [`super::pod`] re-implements the format instead.

#![allow(non_camel_case_types)]

use std::ffi::{CStr, c_char, c_int, c_void};

use scrozz_core::Error;

/// The `enum spa_direction` value for an input (consuming) stream.
pub const DIRECTION_INPUT: u32 = 0;

/// `PW_STREAM_FLAG_AUTOCONNECT`.
pub const STREAM_FLAG_AUTOCONNECT: u32 = 1 << 0;
/// `PW_STREAM_FLAG_MAP_BUFFERS`.
///
/// Without this the client is handed raw file descriptors and must `mmap` them
/// itself; with it PipeWire maps shared-memory buffers and fills in
/// [`spa_data::data`]. It deliberately does *not* map DMA-BUF, which is exactly
/// the behaviour wanted here — see [`super::format`] for why DMA-BUF is
/// avoided rather than handled.
pub const STREAM_FLAG_MAP_BUFFERS: u32 = 1 << 2;
/// `PW_STREAM_FLAG_DONT_RECONNECT`.
///
/// A still capture that silently reconnects to a *different* node after the
/// original disappeared would return someone else's pixels.
pub const STREAM_FLAG_DONT_RECONNECT: u32 = 1 << 7;

/// `enum spa_data_type` values worth distinguishing.
pub mod data_type {
    /// `SPA_DATA_MemPtr`.
    pub const MEM_PTR: u32 = 1;
    /// `SPA_DATA_MemFd`.
    pub const MEM_FD: u32 = 2;
    /// `SPA_DATA_DmaBuf` — never usable here; see [`super::super::format`].
    pub const DMA_BUF: u32 = 3;
}

/// `struct spa_list`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct spa_list {
    /// Next entry.
    pub next: *mut spa_list,
    /// Previous entry.
    pub prev: *mut spa_list,
}

/// `struct spa_callbacks`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct spa_callbacks {
    /// Pointer to the callback vtable.
    pub funcs: *const c_void,
    /// Opaque user data passed to each callback.
    pub data: *mut c_void,
}

/// `struct spa_hook` — an entry in a listener list.
///
/// Must outlive the object it is registered on, and must not move once
/// registered: PipeWire threads it into an intrusive linked list by address.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct spa_hook {
    /// Intrusive list linkage.
    pub link: spa_list,
    /// The callbacks themselves.
    pub cb: spa_callbacks,
    /// Called when the hook is removed.
    pub removed: Option<unsafe extern "C" fn(*mut spa_hook)>,
    /// Private to the hook-list implementation.
    pub priv_: *mut c_void,
}

impl spa_hook {
    /// A zeroed hook, ready to be registered.
    #[must_use]
    pub const fn zeroed() -> Self {
        Self {
            link: spa_list {
                next: std::ptr::null_mut(),
                prev: std::ptr::null_mut(),
            },
            cb: spa_callbacks {
                funcs: std::ptr::null(),
                data: std::ptr::null_mut(),
            },
            removed: None,
            priv_: std::ptr::null_mut(),
        }
    }
}

/// `struct spa_pod` — the eight-byte header [`super::pod`] writes.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct spa_pod {
    /// Body size, excluding this header and any padding.
    pub size: u32,
    /// POD type id.
    pub type_: u32,
}

/// `struct spa_chunk` — which part of a [`spa_data`] holds this frame.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct spa_chunk {
    /// Offset of the valid data within the mapping.
    pub offset: u32,
    /// Number of valid bytes.
    pub size: u32,
    /// Row stride in bytes. Signed in SPA; see [`super::format::pack_rows`].
    pub stride: i32,
    /// `SPA_CHUNK_FLAG_*`.
    pub flags: i32,
}

/// `struct spa_data` — one plane of a buffer.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct spa_data {
    /// `enum spa_data_type`.
    pub type_: u32,
    /// `SPA_DATA_FLAG_*`.
    pub flags: u32,
    /// Backing file descriptor, when there is one.
    pub fd: i64,
    /// Page-aligned offset the mapping starts at.
    pub mapoffset: u32,
    /// Size of the mapping.
    pub maxsize: u32,
    /// The mapped memory, when `PW_STREAM_FLAG_MAP_BUFFERS` applied.
    pub data: *mut c_void,
    /// Which part of `data` is live.
    pub chunk: *mut spa_chunk,
}

/// `struct spa_meta`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct spa_meta {
    /// `enum spa_meta_type`.
    pub type_: u32,
    /// Size of `data`.
    pub size: u32,
    /// The metadata itself.
    pub data: *mut c_void,
}

/// `struct spa_buffer`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct spa_buffer {
    /// Number of metadata entries.
    pub n_metas: u32,
    /// Number of planes.
    pub n_datas: u32,
    /// The metadata array.
    pub metas: *mut spa_meta,
    /// The plane array.
    pub datas: *mut spa_data,
}

/// `struct pw_buffer`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct pw_buffer {
    /// The SPA buffer this wraps.
    pub buffer: *mut spa_buffer,
    /// Client-attached data.
    pub user_data: *mut c_void,
    /// Set by the client for queue accounting.
    pub size: u64,
    /// Suggested amount for playback streams; zero here.
    pub requested: u64,
    /// Cycle time in nanoseconds, since PipeWire 1.0.5.
    pub time: u64,
}

/// `struct pw_stream_events`, version 2.
///
/// The field order is load-bearing — this is a vtable, not a bag of options.
/// Every pointer is `Option<…>` so unused callbacks are a null entry, which is
/// what PipeWire checks for.
#[repr(C)]
pub struct pw_stream_events {
    /// Must be `PW_VERSION_STREAM_EVENTS`.
    pub version: u32,
    /// The stream is being destroyed.
    pub destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    /// The stream changed state; `error` is set for the error state.
    pub state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    /// A control's value or range changed.
    pub control_info: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    /// An IO area was attached or removed.
    pub io_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
    /// A parameter was agreed; this is where `Format` arrives.
    pub param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const spa_pod)>,
    /// A buffer was added to the pool.
    pub add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut pw_buffer)>,
    /// A buffer was removed from the pool.
    pub remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut pw_buffer)>,
    /// A buffer is ready to be dequeued.
    pub process: Option<unsafe extern "C" fn(*mut c_void)>,
    /// A drain completed.
    pub drained: Option<unsafe extern "C" fn(*mut c_void)>,
    /// A command arrived.
    pub command: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    /// A trigger completed.
    pub trigger_done: Option<unsafe extern "C" fn(*mut c_void)>,
}

/// `PW_VERSION_STREAM_EVENTS`.
pub const VERSION_STREAM_EVENTS: u32 = 2;

macro_rules! symbols {
    ($($field:ident : $ty:ty = $name:literal),+ $(,)?) => {
        /// The PipeWire entry points this crate calls.
        ///
        /// Every field is resolved eagerly at load time, so a library that is
        /// present but too old to export one of them fails immediately with the
        /// missing symbol named, rather than crashing later.
        #[allow(missing_docs)]
        pub struct Symbols {
            $(pub $field: $ty,)+
        }

        impl Symbols {
            unsafe fn resolve(library: &libloading::Library) -> Result<Self, String> {
                Ok(Self {
                    $($field: unsafe {
                        *library
                            .get::<$ty>($name)
                            .map_err(|err| format!(
                                "{} is missing the `{}` symbol ({err})",
                                LIBRARY_SONAME,
                                String::from_utf8_lossy(&$name[..$name.len() - 1]),
                            ))?
                    },)+
                })
            }
        }
    };
}

symbols! {
    pw_init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char) = b"pw_init\0",
    pw_deinit: unsafe extern "C" fn() = b"pw_deinit\0",

    pw_thread_loop_new:
        unsafe extern "C" fn(*const c_char, *const c_void) -> *mut c_void = b"pw_thread_loop_new\0",
    pw_thread_loop_destroy: unsafe extern "C" fn(*mut c_void) = b"pw_thread_loop_destroy\0",
    pw_thread_loop_get_loop:
        unsafe extern "C" fn(*mut c_void) -> *mut c_void = b"pw_thread_loop_get_loop\0",
    pw_thread_loop_start: unsafe extern "C" fn(*mut c_void) -> c_int = b"pw_thread_loop_start\0",
    pw_thread_loop_stop: unsafe extern "C" fn(*mut c_void) = b"pw_thread_loop_stop\0",
    pw_thread_loop_lock: unsafe extern "C" fn(*mut c_void) = b"pw_thread_loop_lock\0",
    pw_thread_loop_unlock: unsafe extern "C" fn(*mut c_void) = b"pw_thread_loop_unlock\0",
    pw_thread_loop_timed_wait:
        unsafe extern "C" fn(*mut c_void, c_int) -> c_int = b"pw_thread_loop_timed_wait\0",
    pw_thread_loop_signal: unsafe extern "C" fn(*mut c_void, bool) = b"pw_thread_loop_signal\0",

    pw_context_new:
        unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void = b"pw_context_new\0",
    pw_context_destroy: unsafe extern "C" fn(*mut c_void) = b"pw_context_destroy\0",
    pw_context_connect_fd:
        unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize) -> *mut c_void
        = b"pw_context_connect_fd\0",
    pw_core_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int = b"pw_core_disconnect\0",

    pw_properties_new:
        unsafe extern "C" fn(*const c_char, ...) -> *mut c_void = b"pw_properties_new\0",

    pw_stream_new:
        unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> *mut c_void
        = b"pw_stream_new\0",
    pw_stream_destroy: unsafe extern "C" fn(*mut c_void) = b"pw_stream_destroy\0",
    pw_stream_add_listener:
        unsafe extern "C" fn(*mut c_void, *mut spa_hook, *const pw_stream_events, *mut c_void)
        = b"pw_stream_add_listener\0",
    pw_stream_connect:
        unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut *const spa_pod, u32) -> c_int
        = b"pw_stream_connect\0",
    pw_stream_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int = b"pw_stream_disconnect\0",
    pw_stream_dequeue_buffer:
        unsafe extern "C" fn(*mut c_void) -> *mut pw_buffer = b"pw_stream_dequeue_buffer\0",
    pw_stream_queue_buffer:
        unsafe extern "C" fn(*mut c_void, *mut pw_buffer) -> c_int = b"pw_stream_queue_buffer\0",
}

/// The versioned soname, which is what is actually installed.
const LIBRARY_SONAME: &str = "libpipewire-0.3.so.0";

/// The unversioned name, present only when a `-dev` package is installed.
const LIBRARY_DEV_NAME: &str = "libpipewire-0.3.so";

/// A loaded PipeWire, and the symbols resolved from it.
pub struct Library {
    /// Resolved entry points.
    pub symbols: Symbols,
    /// Kept alive so the code stays mapped; never unloaded.
    _library: libloading::Library,
}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library").finish_non_exhaustive()
    }
}

impl Library {
    /// Opens PipeWire, or explains what to install.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when the library is absent or too old, with the
    /// package name for the common distributions. This is a first-class outcome
    /// rather than an exceptional one: plenty of machines that run Scrozz
    /// perfectly well over X11 have no PipeWire at all.
    pub fn open() -> Result<Self, Error> {
        // The versioned soname first: it is what a runtime package installs,
        // and the unversioned symlink exists only with the -dev package.
        let mut last = String::new();
        for name in [LIBRARY_SONAME, LIBRARY_DEV_NAME] {
            match unsafe { libloading::Library::new(name) } {
                Ok(library) => {
                    let symbols = unsafe { Symbols::resolve(&library) }.map_err(|why| {
                        Error::Unsupported {
                            what: "capturing on Wayland".into(),
                            why: format!(
                                "{why}. This build needs PipeWire 0.3 or newer; the installed \
                                 library is older than that"
                            ),
                        }
                    })?;
                    return Ok(Self {
                        symbols,
                        _library: library,
                    });
                }
                Err(err) => last = err.to_string(),
            }
        }

        Err(Error::Unsupported {
            what: "capturing on Wayland".into(),
            why: format!(
                "PipeWire is not installed, so the frames the desktop portal offers cannot be \
                 read. Wayland has no other route to screen pixels. Install it with `sudo apt \
                 install pipewire libpipewire-0.3-0` on Debian or Ubuntu, `sudo dnf install \
                 pipewire` on Fedora, or `sudo pacman -S pipewire` on Arch, and make sure the \
                 `pipewire.service` user unit is running. X11 capture is unaffected. \
                 (dlopen said: {last})"
            ),
        })
    }
}

/// Reads a C string that may be null.
///
/// # Safety
///
/// `pointer` must be null or point to a NUL-terminated string that stays valid
/// for the duration of the call.
pub unsafe fn optional_string(pointer: *const c_char) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned(),
    )
}
