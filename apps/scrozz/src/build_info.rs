//! Compile-time identity shared by the CLI and graphical About view.

/// The user-facing application version.
pub const VERSION: &str = match option_env!("SCROZZ_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

/// The monotonically increasing package build number.
pub const BUILD: &str = match option_env!("SCROZZ_BUILD_NUMBER") {
    Some(build) => build,
    None => "development",
};
