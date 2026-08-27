//! Folder picker: a native "choose a folder" dialog behind one trait.
//!
//! Backs feature SYS-04 (configurable save location) and decision D18's
//! arbitrary save/import folder. The mechanism is a different native surface
//! on every platform:
//!
//! | Platform | Mechanism |
//! |---|---|
//! | macOS | `NSOpenPanel`, configured to choose directories only |
//! | Windows | `IFileOpenDialog` with `FOS_PICKFOLDERS` |
//! | Linux | `zenity --file-selection --directory`, falling back to `kdialog --getexistingdirectory` |
//!
//! [`crate::FolderPicker`] is the trait the app coordinator holds;
//! `scrozz-ui` emits only a browse intent. [`native_folder_picker`] returns the real backend
//! for whichever platform this was built for, and [`StubFolderPicker`] is a
//! non-native, deterministic stand-in for tests — opening a real dialog from
//! a test suite would hang waiting for a human, or fail outright with no
//! display server in CI.
//!
//! # Cancellation, unsupported, and error are three different outcomes
//!
//! Every backend distinguishes:
//!
//! - the user dismissed the dialog without choosing anything —
//!   [`scrozz_core::Error::Cancelled`];
//! - no picker mechanism exists here at all, e.g. neither `zenity` nor
//!   `kdialog` is installed — [`scrozz_core::Error::Unsupported`];
//! - anything else that went wrong presenting the dialog or reading its
//!   result — [`scrozz_core::Error::Platform`].
//!
//! Collapsing any two of these into one would either make "the user changed
//! their mind" look like a bug, or make "this doesn't work here" look like a
//! transient failure worth retrying.
//!
//! # What is testable without opening a dialog
//!
//! The Linux backend's helper selection ([`find_picker_binary`]) and argument
//! construction ([`zenity_args`], [`kdialog_args`]) are ordinary,
//! platform-independent functions with no subprocess execution — they are
//! exercised directly in this module's tests on any host, including macOS.
//! Only the `cfg(target_os = "linux")` subprocess-spawning code that calls
//! them is native and untestable here; see the crate's test report for what
//! that leaves unverified. The macOS `NSOpenPanel` and Windows
//! `IFileOpenDialog` backends open a real, modal, human-facing dialog and are
//! not exercised by any automated test — [`StubFolderPicker`] exists
//! precisely so callers never need to.

use std::path::PathBuf;

use scrozz_core::{Error, Result};

use crate::{FolderPicker, FolderPickerRequest};

// ---------------------------------------------------------------------------
// Platform selection
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub use linux_picker::SubprocessPicker as NativeFolderPicker;
#[cfg(target_os = "macos")]
pub use macos_picker::NsOpenPanelPicker as NativeFolderPicker;
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub use unimplemented_platform::PlannedFolderPicker as NativeFolderPicker;
#[cfg(target_os = "windows")]
pub use windows_picker::ShellItemPicker as NativeFolderPicker;

/// The folder-picker backend for this platform.
///
/// # Errors
///
/// Returns [`Error::Platform`] on macOS if called off the main thread.
/// Construction otherwise always succeeds on every platform; a picker that
/// cannot work at all (no `zenity`/`kdialog`, or an unimplemented platform)
/// still constructs successfully and reports [`Error::Unsupported`] only when
/// [`FolderPicker::pick_folder`] is actually called, matching the rest of
/// this crate's native-backend constructors.
pub fn native_folder_picker() -> Result<NativeFolderPicker> {
    NativeFolderPicker::new()
}

// ---------------------------------------------------------------------------
// Test stub
// ---------------------------------------------------------------------------

/// A [`FolderPicker`] that never opens a dialog.
///
/// The mock every app-level test should use instead of
/// [`native_folder_picker`]: construct it with the outcome the test wants —
/// [`StubFolderPicker::choosing`], [`StubFolderPicker::cancelling`], or
/// [`StubFolderPicker::unsupported`] — and it reports that outcome
/// synchronously, with no window server, no subprocess, and no human required
/// to click anything.
#[derive(Debug, Clone)]
pub struct StubFolderPicker {
    outcome: StubOutcome,
}

