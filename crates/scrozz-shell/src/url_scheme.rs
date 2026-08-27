//! Registration of the `scrozz://` URL scheme.
//!
//! Registration only tells the desktop how to launch Scrozz. Whether an
//! incoming URL is allowed to trigger anything is a separate, default-off
//! setting enforced by the application.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use scrozz_core::{
    Error, Result,
    identity::{BUNDLE_ID, PRODUCT_NAME, URL_SCHEME},
};

use crate::{
    CommandPlan, RegistrationStatus, SystemPlatform, autostart::write_registration_file,
    registry_value_inspection, registry_value_status,
};

const WINDOWS_CLASSES_KEY: &str = r"HKCU\Software\Classes";
const WINDOWS_CLASSES_SUBKEY: &str = r"Software\Classes";
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

/// The OS object that owns URL-scheme registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemeTarget {
    /// The application bundle whose Info.plist declares the scheme.
    ApplicationBundle(PathBuf),
    /// An XDG desktop entry.
    DesktopFile(PathBuf),
    /// A per-user Windows class.
    RegistryClass(String),
}

/// A complete scheme-registration plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemeRegistration {
    platform: SystemPlatform,
    target: SchemeTarget,
    contents: Option<Vec<u8>>,
    apply: Vec<CommandPlan>,
    remove: Vec<CommandPlan>,
    inspect: Option<CommandPlan>,
    handler_command: Option<String>,
}

impl SchemeRegistration {
    /// Computes the registration for any supported platform.
    ///
    /// `bundle` is the `.app` path on macOS and is ignored elsewhere.
    /// `data_home` is the resolved XDG data directory on Linux.
    ///
    /// # Errors
    ///
    /// Rejects paths that cannot be represented safely in the target format.
    pub fn for_platform(
        platform: SystemPlatform,
        executable: &Path,
        bundle: &Path,
        data_home: &Path,
    ) -> Result<Self> {
        let executable = path_text(executable)?;
        match platform {
            SystemPlatform::MacOS => {
                if bundle.extension().and_then(|value| value.to_str()) != Some("app") {
                    return Err(Error::InvalidRequest(format!(
                        "the macOS scheme owner must be a .app bundle, got {}",
                        bundle.display()
                    )));
                }
                Ok(Self {
                    platform,
                    target: SchemeTarget::ApplicationBundle(bundle.to_owned()),
                    contents: None,
                    apply: vec![CommandPlan::new(
                        LSREGISTER,
                        vec![OsString::from("-f"), bundle.as_os_str().to_owned()],
                    )],
                    remove: vec![CommandPlan::new(
                        LSREGISTER,
                        vec![OsString::from("-u"), bundle.as_os_str().to_owned()],
                    )],
                    inspect: None,
                    handler_command: None,
                })
            }
            SystemPlatform::Linux => {
                let file_name = format!("{BUNDLE_ID}-url.desktop");
                let target = data_home.join("applications").join(&file_name);
                let contents = format!(
                    "[Desktop Entry]\n\
                     Type=Application\n\
                     Version=1.0\n\
                     Name={PRODUCT_NAME} URL Handler\n\
                     Exec={} url handle %u\n\
                     Icon=scrozz\n\
                     Terminal=false\n\
                     NoDisplay=true\n\
                     MimeType=x-scheme-handler/{URL_SCHEME};\n",
                    desktop_exec(&executable)
                )
                .into_bytes();
                let mime = format!("x-scheme-handler/{URL_SCHEME}");
                Ok(Self {
                    platform,
                    target: SchemeTarget::DesktopFile(target),
                    contents: Some(contents),
                    apply: vec![CommandPlan::new(
                        "xdg-mime",
                        ["default", file_name.as_str(), mime.as_str()],
                    )],
                    remove: Vec::new(),
                    inspect: Some(CommandPlan::new(
                        "xdg-mime",
                        ["query", "default", mime.as_str()],
                    )),
                    handler_command: None,
                })
            }
            SystemPlatform::Windows => {
                if executable.contains('"') {
                    return Err(Error::InvalidRequest(
                        "the Windows executable path cannot contain a quote".to_owned(),
                    ));
                }
                let class = format!(r"{WINDOWS_CLASSES_KEY}\{URL_SCHEME}");
                let command_key = format!(r"{class}\shell\open\command");
                let command_subkey =
                    format!(r"{WINDOWS_CLASSES_SUBKEY}\{URL_SCHEME}\shell\open\command");
                let invocation = format!("\"{executable}\" url handle \"%1\"");
                let apply = vec![
                    reg_add(&class, None, &format!("URL:{PRODUCT_NAME} Protocol")),
                    reg_add(&class, Some("URL Protocol"), ""),
                    reg_add(&command_key, None, &invocation),
                ];
                let remove = vec![CommandPlan::new(
                    "reg.exe",
                    ["delete", class.as_str(), "/f"],
                )];
                let inspect = registry_value_inspection(&command_subkey, "", &invocation);
                Ok(Self {
                    platform,
                    target: SchemeTarget::RegistryClass(class),
                    contents: None,
                    apply,
                    remove,
                    inspect: Some(inspect),
                    handler_command: Some(invocation),
                })
            }
        }
    }

