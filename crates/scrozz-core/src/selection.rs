//! How a platform lets the user pick a window, and what it will give back.
//!
//! # Why this is a contract rather than a per-backend detail
//!
//! Interactive window capture is the same gesture everywhere — point at a
//! window, see it highlighted, click it — but the mechanism underneath differs
//! so completely that pretending otherwise produces a broken app on one
//! platform. macOS, Windows and X11 all let Scrozz enumerate windows and draw
//! its own highlight. **Wayland does not, on any compositor, by design.** There
//! the selection happens inside `xdg-desktop-portal`, in a process Scrozz
//! cannot see into, and the application learns only what the user chose.
//!
//! The interesting failure is not "the portal is different". It is that a
//! caller written against the enumerating platforms will *silently* do the wrong
//! thing on Wayland: enumerate nothing, highlight nothing, and either show an
//! empty picker or fabricate a list where every entry fails at capture time.
//! Decision D8 forbids papering over that gap, so it is modelled here as a value
//! the caller must match on — [`WindowSelection`] — rather than discovered as an
//! error after the overlay is already on screen.
//!
//! # The shadow toggle is the same shape of problem
//!
//! CAP-13 offers "shadow on/off" for window captures, and D9 says window pixels
//! are sacred: the OS's own shadow is the truth and nothing synthetic may be
//! drawn in its place. Those two only coexist where the *compositor* can be
//! asked to omit the shadow. Where it cannot, the honest answer is that the
//! toggle does not exist here — not a toggle that quietly does nothing, and
//! certainly not one that fakes a shadow. [`ShadowSupport`] is that answer, and
//! it is reported per backend so the UI can disable the control with a reason
//! instead of lying about it.

use serde::{Deserialize, Serialize};

/// How the user chooses a window on this platform.
///
/// Returned by [`crate::WindowPicking::window_selection`] so a caller can decide
/// *before* putting anything on screen which of two entirely different flows it
/// is running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowSelection {
    /// Scrozz enumerates windows and hosts the picker itself.
    ///
    /// macOS, Windows and X11. [`crate::TargetEnumerator::windows`] returns a
    /// usable list, hover highlighting is ours to draw, and the chosen
    /// [`crate::WindowId`] can be captured directly.
    InProcess,

    /// The compositor owns the picker; Scrozz never sees a window list.
    ///
    /// Wayland. The portal shows its own chooser out-of-process and hands back
    /// a stream for the window the user picked. There is nothing to hover and
    /// nothing to highlight, because there is nothing to enumerate — so a caller
    /// that matches on this must skip the overlay entirely rather than open an
    /// empty one.
    PortalPicker {
        /// The portal interface that performs the selection, for diagnostics
        /// and for the message shown while the portal dialog is up.
        portal: String,
    },

    /// Window selection cannot work at all here.
    ///
    /// A headless session, or a compositor with neither enumeration nor a
    /// portal. Carries the sentence to show the user, because "unavailable" with
    /// no reason is indistinguishable from a bug.
    Unavailable {
        /// Why, in the user's terms.
        why: String,
    },
}

impl WindowSelection {
    /// Whether Scrozz draws the picker itself.
    ///
    /// The single question every call site actually asks. Written as a method so
    /// adding a fourth mechanism later cannot silently take the wrong branch in
    /// code that only matched two.
    #[must_use]
    pub const fn is_in_process(&self) -> bool {
        matches!(self, Self::InProcess)
    }

    /// Whether the platform can select a window at all, by any mechanism.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        !matches!(self, Self::Unavailable { .. })
    }

    /// A short, stable identifier for logs, JSON output and tests.
    #[must_use]
    pub const fn slug(&self) -> &'static str {
        match self {
            Self::InProcess => "in-process",
            Self::PortalPicker { .. } => "portal",
            Self::Unavailable { .. } => "unavailable",
        }
    }
}

