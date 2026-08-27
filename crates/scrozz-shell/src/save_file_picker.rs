//! Native save-file dialogs behind a small, injectable contract.

use std::path::PathBuf;

use scrozz_core::{Error, Result};

/// Everything the native dialog needs for one save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveFileRequest {
    /// Dialog title.
    pub title: String,
    /// Initial directory and filename.
    pub suggested_path: PathBuf,
    /// File extension without a leading dot.
    pub extension: String,
}

impl SaveFileRequest {
    /// Builds a save request from a fully rendered path.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when the path has no filename or file
    /// extension.
    pub fn new(title: impl Into<String>, suggested_path: PathBuf) -> Result<Self> {
        let extension = suggested_path
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| !extension.is_empty())
            .ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "suggested save path has no extension: {}",
                    suggested_path.display()
                ))
            })?
            .to_owned();
        if suggested_path.file_name().is_none() {
            return Err(Error::InvalidRequest(format!(
                "suggested save path has no filename: {}",
                suggested_path.display()
            )));
        }
        Ok(Self {
            title: title.into(),
            suggested_path,
            extension,
        })
    }

    /// Forces a native selection to use the encoded image's extension.
    ///
    /// AppKit and the Windows common item dialog normally do this themselves.
    /// The normalization also covers Linux helpers and protects against a user
    /// typing a misleading extension for bytes encoded in another format.
    #[must_use]
    pub fn normalize_selection(&self, mut path: PathBuf) -> PathBuf {
        if path.extension().and_then(|extension| extension.to_str())
            != Some(self.extension.as_str())
        {
            path.set_extension(&self.extension);
        }
        path
    }
}

/// A save-file chooser invoked on the GUI thread.
pub trait SaveFilePicker {
    /// Shows the picker and returns the approved path.
    ///
    /// `Ok(None)` means the user cancelled without changing anything.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Platform`] when the native dialog cannot be opened or
    /// cannot return a filesystem path.
    fn pick_file(&self, request: &SaveFileRequest) -> Result<Option<PathBuf>>;
}

/// A deterministic save-file chooser for tests and headless integrations.
#[derive(Debug, Clone, Default)]
pub struct StubSaveFilePicker {
    selection: Option<PathBuf>,
}

impl StubSaveFilePicker {
    /// A picker that approves `path`.
    #[must_use]
    pub fn selecting(path: impl Into<PathBuf>) -> Self {
        Self {
            selection: Some(path.into()),
        }
    }

    /// A picker that behaves like a user cancellation.
    #[must_use]
    pub const fn cancelling() -> Self {
        Self { selection: None }
    }
}

impl SaveFilePicker for StubSaveFilePicker {
    fn pick_file(&self, _request: &SaveFileRequest) -> Result<Option<PathBuf>> {
        Ok(self.selection.clone())
    }
}

/// Returns the native save-file chooser for this target.
#[must_use]
pub fn native_save_file_picker() -> NativeSaveFilePicker {
    NativeSaveFilePicker
}

#[cfg(target_os = "macos")]
/// The AppKit `NSSavePanel` implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeSaveFilePicker;

#[cfg(target_os = "macos")]
impl SaveFilePicker for NativeSaveFilePicker {
    fn pick_file(&self, request: &SaveFileRequest) -> Result<Option<PathBuf>> {
        use objc2_app_kit::{NSModalResponseOK, NSSavePanel};
        use objc2_foundation::{NSArray, NSString, NSURL};

        let panel = NSSavePanel::savePanel(crate::macos::main_thread("save file dialog")?);
        panel.setTitle(Some(&NSString::from_str(&request.title)));
        let name = request
            .suggested_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "suggested save filename is not valid Unicode: {}",
                    request.suggested_path.display()
                ))
            })?;
        panel.setNameFieldStringValue(&NSString::from_str(name));
        if let Some(parent) = request.suggested_path.parent() {
            let parent = parent.to_str().ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "suggested save directory is not valid Unicode: {}",
                    parent.display()
                ))
            })?;
            panel.setDirectoryURL(Some(&NSURL::fileURLWithPath(&NSString::from_str(parent))));
        }
        let types = NSArray::from_retained_slice(&[NSString::from_str(&request.extension)]);
        #[allow(deprecated)]
        panel.setAllowedFileTypes(Some(&types));
        panel.setAllowsOtherFileTypes(false);
        panel.setCanCreateDirectories(true);

        if panel.runModal() != NSModalResponseOK {
            return Ok(None);
        }
        panel
            .URL()
            .and_then(|url| url.path())
            .map(|path| PathBuf::from(path.to_string()))
            .map(Some)
            .ok_or_else(|| Error::Platform("the save panel returned no filesystem path".to_owned()))
    }
}