    /// The platform this plan targets.
    #[must_use]
    pub const fn platform(&self) -> SystemPlatform {
        self.platform
    }

    /// The bundle, desktop file, or registry class that owns registration.
    #[must_use]
    pub fn target(&self) -> &SchemeTarget {
        &self.target
    }

    /// Exact desktop-entry bytes, when applicable.
    #[must_use]
    pub fn contents(&self) -> Option<&[u8]> {
        self.contents.as_deref()
    }

    /// Commands run after writing any registration file.
    #[must_use]
    pub fn apply_commands(&self) -> &[CommandPlan] {
        &self.apply
    }

    /// Commands used to remove registration.
    #[must_use]
    pub fn remove_commands(&self) -> &[CommandPlan] {
        &self.remove
    }

    /// Applies this registration. It does not enable URL automation.
    ///
    /// # Errors
    ///
    /// Returns filesystem or platform-command failures.
    pub fn apply(&self) -> Result<()> {
        if let (SchemeTarget::DesktopFile(path), Some(contents)) = (&self.target, &self.contents) {
            write_registration_file(path, contents)?;
        }
        for command in &self.apply {
            command.apply("register URL scheme")?;
        }
        Ok(())
    }

    /// Removes this registration. It does not execute an incoming URL.
    ///
    /// # Errors
    ///
    /// Returns filesystem or platform-command failures.
    pub fn remove(&self) -> Result<()> {
        if let SchemeTarget::DesktopFile(path) = &self.target {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Error::Io(error)),
            }
        }
        if matches!(self.target, SchemeTarget::RegistryClass(_))
            && self.status()? == RegistrationStatus::Disabled
        {
            return Ok(());
        }
        for command in &self.remove {
            command.apply("unregister URL scheme")?;
        }
        Ok(())
    }

    /// Compares registration with this plan.
    ///
    /// # Errors
    ///
    /// Returns inspection failures. A missing registration is not an error.
    pub fn status(&self) -> Result<RegistrationStatus> {
        match (&self.target, &self.contents) {
            (SchemeTarget::ApplicationBundle(bundle), None) => {
                let plist = bundle.join("Contents/Info.plist");
                match fs::read_to_string(plist) {
                    Ok(contents)
                        if contents.contains("CFBundleURLSchemes")
                            && contents.contains(&format!("<string>{URL_SCHEME}</string>")) =>
                    {
                        Ok(RegistrationStatus::Enabled)
                    }
                    Ok(_) => Ok(RegistrationStatus::Drifted),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        Ok(RegistrationStatus::Disabled)
                    }
                    Err(error) => Err(Error::Io(error)),
                }
            }
            (SchemeTarget::DesktopFile(path), Some(contents)) => {
                let file_status = match fs::read(path) {
                    Ok(found) if found.as_slice() == contents.as_slice() => {
                        RegistrationStatus::Enabled
                    }
                    Ok(_) => RegistrationStatus::Drifted,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        RegistrationStatus::Disabled
                    }
                    Err(error) => return Err(Error::Io(error)),
                };
                if file_status != RegistrationStatus::Enabled {
                    return Ok(file_status);
                }
                let output = self
                    .inspect
                    .as_ref()
                    .ok_or_else(|| {
                        Error::InvalidRequest("desktop plan has no inspect command".to_owned())
                    })?
                    .output()?;
                if !output.status.success() {
                    return Ok(RegistrationStatus::Drifted);
                }
                let expected = format!("{BUNDLE_ID}-url.desktop");
                if String::from_utf8_lossy(&output.stdout).trim() == expected {
                    Ok(RegistrationStatus::Enabled)
                } else {
                    Ok(RegistrationStatus::Drifted)
                }
            }
            (SchemeTarget::RegistryClass(_), None) => {
                self.handler_command.as_deref().ok_or_else(|| {
                    Error::InvalidRequest("registry plan has no handler command".to_owned())
                })?;
                let inspect = self.inspect.as_ref().ok_or_else(|| {
                    Error::InvalidRequest("registry plan has no inspect command".to_owned())
                })?;
                registry_value_status(inspect, "inspect URL-handler registry value")
            }
            _ => Err(Error::InvalidRequest(
                "scheme plan target and contents disagree".to_owned(),
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
            "the executable path is not safe for scheme registration".to_owned(),
        ));
    }
    Ok(value.to_owned())
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