/// Whether a window's own shadow can be included or omitted on request.
///
/// D9's fourth acceptance criterion asks for the shadow as a *separable layer*.
/// Separable means the compositor can be asked for the window with it or without
/// it. Where that is true the toggle is real; where it is not, the toggle must
/// be absent rather than inert, and synthesising a shadow to fill the gap is
/// forbidden outright.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShadowSupport {
    /// The compositor honours both answers. The control is enabled.
    Toggleable,

    /// The capture source neither controls nor reports whether a shadow is present.
    ///
    /// The control is hidden and capture metadata remains unknown. This is
    /// distinct from [`Self::AlwaysExcluded`]: claiming "off" would fabricate a
    /// fact about pixels the backend cannot inspect independently.
    Unchecked {
        /// Why no reliable shadow state is available, in the user's terms.
        why: String,
    },

    /// Every capture arrives with the shadow, and it cannot be removed.
    ///
    /// The control is shown disabled and pinned on, with `why` as its
    /// explanation. Cropping the shadow off afterwards is not offered: the
    /// shadow is antialiased into the edge pixels, so "removing" it means
    /// guessing where the window ends, which is exactly the guessed geometry D9
    /// exists to prevent.
    AlwaysIncluded {
        /// Why it cannot be dropped, in the user's terms.
        why: String,
    },

    /// No capture ever contains the shadow, and one cannot be added.
    ///
    /// The control is shown disabled and pinned off. Drawing a shadow here would
    /// be a *synthetic* shadow at a guessed offset, blur and opacity — a
    /// fabrication that D9 forbids for exactly the reason it forbids synthetic
    /// corners.
    AlwaysExcluded {
        /// Why it is absent, in the user's terms.
        why: String,
    },
}

impl ShadowSupport {
    /// Whether the user may change the setting.
    #[must_use]
    pub const fn is_toggleable(&self) -> bool {
        matches!(self, Self::Toggleable)
    }

    /// What the shadow flag actually resolves to, given what the user asked for.
    ///
    /// A platform that cannot honour the request wins, because the returned
    /// value is what the pixels will show — and a request field that disagrees
    /// with the image is how a capture ends up labelled wrongly in history.
    #[must_use]
    pub const fn resolve(&self, requested: bool) -> bool {
        match self {
            Self::Toggleable => requested,
            Self::Unchecked { .. } => false,
            Self::AlwaysIncluded { .. } => true,
            Self::AlwaysExcluded { .. } => false,
        }
    }

    /// The explanation to show beside a disabled control, if it is disabled.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Toggleable => None,
            Self::Unchecked { why }
            | Self::AlwaysIncluded { why }
            | Self::AlwaysExcluded { why } => Some(why),
        }
    }
}

/// What a backend can do for interactive window capture.
///
/// One value, fetched once, that answers every question the picker flow needs to
/// ask before it draws anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowPickingCapability {
    /// How the user chooses a window.
    pub selection: WindowSelection,
    /// Whether the shadow toggle is real here.
    pub shadow: ShadowSupport,
    /// Whether captured window pixels carry genuine transparency outside the
    /// window's rounded corners.
    ///
    /// D9's second criterion. Where this is `false` the backend is reporting a
    /// known fidelity gap — an opaque matte where the corner should be
    /// see-through — and the honest thing is to say so rather than round the
    /// corners ourselves at a guessed radius, which is precisely the defect D9
    /// was written about.
    pub native_alpha: bool,
}

impl WindowPickingCapability {
    /// The capability of a platform that enumerates windows itself.
    #[must_use]
    pub const fn in_process(shadow: ShadowSupport, native_alpha: bool) -> Self {
        Self {
            selection: WindowSelection::InProcess,
            shadow,
            native_alpha,
        }
    }
}

/// The application a capture came from.
///
/// Recorded at capture time and carried with the image, because it cannot be
/// recovered later: by the time a capture is shown in history the window may be
/// closed and the process gone. History badges are the visible consumer, but the
/// same value is what lets a filename default to `Safari 2026-08-27.png` rather
/// than `Screenshot 3.png`.
///
/// Every field is optional and independently so. A window may have an owner with
/// no title, a title with no resolvable owner, or — on Wayland — neither, because
/// the portal deliberately does not disclose which client it captured. An empty
/// value here means "the OS did not say", never "there is none".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceApp {
    /// The application's display name, e.g. "Safari".
    pub name: Option<String>,
    /// A stable per-platform identifier: bundle id, executable name, or
    /// `WM_CLASS`. Survives localisation and renaming, which display names do
    /// not, so it is what an icon lookup or a per-app rule should key on.
    pub identifier: Option<String>,
    /// The window's title at capture time.
    pub window_title: Option<String>,
}

impl SourceApp {
    /// Whether the OS told us anything at all.
    #[must_use]
    pub fn is_known(&self) -> bool {
        self.name.is_some() || self.identifier.is_some() || self.window_title.is_some()
    }

