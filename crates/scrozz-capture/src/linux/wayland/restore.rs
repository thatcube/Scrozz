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
//! A process-wide negotiation mutex and [`TokenFileLock`] together span
//! load → portal use → rotation → persistence. This prevents both another backend
//! and another process from reusing or overwriting a single-use token mid-flight.
//!
//! # Format
//!
//! One `key<TAB>token` pair per line. Deliberately not JSON: the crate has no
//! serialisation dependency, and a format this small is easier to verify by
//! reading than a dependency would be to justify.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use scrozz_core::DisplayId;

const MAX_TOKEN_FILE_BYTES: u64 = 64 * 1024;

#[cfg(unix)]
fn open_private(options: &mut OpenOptions, path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    options
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the Wayland portal token path is not a private, singly-linked regular file owned by \
             this user",
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_private(options: &mut OpenOptions, path: &Path) -> std::io::Result<File> {
    options.open(path)
}

#[cfg(unix)]
fn harden_existing_token(path: &Path) -> std::io::Result<()> {
    let mut options = File::options();
    options.read(true);
    match open_private(&mut options, path) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    }
}

#[cfg(not(unix))]
fn harden_existing_token(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// A restore token as issued by the portal.
///
/// Opaque and compositor-specific. GNOME's are UUIDs, KDE's are not; nothing
/// here should assume either.
pub type Token = String;

/// Cross-process lock for one restore-token store.
///
/// The OS releases it if the process exits, unlike a sentinel file. Callers hold
/// it across load, portal use, rotation, and persistence so a single-use token is
/// never presented concurrently.
pub struct TokenFileLock(File);

impl TokenFileLock {
    /// Acquires the advisory lock associated with `path`.
    pub fn acquire(path: &Path) -> std::io::Result<Self> {
        let file = Self::open(path)?;
        file.lock()?;
        Ok(Self(file))
    }

    /// Attempts the advisory lock without blocking.
    pub fn try_acquire(path: &Path) -> std::io::Result<Option<Self>> {
        let file = Self::open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self(file))),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(err)) => Err(err),
        }
    }

    fn open(path: &Path) -> std::io::Result<File> {
        if let Some(parent) = path.parent() {
            create_dir_all_durable(parent)?;
        }
        // The token authorises future screen capture until the user revokes it.
        // Harden an older permissive store before the caller reads it.
        harden_existing_token(path)?;
        let mut lock_name = path.as_os_str().to_owned();
        lock_name.push(".lock");
        let mut options = File::options();
        options.create(true).truncate(false).read(true).write(true);
        open_private(&mut options, &PathBuf::from(lock_name))
    }
}

fn create_dir_all_durable(path: &Path) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut candidate = Some(path);
    while let Some(directory) = candidate {
        if directory.try_exists()? {
            break;
        }
        missing.push(directory.to_path_buf());
        candidate = directory.parent();
    }

    std::fs::create_dir_all(path)?;
    for directory in missing.iter().rev() {
        if let Some(parent) = directory.parent() {
            sync_directory(parent)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

impl Drop for TokenFileLock {
    fn drop(&mut self) {
        if let Err(err) = self.0.unlock() {
            tracing::warn!(%err, "could not unlock the Wayland portal token store");
        }
    }
}

/// The kinds of session a token can belong to.
///
/// Tokens are not interchangeable: a grant for one monitor does not authorise
/// another, and reusing a display token for a window session earns a fresh
/// prompt at best. Keying by what was asked for keeps them apart.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum TokenKey {
    /// A grant for one exact display.
    ///
    /// The string is the complete, encoded on-disk key. Encoding keeps tabs,
    /// newlines, and compositor-specific display names out of the line-oriented
    /// token file.
    Display(String),
    /// The pre-display-identity spelling used by older Scrozz builds.
    ///
    /// New captures use [`Self::Display`]. This key is read only as a migration
    /// fallback, and only retained if the restored stream is independently
    /// verified against the requested display geometry.
    Monitor,
    /// A grant covering a single window, chosen by the user in the picker.
    Window,
    /// A grant covering the whole virtual desktop.
    AllDisplays,
}

impl TokenKey {
    /// A key scoped to one display identity.
    #[must_use]
    pub fn display(id: &DisplayId) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";

        let mut key = String::with_capacity("display:".len() + id.0.len() * 2);
        key.push_str("display:");
        for byte in id.0.bytes() {
            key.push(char::from(HEX[usize::from(byte >> 4)]));
            key.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Self::Display(key)
    }

    /// The stable on-disk spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Display(key) => key,
            Self::Monitor => "monitor",
            Self::Window => "window",
            Self::AllDisplays => "all-displays",
        }
    }

    /// Whether this key names an exact display.
    #[must_use]
    pub const fn is_display(&self) -> bool {
        matches!(self, Self::Display(_))
    }

    /// Parses the on-disk spelling.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "monitor" => Some(Self::Monitor),
            "window" => Some(Self::Window),
            "all-displays" => Some(Self::AllDisplays),
            _ => {
                let encoded = text.strip_prefix("display:")?;
                (!encoded.is_empty()
                    && encoded.len() % 2 == 0
                    && encoded.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .then(|| Self::Display(text.to_ascii_lowercase()))
            }
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
    pub fn get(&self, key: &TokenKey) -> Option<&str> {
        self.tokens.get(key.as_str()).map(String::as_str)
    }

    /// Chooses the token to try for `key`.
    ///
    /// Exact-display keys may fall back once to the legacy generic monitor key.
    /// The caller must verify the restored stream geometry before retaining its
    /// replacement token; this method only supports safe migration and never
    /// establishes target identity itself.
    #[must_use]
    pub fn candidate(&self, key: &TokenKey) -> Option<(TokenKey, &str)> {
        self.get(key).map(|token| (key.clone(), token)).or_else(|| {
            key.is_display()
                .then_some(TokenKey::Monitor)
                .and_then(|legacy| self.get(&legacy).map(|token| (legacy, token)))
        })
    }

    /// Records a token, replacing any previous one.
    ///
    /// An empty token is treated as a removal. The portal returns an empty
    /// string when the user declined persistence, and storing that would make
    /// every later attempt send a token the portal must reject.
    pub fn set(&mut self, key: &TokenKey, token: &str) {
        if token.is_empty() {
            self.tokens.remove(key.as_str());
        } else {
            self.tokens
                .insert(key.as_str().to_owned(), token.to_owned());
        }
    }

    /// Discards a token the portal refused.
    ///
    /// Called when a restore attempt fails, so the next capture presents the
    /// picker cleanly instead of retrying a token that will never work again.
    pub fn invalidate(&mut self, key: &TokenKey) {
        self.tokens.remove(key.as_str());
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
            let Some(key) = TokenKey::parse(key) else {
                continue;
            };
            if token.is_empty() {
                continue;
            }
            store
                .tokens
                .insert(key.as_str().to_owned(), token.to_owned());
        }
        store
    }
}

