//! Launch-at-login plans and mutation.
//!
//! Planning is pure and host-independent. Applying a plan is explicit and is
//! the only code in this module that writes a file or invokes an OS command.

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use scrozz_core::{
    Error, Result,
    identity::{AUTOSTART_LABEL, PRODUCT_NAME, WINDOWS_STARTUP_TASK_ID},
};

use crate::{
    CommandPlan, RegistrationStatus, SystemPlatform, registry_value_inspection,
    registry_value_status,
};

const WINDOWS_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const WINDOWS_RUN_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
static REGISTRATION_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The OS object that owns launch-at-login state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutostartTarget {
    /// A LaunchAgent or XDG autostart file.
    File(PathBuf),
    /// A named value under the current user's Windows Run key.
    RegistryValue {
        /// Registry key path.
        key: String,
        /// Value name.
        name: String,
    },
    /// The default-off startup task declared by the installed MSIX manifest.
    PackageStartupTask {
        /// Manifest `TaskId`.
        task_id: String,
    },
}

/// A complete launch-at-login plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutostartPlan {
    platform: SystemPlatform,
    target: AutostartTarget,
    contents: Option<Vec<u8>>,
    launch_command: Vec<String>,
    enable: Vec<CommandPlan>,
    disable: Vec<CommandPlan>,
    inspect: Option<CommandPlan>,
}

impl AutostartPlan {
    /// Computes the plan for any supported platform without applying it.
    ///
    /// `home` is used for the macOS LaunchAgents directory. `config_home` is the
    /// resolved XDG config directory on Linux. Windows records no file.
    ///
    /// # Errors
    ///
    /// Rejects executable paths containing NUL, quotes on Windows, or newlines
    /// that would corrupt a plist or desktop entry.
    pub fn for_platform(
        platform: SystemPlatform,
        executable: &Path,
        home: &Path,
        config_home: &Path,
    ) -> Result<Self> {
        Self::for_platform_with_windows_package(platform, executable, home, config_home, false)
    }

