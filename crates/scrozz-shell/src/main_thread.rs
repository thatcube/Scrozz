//! Knowing whether this is the thread the tray and the hotkeys need.
//!
//! # Why a latch instead of an assertion
//!
//! `tray-icon` and `muda` are `Rc`-based and therefore `!Send`; `global-hotkey`
//! installs its handler on the thread that creates it; winit demands the main
//! thread everywhere. All three want the *same* thread, and all three fail
//! **silently** when they do not get it. The tray appears and never responds to
//! a click. The hotkey registers successfully and never fires. On Windows the
//! shape of the mistake is a little different — the tray icon's window needs a
//! message pump, and a worker thread has none — but the symptom is identical:
//! nothing, forever, with no error anywhere.
//!
//! A silent failure needs a loud check, and the check has to happen somewhere
//! that *knows* which thread is the main one. Rust does not offer that: there
//! is no `std::thread::is_main`. `MainThreadMarker` exists on macOS via
//! `objc2`, and nothing equivalent exists on Windows.
//!
//! So the main thread announces itself. [`claim`] is called once, early, by
//! whoever owns the process entry point, and every later question is answered
//! by comparing thread identities. That is exact on every platform, needs no OS
//! call, and — the part that matters for this project — is testable on any of
//! them.
//!
//! # Why this warns rather than refuses
//!
//! A tray that will not appear is a degradation, not a catastrophe: per D8 a
//! missing capability is explained and the app runs on. Refusing to construct
//! would turn a bad menu bar into no application at all. What the caller gets
//! instead is a sentence it can put in front of a developer, which is the thing
//! that was missing.

use std::sync::OnceLock;
use std::thread::ThreadId;

/// The thread that called [`claim`], if anything did.
static MAIN: OnceLock<ThreadId> = OnceLock::new();

/// Records the calling thread as the one that owns the event loop.
///
/// Called once from the process entry point, before any UI exists. Later calls
/// are ignored rather than panicking: a second claim is a programming mistake,
/// but taking the process down over it would be a worse one than the mistake.
///
/// Returns `true` if this call was the one that established the main thread.
pub fn claim() -> bool {
    let me = std::thread::current().id();
    MAIN.set(me).is_ok()
}

/// Whether [`claim`] has been called at all.
///
/// `false` in unit tests and in library consumers that never opted in, which is
/// why [`check`] treats it as "unknown" rather than "wrong".
#[must_use]
pub fn is_claimed() -> bool {
    MAIN.get().is_some()
}

/// Whether the calling thread is the one that claimed the event loop.
///
/// `None` when nobody has claimed one, because "no answer" and "no" lead to
/// very different messages: the first is a test harness, the second is a bug.
#[must_use]
pub fn is_main() -> Option<bool> {
    MAIN.get().map(|main| *main == std::thread::current().id())
}

/// A sentence explaining that `what` is on the wrong thread, or `None` if it is
/// on the right one — or if nobody ever said which thread that is.
///
/// Returned rather than logged so the wording can be asserted on. The failure
/// this guards against is invisible by nature, so "we would have said
/// something" is worth a test of its own.
#[must_use]
pub fn check(what: &str) -> Option<String> {
    match is_main() {
        // On the main thread, or in a context that never claimed one — a unit
        // test, or an embedder with its own conventions. Nothing to say.
        Some(true) | None => None,
        Some(false) => Some(format!(
            "{what} is being set up on a worker thread, not the main thread that owns \
             the event loop. It will appear to succeed and then never do anything: \
             tray menus stop responding to clicks and global hotkeys stop firing, \
             with no error reported anywhere. Move this call onto the main thread"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{check, claim, is_claimed, is_main};

    // These share one process-global latch, so they are written to be
    // order-independent: each asserts only what holds regardless of who ran
    // first. `claim` returning `false` is itself meaningful \u2014 somebody already
    // claimed \u2014 and is exercised below.

    #[test]
    fn an_unclaimed_process_says_nothing_rather_than_guessing() {
        // Before anyone claims, "is this the main thread?" has no answer, and
        // inventing one would make every unit test in the workspace warn.
        if !is_claimed() {
            assert_eq!(is_main(), None);
            assert_eq!(check("the tray"), None);
        }
    }

    #[test]
    fn claiming_twice_is_refused_rather_than_fatal() {
        let first = claim();
        let second = claim();
        assert!(
            !second,
            "a second claim must not silently move the main thread"
        );
        // Whether `first` won depends on test order; either way the latch is
        // now set and stays set.
        let _ = first;
        assert!(is_claimed());
    }

    #[test]
    fn a_worker_thread_is_told_exactly_what_is_wrong_with_it() {
        claim();
        let message = std::thread::spawn(|| check("the tray"))
            .join()
            .expect("the probe thread panicked");
        let message = message.expect("a worker thread must be warned, not silently accepted");
        assert!(message.contains("main thread"), "{message}");
        // D15: name the fix, not just the fault.
        assert!(message.contains("Move this call"), "{message}");
        // Name the symptom too, because the symptom is *nothing happening* and
        // a reader who has not seen it before will not connect the two.
        assert!(message.contains("never do anything"), "{message}");
    }

    #[test]
    fn the_claiming_thread_is_not_warned() {
        claim();
        // Whichever thread won the latch, *that* thread is happy. Asking from
        // the claimant's own thread is the case this asserts; libtest gives
        // each test its own thread, so this is only meaningful when this test
        // happened to claim.
        if is_main() == Some(true) {
            assert_eq!(check("the tray"), None);
        }
    }
}