/// Loads a token store without following a replaced leaf symlink.
///
/// Callers hold [`TokenFileLock`], which serializes cooperating Scrozz
/// processes; the descriptor-based checks also fail closed if another local
/// process swaps the path between metadata inspection and open.
pub fn load(path: &Path) -> std::io::Result<TokenStore> {
    let mut options = File::options();
    options.read(true);
    let file = open_private(&mut options, path)?;
    if file.metadata()?.len() > MAX_TOKEN_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the Wayland portal token store exceeds its size limit",
        ));
    }

    let mut bytes = Vec::new();
    file.take(MAX_TOKEN_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TOKEN_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the Wayland portal token store grew beyond its size limit while being read",
        ));
    }
    Ok(TokenStore::parse(&String::from_utf8_lossy(&bytes)))
}

/// Atomically persists a token store and makes the replacement durable.
///
/// Callers hold [`TokenFileLock`] across this operation. The fixed temporary
/// name is therefore private to one writer even when several Scrozz processes
/// start together.
pub fn persist(path: Option<&Path>, store: &TokenStore) -> std::io::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        create_dir_all_durable(parent)?;
    }

    let mut temporary_name = path.as_os_str().to_owned();
    temporary_name.push(".tmp");
    let temporary = PathBuf::from(temporary_name);
    let result = (|| {
        let content = store.serialise();
        if content.len() as u64 > MAX_TOKEN_FILE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the Wayland portal token store exceeds its size limit",
            ));
        }

        match std::fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        let mut file = open_private(&mut options, &temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Persists a token update, removing the old store if the replacement fails.
///
/// A successful ScreenCast `Start` may consume the token already on disk before
/// issuing its replacement. Leaving that old value behind after a failed write
/// would make every later process repeat a restore that can no longer succeed.
/// Removing the whole store sacrifices unrelated grants but is the only
/// fail-closed outcome when the exact replacement cannot be made durable.
pub fn persist_fail_closed(path: Option<&Path>, store: &TokenStore) -> std::io::Result<()> {
    match persist(path, store) {
        Ok(()) => Ok(()),
        Err(write_error) => {
            let Some(path) = path else {
                return Err(write_error);
            };
            match remove_store(path) {
                Ok(()) => Err(write_error),
                Err(remove_error) => Err(std::io::Error::new(
                    write_error.kind(),
                    format!(
                        "{write_error}; the previous restore-token store also could not be removed: \
                         {remove_error}"
                    ),
                )),
            }
        }
    }
}

/// Removes a restore-token store without following a leaf link.
pub fn remove_store(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the restore-token path has no parent directory",
        )
    })?;
    match std::fs::remove_file(path) {
        Ok(()) => sync_directory(parent),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
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