    /// Computes a plan while explicitly selecting Windows package ownership.
    ///
    /// Tests and packaging checks use this entry point to inspect both Windows
    /// distributions from any host. Callers should pass `true` only after the
    /// runtime package identity probe reports MSIX.
    ///
    /// # Errors
    ///
    /// As [`Self::for_platform`].
    pub fn for_platform_with_windows_package(
        platform: SystemPlatform,
        executable: &Path,
        home: &Path,
        config_home: &Path,
        windows_package: bool,
    ) -> Result<Self> {
        let executable = path_text(executable)?;
        match platform {
            SystemPlatform::MacOS => {
                let launch_command = vec![executable.clone(), "gui".to_owned()];
                let target = home
                    .join("Library")
                    .join("LaunchAgents")
                    .join(format!("{AUTOSTART_LABEL}.plist"));
                let contents = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                     <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
                     \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                     <plist version=\"1.0\">\n\
                     <dict>\n\
                     \x20 <key>Label</key><string>{}</string>\n\
                     \x20 <key>ProgramArguments</key>\n\
                     \x20 <array><string>{}</string><string>gui</string></array>\n\
                     \x20 <key>RunAtLoad</key><true/>\n\
                     </dict>\n\
                     </plist>\n",
                    xml(AUTOSTART_LABEL),
                    xml(&executable)
                )
                .into_bytes();
                Ok(Self {
                    platform,
                    target: AutostartTarget::File(target),
                    contents: Some(contents),
                    launch_command,
                    enable: Vec::new(),
                    disable: Vec::new(),
                    inspect: None,
                })
            }
            SystemPlatform::Linux => {
                let launch_command = vec![executable.clone(), "gui".to_owned()];
                let target = config_home
                    .join("autostart")
                    .join(format!("{AUTOSTART_LABEL}.desktop"));
                let contents = format!(
                    "[Desktop Entry]\n\
                     Type=Application\n\
                     Version=1.0\n\
                     Name={PRODUCT_NAME}\n\
                     Exec={} gui\n\
                     Terminal=false\n\
                     X-GNOME-Autostart-enabled=true\n",
                    desktop_exec(&executable)
                )
                .into_bytes();
                Ok(Self {
                    platform,
                    target: AutostartTarget::File(target),
                    contents: Some(contents),
                    launch_command,
                    enable: Vec::new(),
                    disable: Vec::new(),
                    inspect: None,
                })
            }
            SystemPlatform::Windows => {
                if windows_package {
                    return Ok(Self {
                        platform,
                        target: AutostartTarget::PackageStartupTask {
                            task_id: WINDOWS_STARTUP_TASK_ID.to_owned(),
                        },
                        contents: None,
                        launch_command: vec![executable, "gui".to_owned()],
                        enable: Vec::new(),
                        disable: Vec::new(),
                        inspect: None,
                    });
                }
                if executable.contains('"') {
                    return Err(Error::InvalidRequest(
                        "the Windows executable path cannot contain a quote".to_owned(),
                    ));
                }
                let command = format!("\"{executable}\" gui");
                let enable = CommandPlan::new(
                    "reg.exe",
                    [
                        "add",
                        WINDOWS_RUN_KEY,
                        "/v",
                        AUTOSTART_LABEL,
                        "/t",
                        "REG_SZ",
                        "/d",
                        command.as_str(),
                        "/f",
                    ],
                );
                let disable = CommandPlan::new(
                    "reg.exe",
                    ["delete", WINDOWS_RUN_KEY, "/v", AUTOSTART_LABEL, "/f"],
                );
                let inspect =
                    registry_value_inspection(WINDOWS_RUN_SUBKEY, AUTOSTART_LABEL, &command);
                Ok(Self {
                    platform,
                    target: AutostartTarget::RegistryValue {
                        key: WINDOWS_RUN_KEY.to_owned(),
                        name: AUTOSTART_LABEL.to_owned(),
                    },
                    contents: None,
                    launch_command: vec![executable, "gui".to_owned()],
                    enable: vec![enable],
                    disable: vec![disable],
                    inspect: Some(inspect),
                })
            }
        }
    }

    /// The platform this plan targets.
    #[must_use]
    pub const fn platform(&self) -> SystemPlatform {
        self.platform
    }

    /// The file or registry value that owns the setting.
    #[must_use]
    pub fn target(&self) -> &AutostartTarget {
        &self.target
    }

    /// Exact file bytes, when the platform uses a file.
    #[must_use]
    pub fn contents(&self) -> Option<&[u8]> {
        self.contents.as_deref()
    }

    /// The exact command the OS will launch.
    #[must_use]
    pub fn launch_command(&self) -> &[String] {
        &self.launch_command
    }

    /// Commands used to enable the plan.
    #[must_use]
    pub fn enable_commands(&self) -> &[CommandPlan] {
        &self.enable
    }

    /// Commands used to disable the plan.
    #[must_use]
    pub fn disable_commands(&self) -> &[CommandPlan] {
        &self.disable
    }

    /// Applies this plan.
    ///
    /// # Errors
    ///
    /// Returns an I/O or platform-command failure without reporting success.
    pub fn apply(&self) -> Result<()> {
        match (&self.target, &self.contents) {
            (AutostartTarget::File(path), Some(contents)) => {
                write_registration_file(path, contents)
            }
            (AutostartTarget::RegistryValue { .. }, None) => {
                for command in &self.enable {
                    command.apply("enable launch at login")?;
                }
                Ok(())
            }
            (AutostartTarget::PackageStartupTask { task_id }, None) => {
                crate::package::set_startup_task_enabled(task_id, true)
            }
            _ => Err(Error::InvalidRequest(
                "autostart plan target and contents disagree".to_owned(),
            )),
        }
    }

    /// Removes this plan's registration.
    ///
    /// # Errors
    ///
    /// Returns a real filesystem or platform-command failure. A missing
    /// registration is already disabled and succeeds.
    pub fn remove(&self) -> Result<()> {
        match &self.target {
            AutostartTarget::File(path) => remove_file(path),
            AutostartTarget::RegistryValue { .. } => {
                if self.status()? == RegistrationStatus::Disabled {
                    return Ok(());
                }
                for command in &self.disable {
                    command.apply("disable launch at login")?;
                }
                Ok(())
            }
            AutostartTarget::PackageStartupTask { task_id } => {
                crate::package::set_startup_task_enabled(task_id, false)
            }
        }
    }

    /// Compares installed state with the plan.
    ///
    /// # Errors
    ///
    /// Returns filesystem inspection or registry-command failures.
    pub fn status(&self) -> Result<RegistrationStatus> {
        match (&self.target, &self.contents) {
            (AutostartTarget::File(path), Some(contents)) => match fs::read(path) {
                Ok(found) if found.as_slice() == contents.as_slice() => {
                    Ok(RegistrationStatus::Enabled)
                }
                Ok(_) => Ok(RegistrationStatus::Drifted),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(RegistrationStatus::Disabled)
                }
                Err(error) => Err(Error::Io(error)),
            },
            (AutostartTarget::RegistryValue { .. }, None) => {
                let inspect = self.inspect.as_ref().ok_or_else(|| {
                    Error::InvalidRequest("registry plan has no inspect command".to_owned())
                })?;
                registry_value_status(inspect, "inspect launch-at-login registry value")
            }
            (AutostartTarget::PackageStartupTask { task_id }, None) => {
                crate::package::startup_task_status(task_id)
            }
            _ => Err(Error::InvalidRequest(
                "autostart plan target and contents disagree".to_owned(),
            )),
        }
    }
}

