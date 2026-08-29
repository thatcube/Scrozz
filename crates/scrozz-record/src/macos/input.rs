//! Listen-only macOS input tap owned exactly by one active recording.

use std::{
    ffi::c_void,
    ptr::NonNull,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventMask, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType,
};
use scrozz_core::{Error, LogicalPoint, Result};

use crate::{
    interaction::{
        CapturedKeystroke, InputMonitor, InputSecurity, InteractionConsumer, InteractionProducer,
        PointerButton, interaction_channel,
    },
    overlay::KeystrokeKind,
    settings::RecordingSettings,
};

const TAP_DISABLED_TIMEOUT: u32 = 0xffff_fffe;
const TAP_DISABLED_USER_INPUT: u32 = 0xffff_ffff;
const KEY_DOWN: u32 = 10;
const FLAGS_CHANGED: u32 = 12;
const LEFT_MOUSE_DOWN: u32 = 1;
const RIGHT_MOUSE_DOWN: u32 = 3;
const OTHER_MOUSE_DOWN: u32 = 25;
const MODIFIER_COMMAND: u64 = 1 << 20;
const MODIFIER_SHIFT: u64 = 1 << 17;
const MODIFIER_CONTROL: u64 = 1 << 18;
const MODIFIER_OPTION: u64 = 1 << 19;
const IO_HID_LISTEN_EVENT: u32 = 1;
const IO_HID_ACCESS_GRANTED: u32 = 0;
const IO_HID_ACCESS_UNKNOWN: u32 = 2;
static ACTIVE_TAPS: AtomicUsize = AtomicUsize::new(0);

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDCheckAccess(request_type: u32) -> u32;
    fn IOHIDRequestAccess(request_type: u32) -> u8;
}

struct ActiveClock {
    started: Instant,
    paused_at: Option<Instant>,
    paused: Duration,
    synced: bool,
}

impl ActiveClock {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            paused_at: None,
            paused: Duration::ZERO,
            synced: false,
        }
    }

    fn elapsed(&self, now: Instant) -> Option<Duration> {
        if self.paused_at.is_some() || !self.synced {
            None
        } else {
            Some(
                now.saturating_duration_since(self.started)
                    .saturating_sub(self.paused),
            )
        }
    }

    fn sync(&mut self, now: Instant, media_elapsed: Duration) {
        if self.synced {
            return;
        }
        self.started = now.checked_sub(media_elapsed).unwrap_or(now);
        self.synced = true;
    }

    fn pause(&mut self, now: Instant) {
        self.paused_at.get_or_insert(now);
    }

    fn resume(&mut self, now: Instant) {
        if let Some(paused_at) = self.paused_at.take() {
            self.paused = self
                .paused
                .saturating_add(now.saturating_duration_since(paused_at));
        }
    }
}

struct CallbackState {
    producer: InteractionProducer,
    clock: Mutex<ActiveClock>,
    clicks: bool,
    keys: bool,
    all_keys: bool,
    active: AtomicBool,
    callback_drops: AtomicU64,
    warning: Mutex<Option<String>>,
    run_loop: Mutex<Option<SendRunLoop>>,
}

struct SendRunLoop(CFRetained<CFRunLoop>);

// SAFETY: cross-thread access is restricted to CFRunLoopStop, which is
// thread-safe. The retained reference prevents a stale pointer during teardown.
unsafe impl Send for SendRunLoop {}
// SAFETY: shared access occurs only while CallbackState::run_loop is locked.
unsafe impl Sync for SendRunLoop {}

pub(crate) struct MacInputMonitor {
    consumer: InteractionConsumer,
    state: Arc<CallbackState>,
    worker: Option<JoinHandle<()>>,
}

// SAFETY: native CoreFoundation objects remain confined to the worker. The
// owner communicates through atomics and joins the worker before dropping state.
unsafe impl Send for MacInputMonitor {}

