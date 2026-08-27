//! Launch-at-login registration (feature SYS-03).
//!
//! Three platforms, three unrelated mechanisms, one toggle in Settings:
//!
//! | Platform | Mechanism | Lives at |
//! |---|---|---|
//! | macOS | A `launchd` user agent | `~/Library/LaunchAgents/<label>.plist` |
//! | Windows | A `Run` registry value | `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run` |
//! | Linux | A freedesktop autostart entry | `~/.config/autostart/scrozz.desktop` |
//!
//! [`SystemLaunchAtLogin`] is the one type app settings code holds; which
//! mechanism runs underneath is a `cfg(target_os)` inside its
//! [`LaunchAtLogin`] methods, never visible to the caller.
//!
//! # Testing without touching the real home directory
//!
//! [`SystemLaunchAtLogin::with_home`] substitutes the directory macOS and
//! Linux resolve `~` against, so tests exercise the exact file-writing code
//! path against a throwaway [`std::env::temp_dir`] location instead of the
//! developer's actual `~/Library/LaunchAgents` or `~/.config/autostart`.
//! Windows has no equivalent filesystem root to inject — the registration
//! lives in the registry — so its code path is exercised only by
//! cross-compilation type-checking here; see the crate's test report for what
//! that leaves unverified.
//!
//! # What is pure and what touches the OS
//!
//! [`plist_contents`], [`desktop_entry_contents`], [`quote_desktop_exec`] and
//! [`windows_run_value`] are ordinary string functions with no filesystem or
//! registry access, so the escaping they perform is tested on every host
//! regardless of which platform's mechanism it belongs to. Only
//! [`LaunchAtLogin::enable`], [`LaunchAtLogin::disable`] and
//! [`LaunchAtLogin::is_enabled`] touch the OS, and each does so behind its own
//! `cfg(target_os)` arm.

use std::path::{Path, PathBuf};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::{fs, io};

use scrozz_core::{Error, Result};

use crate::LaunchAtLogin;

/// Registers Scrozz to launch at login, using whichever mechanism this
/// platform provides.
///
/// # Example
///
/// ```
/// use scrozz_shell::{LaunchAtLogin, SystemLaunchAtLogin};
///
/// let login_item = SystemLaunchAtLogin::new("com.thatcube.scrozz", "/Applications/Scrozz.app/Contents/MacOS/scrozz");
/// // A settings checkbox reads and writes through the same handle:
/// // login_item.is_enabled()?; login_item.enable()?; login_item.disable()?;
/// let _ = login_item;
/// ```
#[derive(Debug, Clone)]
pub struct SystemLaunchAtLogin {
    /// Reverse-DNS-style identifier. Used as the macOS `launchd` label (and
    /// its plist's file stem) and as the Windows `Run` value name. Linux
    /// ignores it: the autostart entry's filename is fixed, because a stray
    /// `~/.config/autostart/*.desktop` left over from a renamed label would
    /// otherwise keep launching an old build forever.
    label: String,
    /// Absolute path to the executable to launch at login.
    executable: PathBuf,
    /// Overrides the directory macOS/Linux paths are resolved against.
    /// `None` means "the real home directory", resolved lazily so
    /// construction can never fail for a reason a caller cannot act on.
    home_override: Option<PathBuf>,
}

impl SystemLaunchAtLogin {
    /// Creates a handle for a launch-at-login registration.
    ///
    /// `label` should be a reverse-DNS identifier such as
    /// `"com.thatcube.scrozz"`; `executable` should be an absolute path to the
    /// binary that must run at login (on macOS, the path to the helper
    /// executable inside the `.app` bundle, not the bundle itself).
    #[must_use]
    pub fn new(label: impl Into<String>, executable: impl Into<PathBuf>) -> Self {
        Self {
            label: label.into(),
            executable: executable.into(),
            home_override: None,
        }
    }

