//! Scrozz's user interface: surfaces, design tokens, motion, and the screenshot
//! harness.
//!
//! # The capture stack
//!
//! The primary surface, and per decision D12 the primary interface of the whole
//! app. It is a bottom-left anchored pile of capture cards that grows **upward**:
//! the newest capture is on top, the oldest at the bottom. Cards only ever move
//! downward — when the oldest exits at the bottom, everything above falls one
//! slot, like a stack settling under gravity. New cards enter from the left at
//! their destination height.
//!
//! Direction carries meaning (D21): **left dismisses, right or up begins a drag
//! onto another app, down collapses the pile into the capture dock** (D20).
//!
//! # Layers
//!
//! The crate is built in layers, each depending only on the ones above it:
//!
//! | Layer | Module | What it owns |
//! |---|---|---|
//! | Tokens | [`theme`] | Colour, space, radius, elevation, type |
//! | Time | [`motion`] | Durations, easing, springs, the virtual clock |
//! | Assets | [`icons`] | SVG → texture, rasterised once per context |
//! | Platform | [`vibrancy`] | OS window materials, where they exist |
//! | Drawing | [`paint`] | Primitives and controls built from all of the above |
//! | Surfaces | [`card`], [`stack`], [`form`], [`settings_view`], [`onboarding_view`] | The product's actual screens |
//! | Window | [`overlay_app`] | The floating window the stack lives in |
//! | Verification | [`harness`] | Headless rendering of any surface |
//!
//! # Settings and onboarding
//!
//! [`form`] is a UI-only, owned view model: rows with stable ids, described
//! kinds (toggle, text, dropdown, slider, path, shortcut, section header, and
//! a validated filename template), and current values. The app maps its own
//! settings schema onto and off of it; this crate never persists anything.
//! [`settings_view::render`] draws that form — sectioned, scrollable, with a
//! footer carrying dirty/error state and Save/Reset/Re-run-onboarding — and
//! returns the [`settings_view::SettingsAction`]s the app should apply. The
//! live shortcut recorder, with its own conflict/validation state
//! ([`form::ShortcutStatus`]), is the surface's signature control.
//!
//! [`onboarding_view`] is the first-run wizard for exactly the four D26
//! topics: the drag-out gesture, the capture hotkey, where captures go, and —
//! on Linux under a wlroots compositor — the keybinding line the user has to
//! add themselves. [`onboarding_view::OnboardingState::apply`] is a small,
//! independently testable state machine with explicit
//! [`onboarding_view::OnboardingAction::Back`]/`Next`/`Skip`/`Finish`
//! transitions; it is never a permissions wall — every path out is
//! re-runnable.
//!
//! # Driving the overlay
//!
//! [`overlay_app`] is the seam the rest of the application uses. Build an
//! [`OverlayHandle`], keep a clone, hand the other to [`OverlayApp::new`] inside
//! `eframe`'s app creator, then [`OverlayHandle::push`] captures in and
//! [`OverlayHandle::drain_events`] results out. The handle is `Send + Sync` and
//! works before the window exists, so a hotkey thread can be wired to it at
//! start-up.
//!
//! Two things the window cannot do for itself are supplied as hooks, because
//! this crate is `#![forbid(unsafe_code)]` and does not depend on
//! `scrozz-shell`: [`overlay_app::PanelHook`] converts the native window into a
//! non-activating panel, and [`overlay_app::PointerProbe`] reports the cursor
//! position while the window is passing clicks through.
//!
//! # Window captures are never composited onto
//!
//! Per decision D9 a window capture arrives with the compositor's own corner
//! radius and shadow already in its pixels, and nothing synthetic may be layered
//! over it — including the scrim behind a caption, which must take the same
//! rounding as the thing beneath it or it squares the card's corners.
//! [`card::CardChrome::for_provenance`] is the single place that decision is
//! made, and [`card::CardChrome::composites`] reports it so a test can assert
//! it.
//!
//! # Motion
//!
//! Per decision D19 motion applies to *objects*, not controls. Cards animate;
//! buttons change state instantly, because instant feedback reads as more
//! responsive, not less. [`paint`]'s controls therefore contain no animation at
//! all — by design, not by omission.
//!
//! Per decision D13 the OS reduce-motion setting collapses every duration to
//! zero through a single choke point, [`motion::Motion::resolve`]. There is no
//! second path by which a duration can reach a curve.
//!
//! # Screenshots are generated
//!
//! Per decision D25 no product screenshot is ever taken by hand. [`harness`]
//! renders any surface headlessly, with a fixed seed and a virtual clock, so the
//! same code produces golden-image tests, store assets and README imagery. The
//! virtual clock is what makes a motion-heavy UI testable at all: a test renders
//! a *named instant* such as "card entry at 180 ms" rather than whichever frame
//! it happened to catch.
//!
//! That is a constraint on the whole crate, not a feature of the harness:
//! **no drawing code may read a clock.** Time arrives as [`motion::Motion`], a
//! value passed down the call tree, and a surface given the same `Motion` and
//! the same state paints the same pixels every time.

#![forbid(unsafe_code)]

pub mod card;
pub mod form;
pub mod harness;
pub mod icons;
pub mod motion;
pub mod onboarding_view;
pub mod overlay_app;
pub mod paint;
pub mod settings_view;
pub mod stack;
pub mod theme;
pub mod vibrancy;

pub use card::{CardAction, CardChrome, CardContent, CardResponse};
pub use form::{
    ApplyOutcome, Row, RowChange, RowId, RowKind, SettingsForm, ShortcutChord, ShortcutStatus,
    Validation,
};
pub use motion::{Activity, Duration, Ease, Motion, MotionPrefs};
pub use onboarding_view::{OnboardingAction, OnboardingOutcome, OnboardingState, OnboardingTopic};
pub use overlay_app::{
    CaptureRequest, DismissReason, OverlayApp, OverlayEvent, OverlayGeometry, OverlayHandle,
    OverlayOptions, PanelHook, PanelReport, Passthrough, PointerProbe,
};
pub use settings_view::{SettingsAction, SettingsResponse};
pub use theme::{Appearance, Elevation, Palette, Radius, Space, Text, Theme};
