//! Application-facing private sharing.

#[cfg(feature = "cloud")]
use crate::cli::CloudProvider;
use crate::{
    cli::ShareArgs,
    fault::{CliError, CliResult},
};

/// Secret-free upload capability exposed to capture actions and Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadAvailability {
    /// Both the backend and provider configuration are usable.
    pub enabled: bool,
    /// Accessible explanation when disabled.
    pub reason: Option<String>,
}

impl UploadAvailability {
    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            enabled: false,
            reason: Some(reason.into()),
        }
    }
}

/// Kind of finalized artifact handed to sharing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    /// Encoded still image after all destructive redactions.
    Screenshot,
    /// Final playable recording after trimming/redaction.
    Recording,
}

impl ArtifactKind {
    /// Stable token for naming and history.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Screenshot => "screenshot",
            Self::Recording => "recording",
        }
    }
}

/// Immutable bytes for one exact finalized editor/recording revision.
#[derive(Clone)]
pub struct FinalizedArtifact {
    bytes: Vec<u8>,
    content_type: String,
    file_name: String,
    kind: ArtifactKind,
}

impl std::fmt::Debug for FinalizedArtifact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FinalizedArtifact")
            .field("bytes", &format_args!("[{} BYTES]", self.bytes.len()))
            .field("content_type", &self.content_type)
            .field("file_name", &self.file_name)
            .field("kind", &self.kind)
            .finish()
    }
}

impl FinalizedArtifact {
    /// Final PNG produced by the screenshot/editor pipeline.
    pub fn screenshot_png(bytes: Vec<u8>, file_name: impl Into<String>) -> CliResult<Self> {
        Self::new(bytes, "image/png", file_name, ArtifactKind::Screenshot)
    }

    /// Final recording produced by a platform recorder/editor.
    pub fn recording(
        bytes: Vec<u8>,
        content_type: impl Into<String>,
        file_name: impl Into<String>,
    ) -> CliResult<Self> {
        Self::new(bytes, content_type, file_name, ArtifactKind::Recording)
    }

    fn new(
        bytes: Vec<u8>,
        content_type: impl Into<String>,
        file_name: impl Into<String>,
        kind: ArtifactKind,
    ) -> CliResult<Self> {
        let content_type = content_type.into();
        let file_name = file_name.into();
        if bytes.is_empty() {
            return Err(CliError::usage(
                "a finalized share artifact cannot be empty",
            ));
        }
        if file_name.is_empty()
            || file_name.len() > 255
            || file_name
                .bytes()
                .any(|byte| byte.is_ascii_control() || matches!(byte, b'/' | b'\\'))
        {
            return Err(CliError::usage(
                "a finalized share artifact needs a safe 1 to 255 byte file name",
            ));
        }
        let valid_content_type = match kind {
            ArtifactKind::Screenshot => {
                matches!(
                    content_type.as_str(),
                    "image/png" | "image/jpeg" | "image/webp"
                )
            }
            ArtifactKind::Recording => matches!(
                content_type.as_str(),
                "image/gif" | "video/mp4" | "video/quicktime" | "video/webm" | "video/x-matroska"
            ),
        };
        if !valid_content_type {
            return Err(CliError::usage(format!(
                "{content_type:?} is not a supported finalized {} type",
                kind.slug()
            )));
        }
        Ok(Self {
            bytes,
            content_type,
            file_name,
            kind,
        })
    }

    /// Exact encoded bytes for this revision.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Browser media type.
    #[must_use]
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// Safe finalized file name.
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// Screenshot or recording.
    #[must_use]
    pub const fn kind(&self) -> ArtifactKind {
        self.kind
    }
}

/// Secret-free native credential status for Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialStatus {
    /// Native store name.
    pub backend: &'static str,
    /// Whether a complete provider credential entry exists.
    pub stored: bool,
    /// Safe explanation when the store cannot be reached.
    pub problem: Option<String>,
}

/// Whether upload actions may be offered in this build and profile.
#[must_use]
pub fn upload_availability() -> UploadAvailability {
    upload_availability_impl()
}

/// Status of the selected provider's native credential entry.
#[must_use]
pub fn credential_status() -> CredentialStatus {
    credential_status_impl()
}

/// Adds or replaces native provider credentials and an optional share password.
pub fn store_credentials(
    access_key_id: &str,
    secret_access_key: &str,
    session_token: Option<&str>,
    share_password: Option<&str>,
) -> CliResult<()> {
    store_credentials_impl(
        access_key_id,
        secret_access_key,
        session_token,
        share_password,
    )
}

