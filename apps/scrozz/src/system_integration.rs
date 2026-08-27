//! Host path resolution and composition of pure `scrozz-shell` plans.

use std::{
    env,
    path::{Path, PathBuf},
};

use scrozz_core::Error as CoreError;
use scrozz_shell::{
    PackageKind, SystemPlatform, autostart::AutostartPlan, url_scheme::SchemeRegistration,
};

use crate::{
    fault::{CliError, CliResult},
    settings_store::SettingsStore,
};

const EXECUTABLE_ENV: &str = "SCROZZ_EXECUTABLE";
const HOME_ENV: &str = "SCROZZ_HOME";
const CONFIG_HOME_ENV: &str = "SCROZZ_CONFIG_HOME";
const DATA_HOME_ENV: &str = "SCROZZ_DATA_HOME";
const BUNDLE_ENV: &str = "SCROZZ_APP_BUNDLE";

/// Resolved paths used to construct system-integration plans.
#[derive(Debug, Clone)]
pub struct SystemContext {
    pub platform: SystemPlatform,
    pub package_kind: PackageKind,
    pub executable: PathBuf,
    pub home: PathBuf,
    pub config_home: PathBuf,
    pub data_home: PathBuf,
    pub bundle: PathBuf,
    pub update_state: PathBuf,
}

impl SystemContext {
    /// Resolves the current host and per-user paths.
    ///
    /// # Errors
    ///
    /// Returns a platform or storage error when a required host path is absent.
    pub fn current() -> CliResult<Self> {
        let platform = SystemPlatform::current()?;
        let package_kind = scrozz_shell::package_kind()?;
        let executable = optional_path(EXECUTABLE_ENV)
            .map_or_else(env::current_exe, Ok)
            .map_err(CoreError::Io)?;
        let home = optional_path(HOME_ENV)
            .or_else(dirs::home_dir)
            .ok_or_else(|| storage("this platform has no user home directory"))?;
        let config_home = optional_path(CONFIG_HOME_ENV)
            .or_else(dirs::config_dir)
            .ok_or_else(|| storage("this platform has no user configuration directory"))?;
        let data_home = optional_path(DATA_HOME_ENV)
            .or_else(dirs::data_dir)
            .ok_or_else(|| storage("this platform has no user data directory"))?;
        let bundle = optional_path(BUNDLE_ENV)
            .or_else(|| enclosing_app_bundle(&executable))
            .unwrap_or_else(|| PathBuf::from("/Applications/Scrozz.app"));
        let update_state = SettingsStore::open_default()?
            .path()
            .parent()
            .ok_or_else(|| storage("the settings path has no parent directory"))?
            .join("update-state.json");
        Ok(Self {
            platform,
            package_kind,
            executable,
            home,
            config_home,
            data_home,
            bundle,
            update_state,
        })
    }

    /// Creates the launch-at-login plan for this context.
    pub fn autostart(&self) -> CliResult<AutostartPlan> {
        Ok(AutostartPlan::for_platform_with_windows_package(
            self.platform,
            &self.executable,
            &self.home,
            &self.config_home,
            self.package_kind.is_msix(),
        )?)
    }

    /// Creates the URL-scheme registration plan for this context.
    pub fn url_scheme(&self) -> CliResult<SchemeRegistration> {
        Ok(SchemeRegistration::for_platform_with_windows_package(
            self.platform,
            &self.executable,
            &self.bundle,
            &self.data_home,
            self.package_kind.is_msix(),
        )?)
    }
}

fn optional_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn enclosing_app_bundle(executable: &Path) -> Option<PathBuf> {
    executable
        .ancestors()
        .find(|ancestor| ancestor.extension().and_then(|value| value.to_str()) == Some("app"))
        .map(Path::to_path_buf)
}

fn storage(message: impl Into<String>) -> CliError {
    CliError::Core(CoreError::Storage(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_enclosing_application_bundle() {
        assert_eq!(
            enclosing_app_bundle(Path::new("/Applications/Scrozz.app/Contents/MacOS/scrozz")),
            Some(PathBuf::from("/Applications/Scrozz.app"))
        );
        assert_eq!(enclosing_app_bundle(Path::new("/usr/bin/scrozz")), None);
    }
}