    /// The best available label for a badge, preferring the app over the title.
    ///
    /// A badge has room for one short string, and the app name is the more
    /// useful of the two: window titles are long, change constantly, and are
    /// frequently just the document name. Falls through to the identifier
    /// because "com.apple.Safari" still tells a user more than nothing.
    #[must_use]
    pub fn badge(&self) -> Option<&str> {
        self.name
            .as_deref()
            .or(self.identifier.as_deref())
            .or(self.window_title.as_deref())
    }

    /// Builds metadata from an enumerated window.
    ///
    /// Blank strings are dropped rather than stored: a badge rendering an empty
    /// string is a visible gap where a user reads a bug.
    #[must_use]
    pub fn from_window(window: &crate::Window) -> Self {
        fn present(value: Option<&String>) -> Option<String> {
            value
                .map(|text| text.trim())
                .filter(|text| !text.is_empty())
                .map(std::borrow::ToOwned::to_owned)
        }

        Self {
            name: present(window.application.as_ref()),
            identifier: present(window.application_id.as_ref()),
            window_title: present(window.title.as_ref()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_platform_that_cannot_drop_the_shadow_overrules_the_request() {
        let pinned = ShadowSupport::AlwaysIncluded {
            why: "the compositor composites the shadow into the window's own surface".to_owned(),
        };
        // The user asked for no shadow; the pixels will have one anyway, and the
        // resolved value is what the pixels show.
        assert!(pinned.resolve(false));
        assert!(pinned.resolve(true));
        assert!(!pinned.is_toggleable());
        assert!(pinned.reason().is_some());
    }

    #[test]
    fn a_platform_that_never_has_a_shadow_never_reports_one() {
        let absent = ShadowSupport::AlwaysExcluded {
            why: "X11 has no compositor-drawn window shadow to capture".to_owned(),
        };
        assert!(!absent.resolve(true));
        assert!(!absent.resolve(false));
    }

    #[test]
    fn a_toggleable_shadow_returns_exactly_what_was_asked_for() {
        assert!(ShadowSupport::Toggleable.resolve(true));
        assert!(!ShadowSupport::Toggleable.resolve(false));
        assert!(ShadowSupport::Toggleable.reason().is_none());
    }

    #[test]
    fn the_portal_is_available_but_never_in_process() {
        let portal = WindowSelection::PortalPicker {
            portal: "org.freedesktop.portal.ScreenCast".to_owned(),
        };
        assert!(portal.is_available());
        assert!(
            !portal.is_in_process(),
            "a caller must not open our own picker over the portal's"
        );
        assert_eq!(portal.slug(), "portal");
    }

    #[test]
    fn an_unavailable_selection_is_not_available() {
        let none = WindowSelection::Unavailable {
            why: "no display server".to_owned(),
        };
        assert!(!none.is_available());
        assert!(!none.is_in_process());
    }

    #[test]
    fn source_app_prefers_the_application_name_for_a_badge() {
        let app = SourceApp {
            name: Some("Safari".to_owned()),
            identifier: Some("com.apple.Safari".to_owned()),
            window_title: Some("GitHub".to_owned()),
        };
        assert_eq!(app.badge(), Some("Safari"));
        assert!(app.is_known());
    }

    #[test]
    fn source_app_falls_back_through_identifier_to_title() {
        let by_id = SourceApp {
            identifier: Some("firefox".to_owned()),
            ..SourceApp::default()
        };
        assert_eq!(by_id.badge(), Some("firefox"));

        let by_title = SourceApp {
            window_title: Some("Untitled".to_owned()),
            ..SourceApp::default()
        };
        assert_eq!(by_title.badge(), Some("Untitled"));
    }

    #[test]
    fn an_unknown_source_app_has_no_badge() {
        let unknown = SourceApp::default();
        assert!(!unknown.is_known());
        assert_eq!(unknown.badge(), None);
    }

    #[test]
    fn blank_titles_are_dropped_rather_than_badged_as_empty() {
        let window = crate::Window {
            id: crate::WindowId("1".to_owned()),
            title: Some("   ".to_owned()),
            application: Some(String::new()),
            application_id: Some("org.gnome.Nautilus".to_owned()),
            bounds: crate::LogicalRect::default(),
            picker_bounds: None,
            corner_radius: None,
            display: crate::DisplayId("d".to_owned()),
            is_visible: true,
        };
        let app = SourceApp::from_window(&window);
        assert_eq!(app.name, None);
        assert_eq!(app.window_title, None);
        assert_eq!(app.badge(), Some("org.gnome.Nautilus"));
    }
}