/// Removes the selected provider's native credential entry.
pub fn remove_credentials() -> CliResult<bool> {
    remove_credentials_impl()
}

/// Performs a signed, read-only bucket probe using current settings.
pub fn test_connection() -> CliResult<()> {
    test_connection_impl()
}

/// Builds the secret-free Settings model from disk and native-vault status.
pub fn settings_model(
    connection: scrozz_ui::CloudConnectionState,
) -> CliResult<scrozz_ui::CloudSettingsModel> {
    let settings = crate::settings::SettingsStore::default_location()?.load()?;
    let availability = upload_availability_impl();
    let credentials = credential_status_impl();
    Ok(build_settings_model(
        &settings,
        availability,
        credentials,
        connection,
    ))
}

/// Model used when the settings document itself cannot be read.
#[must_use]
pub fn settings_error_model(reason: impl Into<String>) -> scrozz_ui::CloudSettingsModel {
    build_settings_model(
        &crate::settings::StoredSettings::default(),
        UploadAvailability::unavailable(reason),
        credential_status_impl(),
        scrozz_ui::CloudConnectionState::Idle,
    )
}

/// Secret-free model for tests that promise not to touch native services.
#[must_use]
pub fn sealed_settings_model() -> scrozz_ui::CloudSettingsModel {
    build_settings_model(
        &crate::settings::StoredSettings::default(),
        UploadAvailability::unavailable("sharing is disabled in this sealed run"),
        CredentialStatus {
            backend: "native credential vault",
            stored: false,
            problem: Some("not queried in a sealed run".to_owned()),
        },
        scrozz_ui::CloudConnectionState::Idle,
    )
}

fn build_settings_model(
    settings: &crate::settings::StoredSettings,
    availability: UploadAvailability,
    credentials: CredentialStatus,
    connection: scrozz_ui::CloudConnectionState,
) -> scrozz_ui::CloudSettingsModel {
    scrozz_ui::CloudSettingsModel {
        config: scrozz_ui::CloudSettingsDraft {
            provider: setting_value(settings, "cloud.provider"),
            bucket: setting_value(settings, "cloud.bucket"),
            region: setting_value(settings, "cloud.region"),
            endpoint: setting_value(settings, "cloud.endpoint"),
            account_id: setting_value(settings, "cloud.account-id"),
            prefix: setting_value(settings, "cloud.prefix"),
            public_base_url: setting_value(settings, "cloud.public-base-url"),
            url_policy: setting_value(settings, "cloud.url-policy"),
            expiry_seconds: settings
                .value("cloud.expiry-seconds")
                .unwrap_or("86400")
                .parse()
                .unwrap_or(86_400),
            naming_template: setting_value(settings, "cloud.naming-template"),
            tags: setting_value(settings, "cloud.tags"),
            protection_mode: setting_value(settings, "cloud.protection-mode"),
            viewer_title: setting_value(settings, "cloud.viewer-title"),
            viewer_accent: setting_value(settings, "cloud.viewer-accent"),
        },
        credentials: scrozz_ui::CloudCredentialView {
            backend: credentials.backend.to_owned(),
            stored: credentials.stored,
            problem: credentials.problem,
        },
        upload_enabled: availability.enabled,
        unavailable_reason: availability.reason,
        connection,
    }
}

/// Persists every non-secret field from the Settings viewport atomically.
pub fn save_settings(draft: &scrozz_ui::CloudSettingsDraft) -> CliResult<()> {
    let values = [
        ("cloud.provider", draft.provider.as_str()),
        ("cloud.bucket", draft.bucket.as_str()),
        ("cloud.region", draft.region.as_str()),
        ("cloud.endpoint", draft.endpoint.as_str()),
        ("cloud.account-id", draft.account_id.as_str()),
        ("cloud.prefix", draft.prefix.as_str()),
        ("cloud.public-base-url", draft.public_base_url.as_str()),
        ("cloud.url-policy", draft.url_policy.as_str()),
        ("cloud.naming-template", draft.naming_template.as_str()),
        ("cloud.tags", draft.tags.as_str()),
        ("cloud.protection-mode", draft.protection_mode.as_str()),
        ("cloud.viewer-title", draft.viewer_title.as_str()),
        ("cloud.viewer-accent", draft.viewer_accent.as_str()),
    ];
    let expiry = draft.expiry_seconds.to_string();
    crate::settings::SettingsStore::default_location()?
        .update(|settings| {
            for (key, value) in values {
                settings.set(key, value)?;
            }
            settings.set("cloud.expiry-seconds", &expiry)
        })
        .map(|_| ())
}