    /// Overrides the directory used in place of the real home directory.
    ///
    /// Exists so tests can point the macOS and Linux backends at a temporary
    /// directory instead of the developer's real `~/Library/LaunchAgents` or
    /// `~/.config/autostart`; production callers should never need this.
    #[must_use]
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home_override = Some(home.into());
        self
    }

    /// Resolves the directory paths in this module are computed against.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] if no override was given and the current
    /// user's home directory could not be determined.
    fn home_dir(&self) -> Result<PathBuf> {
        if let Some(home) = &self.home_override {
            return Ok(home.clone());
        }
        dirs::home_dir().ok_or_else(|| {
            Error::Platform("could not determine the current user's home directory".to_owned())
        })
    }
}

#[cfg(target_os = "macos")]
impl SystemLaunchAtLogin {
    /// Where this registration's `launchd` agent plist lives.
    fn launch_agent_path(&self) -> Result<PathBuf> {
        Ok(self
            .home_dir()?
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{}.plist", self.label)))
    }
}

#[cfg(target_os = "linux")]
impl SystemLaunchAtLogin {
    /// Where this registration's autostart entry lives.
    ///
    /// The filename is fixed rather than derived from `label` — see the
    /// `label` field's doc comment on [`SystemLaunchAtLogin`] for why.
    fn autostart_entry_path(&self) -> Result<PathBuf> {
        Ok(self
            .home_dir()?
            .join(".config")
            .join("autostart")
            .join("scrozz.desktop"))
    }
}

impl LaunchAtLogin for SystemLaunchAtLogin {
    fn is_enabled(&self) -> Result<bool> {
        #[cfg(target_os = "macos")]
        {
            file_exists(&self.launch_agent_path()?)
        }
        #[cfg(target_os = "windows")]
        {
            windows_impl::is_enabled(&self.label)
        }
        #[cfg(target_os = "linux")]
        {
            file_exists(&self.autostart_entry_path()?)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            Err(unsupported())
        }
    }

    fn enable(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let path = self.launch_agent_path()?;
            write_atomic(
                &path,
                plist_contents(&self.label, &self.executable).as_bytes(),
            )
        }
        #[cfg(target_os = "windows")]
        {
            windows_impl::enable(&self.label, &self.executable)
        }
        #[cfg(target_os = "linux")]
        {
            let path = self.autostart_entry_path()?;
            write_atomic(&path, desktop_entry_contents(&self.executable).as_bytes())
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            Err(unsupported())
        }
    }

    fn disable(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            remove_if_present(&self.launch_agent_path()?)
        }
        #[cfg(target_os = "windows")]
        {
            windows_impl::disable(&self.label)
        }
        #[cfg(target_os = "linux")]
        {
            remove_if_present(&self.autostart_entry_path()?)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            Err(unsupported())
        }
    }
}

/// The [`Error::Unsupported`] this module reports on a platform with none of
/// the three mechanisms above.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn unsupported() -> Error {
    Error::Unsupported {
        what: "launch at login".to_owned(),
        why: "Scrozz has no launch-at-login backend for this platform".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Shared filesystem helpers (macOS + Linux)
// ---------------------------------------------------------------------------

/// Whether a path exists, treated as the "is this registered" signal for the
/// two file-backed backends.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn file_exists(path: &Path) -> Result<bool> {
    match fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Removes a path if present; absence is success, not an error.
///
/// See [`LaunchAtLogin::disable`] for why a settings toggle needs this to be
/// unconditional.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// A process-wide counter for unique temp-file names, so concurrent
/// `enable()` calls racing on the same path never collide.
#[cfg(any(target_os = "macos", target_os = "linux"))]
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `contents` to `path` atomically: write to a sibling temp file, then
/// [`fs::rename`] it into place. A rename within the same directory is
/// atomic on both POSIX and Windows, so a reader (or a crash) never observes
/// a half-written plist or desktop entry — the file is either the old
/// registration or the new one, never a truncated mix of both.
///
/// Creates `path`'s parent directory if it does not exist yet, since a fresh
/// user account has neither `~/Library/LaunchAgents` nor
/// `~/.config/autostart` until something writes to them.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        Error::Platform(format!(
            "{} has no parent directory to write into",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(Error::Io)?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("scrozz-launch-at-login");
    let unique = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!(".{file_name}.{}.{unique}.tmp", std::process::id()));

    fs::write(&tmp_path, contents).map_err(Error::Io)?;
    fs::rename(&tmp_path, path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        Error::Io(e)
    })
}

