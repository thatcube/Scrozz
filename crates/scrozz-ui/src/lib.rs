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
//! | Surfaces | [`card`], [`stack`] | The product's actual screens |
//! | Window | [`recent_captures_overlay`] | The floating window the stack lives in |
//! | Verification | [`harness`] | Headless rendering of any surface |
//!
//! # Driving the overlay
//!
//! [`recent_captures_overlay`] is the seam the rest of the application uses.
//! Build a [`RecentCapturesOverlayHandle`], keep a clone, hand the other to
//! [`RecentCapturesOverlayApp::new`] inside `eframe`'s app creator, then
//! [`RecentCapturesOverlayHandle::push`] captures in and
//! [`RecentCapturesOverlayHandle::drain_events`] results out. The handle is
//! `Send + Sync` and
//! works before the window exists, so a hotkey thread can be wired to it at
//! start-up.
//!
//! Two things the window cannot do for itself are supplied as hooks, because
//! this crate is `#![forbid(unsafe_code)]` and does not depend on
//! `scrozz-shell`: [`recent_captures_overlay::PanelHook`] converts the native
//! window into a non-activating panel, and
//! [`recent_captures_overlay::PointerProbe`] reports the cursor
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

pub mod camera_settings;
pub mod card;
pub mod cloud_settings;
mod crop_chrome;
pub mod editor;
pub mod harness;
pub mod history;
pub mod icons;
pub mod motion;
pub mod onboarding;
pub mod paint;
pub mod permission;
pub mod pinned;
pub mod recent_captures_overlay;
mod recording_controls;
pub mod recording_settings;
pub mod scrolling;
pub mod select;
pub mod sensitive;
pub mod settings;
pub mod stack;
pub mod theme;
pub mod vibrancy;
pub mod video_editor;

pub use camera_settings::{
    CAMERA_SETTINGS_WINDOW_TITLE, CameraLiveModel, CameraLiveSnapshot, CameraSettingsAction,
    CameraSettingsModel, CameraSettingsPanel, CameraSettingsResponse, CameraSettingsSnapshot,
    show_window as show_camera_settings_window,
    viewport_builder as camera_settings_viewport_builder,
    viewport_id as camera_settings_viewport_id,
};
pub use card::{CardAction, CardChrome, CardContent, CardMedia, CardResponse};
pub use cloud_settings::{
    CloudConnectionState, CloudCredentialView, CloudSettingsDraft, CloudSettingsEvent,
    CloudSettingsModel, CloudSettingsPreview, CloudSettingsWindow, CredentialDraft,
    SettingsPlatform,
};
pub use history::{
    DateFilter, HistoryAction, HistoryEntry, HistoryFilters, HistoryPage, HistoryThumbnail,
    HistoryViewModel,
};
pub use motion::{Activity, Duration, Ease, Motion, MotionPrefs};
pub use onboarding::{OcrOnboarding, OnboardingResponse};
pub use recent_captures_overlay::{
    CaptureMedia, CaptureRequest, DismissReason, NativePassthrough, PanelHook, PanelReport,
    PanelSetup, Passthrough, PointerProbe, RecentCapturesOverlayApp, RecentCapturesOverlayEvent,
    RecentCapturesOverlayGeometry, RecentCapturesOverlayHandle, RecentCapturesOverlayOptions,
    RecentCapturesOverlaySettings, RecentCapturesPlacement,
};
pub use recording_settings::{
    RecordingSettingsAction, RecordingSettingsPanel, RecordingSettingsResponse,
};
pub use scrolling::{
    ScrollHudAction, ScrollHudResponse, ScrollHudState, ScrollHudStatus, ScrollHudSurface,
    ScrollPreviewGeometry, ScrollingHud,
};

pub use select::{
    AxisDirection, DisplayLayout, DragModifiers, FrozenDesktop, FrozenDisplayFrame, FrozenPixel,
    HudEntry, HudModel, HudNav, MagnifierCell, MagnifierConfig, MagnifierGrid, ResizeHandle,
    SelectionAnnouncement, SelectionDecision, SelectionScene, SelectionState, SelectionUi,
};
pub use sensitive::{
    FindingDecision, SensitiveReview, SensitiveReviewResponse, SensitiveReviewScene,
};
pub use theme::{Appearance, Elevation, Palette, Radius, Space, Text, Theme};
pub use video_editor::{
    TranscodeView, VIDEO_EDITOR_WINDOW_TITLE, VideoEditor, VideoEditorAction, VideoEditorControls,
    VideoEditorLayout, VideoEditorModel, VideoEditorResponse, VideoEditorSnapshot, VideoPreview,
    show_window as show_video_editor_window, viewport_builder as video_editor_viewport_builder,
    viewport_id as video_editor_viewport_id,
};