fn setting_value(settings: &crate::settings::StoredSettings, key: &str) -> String {
    settings.value(key).unwrap_or_default().to_owned()
}

/// A completed share. It intentionally has no `Debug`: a presigned URL is a
/// bearer value and belongs in explicit command output or the clipboard, not a
/// log line.
#[derive(Clone)]
pub struct Shared {
    /// URL for the recipient.
    pub url: String,
    /// Final object key.
    pub key: String,
    /// Provider slug.
    pub provider: &'static str,
    /// Provider-enforced lifetime.
    pub expires_seconds: Option<u32>,
    /// Exact wall-clock instant at which a signed URL stops being valid.
    pub expires_at: Option<std::time::SystemTime>,
    /// Bucket lifecycle XML matching the PUT tag or reserved prefix.
    pub lifecycle_rule: Option<String>,
    /// Whether the uploaded object contains only ciphertext.
    pub encrypted: bool,
    /// Non-secret object tags recorded with capture history.
    pub tags: Vec<(String, String)>,
    /// Final media kind represented by the object.
    pub media_kind: ArtifactKind,
}

/// Cancels a queued or retrying card upload during application shutdown.
#[derive(Debug, Clone, Default)]
pub struct ShareCancellation {
    #[cfg(feature = "cloud")]
    inner: scrozz_cloud::CancellationToken,
}

impl ShareCancellation {
    /// Requests cooperative cancellation.
    pub fn cancel(&self) {
        #[cfg(feature = "cloud")]
        self.inner.cancel();
    }

    /// Whether shutdown has requested cancellation.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        #[cfg(feature = "cloud")]
        {
            self.inner.is_cancelled()
        }
        #[cfg(not(feature = "cloud"))]
        {
            false
        }
    }
}

/// Shares a file named by the CLI.
pub fn share_file(args: &ShareArgs) -> CliResult<Shared> {
    args.validate()?;
    if !args.file.is_file() {
        return Err(CliError::usage(format!(
            "{} is not an image file",
            args.file.display()
        )));
    }
    share_file_impl(args)
}

/// Shares one card's cached PNG.
pub fn share_artifact(
    artifact: &FinalizedArtifact,
    card: u64,
    cancellation: &ShareCancellation,
) -> CliResult<Shared> {
    share_artifact_impl(artifact, card, cancellation)
}

#[cfg(feature = "cloud")]
fn share_file_impl(args: &ShareArgs) -> CliResult<Shared> {
    use scrozz_cloud::{
        Branding, CredentialCommand, Expiry, ExpiryPolicy, ObjectTag, Secret, ShareInput,
        ShareOptions, unique_object_key,
    };

    let bytes = std::fs::read(&args.file)?;
    let format = scrozz_export::ImageFormat::sniff(&bytes).ok_or_else(|| {
        CliError::usage(
            "scrozz share accepts PNG, JPEG or WebP captures; the file's bytes \
             do not match any of those formats",
        )
    })?;
    let file_name = args
        .file
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CliError::usage("the shared file needs a UTF-8 file name"))?;
    let generated_key = if args.key.is_none() {
        Some(unique_object_key(file_name).map_err(core_error)?)
    } else {
        None
    };
    let key = args
        .key
        .as_deref()
        .or(generated_key.as_deref())
        .ok_or_else(|| CliError::usage("could not generate an object key"))?;
    let branding = match (&args.title, &args.accent) {
        (None, None) => None,
        (title, accent) => Some(Branding {
            title: title.clone().unwrap_or_else(|| Branding::default().title),
            accent: accent.clone().unwrap_or_else(|| Branding::default().accent),
        }),
    };
    let credential_command = args
        .credential_command
        .as_ref()
        .map(|program| CredentialCommand {
            program: program.clone(),
            args: args.credential_args.to_vec(),
            access_key_id: None,
        });
    let expiry = if args.no_expiry {
        ExpiryPolicy::Never
    } else if let Some(expiry) = args.expires {
        ExpiryPolicy::After(Expiry::from_seconds(expiry.seconds()).map_err(core_error)?)
    } else {
        ExpiryPolicy::Configured
    };
    let mut overrides = stored_overrides()?;
    if let Some(selected) = args.provider {
        overrides.provider = Some(provider(selected));
    }
    replace_some(&mut overrides.endpoint, &args.endpoint);
    replace_some(&mut overrides.region, &args.region);
    replace_some(&mut overrides.account_id, &args.account_id);
    replace_some(&mut overrides.bucket, &args.bucket);
    replace_some(&mut overrides.prefix, &args.prefix);
    replace_some(&mut overrides.public_base_url, &args.public_base_url);
    if !matches!(expiry, ExpiryPolicy::Configured) {
        overrides.default_expiry = Some(match expiry {
            ExpiryPolicy::Configured => unreachable!(),
            ExpiryPolicy::Never => None,
            ExpiryPolicy::After(expiry) => Some(expiry),
        });
    }
    if credential_command.is_some() {
        overrides.credential_command = credential_command;
    }
    if branding.is_some() {
        overrides.branding.clone_from(&branding);
    }
    let client = client_with_vault(overrides, || {
        explicit_credentials_from_stdin(args.secret_key_stdin)
    })?;
    let password = if args.password_stdin {
        Some(Secret::new(read_secret_line("share password")?))
    } else {
        None
    };
    let tags = args
        .tags
        .iter()
        .map(|tag| ObjectTag::new(tag.key.clone(), tag.value.clone()).map_err(core_error))
        .collect::<CliResult<Vec<_>>>()?;
    let result = client
        .share(
            ShareInput {
                bytes: &bytes,
                content_type: format.media_type(),
                key,
            },
            ShareOptions {
                expiry,
                password,
                tags,
                branding,
            },
        )
        .map_err(core_error)?;
    Ok(Shared::from_result(result, ArtifactKind::Screenshot))
}