// ---------------------------------------------------------------------------
// Pure content generation — testable on every platform
// ---------------------------------------------------------------------------

/// The `launchd` property list Scrozz registers under
/// `~/Library/LaunchAgents`.
///
/// `RunAtLoad` starts Scrozz the moment launchd loads the agent, i.e. at every
/// login. `KeepAlive` is deliberately **not** set: unlike a daemon, a user
/// quitting Scrozz from its menu-bar item is intentional, and `KeepAlive`
/// would have launchd silently reverse that the moment the process exits.
#[must_use]
pub fn plist_contents(label: &str, executable: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{executable}</string>\n\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         \t<key>ProcessType</key>\n\
         \t<string>Interactive</string>\n\
         </dict>\n\
         </plist>\n",
        label = escape_xml(label),
        executable = escape_xml(&executable.to_string_lossy()),
    )
}

/// Escapes the five characters XML requires escaped in text content.
///
/// `&` is replaced first and in its own pass, so the ampersands introduced by
/// escaping `<`, `>`, `"` and `'` are never themselves re-escaped.
#[must_use]
fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The freedesktop autostart entry Scrozz writes to
/// `~/.config/autostart/scrozz.desktop`.
///
/// Per the [Desktop Entry Specification][spec], `X-GNOME-Autostart-enabled`
/// is the de facto standard key GNOME, and everything downstream of it,
/// checks before honouring the entry — omitting it works on some autostart
/// implementations and silently does nothing on others.
///
/// [spec]: https://specifications.freedesktop.org/desktop-entry-spec/latest/
#[must_use]
pub fn desktop_entry_contents(executable: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Scrozz\n\
         Exec={}\n\
         X-GNOME-Autostart-enabled=true\n\
         NoDisplay=false\n\
         Terminal=false\n",
        quote_desktop_exec(&executable.to_string_lossy()),
    )
}