fn path_text(path: &Path) -> Result<String> {
    let value = path.to_str().ok_or_else(|| {
        Error::InvalidRequest("the executable path is not valid Unicode".to_owned())
    })?;
    if value.is_empty() || value.contains('\0') || value.contains('\n') || value.contains('\r') {
        return Err(Error::InvalidRequest(
            "the executable path is not safe for an autostart entry".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn desktop_exec(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

pub(crate) fn write_registration_file(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::InvalidRequest(format!("{} has no parent", path.display())))?;
    fs::create_dir_all(parent)?;
    let (temporary, mut file) = reserve_registration_temp(parent)?;
    let result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(Error::Io)
}

fn reserve_registration_temp(parent: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    for _ in 0..128 {
        let sequence = REGISTRATION_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".scrozz-registration-{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                return Ok((candidate, file));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "could not reserve a registration temporary in {}",
            parent.display()
        ),
    ))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?
        .sync_all()
}

#[cfg(not(any(unix, windows)))]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> (&'static Path, &'static Path, &'static Path) {
        (
            Path::new("/Users/alice"),
            Path::new("/Users/alice/.config"),
            Path::new("/Applications/Scrozz.app/Contents/MacOS/Scrozz"),
        )
    }

    #[test]
    fn macos_plan_is_a_launch_agent_with_exact_arguments() {
        let (home, config, executable) = context();
        let plan =
            AutostartPlan::for_platform(SystemPlatform::MacOS, executable, home, config).unwrap();
        assert_eq!(
            plan.target(),
            &AutostartTarget::File(home.join("Library/LaunchAgents/com.thatcube.Scrozz.plist"))
        );
        let text = String::from_utf8(plan.contents().unwrap().to_vec()).unwrap();
        assert!(text.contains("<string>com.thatcube.Scrozz</string>"));
        assert!(text.contains("<string>gui</string>"));
        assert_eq!(
            plan.launch_command(),
            ["/Applications/Scrozz.app/Contents/MacOS/Scrozz", "gui"]
        );
    }

    #[test]
    fn linux_plan_is_an_xdg_autostart_entry() {
        let (home, config, executable) = context();
        let plan =
            AutostartPlan::for_platform(SystemPlatform::Linux, executable, home, config).unwrap();
        assert_eq!(
            plan.target(),
            &AutostartTarget::File(config.join("autostart/com.thatcube.Scrozz.desktop"))
        );
        let text = String::from_utf8(plan.contents().unwrap().to_vec()).unwrap();
        assert!(text.contains("X-GNOME-Autostart-enabled=true"));
        assert!(text.contains("Exec=\"/Applications/Scrozz.app/Contents/MacOS/Scrozz\" gui"));
    }

    #[test]
    fn linux_exec_escapes_desktop_field_codes_in_paths() {
        let plan = AutostartPlan::for_platform(
            SystemPlatform::Linux,
            Path::new("/opt/scrozz%u/scrozz"),
            Path::new("/home/alice"),
            Path::new("/home/alice/.config"),
        )
        .unwrap();
        let text = String::from_utf8(plan.contents().unwrap().to_vec()).unwrap();
        assert!(text.contains("Exec=\"/opt/scrozz%%u/scrozz\" gui"));
    }

    #[test]
    fn windows_plan_uses_the_current_user_run_key() {
        let plan = AutostartPlan::for_platform(
            SystemPlatform::Windows,
            Path::new(r"C:\Program Files\Scrozz\scrozz.exe"),
            Path::new(r"C:\Users\alice"),
            Path::new(r"C:\Users\alice\AppData\Roaming"),
        )
        .unwrap();
        assert!(matches!(
            plan.target(),
            AutostartTarget::RegistryValue { key, name }
                if key == WINDOWS_RUN_KEY && name == AUTOSTART_LABEL
        ));
        assert_eq!(plan.enable_commands().len(), 1);
        assert!(plan.enable_commands()[0].arg_eq(std::ffi::OsStr::new(
            r#""C:\Program Files\Scrozz\scrozz.exe" gui"#
        )));
        let inspect = plan.inspect.as_ref().unwrap();
        assert_eq!(inspect.program(), Path::new("powershell.exe"));
        assert!(inspect.env().iter().any(|(key, value)| {
            key == "SCROZZ_REGISTRY_EXPECTED"
                && value == r#""C:\Program Files\Scrozz\scrozz.exe" gui"#
        }));
    }

    #[test]
    fn packaged_windows_plan_uses_the_manifest_startup_task() {
        let plan = AutostartPlan::for_platform_with_windows_package(
            SystemPlatform::Windows,
            Path::new(r"C:\Program Files\WindowsApps\Scrozz\scrozz.exe"),
            Path::new(r"C:\Users\alice"),
            Path::new(r"C:\Users\alice\AppData\Roaming"),
            true,
        )
        .unwrap();
        assert_eq!(
            plan.target(),
            &AutostartTarget::PackageStartupTask {
                task_id: WINDOWS_STARTUP_TASK_ID.to_owned()
            }
        );
        assert!(plan.enable_commands().is_empty());
        assert!(plan.disable_commands().is_empty());
        assert_eq!(
            plan.launch_command(),
            [r"C:\Program Files\WindowsApps\Scrozz\scrozz.exe", "gui"]
        );
    }

    #[test]
    fn file_apply_status_and_remove_are_separate() {
        let root = std::env::temp_dir().join(format!(
            "scrozz-autostart-{}-{}",
            std::process::id(),
            crate::test_support::next_nonce()
        ));
        let plan = AutostartPlan::for_platform(
            SystemPlatform::Linux,
            Path::new("/opt/scrozz/scrozz"),
            &root,
            &root,
        )
        .unwrap();
        assert_eq!(plan.status().unwrap(), RegistrationStatus::Disabled);
        plan.apply().unwrap();
        assert_eq!(plan.status().unwrap(), RegistrationStatus::Enabled);
        plan.remove().unwrap();
        assert_eq!(plan.status().unwrap(), RegistrationStatus::Disabled);
        let _ = fs::remove_dir_all(root);
    }
}