#[cfg(not(feature = "cloud"))]
fn share_file_impl(_args: &ShareArgs) -> CliResult<Shared> {
    Err(cloud_not_built())
}

#[cfg(feature = "cloud")]
fn share_artifact_impl(
    artifact: &FinalizedArtifact,
    card: u64,
    cancellation: &ShareCancellation,
) -> CliResult<Shared> {
    use scrozz_cloud::{ShareInput, ShareOptions, unique_object_key};

    if cancellation.is_cancelled() {
        return Err(CliError::Core(scrozz_core::Error::Cancelled));
    }
    let settings = crate::settings::SettingsStore::default_location()?.load()?;
    let name = render_name(
        settings
            .value("cloud.naming-template")
            .unwrap_or("Screenshot-{timestamp}"),
        card,
        artifact.kind().slug(),
    )?;
    let extension = std::path::Path::new(artifact.file_name())
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| CliError::usage("the finalized artifact needs a UTF-8 extension"))?;
    let key = unique_object_key(&format!("{name}.{extension}")).map_err(core_error)?;
    let overrides = stored_overrides_from(&settings)?;
    let client = client_with_vault(overrides, || Ok(None))?;
    let profile = client.config().provider.kind.slug().to_owned();
    let password = match settings.value("cloud.protection-mode") {
        Some("vault") => Some(vault_password(&profile)?.ok_or_else(|| {
            CliError::Core(scrozz_core::Error::InvalidRequest(
                "default password protection is enabled, but no password is stored in the native credential vault"
                    .to_owned(),
            ))
        })?),
        _ => None,
    };
    client
        .with_cancellation(cancellation.inner.clone())
        .share(
            ShareInput {
                bytes: artifact.bytes(),
                content_type: artifact.content_type(),
                key: &key,
            },
            ShareOptions {
                password,
                ..ShareOptions::default()
            },
        )
        .map(|result| Shared::from_result(result, artifact.kind()))
        .map_err(core_error)
}

#[cfg(feature = "cloud")]
fn upload_availability_impl() -> UploadAvailability {
    let settings =
        match crate::settings::SettingsStore::default_location().and_then(|store| store.load()) {
            Ok(settings) => settings,
            Err(error) => return UploadAvailability::unavailable(error.to_string()),
        };
    let overrides = match stored_overrides_from(&settings) {
        Ok(overrides) => overrides,
        Err(error) => return UploadAvailability::unavailable(error.to_string()),
    };
    match scrozz_cloud::ShareConfig::from_env(overrides) {
        Ok(config) => {
            let profile = config.provider.kind.slug();
            let vault_bundle = scrozz_cloud::NativeCredentialVault.load(profile);
            let has_credentials = environment_credentials_configured()
                || credential_command_configured(&config)
                || vault_bundle.as_ref().is_ok_and(Option::is_some);
            if !has_credentials {
                return UploadAvailability::unavailable(match vault_bundle {
                    Err(error) => error.to_string(),
                    Ok(_) => format!(
                        "No {profile} credentials are configured. Store them in the native vault or configure an environment/credential-command source."
                    ),
                });
            }
            if settings.value("cloud.protection-mode") == Some("vault") {
                match vault_bundle {
                    Ok(Some(bundle)) if bundle.share_password.is_some() => {}
                    Ok(_) => {
                        return UploadAvailability::unavailable(missing_vault_password_reason());
                    }
                    Err(error) => return UploadAvailability::unavailable(error.to_string()),
                }
            }

            UploadAvailability {
                enabled: true,
                reason: None,
            }
        }
        Err(error) => UploadAvailability::unavailable(error.to_string()),
    }
}