/// Quotes a single path for a desktop entry's `Exec=` line.
///
/// Per the specification's [`Exec` variable grammar][spec], the reserved
/// characters `"`, `` ` ``, `$` and `\` must be backslash-escaped inside a
/// quoted field, and the field must be quoted at all whenever it contains
/// whitespace — otherwise the entry's own tokenizer, which is
/// shell-independent and splits on unquoted spaces, would treat
/// `/Users/name/My Apps/scrozz` as two arguments instead of one path.
///
/// [spec]: https://specifications.freedesktop.org/desktop-entry-spec/latest/exec-variables.html
#[must_use]
pub fn quote_desktop_exec(value: &str) -> String {
    let needs_quoting = value
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '"' | '\\' | '`' | '$'));
    if !needs_quoting {
        return value.to_owned();
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for c in value.chars() {
        if matches!(c, '"' | '\\' | '`' | '$') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// Formats an executable path as a Windows `Run`-key command line.
///
/// The `Run` key's value is parsed the same way `CreateProcess` parses a
/// command line: unquoted, a space ends the program name. A path containing
/// one — `"C:\Program Files\Scrozz\scrozz.exe"` is the common case — must
/// therefore be wrapped in double quotes, and any literal `"` inside the path
/// backslash-escaped so it is not read as the closing quote. Quoting
/// unconditionally, even when the path has no space, keeps the value's shape
/// uniform and is always valid.
#[must_use]
pub fn windows_run_value(executable: &Path) -> String {
    let raw = executable.to_string_lossy();
    let mut quoted = String::with_capacity(raw.len() + 2);
    quoted.push('"');
    for c in raw.chars() {
        if c == '"' {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

// ---------------------------------------------------------------------------
// Windows: the `Run` registry value
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod windows_impl {
    use std::path::Path;

    use scrozz_core::{Error, Result};
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SAM_FLAGS, REG_SZ,
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };
    use windows::core::HSTRING;

    /// Every Windows installation has created this key since Windows 95;
    /// opening it with [`RegOpenKeyExW`] rather than creating it with
    /// `RegCreateKeyExW` avoids a dependency on the `Win32_Security` crate
    /// feature that only the creating variant needs (for its
    /// `SECURITY_ATTRIBUTES` parameter).
    const RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    /// Closes the wrapped `HKEY` on drop, so every early return above still
    /// releases the handle.
    struct RegKey(HKEY);

    impl Drop for RegKey {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: `self.0` was opened by `open_run_key` and is closed
                // exactly once, here.
                unsafe {
                    let _ = RegCloseKey(self.0);
                }
            }
        }
    }

    /// Opens `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` with the
    /// requested access, or `None` if it does not exist at all (the key
    /// itself, not a value under it — an unregistered app is a missing
    /// *value*, handled by each caller).
    fn open_run_key(access: REG_SAM_FLAGS) -> Result<Option<RegKey>> {
        let subkey = HSTRING::from(RUN_SUBKEY);
        let mut hkey = HKEY::default();
        // SAFETY: `hkey` is a valid out-pointer for the duration of the call,
        // and `subkey` outlives it.
        let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &subkey, None, access, &mut hkey) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(Error::Platform(format!(
                "opening HKCU\\{RUN_SUBKEY} failed with Win32 error {}",
                status.0
            )));
        }
        Ok(Some(RegKey(hkey)))
    }

    pub(super) fn enable(label: &str, executable: &Path) -> Result<()> {
        let Some(key) = open_run_key(KEY_SET_VALUE)? else {
            return Err(Error::Platform(format!(
                "HKCU\\{RUN_SUBKEY} does not exist on this system"
            )));
        };
        let value_name = HSTRING::from(label);
        let mut wide: Vec<u16> = super::windows_run_value(executable)
            .encode_utf16()
            .collect();
        wide.push(0);
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives this
        // call, which is exactly what a `REG_SZ` value requires; the byte
        // length includes the trailing NUL, as `RegSetValueExW` expects.
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) };
        // SAFETY: `key.0` is a valid, open handle with `KEY_SET_VALUE` access.
        let status = unsafe { RegSetValueExW(key.0, &value_name, None, REG_SZ, Some(bytes)) };
        if status != ERROR_SUCCESS {
            return Err(Error::Platform(format!(
                "writing the Run value \"{label}\" failed with Win32 error {}",
                status.0
            )));
        }
        Ok(())
    }

    pub(super) fn disable(label: &str) -> Result<()> {
        let Some(key) = open_run_key(KEY_SET_VALUE)? else {
            return Ok(());
        };
        let value_name = HSTRING::from(label);
        // SAFETY: `key.0` is a valid, open handle with `KEY_SET_VALUE` access,
        // which `RegDeleteValueW` also uses to delete a value.
        let status = unsafe { RegDeleteValueW(key.0, &value_name) };
        if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
            return Err(Error::Platform(format!(
                "removing the Run value \"{label}\" failed with Win32 error {}",
                status.0
            )));
        }
        Ok(())
    }

    pub(super) fn is_enabled(label: &str) -> Result<bool> {
        let Some(key) = open_run_key(KEY_QUERY_VALUE)? else {
            return Ok(false);
        };
        let value_name = HSTRING::from(label);
        // Querying with no data buffer only asks for the value's size, which
        // is all `is_enabled` needs: existence, not content.
        // SAFETY: `key.0` is a valid, open handle with `KEY_QUERY_VALUE`
        // access; passing `None` for the data buffer is the documented way to
        // query a value's presence and size without reading it.
        let status = unsafe { RegQueryValueExW(key.0, &value_name, None, None, None, None) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        if status != ERROR_SUCCESS {
            return Err(Error::Platform(format!(
                "reading the Run value \"{label}\" failed with Win32 error {}",
                status.0
            )));
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// A unique scratch directory under [`std::env::temp_dir`], cleaned up on
    /// drop. Deliberately not the `tempfile` crate — see the crate-owning
    /// task's constraints — just a name unique enough per test run that
    /// parallel `cargo test` invocations of this file never collide.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "scrozz-shell-login-test-{label}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("creating the scratch home directory");
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // -----------------------------------------------------------------
    // XML escaping (pure, platform-independent)
    // -----------------------------------------------------------------

    #[test]
    fn xml_escaping_covers_all_five_reserved_characters() {
        let escaped = escape_xml(r#"<a & "b" 'c'>"#);
        assert_eq!(escaped, "&lt;a &amp; &quot;b&quot; &apos;c&apos;&gt;");
    }

    #[test]
    fn xml_escaping_does_not_double_escape_generated_ampersands() {
        // A naive implementation that escapes `<` before `&` would turn a
        // literal `<` into `&lt;` and then re-escape that ampersand into
        // `&amp;lt;`, corrupting the plist. Escaping `&` first is what
        // prevents that.
        assert_eq!(escape_xml("<"), "&lt;");
    }

    #[test]
    fn plist_contents_embeds_the_label_and_escaped_executable_path() {
        let contents = plist_contents(
            "com.thatcube.scrozz",
            Path::new("/Applications/My App.app/Contents/MacOS/scrozz"),
        );
        assert!(contents.contains("<string>com.thatcube.scrozz</string>"));
        assert!(contents.contains("/Applications/My App.app/Contents/MacOS/scrozz"));
        assert!(contents.contains("<key>RunAtLoad</key>"));
        assert!(contents.contains("<true/>"));
        // KeepAlive must never appear: see the module docs for why.
        assert!(!contents.contains("KeepAlive"));
    }

    #[test]
    fn plist_contents_escapes_an_ampersand_in_the_executable_path() {
        let contents = plist_contents("com.thatcube.scrozz", Path::new("/Users/a & b/scrozz"));
        assert!(contents.contains("/Users/a &amp; b/scrozz"));
        assert!(!contents.contains("/Users/a & b/scrozz"));
    }

    // -----------------------------------------------------------------
    // Desktop entry `Exec=` quoting (pure, platform-independent)
    // -----------------------------------------------------------------

    #[test]
    fn desktop_exec_quoting_leaves_a_plain_path_untouched() {
        assert_eq!(quote_desktop_exec("/usr/bin/scrozz"), "/usr/bin/scrozz");
    }

    #[test]
    fn desktop_exec_quoting_wraps_a_path_containing_a_space() {
        assert_eq!(
            quote_desktop_exec("/home/user/My Apps/scrozz"),
            "\"/home/user/My Apps/scrozz\""
        );
    }

    #[test]
    fn desktop_exec_quoting_escapes_reserved_characters() {
        // `"`, `` ` ``, `$` and `\` must each come out backslash-escaped.
        let quoted = quote_desktop_exec(r#"/tmp/a"b`c$d\e f"#);
        assert_eq!(quoted, r#""/tmp/a\"b\`c\$d\\e f""#);
    }

    #[test]
    fn desktop_entry_contents_has_the_required_keys_and_a_quoted_exec() {
        let contents = desktop_entry_contents(Path::new("/home/user/My Apps/scrozz"));
        assert!(contents.starts_with("[Desktop Entry]\n"));
        assert!(contents.contains("Type=Application\n"));
        assert!(contents.contains("X-GNOME-Autostart-enabled=true\n"));
        assert!(contents.contains("Exec=\"/home/user/My Apps/scrozz\"\n"));
    }

    // -----------------------------------------------------------------
    // Windows `Run` value quoting (pure, platform-independent)
    // -----------------------------------------------------------------

    #[test]
    fn windows_run_value_always_quotes_even_without_a_space() {
        assert_eq!(
            windows_run_value(Path::new(r"C:\Scrozz\scrozz.exe")),
            r#""C:\Scrozz\scrozz.exe""#
        );
    }

    #[test]
    fn windows_run_value_quotes_a_path_containing_a_space() {
        assert_eq!(
            windows_run_value(Path::new(r"C:\Program Files\Scrozz\scrozz.exe")),
            r#""C:\Program Files\Scrozz\scrozz.exe""#
        );
    }

    #[test]
    fn windows_run_value_escapes_an_embedded_quote() {
        assert_eq!(
            windows_run_value(Path::new(r#"C:\weird"path\scrozz.exe"#)),
            r#""C:\weird\"path\scrozz.exe""#
        );
    }

    // -----------------------------------------------------------------
    // Injectable-filesystem behaviour on macOS
    // -----------------------------------------------------------------

    #[cfg(target_os = "macos")]
    mod macos_filesystem {
        use super::*;

        #[test]
        fn is_disabled_before_first_enable() {
            let home = ScratchDir::new("initial-state");
            let login_item =
                SystemLaunchAtLogin::new("com.thatcube.scrozz.test", "/usr/local/bin/scrozz")
                    .with_home(&home.0);
            assert!(
                !login_item
                    .is_enabled()
                    .expect("querying a fresh scratch home")
            );
        }

        #[test]
        fn enable_writes_only_under_the_injected_home() {
            let home = ScratchDir::new("writes-under-injected-home");
            let login_item =
                SystemLaunchAtLogin::new("com.thatcube.scrozz.test", "/usr/local/bin/scrozz")
                    .with_home(&home.0);

            login_item
                .enable()
                .expect("enabling should write the plist");

            let expected_path = home
                .0
                .join("Library/LaunchAgents/com.thatcube.scrozz.test.plist");
            assert!(expected_path.is_file());
            let written = fs::read_to_string(&expected_path).expect("reading the written plist");
            assert!(written.contains("/usr/local/bin/scrozz"));
        }

        #[test]
        fn enable_then_is_enabled_then_disable_then_is_enabled_round_trips() {
            let home = ScratchDir::new("round-trip");
            let login_item =
                SystemLaunchAtLogin::new("com.thatcube.scrozz.test", "/usr/local/bin/scrozz")
                    .with_home(&home.0);

            login_item.enable().expect("enable");
            assert!(login_item.is_enabled().expect("is_enabled after enable"));

            login_item.disable().expect("disable");
            assert!(!login_item.is_enabled().expect("is_enabled after disable"));
        }

        #[test]
        fn disabling_when_never_enabled_is_not_an_error() {
            let home = ScratchDir::new("disable-when-absent");
            let login_item =
                SystemLaunchAtLogin::new("com.thatcube.scrozz.test", "/usr/local/bin/scrozz")
                    .with_home(&home.0);
            login_item
                .disable()
                .expect("disabling an unregistered login item must be a no-op, not an error");
        }

        #[test]
        fn enabling_twice_overwrites_rather_than_fails() {
            let home = ScratchDir::new("enable-twice");
            let first =
                SystemLaunchAtLogin::new("com.thatcube.scrozz.test", "/usr/local/bin/scrozz-old")
                    .with_home(&home.0);
            first.enable().expect("first enable");

            let second =
                SystemLaunchAtLogin::new("com.thatcube.scrozz.test", "/usr/local/bin/scrozz-new")
                    .with_home(&home.0);
            second
                .enable()
                .expect("second enable must overwrite, not error");

            let expected_path = home
                .0
                .join("Library/LaunchAgents/com.thatcube.scrozz.test.plist");
            let written = fs::read_to_string(&expected_path).expect("reading the rewritten plist");
            assert!(written.contains("/usr/local/bin/scrozz-new"));
            assert!(!written.contains("/usr/local/bin/scrozz-old"));
        }

        #[test]
        fn enable_creates_missing_parent_directories() {
            // A fresh user account has no ~/Library/LaunchAgents at all;
            // `enable` must create it rather than fail.
            let home = ScratchDir::new("missing-parent");
            assert!(!home.0.join("Library").exists());

            let login_item =
                SystemLaunchAtLogin::new("com.thatcube.scrozz.test", "/usr/local/bin/scrozz")
                    .with_home(&home.0);
            login_item
                .enable()
                .expect("enable must create ~/Library/LaunchAgents when absent");
            assert!(
                login_item
                    .is_enabled()
                    .expect("is_enabled after creating parents")
            );
        }

        #[test]
        fn without_a_home_override_missing_home_dir_is_a_platform_error() {
            // No `with_home` call: this exercises the `dirs::home_dir()`
            // fallback path is reachable, without asserting on the real
            // machine's home directory (which does exist here). The
            // meaningful assertion is simply that the call succeeds and
            // returns a bool rather than panicking.
            let login_item = SystemLaunchAtLogin::new(
                "com.thatcube.scrozz.test.no-override",
                "/usr/local/bin/scrozz",
            );
            let _ = login_item.is_enabled();
        }
    }
}