pub(crate) fn start(settings: &RecordingSettings) -> Result<Box<dyn InputMonitor>> {
    let (producer, consumer) = interaction_channel(
        crate::interaction::MAX_PENDING_INTERACTIONS,
        settings.keystrokes.scope,
    )?;
    let state = Arc::new(CallbackState {
        producer,
        clock: Mutex::new(ActiveClock::new()),
        clicks: settings.clicks.enabled,
        keys: settings.keystrokes.enabled,
        all_keys: settings.keystrokes.scope == crate::settings::KeystrokeScope::All,
        active: AtomicBool::new(true),
        callback_drops: AtomicU64::new(0),
        warning: Mutex::new(None),
        run_loop: Mutex::new(None),
    });
    if !state.clicks && !state.keys {
        return Ok(Box::new(MacInputMonitor {
            consumer,
            state,
            worker: None,
        }));
    }
    ensure_input_monitoring()?;

    let worker_state = Arc::clone(&state);
    let (ready_send, ready_receive) = mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("scrozz-input-monitor".to_owned())
        .spawn(move || run_event_tap(worker_state, ready_send))
        .map_err(|error| Error::Platform(format!("could not start input monitor: {error}")))?;
    match ready_receive.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(Box::new(MacInputMonitor {
            consumer,
            state,
            worker: Some(worker),
        })),
        Ok(Err(error)) => {
            let _ = worker.join();
            Err(error)
        }

        Err(error) => {
            stop_worker(&state, worker);
            Err(Error::Platform(format!(
                "macOS input monitor did not start in time: {error}"
            )))
        }
    }
}

fn ensure_input_monitoring() -> Result<()> {
    // SAFETY: read-only process permission query available since macOS 10.15.
    let access = unsafe { IOHIDCheckAccess(IO_HID_LISTEN_EVENT) };
    if access == IO_HID_ACCESS_GRANTED {
        return Ok(());
    }
    if access == IO_HID_ACCESS_UNKNOWN {
        // SAFETY: called only because the user enabled click/key capture.
        if unsafe { IOHIDRequestAccess(IO_HID_LISTEN_EVENT) } != 0 {
            return Ok(());
        }
    }
    // Access changes after the user acts in System Settings; this recording
    // attempt fails closed rather than starting a silent partial monitor.
    Err(Error::PermissionDenied {
        capability: "Input Monitoring".to_owned(),
        remedy: "System Settings → Privacy & Security → Input Monitoring: enable Scrozz, then retry the recording"
            .to_owned(),
    })
}

fn run_event_tap(state: Arc<CallbackState>, ready: mpsc::SyncSender<Result<()>>) {
    let mask = event_mask(&state);
    let context = Arc::into_raw(Arc::clone(&state))
        .cast_mut()
        .cast::<c_void>();
    // SAFETY: the callback returns every event unchanged, context is one retained
    // Arc reclaimed below, and ListenOnly cannot modify or suppress input.
    let tap = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::ListenOnly,
            mask,
            Some(event_callback),
            context,
        )
    };
    let Some(tap) = tap else {
        // SAFETY: balances Arc::into_raw above; the tap did not retain context.
        drop(unsafe { Arc::from_raw(context.cast::<CallbackState>()) });
        let _ = ready.send(Err(Error::PermissionDenied {
            capability: "Input Monitoring".to_owned(),
            remedy: "System Settings → Privacy & Security → Input Monitoring: enable Scrozz, then retry the recording"
                .to_owned(),
        }));
        return;
    };
    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&tap), 0) else {
        tap.invalidate();
        // SAFETY: balances Arc::into_raw above.
        drop(unsafe { Arc::from_raw(context.cast::<CallbackState>()) });
        let _ = ready.send(Err(Error::Platform(
            "macOS could not attach the input monitor to a run loop".to_owned(),
        )));
        return;
    };
    let Some(run_loop) = CFRunLoop::current() else {
        tap.invalidate();
        // SAFETY: balances Arc::into_raw above.
        drop(unsafe { Arc::from_raw(context.cast::<CallbackState>()) });
        let _ = ready.send(Err(Error::Platform(
            "macOS returned no run loop for the input monitor".to_owned(),
        )));
        return;
    };
    let mode = unsafe { kCFRunLoopCommonModes };
    run_loop.add_source(Some(&source), mode);
    CGEvent::tap_enable(&tap, true);
    ACTIVE_TAPS.fetch_add(1, Ordering::AcqRel);
    *lock(&state.run_loop) = Some(SendRunLoop(run_loop.clone()));
    let _ = ready.send(Ok(()));
    if state.active.load(Ordering::Acquire) {
        CFRunLoop::run();
    }
    lock(&state.run_loop).take();
    run_loop.remove_source(Some(&source), mode);
    tap.invalidate();
    ACTIVE_TAPS.fetch_sub(1, Ordering::AcqRel);
    // SAFETY: callback execution ended before the event tap was invalidated and
    // the run loop stopped, so no callback can observe this final release.
    drop(unsafe { Arc::from_raw(context.cast::<CallbackState>()) });
}