#[cfg(feature = "cloud")]
fn missing_vault_password_reason() -> &'static str {
    "Password protection is enabled, but the selected provider has no default share password in the native vault."
}

#[cfg(feature = "cloud")]
fn environment_credentials_configured() -> bool {
    [
        ("SCROZZ_S3_ACCESS_KEY_ID", "SCROZZ_S3_SECRET_ACCESS_KEY"),
        ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"),
    ]
    .into_iter()
    .any(|(access, secret)| {
        std::env::var(access).is_ok_and(|value| !value.trim().is_empty())
            && std::env::var(secret).is_ok_and(|value| !value.is_empty())
    })
}

#[cfg(feature = "cloud")]
fn credential_command_configured(config: &scrozz_cloud::ShareConfig) -> bool {
    config.credential_command.as_ref().is_some_and(|command| {
        credential_command_has_access_key(
            command,
            ["SCROZZ_S3_ACCESS_KEY_ID", "AWS_ACCESS_KEY_ID"]
                .into_iter()
                .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty())),
        )
    })
}

#[cfg(feature = "cloud")]
fn credential_command_has_access_key(
    command: &scrozz_cloud::CredentialCommand,
    environment_has_access_key: bool,
) -> bool {
    command
        .access_key_id
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty())
        || environment_has_access_key
}

#[cfg(not(feature = "cloud"))]
fn upload_availability_impl() -> UploadAvailability {
    UploadAvailability::unavailable(
        "Cloud sharing is unavailable in this build. Install a cloud-enabled Scrozz package.",
    )
}

#[cfg(feature = "cloud")]
fn credential_status_impl() -> CredentialStatus {
    use scrozz_cloud::{NativeCredentialVault, VaultStatus};

    let profile = credential_profile().unwrap_or_else(|_| "default".to_owned());
    let status = NativeCredentialVault.status(&profile);
    let backend = status.backend().label();
    match status {
        VaultStatus::Stored { .. } => CredentialStatus {
            backend,
            stored: true,
            problem: None,
        },
        VaultStatus::Missing { .. } => CredentialStatus {
            backend,
            stored: false,
            problem: None,
        },
        VaultStatus::Unavailable { reason, .. } => CredentialStatus {
            backend,
            stored: false,
            problem: Some(reason),
        },
    }
}

#[cfg(not(feature = "cloud"))]
fn credential_status_impl() -> CredentialStatus {
    CredentialStatus {
        backend: "native credential vault",
        stored: false,
        problem: Some("this build has no native credential adapter".to_owned()),
    }
}

#[cfg(feature = "cloud")]
fn store_credentials_impl(
    access_key_id: &str,
    secret_access_key: &str,
    session_token: Option<&str>,
    share_password: Option<&str>,
) -> CliResult<()> {
    use scrozz_cloud::{Credentials, NativeCredentialVault, Secret, VaultBundle};

    let credentials = Credentials::new(
        access_key_id,
        Secret::from_text(secret_access_key),
        session_token
            .filter(|value| !value.is_empty())
            .map(Secret::from_text),
    )
    .map_err(core_error)?;
    let bundle = VaultBundle {
        credentials,
        share_password: share_password
            .filter(|value| !value.is_empty())
            .map(Secret::from_text),
    };
    NativeCredentialVault
        .store(&credential_profile()?, &bundle)
        .map_err(core_error)
}

#[cfg(not(feature = "cloud"))]
fn store_credentials_impl(
    _access_key_id: &str,
    _secret_access_key: &str,
    _session_token: Option<&str>,
    _share_password: Option<&str>,
) -> CliResult<()> {
    Err(cloud_not_built())
}

#[cfg(feature = "cloud")]
fn remove_credentials_impl() -> CliResult<bool> {
    scrozz_cloud::NativeCredentialVault
        .remove(&credential_profile()?)
        .map_err(core_error)
}

#[cfg(not(feature = "cloud"))]
fn remove_credentials_impl() -> CliResult<bool> {
    Err(cloud_not_built())
}

#[cfg(feature = "cloud")]
fn test_connection_impl() -> CliResult<()> {
    let overrides = stored_overrides()?;
    let client =
        client_with_vault_timeout(overrides, || Ok(None), std::time::Duration::from_secs(5))?;
    client.test_connection().map_err(core_error)
}