fn reg_add(key: &str, name: Option<&str>, value: &str) -> CommandPlan {
    let mut args = vec!["add", key];
    match name {
        Some(name) => args.extend(["/v", name]),
        None => args.push("/ve"),
    }
    args.extend(["/t", "REG_SZ", "/d", value, "/f"]);
    CommandPlan::new("reg.exe", args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_registration_uses_launch_services_and_the_bundle() {
        let plan = SchemeRegistration::for_platform(
            SystemPlatform::MacOS,
            Path::new("/Applications/Scrozz.app/Contents/MacOS/Scrozz"),
            Path::new("/Applications/Scrozz.app"),
            Path::new("/unused"),
        )
        .unwrap();
        assert_eq!(
            plan.target(),
            &SchemeTarget::ApplicationBundle(PathBuf::from("/Applications/Scrozz.app"))
        );
        assert_eq!(plan.apply_commands()[0].program(), Path::new(LSREGISTER));
        assert!(plan.apply_commands()[0].arg_eq(std::ffi::OsStr::new("/Applications/Scrozz.app")));
    }

    #[test]
    fn linux_registration_is_hidden_and_accepts_only_the_scheme() {
        let plan = SchemeRegistration::for_platform(
            SystemPlatform::Linux,
            Path::new("/opt/Scrozz/scrozz"),
            Path::new("/unused"),
            Path::new("/home/alice/.local/share"),
        )
        .unwrap();
        assert_eq!(
            plan.target(),
            &SchemeTarget::DesktopFile(PathBuf::from(
                "/home/alice/.local/share/applications/com.thatcube.Scrozz-url.desktop"
            ))
        );
        let contents = String::from_utf8(plan.contents().unwrap().to_vec()).unwrap();
        assert!(contents.contains("Exec=\"/opt/Scrozz/scrozz\" url handle %u"));
        assert!(contents.contains("NoDisplay=true"));
        assert!(contents.contains("MimeType=x-scheme-handler/scrozz;"));
    }

    #[test]
    fn linux_handler_escapes_desktop_field_codes_in_executable_paths() {
        let plan = SchemeRegistration::for_platform(
            SystemPlatform::Linux,
            Path::new("/opt/scrozz%f/scrozz"),
            Path::new("/unused"),
            Path::new("/home/alice/.local/share"),
        )
        .unwrap();
        let contents = String::from_utf8(plan.contents().unwrap().to_vec()).unwrap();
        assert!(contents.contains("Exec=\"/opt/scrozz%%f/scrozz\" url handle %u"));
    }

    #[test]
    fn windows_registration_never_interpolates_the_url_into_a_shell() {
        let plan = SchemeRegistration::for_platform(
            SystemPlatform::Windows,
            Path::new(r"C:\Program Files\Scrozz\scrozz.exe"),
            Path::new("/unused"),
            Path::new("/unused"),
        )
        .unwrap();
        assert_eq!(plan.apply_commands().len(), 3);
        assert!(plan.apply_commands().iter().all(|command| {
            command.program() == Path::new("reg.exe")
                && !command.arg_eq(std::ffi::OsStr::new("cmd.exe"))
        }));
        assert!(plan.apply_commands()[2].arg_eq(std::ffi::OsStr::new(
            r#""C:\Program Files\Scrozz\scrozz.exe" url handle "%1""#
        )));
        let inspect = plan.inspect.as_ref().unwrap();
        assert_eq!(inspect.program(), Path::new("powershell.exe"));
        assert!(inspect.env().iter().any(|(key, value)| {
            key == "SCROZZ_REGISTRY_EXPECTED"
                && value == r#""C:\Program Files\Scrozz\scrozz.exe" url handle "%1""#
        }));
    }

    #[test]
    fn desktop_registration_apply_and_remove_are_explicit() {
        let root = std::env::temp_dir().join(format!(
            "scrozz-scheme-{}-{}",
            std::process::id(),
            crate::test_support::next_nonce()
        ));
        let plan = SchemeRegistration::for_platform(
            SystemPlatform::Linux,
            Path::new("/opt/scrozz/scrozz"),
            Path::new("/unused"),
            &root,
        )
        .unwrap();
        let SchemeTarget::DesktopFile(path) = plan.target() else {
            panic!("expected desktop file")
        };
        write_registration_file(path, plan.contents().unwrap()).unwrap();
        assert_eq!(fs::read(path).unwrap(), plan.contents().unwrap());
        fs::remove_file(path).unwrap();
        let _ = fs::remove_dir_all(root);
    }
}