pub(crate) fn active_count() -> usize {
    ACTIVE_TAPS.load(Ordering::Acquire)
}

fn event_mask(state: &CallbackState) -> CGEventMask {
    let mut mask = 0;
    if state.clicks {
        mask |= bit(LEFT_MOUSE_DOWN) | bit(RIGHT_MOUSE_DOWN) | bit(OTHER_MOUSE_DOWN);
    }
    if state.keys {
        mask |= bit(KEY_DOWN) | bit(FLAGS_CHANGED);
    }
    mask
}

const fn bit(event_type: u32) -> CGEventMask {
    1_u64 << event_type
}

unsafe extern "C-unwind" fn event_callback(
    _proxy: objc2_core_graphics::CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    context: *mut c_void,
) -> *mut CGEvent {
    let original = event.as_ptr();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if context.is_null() {
            return;
        }
        // SAFETY: context is a live Arc<CallbackState> retained by run_event_tap
        // until the tap is invalidated and its run loop exits.
        let state = unsafe { &*context.cast::<CallbackState>() };
        if !state.active.load(Ordering::Acquire) {
            return;
        }
        if matches!(event_type.0, TAP_DISABLED_TIMEOUT | TAP_DISABLED_USER_INPUT) {
            disable_monitor(state);
            return;
        }

        let Ok(clock) = state.clock.try_lock() else {
            state.callback_drops.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let Some(at) = clock.elapsed(Instant::now()) else {
            return;
        };
        drop(clock);
        let event = unsafe { event.as_ref() };
        match event_type.0 {
            LEFT_MOUSE_DOWN | RIGHT_MOUSE_DOWN | OTHER_MOUSE_DOWN if state.clicks => {
                let location = CGEvent::location(Some(event));
                let raw_button =
                    CGEvent::integer_value_field(Some(event), CGEventField::MouseEventButtonNumber);
                let button = match event_type.0 {
                    LEFT_MOUSE_DOWN => PointerButton::Primary,
                    RIGHT_MOUSE_DOWN => PointerButton::Secondary,
                    _ if raw_button == 2 => PointerButton::Middle,
                    _ => PointerButton::Other(u8::try_from(raw_button).unwrap_or(u8::MAX)),
                };
                let _ = state.producer.push_click(
                    at,
                    LogicalPoint::new(location.x, location.y),
                    button,
                );
            }
            KEY_DOWN if state.keys => capture_key(state, at, event),
            FLAGS_CHANGED if state.keys => capture_modifier(state, at, event),
            _ => {}
        }
    }));
    original
}

fn disable_monitor(state: &CallbackState) {
    state.active.store(false, Ordering::Release);
    *lock(&state.warning) =
        Some("macOS disabled Input Monitoring; interaction capture stopped immediately".to_owned());
    if let Some(run_loop) = lock(&state.run_loop).as_ref() {
        run_loop.0.stop();
    }
}