#[cfg(not(feature = "cloud"))]
fn test_connection_impl() -> CliResult<()> {
    Err(cloud_not_built())
}

#[cfg(feature = "cloud")]
fn stored_overrides() -> CliResult<scrozz_cloud::ConfigOverrides> {
    let settings = crate::settings::SettingsStore::default_location()?.load()?;
    stored_overrides_from(&settings)
}

#[cfg(feature = "cloud")]
fn stored_overrides_from(
    settings: &crate::settings::StoredSettings,
) -> CliResult<scrozz_cloud::ConfigOverrides> {
    use std::{path::PathBuf, str::FromStr as _};

    use scrozz_cloud::{
        Branding, ConfigOverrides, CredentialCommand, Expiry, ObjectTag, ProviderKind,
    };

    let mut overrides = ConfigOverrides::default();
    if settings.is_user_set("cloud.provider") {
        overrides.provider = Some(
            ProviderKind::from_str(settings.value("cloud.provider").unwrap_or("aws"))
                .map_err(core_error)?,
        );
    }
    overrides.endpoint = user_nonempty(settings, "cloud.endpoint");
    overrides.region = user_nonempty(settings, "cloud.region");
    overrides.account_id = user_nonempty(settings, "cloud.account-id");
    overrides.bucket = user_nonempty(settings, "cloud.bucket");
    overrides.prefix = user_value(settings, "cloud.prefix");
    overrides.public_base_url = user_nonempty(settings, "cloud.public-base-url");
    if settings.is_user_set("cloud.url-policy") || settings.is_user_set("cloud.expiry-seconds") {
        overrides.default_expiry = Some(
            if settings.value("cloud.url-policy") == Some("public-base") {
                None
            } else {
                let seconds = settings
                    .value("cloud.expiry-seconds")
                    .unwrap_or("86400")
                    .parse::<u32>()
                    .map_err(|_| CliError::usage("cloud.expiry-seconds is not a whole number"))?;
                if seconds == 0 {
                    None
                } else {
                    Some(Expiry::from_seconds(seconds).map_err(core_error)?)
                }
            },
        );
    }
    if let Some(program) = user_nonempty(settings, "cloud.credential-command") {
        overrides.credential_command = Some(CredentialCommand {
            program: PathBuf::from(program),
            args: Vec::new(),
            access_key_id: None,
        });
    }
    if settings.is_user_set("cloud.viewer-title") || settings.is_user_set("cloud.viewer-accent") {
        overrides.branding = Some(Branding {
            title: settings
                .value("cloud.viewer-title")
                .unwrap_or("Scrozz share")
                .to_owned(),
            accent: settings
                .value("cloud.viewer-accent")
                .unwrap_or("#f05a28")
                .to_owned(),
        });
    }
    if let Some(tags) = user_nonempty(settings, "cloud.tags") {
        overrides.tags = tags
            .split(',')
            .filter(|item| !item.trim().is_empty())
            .map(|item| ObjectTag::parse(item.trim()).map_err(core_error))
            .collect::<CliResult<Vec<_>>>()?;
    }
    Ok(overrides)
}

#[cfg(feature = "cloud")]
fn user_value(settings: &crate::settings::StoredSettings, key: &str) -> Option<String> {
    settings
        .is_user_set(key)
        .then(|| settings.value(key).map(str::to_owned))
        .flatten()
}

#[cfg(feature = "cloud")]
fn user_nonempty(settings: &crate::settings::StoredSettings, key: &str) -> Option<String> {
    user_value(settings, key).filter(|value| !value.trim().is_empty())
}

#[cfg(feature = "cloud")]
fn replace_some(target: &mut Option<String>, value: &Option<String>) {
    if let Some(value) = value {
        *target = Some(value.clone());
    }
}

#[cfg(feature = "cloud")]
fn client_with_vault<F>(
    overrides: scrozz_cloud::ConfigOverrides,
    explicit: F,
) -> CliResult<scrozz_cloud::ShareClient<scrozz_cloud::UreqTransport>>
where
    F: FnOnce() -> scrozz_cloud::Result<Option<scrozz_cloud::Credentials>>,
{
    client_with_vault_timeout(overrides, explicit, std::time::Duration::from_secs(30))
}

