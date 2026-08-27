//! Persisting ScreenCast restore tokens.
//!
//! # Why this exists at all
//!
//! `xdg-desktop-portal`'s ScreenCast interface shows a system dialog asking the
//! user which screen to share. Without persistence it shows that dialog on
//! *every* capture. A screenshot tool that asks permission each time a key is
//! pressed is not a screenshot tool, so this is not an optimisation — it is the
//! difference between the Wayland backend being usable and not.
//!
//! The mechanism: passing `persist_mode = 2` ("persist until explicitly
//! revoked") makes the portal return a `restore_token` alongside the session.
//! Handing that token back on the next `SelectSources` call reuses the previous
//! grant and skips the dialog. Tokens are single-use — the portal issues a fresh
//! one each time — so the store must be rewritten after every session, and a
//! stale token must fail softly back to the prompt rather than fail the capture.
//!
//! # Format
//!
//! One `key<TAB>token` pair per line. Deliberately not JSON: the crate has no
//! serialisation dependency, and a format this small is easier to verify by
//! reading than a dependency would be to justify.

use std::collections::BTreeMap;

/// A restore token as issued by the portal.
///
/// Opaque and compositor-specific. GNOME's are UUIDs, KDE's are not; nothing
/// here should assume either.
pub type Token = String;

/// The kinds of session a token can belong to.
///
/// Tokens are not interchangeable: a grant for one monitor does not authorise
/// another, and reusing a display token for a window session earns a fresh
/// prompt at best. Keying by what was asked for keeps them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TokenKey {
    /// A grant covering a single monitor, chosen by the user in the picker.
    Monitor,
    /// A grant covering a single window, chosen by the user in the picker.
    Window,
    /// A grant covering the whole virtual desktop.
    AllDisplays,
    /// A grant bound to one stable display-identity fingerprint.
    Display(u64),
}

impl TokenKey {
    /// The stable on-disk spelling.
    #[must_use]
    pub fn storage_key(self) -> String {
        match self {
            Self::Monitor => "monitor".into(),
            Self::Window => "window".into(),
            Self::AllDisplays => "all-displays".into(),
            Self::Display(fingerprint) => format!("display-{fingerprint:016x}"),
        }
    }

    /// Scopes a token to an opaque display id without writing that id to disk.
    #[must_use]
    pub fn for_display(id: &str) -> Self {
        let fingerprint = id.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
        Self::Display(fingerprint)
    }

    /// Parses the on-disk spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "monitor" => Some(Self::Monitor),
            "window" => Some(Self::Window),
            "all-displays" => Some(Self::AllDisplays),
            _ => text
                .strip_prefix("display-")
                .and_then(|value| u64::from_str_radix(value, 16).ok())
                .map(Self::Display),
        }
    }
}

/// An in-memory view of the token file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenStore {
    tokens: BTreeMap<String, Token>,
}

impl TokenStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The token for a session kind, if one was saved.
    #[must_use]
    pub fn get(&self, key: TokenKey) -> Option<&str> {
        self.tokens.get(&key.storage_key()).map(String::as_str)
    }

    /// Records a token, replacing any previous one.
    ///
    /// An empty token is treated as a removal. The portal returns an empty
    /// string when the user declined persistence, and storing that would make
    /// every later attempt send a token the portal must reject.
    pub fn set(&mut self, key: TokenKey, token: &str) {
        let key = key.storage_key();
        if token.is_empty() {
            self.tokens.remove(&key);
        } else {
            self.tokens.insert(key, token.to_owned());
        }
    }

    /// Discards a token the portal refused.
    ///
    /// Called when a restore attempt fails, so the next capture presents the
    /// picker cleanly instead of retrying a token that will never work again.
    pub fn invalidate(&mut self, key: TokenKey) {
        self.tokens.remove(&key.storage_key());
    }

    /// Whether anything is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Renders the store for writing.
    #[must_use]
    pub fn serialise(&self) -> String {
        let mut out = String::from(
            "# Scrozz xdg-desktop-portal restore tokens.\n\
             # Deleting this file makes the next capture ask permission again.\n",
        );
        for (key, token) in &self.tokens {
            out.push_str(key);
            out.push('\t');
            out.push_str(token);
            out.push('\n');
        }
        out
    }

    /// Reads a store back.
    ///
    /// Unparseable lines are skipped rather than rejected. A corrupt token file
    /// should cost one permission prompt, not a broken application.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut store = Self::new();
        for line in text.lines() {
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, token)) = line.split_once('\t') else {
                continue;
            };
            let (key, token) = (key.trim(), token.trim());
            if token.is_empty() || TokenKey::parse(key).is_none() {
                continue;
            }
            store.tokens.insert(key.to_owned(), token.to_owned());
        }
        store
    }
}

/// Where the token file belongs, per the XDG Base Directory specification.
///
/// State, not config and not cache: a restore token is machine-local, is not
/// something a user edits, and must survive a cache clear or the prompt returns.
/// `$XDG_STATE_HOME` is the specification's answer for exactly this, with
/// `~/.local/state` as its defined fallback.
///
/// Returns `None` when neither variable is set, which happens in containers and
/// on some minimal init systems; the caller then keeps tokens in memory for the
/// process lifetime, which still removes the prompt from all but the first
/// capture of a run.
#[must_use]
pub fn token_path(xdg_state_home: Option<&str>, home: Option<&str>) -> Option<std::path::PathBuf> {
    let base = match xdg_state_home.map(str::trim).filter(|s| !s.is_empty()) {
        // The specification requires an absolute path; a relative one is a
        // misconfiguration, and resolving it against the current directory
        // would scatter token files wherever the app happened to start.
        Some(state) if std::path::Path::new(state).is_absolute() => std::path::PathBuf::from(state),
        _ => {
            let home = home.map(str::trim).filter(|s| !s.is_empty())?;
            std::path::Path::new(home).join(".local").join("state")
        }
    };
    Some(base.join("scrozz").join("portal-tokens"))
}