#[cfg(target_os = "windows")]
/// The Windows common item save dialog implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeSaveFilePicker;

#[cfg(target_os = "windows")]
impl SaveFilePicker for NativeSaveFilePicker {
    fn pick_file(&self, request: &SaveFileRequest) -> Result<Option<PathBuf>> {
        use windows::{
            Win32::{
                Foundation::ERROR_CANCELLED,
                System::Com::{
                    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance,
                    CoInitializeEx, CoTaskMemFree, CoUninitialize,
                },
                UI::Shell::{
                    FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FileSaveDialog, IFileSaveDialog,
                    IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
                },
            },
            core::{HRESULT, HSTRING},
        };

        struct ComApartment;
        impl ComApartment {
            fn enter() -> Result<Self> {
                unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
                    .map(|| Self)
                    .map_err(|error| {
                        Error::Platform(format!("could not initialize the save dialog: {error}"))
                    })
            }
        }
        impl Drop for ComApartment {
            fn drop(&mut self) {
                unsafe { CoUninitialize() };
            }
        }

        let _apartment = ComApartment::enter()?;
        let dialog: IFileSaveDialog =
            unsafe { CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER) }.map_err(
                |error| Error::Platform(format!("could not create save dialog: {error}")),
            )?;
        let options = unsafe { dialog.GetOptions() }
            .map_err(|error| Error::Platform(format!("could not read save options: {error}")))?;
        unsafe { dialog.SetOptions(options | FOS_FORCEFILESYSTEM | FOS_OVERWRITEPROMPT) }
            .map_err(|error| Error::Platform(format!("could not set save options: {error}")))?;
        unsafe { dialog.SetTitle(&HSTRING::from(&request.title)) }
            .map_err(|error| Error::Platform(format!("could not set save title: {error}")))?;
        unsafe { dialog.SetDefaultExtension(&HSTRING::from(&request.extension)) }.map_err(
            |error| Error::Platform(format!("could not set the default save extension: {error}")),
        )?;