#[cfg(feature = "cloud")]
fn client_with_vault_timeout<F>(
    overrides: scrozz_cloud::ConfigOverrides,
    explicit: F,
    timeout: std::time::Duration,
) -> CliResult<scrozz_cloud::ShareClient<scrozz_cloud::UreqTransport>>
where
    F: FnOnce() -> scrozz_cloud::Result<Option<scrozz_cloud::Credentials>>,
{
    use scrozz_cloud::{
        CredentialResolver, NativeCredentialVault, ProcessEnvironment, ShareClient, ShareConfig,
        UreqTransport,
    };

    let environment = ProcessEnvironment;
    let config = ShareConfig::from_environment(&environment, overrides).map_err(core_error)?;
    let profile = config.provider.kind.slug().to_owned();
    let command = config.credential_command.as_ref();
    let resolved = CredentialResolver::new(&environment)
        .resolve_lazy(command, || {
            if let Some(credentials) = explicit()? {
                return Ok(Some(credentials));
            }
            NativeCredentialVault
                .load(&profile)
                .map(|bundle| bundle.map(|bundle| bundle.credentials))
        })
        .map_err(core_error)?;
    Ok(ShareClient::new(
        config,
        resolved.credentials,
        UreqTransport::new(timeout),
    ))
}

#[cfg(feature = "cloud")]
fn vault_password(profile: &str) -> CliResult<Option<scrozz_cloud::Secret>> {
    scrozz_cloud::NativeCredentialVault
        .load(profile)
        .map(|bundle| bundle.and_then(|bundle| bundle.share_password))
        .map_err(core_error)
}

#[cfg(feature = "cloud")]
fn credential_profile() -> CliResult<String> {
    let settings = crate::settings::SettingsStore::default_location()?.load()?;
    credential_profile_from(
        &settings,
        std::env::var("SCROZZ_S3_PROVIDER").ok().as_deref(),
    )
}

#[cfg(feature = "cloud")]
fn credential_profile_from(
    settings: &crate::settings::StoredSettings,
    environment_provider: Option<&str>,
) -> CliResult<String> {
    use std::str::FromStr as _;

    let provider = if settings.is_user_set("cloud.provider") {
        settings.value("cloud.provider").unwrap_or("aws")
    } else {
        environment_provider.unwrap_or("aws")
    };
    scrozz_cloud::ProviderKind::from_str(provider)
        .map(|provider| provider.slug().to_owned())
        .map_err(core_error)
}

#[cfg(feature = "cloud")]
fn render_name(template: &str, card: u64, kind: &str) -> CliResult<String> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| CliError::usage("the system clock is before 1970"))?
        .as_secs();
    let rendered = template
        .replace("{timestamp}", &timestamp.to_string())
        .replace("{card}", &card.to_string())
        .replace("{kind}", kind);
    if rendered.trim().is_empty()
        || rendered.contains(['/', '\\', '{', '}'])
        || rendered.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(CliError::usage(
            "cloud.naming-template must produce a nonempty file name without path separators or unknown placeholders",
        ));
    }
    Ok(rendered)
}

#[cfg(not(feature = "cloud"))]
fn share_artifact_impl(
    _artifact: &FinalizedArtifact,
    _card: u64,
    _cancellation: &ShareCancellation,
) -> CliResult<Shared> {
    Err(cloud_not_built())
}

#[cfg(feature = "cloud")]
fn provider(provider: CloudProvider) -> scrozz_cloud::ProviderKind {
    match provider {
        CloudProvider::Aws => scrozz_cloud::ProviderKind::Aws,
        CloudProvider::R2 => scrozz_cloud::ProviderKind::R2,
        CloudProvider::B2 => scrozz_cloud::ProviderKind::B2,
        CloudProvider::Minio => scrozz_cloud::ProviderKind::Minio,
    }
}

#[cfg(feature = "cloud")]
fn core_error(error: scrozz_cloud::Error) -> CliError {
    CliError::Core(error.into())
}

#[cfg(feature = "cloud")]
impl Shared {
    fn from_result(value: scrozz_cloud::ShareResult, media_kind: ArtifactKind) -> Self {
        Self {
            url: value.url,
            key: value.key,
            provider: value.provider,
            expires_seconds: value.expires_seconds,
            expires_at: value.expires_at,
            lifecycle_rule: value.lifecycle_rule,
            encrypted: value.encrypted,
            tags: value
                .tags
                .iter()
                .map(|tag| (tag.key().to_owned(), tag.value().to_owned()))
                .collect(),
            media_kind,
        }
    }
}

fn cloud_not_built() -> CliError {
    CliError::Core(scrozz_core::Error::Unsupported {
        what: "sharing captures to object storage".to_owned(),
        why: "this binary was built without optional cloud networking. Rebuild with \
              `--features cloud`; the default build deliberately contains no HTTP client."
            .to_owned(),
    })
}