#[derive(Debug, Clone)]
enum StubOutcome {
    Chosen(PathBuf),
    Cancelled,
    Unsupported { what: String, why: String },
}

impl StubFolderPicker {
    /// A stub that reports the user chose `path`.
    #[must_use]
    pub fn choosing(path: impl Into<PathBuf>) -> Self {
        Self {
            outcome: StubOutcome::Chosen(path.into()),
        }
    }

    /// A stub that reports the user dismissed the dialog without choosing.
    #[must_use]
    pub fn cancelling() -> Self {
        Self {
            outcome: StubOutcome::Cancelled,
        }
    }

    /// A stub that reports no picker mechanism is available.
    #[must_use]
    pub fn unsupported(what: impl Into<String>, why: impl Into<String>) -> Self {
        Self {
            outcome: StubOutcome::Unsupported {
                what: what.into(),
                why: why.into(),
            },
        }
    }
}

impl FolderPicker for StubFolderPicker {
    fn pick_folder(&self, _request: &FolderPickerRequest) -> Result<PathBuf> {
        match &self.outcome {
            StubOutcome::Chosen(path) => Ok(path.clone()),
            StubOutcome::Cancelled => Err(Error::Cancelled),
            StubOutcome::Unsupported { what, why } => Err(Error::Unsupported {
                what: what.clone(),
                why: why.clone(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// macOS: NSOpenPanel
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod macos_picker {
    use std::path::PathBuf;

    use objc2_app_kit::{NSModalResponseOK, NSOpenPanel};
    use objc2_foundation::{NSString, NSURL};
    use scrozz_core::{Error, Result};

    use crate::{FolderPicker, FolderPickerRequest};

    /// The macOS backend: `NSOpenPanel` restricted to choosing exactly one
    /// existing directory.
    #[derive(Debug, Default)]
    pub struct NsOpenPanelPicker {
        _private: (),
    }

    impl NsOpenPanelPicker {
        /// Creates the backend.
        ///
        /// # Errors
        ///
        /// Never; the signature matches every other platform's constructor so
        /// [`super::native_folder_picker`] has one shape everywhere.
        pub fn new() -> Result<Self> {
            Ok(Self { _private: () })
        }
    }

    impl FolderPicker for NsOpenPanelPicker {
        fn pick_folder(&self, request: &FolderPickerRequest) -> Result<PathBuf> {
            let mtm = crate::macos::main_thread("presenting a folder picker")?;
            let panel = NSOpenPanel::openPanel(mtm);
            panel.setCanChooseFiles(false);
            panel.setCanChooseDirectories(true);
            panel.setAllowsMultipleSelection(false);
            panel.setCanCreateDirectories(true);
            panel.setResolvesAliases(true);

            if let Some(title) = &request.title {
                panel.setTitle(Some(&NSString::from_str(title)));
            }
            if let Some(prompt) = &request.prompt {
                panel.setMessage(Some(&NSString::from_str(prompt)));
            }
            if let Some(dir) = &request.starting_directory {
                let url = NSURL::fileURLWithPath(&NSString::from_str(&dir.to_string_lossy()));
                panel.setDirectoryURL(Some(&url));
            }

            if panel.runModal() != NSModalResponseOK {
                return Err(Error::Cancelled);
            }

            let url = panel.URL().ok_or_else(|| {
                Error::Platform("NSOpenPanel reported OK but returned no URL".to_owned())
            })?;
            let path = url.path().ok_or_else(|| {
                Error::Platform("NSOpenPanel's chosen URL has no filesystem path".to_owned())
            })?;
            Ok(PathBuf::from(path.to_string()))
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: IFileOpenDialog with FOS_PICKFOLDERS
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod windows_picker {
    use std::path::PathBuf;

    use scrozz_core::{Error, Result};
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize,
    };
    use windows::Win32::UI::Shell::{
        FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
    };
    use windows::core::{HRESULT, HSTRING, IUnknown};

    use crate::{FolderPicker, FolderPickerRequest};

    /// The `HRESULT` `IFileDialog::Show` returns when the user cancels:
    /// `HRESULT_FROM_WIN32(ERROR_CANCELLED)`.
    const ERROR_CANCELLED_HRESULT: HRESULT = HRESULT(0x800704C7u32 as i32);

    /// The Windows backend: a Common Item Dialog restricted to folders.
    #[derive(Debug, Default)]
    pub struct ShellItemPicker {
        _private: (),
    }

    impl ShellItemPicker {
        /// Creates the backend.
        ///
        /// # Errors
        ///
        /// Never; the signature matches every other platform's constructor so
        /// [`super::native_folder_picker`] has one shape everywhere.
        pub fn new() -> Result<Self> {
            Ok(Self { _private: () })
        }
    }

    /// Balances a per-thread `CoInitializeEx` call with `CoUninitialize`,
    /// but only when this call was the one that initialised the apartment —
    /// per the COM contract, calling `CoUninitialize` after a failed
    /// `CoInitializeEx` (for example `RPC_E_CHANGED_MODE`, a different
    /// concurrency model already active on this thread) would unbalance
    /// whichever caller *did* successfully initialise it.
    struct ComApartment {
        owns_apartment: bool,
    }

    impl ComApartment {
        fn enter() -> Self {
            // SAFETY: `CoInitializeEx` may be called more than once per
            // thread; its `HRESULT` says whether this call is the one that
            // took ownership of the apartment, which is what `Drop` below
            // checks before uninitialising.
            let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            Self {
                owns_apartment: result.is_ok(),
            }
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            if self.owns_apartment {
                // SAFETY: paired 1:1 with the successful `CoInitializeEx` in
                // `enter`.
                unsafe {
                    CoUninitialize();
                }
            }
        }
    }

    impl FolderPicker for ShellItemPicker {
        fn pick_folder(&self, request: &FolderPickerRequest) -> Result<PathBuf> {
            let _apartment = ComApartment::enter();

            // SAFETY: the dialog is created, configured, shown modelessly
            // with no owner window, and its result is read only once `Show`
            // has returned success — the sequence the `IFileOpenDialog`
            // contract requires.
            unsafe {
                let dialog: IFileOpenDialog = CoCreateInstance(
                    &FileOpenDialog,
                    None::<&IUnknown>,
                    CLSCTX_ALL,
                )
                .map_err(|e| {
                    Error::Platform(format!("creating the folder picker dialog failed: {e}"))
                })?;

                let options = dialog.GetOptions().map_err(|e| {
                    Error::Platform(format!("reading folder picker options failed: {e}"))
                })?;
                dialog
                    .SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
                    .map_err(|e| {
                        Error::Platform(format!("setting folder picker options failed: {e}"))
                    })?;

                if let Some(title) = &request.title {
                    // A failure here is cosmetic (no title shown) and not
                    // worth aborting the whole picker over.
                    let _ = dialog.SetTitle(&HSTRING::from(title.as_str()));
                }

                match dialog.Show(None) {
                    Ok(()) => {}
                    Err(err) if err.code() == ERROR_CANCELLED_HRESULT => {
                        return Err(Error::Cancelled);
                    }
                    Err(err) => {
                        return Err(Error::Platform(format!(
                            "presenting the folder picker failed: {err}"
                        )));
                    }
                }

                let item = dialog.GetResult().map_err(|e| {
                    Error::Platform(format!("reading the chosen folder failed: {e}"))
                })?;
                let raw_path = item.GetDisplayName(SIGDN_FILESYSPATH).map_err(|e| {
                    Error::Platform(format!("resolving the chosen folder's path failed: {e}"))
                })?;
                let decoded = raw_path.to_string();
                CoTaskMemFree(Some(raw_path.0.cast_const().cast()));
                let decoded = decoded.map_err(|e| {
                    Error::Platform(format!(
                        "the chosen folder's path was not valid UTF-16: {e}"
                    ))
                })?;

                Ok(PathBuf::from(decoded))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Linux: zenity / kdialog subprocess
// ---------------------------------------------------------------------------

/// Which Linux folder-picker helper to shell out to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerBinary {
    /// `zenity --file-selection --directory`, GTK/GNOME's dialog helper.
    Zenity(PathBuf),
    /// `kdialog --getexistingdirectory`, KDE's dialog helper.
    KDialog(PathBuf),
}

impl PickerBinary {
    /// The resolved path to the helper executable.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        match self {
            Self::Zenity(path) | Self::KDialog(path) => path,
        }
    }
}

/// Finds the first available picker helper on `PATH`, preferring `zenity`.
///
/// Takes the `PATH` value as a parameter rather than reading the environment
/// itself, so this selection logic is testable against a handful of files
/// under a temp directory instead of the real `PATH` of whatever machine runs
/// the tests — including on macOS, which has neither helper installed.
///
/// `zenity` is checked first: it is the GTK/GNOME helper and the more
/// commonly preinstalled of the two, since GNOME is the default desktop on
/// the most common distributions. `kdialog` is the fallback for KDE sessions
/// that lack it. Neither present is [`Error::Unsupported`], not a silent
/// failure — see [`FolderPicker::pick_folder`]'s cancellation contract.
///
/// [`Error::Unsupported`]: scrozz_core::Error::Unsupported
#[must_use]
pub fn find_picker_binary(path_var: Option<&std::ffi::OsStr>) -> Option<PickerBinary> {
    let path_var = path_var?;
    let dirs: Vec<PathBuf> = std::env::split_paths(path_var).collect();
    for dir in &dirs {
        let candidate = dir.join("zenity");
        if is_executable_file(&candidate) {
            return Some(PickerBinary::Zenity(candidate));
        }
    }
    for dir in &dirs {
        let candidate = dir.join("kdialog");
        if is_executable_file(&candidate) {
            return Some(PickerBinary::KDialog(candidate));
        }
    }
    None
}

/// Whether `path` exists and is executable, in the sense `PATH` lookup means.
///
/// On Unix this checks the executable permission bit, so a same-named
/// non-executable file (e.g. a stray data file in a `PATH` directory) is
/// correctly skipped. Elsewhere — this function is reachable in the test
/// suite on any host — a plain existence check is the best available
/// approximation, and is only ever load-bearing on Linux, where the real
/// picker backend runs.
fn is_executable_file(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Builds the argument list for `zenity --file-selection --directory`.
///
/// No shell is involved in running this: [`std::process::Command`] passes
/// each element straight to `execve`, so there is no shell metacharacter to
/// escape here. What still has to be kept correct is that each option and its
/// value stay a single argument, so a title containing a space is not split
/// into two by the time `zenity` sees `argv`.
#[must_use]
pub fn zenity_args(request: &FolderPickerRequest) -> Vec<String> {
    let mut args = vec!["--file-selection".to_owned(), "--directory".to_owned()];
    if let Some(title) = &request.title {
        args.push(format!("--title={title}"));
    }
    if let Some(dir) = &request.starting_directory {
        args.push(format!("--filename={}", dir.display()));
    }
    args
}

/// Builds the argument list for `kdialog --getexistingdirectory`.
///
/// Unlike `zenity`'s named flags, `kdialog` takes its starting directory and
/// caption as positional arguments, in that order — so a caption with no
/// starting directory still needs a placeholder (`.`, the current directory)
/// in the first position, or `kdialog` would read the caption text as the
/// starting directory instead.
#[must_use]
pub fn kdialog_args(request: &FolderPickerRequest) -> Vec<String> {
    let mut args = vec!["--getexistingdirectory".to_owned()];
    args.push(
        request
            .starting_directory
            .as_ref()
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| ".".to_owned()),
    );
    if let Some(title) = &request.title {
        args.push(title.clone());
    }
    args
}

#[cfg(target_os = "linux")]
mod linux_picker {
    use std::path::PathBuf;
    use std::process::Command;

    use scrozz_core::{Error, Result};

    use super::{PickerBinary, find_picker_binary, kdialog_args, zenity_args};
    use crate::{FolderPicker, FolderPickerRequest};

    /// The Linux backend: shells out to `zenity` or `kdialog`, whichever is
    /// installed. See the module docs for why there is no portal-based
    /// implementation here yet.
    #[derive(Debug, Default)]
    pub struct SubprocessPicker {
        _private: (),
    }

    impl SubprocessPicker {
        /// Creates the backend.
        ///
        /// # Errors
        ///
        /// Never; the signature matches every other platform's constructor so
        /// [`super::native_folder_picker`] has one shape everywhere. Whether a
        /// helper is actually installed is discovered in
        /// [`FolderPicker::pick_folder`], not here.
        pub fn new() -> Result<Self> {
            Ok(Self { _private: () })
        }
    }

    impl FolderPicker for SubprocessPicker {
        fn pick_folder(&self, request: &FolderPickerRequest) -> Result<PathBuf> {
            let path_var = std::env::var_os("PATH");
            let binary =
                find_picker_binary(path_var.as_deref()).ok_or_else(|| Error::Unsupported {
                    what: "folder picker".to_owned(),
                    why: "neither zenity nor kdialog is installed; Scrozz needs \
                          one of them to present a folder picker on a desktop \
                          with no file-chooser portal wired up"
                        .to_owned(),
                })?;

            let (program, args) = match &binary {
                PickerBinary::Zenity(_) => (binary.path(), zenity_args(request)),
                PickerBinary::KDialog(_) => (binary.path(), kdialog_args(request)),
            };

            let output = Command::new(program).args(&args).output().map_err(|e| {
                Error::Platform(format!("failed to run {}: {e}", program.display()))
            })?;

            match output.status.code() {
                Some(0) => {
                    let chosen = String::from_utf8_lossy(&output.stdout);
                    let chosen = chosen.trim();
                    if chosen.is_empty() {
                        Err(Error::Platform(format!(
                            "{} exited successfully but printed no path",
                            program.display()
                        )))
                    } else {
                        Ok(PathBuf::from(chosen))
                    }
                }
                // Both zenity and kdialog exit 1 when the user cancels.
                Some(1) => Err(Error::Cancelled),
                other => Err(Error::Platform(format!(
                    "{} exited with {other:?}: {}",
                    program.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ))),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Any other platform: designed, not built
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod unimplemented_platform {
    use std::path::PathBuf;

    use scrozz_core::{Error, Result};

    use crate::{FolderPicker, FolderPickerRequest};

    /// The folder-picker backend on a platform Scrozz does not target.
    ///
    /// Construction succeeds so a caller can still query the type; only
    /// [`FolderPicker::pick_folder`] fails, and it fails with
    /// [`Error::Unsupported`], which is an ordinary handled outcome here, not
    /// a crash.
    #[derive(Debug, Default)]
    pub struct PlannedFolderPicker {
        _private: (),
    }

    impl PlannedFolderPicker {
        /// Creates the backend.
        ///
        /// # Errors
        ///
        /// Never; the signature matches every other platform's constructor.
        pub fn new() -> Result<Self> {
            Ok(Self { _private: () })
        }
    }

    impl FolderPicker for PlannedFolderPicker {
        fn pick_folder(&self, _request: &FolderPickerRequest) -> Result<PathBuf> {
            Err(Error::Unsupported {
                what: "folder picker".to_owned(),
                why: "Scrozz has no folder-picker backend for this platform".to_owned(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// A unique scratch directory under [`std::env::temp_dir`], cleaned up on
    /// drop, used to stand in for `PATH` entries without touching the real
    /// one.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "scrozz-shell-picker-test-{label}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("creating the scratch PATH directory");
            Self(path)
        }

        #[cfg(unix)]
        fn touch_executable(&self, name: &str) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = self.0.join(name);
            fs::write(&path, b"#!/bin/sh\n").expect("writing a stub executable");
            let mut perms = fs::metadata(&path).expect("stat stub").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).expect("chmod stub");
            path
        }

        #[cfg(unix)]
        fn touch_non_executable(&self, name: &str) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = self.0.join(name);
            fs::write(&path, b"not a program").expect("writing a non-executable file");
            let mut perms = fs::metadata(&path).expect("stat file").permissions();
            perms.set_mode(0o644);
            fs::set_permissions(&path, perms).expect("chmod file");
            path
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn joined_path_var(dirs: &[PathBuf]) -> std::ffi::OsString {
        std::env::join_paths(dirs).expect("joining scratch PATH entries")
    }

    // -----------------------------------------------------------------
    // StubFolderPicker
    // -----------------------------------------------------------------

    #[test]
    fn stub_choosing_returns_the_given_path() {
        let stub = StubFolderPicker::choosing("/tmp/example");
        let chosen = stub
            .pick_folder(&FolderPickerRequest::default())
            .expect("stub configured to choose should not error");
        assert_eq!(chosen, PathBuf::from("/tmp/example"));
    }

    #[test]
    fn stub_cancelling_reports_cancellation_distinctly() {
        let stub = StubFolderPicker::cancelling();
        let err = stub
            .pick_folder(&FolderPickerRequest::default())
            .expect_err("stub configured to cancel should error");
        assert!(err.is_cancellation());
    }

    #[test]
    fn stub_unsupported_reports_unsupported_not_cancellation() {
        let stub = StubFolderPicker::unsupported("folder picker", "no backend in this test");
        let err = stub
            .pick_folder(&FolderPickerRequest::default())
            .expect_err("stub configured as unsupported should error");
        assert!(!err.is_cancellation());
        assert!(matches!(err, Error::Unsupported { .. }));
    }

    // -----------------------------------------------------------------
    // Linux helper selection (pure, runs on any host)
    // -----------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn find_picker_binary_is_none_with_no_path() {
        assert_eq!(find_picker_binary(None), None);
    }

    #[cfg(unix)]
    #[test]
    fn find_picker_binary_is_none_when_neither_helper_is_present() {
        let dir = ScratchDir::new("neither-present");
        let path_var = joined_path_var(std::slice::from_ref(&dir.0));
        assert_eq!(find_picker_binary(Some(path_var.as_os_str())), None);
    }

    #[cfg(unix)]
    #[test]
    fn find_picker_binary_prefers_zenity_over_kdialog() {
        let dir = ScratchDir::new("prefers-zenity");
        dir.touch_executable("kdialog");
        dir.touch_executable("zenity");
        let path_var = joined_path_var(std::slice::from_ref(&dir.0));

        let found = find_picker_binary(Some(path_var.as_os_str()));
        assert_eq!(found, Some(PickerBinary::Zenity(dir.0.join("zenity"))));
    }

    #[cfg(unix)]
    #[test]
    fn find_picker_binary_falls_back_to_kdialog_when_zenity_is_absent() {
        let dir = ScratchDir::new("falls-back-to-kdialog");
        dir.touch_executable("kdialog");
        let path_var = joined_path_var(std::slice::from_ref(&dir.0));

        let found = find_picker_binary(Some(path_var.as_os_str()));
        assert_eq!(found, Some(PickerBinary::KDialog(dir.0.join("kdialog"))));
    }

    #[cfg(unix)]
    #[test]
    fn find_picker_binary_skips_a_non_executable_same_named_file() {
        let dir = ScratchDir::new("skips-non-executable");
        dir.touch_non_executable("zenity");
        dir.touch_executable("kdialog");
        let path_var = joined_path_var(std::slice::from_ref(&dir.0));

        let found = find_picker_binary(Some(path_var.as_os_str()));
        assert_eq!(found, Some(PickerBinary::KDialog(dir.0.join("kdialog"))));
    }

    #[cfg(unix)]
    #[test]
    fn find_picker_binary_searches_path_entries_in_order() {
        let first = ScratchDir::new("path-order-first");
        let second = ScratchDir::new("path-order-second");
        second.touch_executable("zenity");
        // Only the second directory has zenity; the first has nothing.
        let path_var = joined_path_var(&[first.0.clone(), second.0.clone()]);

        let found = find_picker_binary(Some(path_var.as_os_str()));
        assert_eq!(found, Some(PickerBinary::Zenity(second.0.join("zenity"))));
    }

    #[test]
    fn picker_binary_path_returns_the_resolved_executable() {
        let path = PathBuf::from("/usr/bin/zenity");
        let binary = PickerBinary::Zenity(path.clone());
        assert_eq!(binary.path(), path.as_path());
    }

    // -----------------------------------------------------------------
    // Command argument construction / escaping (pure, runs on any host)
    // -----------------------------------------------------------------

    #[test]
    fn zenity_args_always_requests_a_directory_selection() {
        let args = zenity_args(&FolderPickerRequest::default());
        assert_eq!(
            args,
            vec!["--file-selection".to_owned(), "--directory".to_owned()]
        );
    }

    #[test]
    fn zenity_args_keeps_a_title_with_spaces_as_one_argument() {
        let request = FolderPickerRequest {
            title: Some("Choose a Save Folder".to_owned()),
            ..Default::default()
        };
        let args = zenity_args(&request);
        // Command::args does not go through a shell, so the whole
        // "--title=..." flag must be one Vec element, not split on the
        // spaces inside the title.
        assert_eq!(
            args.last(),
            Some(&"--title=Choose a Save Folder".to_owned())
        );
        assert_eq!(args.len(), 3);
    }

    #[test]
    fn zenity_args_includes_the_starting_directory_when_given() {
        let request = FolderPickerRequest {
            starting_directory: Some(PathBuf::from("/home/user/Pictures")),
            ..Default::default()
        };
        let args = zenity_args(&request);
        assert!(args.contains(&"--filename=/home/user/Pictures".to_owned()));
    }

    #[test]
    fn kdialog_args_defaults_the_starting_directory_to_current() {
        let args = kdialog_args(&FolderPickerRequest::default());
        assert_eq!(
            args,
            vec!["--getexistingdirectory".to_owned(), ".".to_owned()]
        );
    }

    #[test]
    fn kdialog_args_puts_the_starting_directory_before_the_title() {
        let request = FolderPickerRequest {
            title: Some("Choose a Save Folder".to_owned()),
            starting_directory: Some(PathBuf::from("/home/user/Pictures")),
            ..Default::default()
        };
        let args = kdialog_args(&request);
        assert_eq!(
            args,
            vec![
                "--getexistingdirectory".to_owned(),
                "/home/user/Pictures".to_owned(),
                "Choose a Save Folder".to_owned(),
            ]
        );
    }

    #[test]
    fn kdialog_args_title_with_no_starting_directory_still_gets_a_placeholder() {
        let request = FolderPickerRequest {
            title: Some("Choose a Save Folder".to_owned()),
            ..Default::default()
        };
        let args = kdialog_args(&request);
        // Without the "." placeholder, kdialog would read the title as the
        // starting directory instead of showing it as a caption.
        assert_eq!(args[1], ".");
        assert_eq!(args[2], "Choose a Save Folder");
    }
}