fn capture_key(state: &CallbackState, at: Duration, event: &CGEvent) {
    let keycode =
        CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode) as u16;
    let flags = CGEvent::flags(Some(event)).bits();
    let repeat =
        CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventAutorepeat) != 0;
    if is_scrozz_control_shortcut(keycode, flags) {
        return;
    }
    let secure = secure_input();
    let named = named_key(keycode);
    let has_command_modifier = flags & (MODIFIER_COMMAND | MODIFIER_CONTROL | MODIFIER_OPTION) != 0;
    if let Some(named) = named {
        let mut parts = [""; 5];
        let mut len = modifier_parts(flags, &mut parts);
        parts[len] = named;
        len += 1;
        if let Ok(key) = CapturedKeystroke::from_parts(
            &parts[..len],
            KeystrokeKind::NavigationOrEditing,
            repeat,
            false,
        ) {
            let _ = state.producer.push_key(at, key, secure, false);
        }
        return;
    }
    if text_key(keycode).is_some() {
        if !state.all_keys && !has_command_modifier {
            return;
        }
        if secure != InputSecurity::NonSecure {
            return;
        }
        let mut prefixes = [""; 5];
        let prefix_len = modifier_parts(flags, &mut prefixes);
        let mut unicode = [0_u16; 16];
        let mut length = 0_u64;
        // SAFETY: both output pointers reference fixed writable buffers for the
        // declared maximum length. This runs only after secure-input approval.
        unsafe {
            CGEvent::keyboard_get_unicode_string(
                Some(event),
                unicode.len() as u64,
                &mut length,
                unicode.as_mut_ptr(),
            );
        }
        let length = usize::try_from(length)
            .unwrap_or(unicode.len())
            .min(unicode.len());
        let key = if length > 0 {
            CapturedKeystroke::from_utf16_parts(
                &prefixes[..prefix_len],
                &unicode[..length],
                if has_command_modifier {
                    KeystrokeKind::Modifier
                } else {
                    KeystrokeKind::Text
                },
                repeat,
            )
        } else {
            let mut parts = prefixes;
            parts[prefix_len] = text_key(keycode).expect("text key was checked");
            CapturedKeystroke::from_parts(
                &parts[..prefix_len + 1],
                if has_command_modifier {
                    KeystrokeKind::Modifier
                } else {
                    KeystrokeKind::Text
                },
                repeat,
                true,
            )
        };
        if let Ok(key) = key {
            let _ = state.producer.push_key(at, key, secure, false);
        }
    }
}

fn capture_modifier(state: &CallbackState, at: Duration, event: &CGEvent) {
    let keycode =
        CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode) as u16;
    let flags = CGEvent::flags(Some(event)).bits();
    let Some((label, mask)) = modifier_key(keycode) else {
        return;
    };
    if flags & mask == 0 {
        return;
    }
    if let Ok(key) =
        CapturedKeystroke::with_text_content(label, KeystrokeKind::Modifier, false, false)
    {
        let _ = state
            .producer
            .push_key(at, key, InputSecurity::Unknown, false);
    }
}

fn secure_input() -> InputSecurity {
    // SAFETY: Carbon exposes a process-global read-only secure-input query.
    if unsafe { IsSecureEventInputEnabled() } != 0 {
        InputSecurity::Secure
    } else {
        InputSecurity::NonSecure
    }
}

fn modifier_parts(flags: u64, output: &mut [&str; 5]) -> usize {
    let mut len = 0;
    for (mask, label) in [
        (MODIFIER_CONTROL, "Ctrl+"),
        (MODIFIER_OPTION, "Opt+"),
        (MODIFIER_SHIFT, "Shift+"),
        (MODIFIER_COMMAND, "Cmd+"),
    ] {
        if flags & mask != 0 {
            output[len] = label;
            len += 1;
        }
    }
    len
}

fn is_scrozz_control_shortcut(keycode: u16, flags: u64) -> bool {
    let command_shift =
        flags & (MODIFIER_COMMAND | MODIFIER_SHIFT) == MODIFIER_COMMAND | MODIFIER_SHIFT;
    command_shift && matches!(keycode, 15 | 21 | 22 | 23 | 53)
}

fn modifier_key(keycode: u16) -> Option<(&'static str, u64)> {
    Some(match keycode {
        54 | 55 => ("Command", MODIFIER_COMMAND),
        56 | 60 => ("Shift", MODIFIER_SHIFT),
        58 | 61 => ("Option", MODIFIER_OPTION),
        59 | 62 => ("Control", MODIFIER_CONTROL),
        _ => return None,
    })
}

fn named_key(keycode: u16) -> Option<&'static str> {
    Some(match keycode {
        36 | 76 => "Return",
        48 => "Tab",
        51 => "Delete",
        53 => "Escape",
        71 => "Clear",
        115 => "Home",
        116 => "Page Up",
        117 => "Forward Delete",
        119 => "End",
        121 => "Page Down",
        123 => "Left",
        124 => "Right",
        125 => "Down",
        126 => "Up",
        _ => return None,
    })
}