#[cfg(feature = "cloud")]
fn read_secret_line(label: &str) -> CliResult<Vec<u8>> {
    use std::io::{BufRead, Read};

    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_STDIN_SECRET_BYTES + 2)
        .read_until(b'\n', &mut bytes)
        .map_err(CliError::from)?;
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    if bytes.len() as u64 > MAX_STDIN_SECRET_BYTES {
        bytes.fill(0);
        return Err(CliError::usage(format!(
            "{label} read from stdin exceeds 64 KiB"
        )));
    }
    if bytes.is_empty() {
        return Err(CliError::usage(format!(
            "{label} read from stdin was empty"
        )));
    }
    Ok(bytes)
}

#[cfg(feature = "cloud")]
fn explicit_credentials_from_stdin(
    enabled: bool,
) -> scrozz_cloud::Result<Option<scrozz_cloud::Credentials>> {
    use std::io::{BufRead, Read};

    if !enabled {
        return Ok(None);
    }
    let (access_key_id, session_token) = if let Some(access) =
        environment_value("SCROZZ_S3_ACCESS_KEY_ID")
    {
        (
            access,
            environment_value("SCROZZ_S3_SESSION_TOKEN").map(scrozz_cloud::Secret::from_text),
        )
    } else if let Some(access) = environment_value("AWS_ACCESS_KEY_ID") {
        (
            access,
            environment_value("AWS_SESSION_TOKEN").map(scrozz_cloud::Secret::from_text),
        )
    } else {
        return Err(scrozz_cloud::Error::Credentials(
            "--secret-key-stdin also needs SCROZZ_S3_ACCESS_KEY_ID or AWS_ACCESS_KEY_ID".to_owned(),
        ));
    };
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_STDIN_SECRET_BYTES + 2)
        .read_until(b'\n', &mut bytes)
        .map_err(scrozz_cloud::Error::Io)?;
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    if bytes.len() as u64 > MAX_STDIN_SECRET_BYTES {
        bytes.fill(0);
        return Err(scrozz_cloud::Error::Credentials(
            "the secret access key read from stdin exceeds 64 KiB".to_owned(),
        ));
    }
    if bytes.is_empty() {
        return Err(scrozz_cloud::Error::Credentials(
            "the secret access key read from stdin was empty".to_owned(),
        ));
    }
    scrozz_cloud::Credentials::new(
        access_key_id,
        scrozz_cloud::Secret::new(bytes),
        session_token,
    )
    .map(Some)
}

#[cfg(feature = "cloud")]
fn environment_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[cfg(feature = "cloud")]
const MAX_STDIN_SECRET_BYTES: u64 = 64 * 1024;

#[cfg(all(test, feature = "cloud"))]
mod tests {
    use super::*;

    #[test]
    fn vault_profile_uses_the_same_provider_precedence_as_uploads() {
        let mut settings = crate::settings::StoredSettings::default();
        assert_eq!(
            credential_profile_from(&settings, Some("r2")).unwrap(),
            "r2"
        );
        settings.set("cloud.provider", "b2").unwrap();
        assert_eq!(
            credential_profile_from(&settings, Some("r2")).unwrap(),
            "b2"
        );
    }

    #[test]
    fn finalized_artifact_debug_never_prints_media_bytes() {
        let artifact =
            FinalizedArtifact::screenshot_png(b"private-pixel-marker".to_vec(), "capture.png")
                .unwrap();
        let rendered = format!("{artifact:?}");
        assert!(!rendered.contains("private-pixel-marker"), "{rendered}");
        assert!(rendered.contains("20 BYTES"), "{rendered}");
    }

    #[test]
    fn finalized_gif_export_is_a_supported_recording_artifact() {
        let artifact =
            FinalizedArtifact::recording(b"GIF89a".to_vec(), "image/gif", "capture.gif").unwrap();
        assert_eq!(artifact.content_type(), "image/gif");
    }

    #[test]
    fn vault_password_requirement_has_actionable_copy() {
        let reason = missing_vault_password_reason();
        assert!(reason.contains("Password protection"), "{reason}");
        assert!(reason.contains("native vault"), "{reason}");
    }

    #[test]
    fn credential_command_needs_a_separate_access_key_id() {
        let without_access = scrozz_cloud::CredentialCommand {
            program: "vault".into(),
            args: Vec::new(),
            access_key_id: None,
        };
        assert!(!credential_command_has_access_key(&without_access, false));
        assert!(credential_command_has_access_key(&without_access, true));

        let with_access = scrozz_cloud::CredentialCommand {
            access_key_id: Some("access-id".to_owned()),
            ..without_access
        };
        assert!(credential_command_has_access_key(&with_access, false));
    }
}