        let name = request
            .suggested_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "suggested save filename is not valid Unicode: {}",
                    request.suggested_path.display()
                ))
            })?;
        unsafe { dialog.SetFileName(&HSTRING::from(name)) }
            .map_err(|error| Error::Platform(format!("could not set save filename: {error}")))?;
        if let Some(parent) = request.suggested_path.parent() {
            let folder: IShellItem = unsafe {
                SHCreateItemFromParsingName(&HSTRING::from(parent.to_string_lossy().as_ref()), None)
            }
            .map_err(|error| {
                Error::Platform(format!(
                    "could not open save directory {}: {error}",
                    parent.display()
                ))
            })?;
            unsafe { dialog.SetFolder(&folder) }.map_err(|error| {
                Error::Platform(format!("could not set save directory: {error}"))
            })?;
        }

        if let Err(error) = unsafe { dialog.Show(None) } {
            if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
                return Ok(None);
            }
            return Err(Error::Platform(format!(
                "the save dialog could not be shown: {error}"
            )));
        }

        let item = unsafe { dialog.GetResult() }
            .map_err(|error| Error::Platform(format!("save dialog returned no item: {error}")))?;
        let raw = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }
            .map_err(|error| Error::Platform(format!("save item has no path: {error}")))?;
        let path = unsafe { raw.to_string() }
            .map_err(|error| Error::Platform(format!("save path is invalid: {error}")))?;
        unsafe { CoTaskMemFree(Some(raw.0 as _)) };
        Ok(Some(PathBuf::from(path)))
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
/// The native desktop save-dialog implementation for Linux and other Unix
/// builds. GNOME uses `zenity`; KDE uses `kdialog`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeSaveFilePicker;

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
impl SaveFilePicker for NativeSaveFilePicker {
    fn pick_file(&self, request: &SaveFileRequest) -> Result<Option<PathBuf>> {
        if desktop_prefers_kde() {
            match run_kdialog(request) {
                Err(Error::Unsupported { .. }) => run_zenity(request),
                result => result,
            }
        } else {
            match run_zenity(request) {
                Err(Error::Unsupported { .. }) => run_kdialog(request),
                result => result,
            }
        }
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn desktop_prefers_kde() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .is_some_and(|desktop| desktop.to_ascii_lowercase().contains("kde"))
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn run_kdialog(request: &SaveFileRequest) -> Result<Option<PathBuf>> {
    let suggested = request.suggested_path.to_string_lossy().into_owned();
    let filter = format!(
        "*.{}|{} image",
        request.extension,
        request.extension.to_uppercase()
    );
    run_dialog(
        "kdialog",
        &[
            "--getsavefilename",
            &suggested,
            &filter,
            "--title",
            &request.title,
        ],
    )
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn run_zenity(request: &SaveFileRequest) -> Result<Option<PathBuf>> {
    let filename = format!("--filename={}", request.suggested_path.display());
    let title = format!("--title={}", request.title);
    let filter = format!(
        "--file-filter={} image | *.{}",
        request.extension.to_uppercase(),
        request.extension
    );
    run_dialog(
        "zenity",
        &[
            "--file-selection",
            "--save",
            "--confirm-overwrite",
            &title,
            &filename,
            &filter,
        ],
    )
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn run_dialog(program: &str, args: &[&str]) -> Result<Option<PathBuf>> {
    use std::process::Command;

    let output = match Command::new(program).args(args).output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::Unsupported {
                what: "native save dialog".to_owned(),
                why: format!("{program} is not installed"),
            });
        }
        Err(error) => return Err(Error::Io(error)),
    };
    if output.status.success() {
        let path = String::from_utf8(output.stdout)
            .map_err(|error| Error::Platform(format!("{program} returned invalid UTF-8: {error}")))?
            .trim()
            .to_owned();
        if path.is_empty() {
            return Err(Error::Platform(format!(
                "{program} succeeded without returning a path"
            )));
        }
        return Ok(Some(PathBuf::from(path)));
    }
    if matches!(output.status.code(), Some(1)) {
        return Ok(None);
    }
    Err(Error::Platform(format!(
        "{program} failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_derives_the_extension() {
        let request =
            SaveFileRequest::new("Save capture", PathBuf::from("/tmp/Capture.webp")).unwrap();
        assert_eq!(request.extension, "webp");
    }

    #[test]
    fn request_rejects_a_path_without_an_extension() {
        assert!(SaveFileRequest::new("Save capture", PathBuf::from("/tmp/Capture")).is_err());
    }

    #[test]
    fn stubs_distinguish_approval_from_cancellation() {
        let request =
            SaveFileRequest::new("Save capture", PathBuf::from("/tmp/Capture.png")).unwrap();
        assert_eq!(
            StubSaveFilePicker::selecting("/tmp/Approved.png")
                .pick_file(&request)
                .unwrap(),
            Some(PathBuf::from("/tmp/Approved.png"))
        );
        assert_eq!(
            StubSaveFilePicker::cancelling()
                .pick_file(&request)
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_selection_cannot_mislabel_the_encoded_format() {
        let request =
            SaveFileRequest::new("Save capture", PathBuf::from("/tmp/Capture.webp")).unwrap();
        assert_eq!(
            request.normalize_selection(PathBuf::from("/tmp/Approved.png")),
            PathBuf::from("/tmp/Approved.webp")
        );
    }
}