fn text_key(keycode: u16) -> Option<&'static str> {
    Some(match keycode {
        0 => "A",
        1 => "S",
        2 => "D",
        3 => "F",
        4 => "H",
        5 => "G",
        6 => "Z",
        7 => "X",
        8 => "C",
        9 => "V",
        11 => "B",
        12 => "Q",
        13 => "W",
        14 => "E",
        15 => "R",
        16 => "Y",
        17 => "T",
        18 => "1",
        19 => "2",
        20 => "3",
        21 => "4",
        22 => "6",
        23 => "5",
        24 => "=",
        25 => "9",
        26 => "7",
        27 => "-",
        28 => "8",
        29 => "0",
        30 => "]",
        31 => "O",
        32 => "U",
        33 => "[",
        34 => "I",
        35 => "P",
        37 => "L",
        38 => "J",
        39 => "'",
        40 => "K",
        41 => ";",
        42 => "\\",
        43 => ",",
        44 => "/",
        45 => "N",
        46 => "M",
        47 => ".",
        49 => "Space",
        50 => "`",
        _ => return None,
    })
}

impl InputMonitor for MacInputMonitor {
    fn sync_media_time(&mut self, elapsed: Duration) {
        lock(&self.state.clock).sync(Instant::now(), elapsed);
    }

    fn drain(&mut self) -> Vec<crate::interaction::InteractionEvent> {
        self.consumer.drain()
    }

    fn cursor_position(&self) -> Option<LogicalPoint> {
        let event = CGEvent::new(None)?;
        let point = CGEvent::location(Some(&event));
        (point.x.is_finite() && point.y.is_finite()).then(|| LogicalPoint::new(point.x, point.y))
    }

    fn pause(&mut self) {
        lock(&self.state.clock).pause(Instant::now());
    }

    fn resume(&mut self) {
        lock(&self.state.clock).resume(Instant::now());
    }

    fn take_dropped(&mut self) -> u64 {
        self.consumer
            .take_dropped()
            .saturating_add(self.state.callback_drops.swap(0, Ordering::AcqRel))
    }

    fn take_warning(&mut self) -> Option<String> {
        lock(&self.state.warning).take()
    }
}

impl Drop for MacInputMonitor {
    fn drop(&mut self) {
        self.state.active.store(false, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            stop_worker(&self.state, worker);
        }
    }
}

fn stop_worker(state: &Arc<CallbackState>, worker: JoinHandle<()>) {
    state.active.store(false, Ordering::Release);
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        if worker.is_finished() {
            let _ = worker.join();
            return;
        }
        if let Some(run_loop) = lock(&state.run_loop).as_ref() {
            run_loop.0.stop();
        }
        if Instant::now() >= deadline {
            // The callback is already inert. Detaching is safer than hanging the
            // recording owner if a framework call itself is wedged.
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrozz_shortcuts_are_filtered_before_a_label_exists() {
        assert!(is_scrozz_control_shortcut(
            15,
            MODIFIER_COMMAND | MODIFIER_SHIFT
        ));
        assert!(is_scrozz_control_shortcut(
            53,
            MODIFIER_COMMAND | MODIFIER_SHIFT
        ));
        assert!(!is_scrozz_control_shortcut(15, MODIFIER_COMMAND));
    }

    #[test]
    fn key_classification_separates_navigation_and_text() {
        assert_eq!(named_key(53), Some("Escape"));
        assert_eq!(text_key(0), Some("A"));
        assert_eq!(named_key(0), None);
    }

    #[test]
    fn active_clock_drops_pause_events_and_removes_paused_time() {
        let mut clock = ActiveClock::new();
        let start = clock.started;
        clock.sync(start, Duration::ZERO);
        clock.pause(start + Duration::from_secs(2));
        assert_eq!(clock.elapsed(start + Duration::from_secs(3)), None);
        clock.resume(start + Duration::from_secs(5));
        assert_eq!(
            clock.elapsed(start + Duration::from_secs(6)),
            Some(Duration::from_secs(3))
        );
    }

    #[test]
    fn revocation_fails_closed_and_surfaces_one_warning() {
        let (producer, _) =
            interaction_channel(8, crate::settings::KeystrokeScope::ModifiersOnly).unwrap();
        let state = CallbackState {
            producer,
            clock: Mutex::new(ActiveClock::new()),
            clicks: true,
            keys: true,
            all_keys: false,
            active: AtomicBool::new(true),
            callback_drops: AtomicU64::new(0),
            warning: Mutex::new(None),
            run_loop: Mutex::new(None),
        };
        disable_monitor(&state);
        assert!(!state.active.load(Ordering::Acquire));
        let warning = lock(&state.warning).take().unwrap();
        assert!(warning.contains("stopped immediately"));
    }
}
