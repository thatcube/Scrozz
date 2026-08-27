//! Application-facing private sharing.

#[cfg(feature = "cloud")]
use crate::cli::CloudProvider;
use crate::{
    cli::ShareArgs,
    fault::{CliError, CliResult},
};

/// A completed share. It intentionally has no `Debug`: a presigned URL is a
/// bearer value and belongs in explicit command output or the clipboard, not a
/// log line.
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
    share_file_impl(args)
}

/// Shares one card's cached PNG.
pub fn share_card(bytes: &[u8], card: u64, cancellation: &ShareCancellation) -> CliResult<Shared> {
    share_card_impl(bytes, card, cancellation)
}

#[cfg(feature = "cloud")]
fn share_file_impl(args: &ShareArgs) -> CliResult<Shared> {
    use scrozz_cloud::{
        Branding, ConfigOverrides, CredentialCommand, Expiry, ExpiryPolicy, ObjectTag, Secret,
        ShareInput, ShareOptions, client_from_environment_lazy, unique_object_key,
    };

    if !args.file.is_file() {
        return Err(CliError::usage(format!(
            "{} is not an image file",
            args.file.display()
        )));
    }
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
    let default_expiry = match expiry {
        ExpiryPolicy::Configured => None,
        ExpiryPolicy::Never => Some(None),
        ExpiryPolicy::After(expiry) => Some(Some(expiry)),
    };
    let overrides = ConfigOverrides {
        provider: args.provider.map(provider),
        endpoint: args.endpoint.clone(),
        region: args.region.clone(),
        account_id: args.account_id.clone(),
        bucket: args.bucket.clone(),
        prefix: args.prefix.clone(),
        public_base_url: args.public_base_url.clone(),
        default_expiry,
        credential_command,
        branding: branding.clone(),
        tags: Vec::new(),
    };
    let client = client_from_environment_lazy(overrides, || {
        explicit_credentials_from_stdin(args.secret_key_stdin)
    })
    .map_err(core_error)?;
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
    Ok(result.into())
}

#[cfg(not(feature = "cloud"))]
fn share_file_impl(_args: &ShareArgs) -> CliResult<Shared> {
    Err(cloud_not_built())
}

#[cfg(feature = "cloud")]
fn share_card_impl(bytes: &[u8], card: u64, cancellation: &ShareCancellation) -> CliResult<Shared> {
    use scrozz_cloud::{
        ConfigOverrides, ShareInput, ShareOptions, client_from_environment, unique_object_key,
    };

    if cancellation.is_cancelled() {
        return Err(CliError::Core(scrozz_core::Error::Cancelled));
    }
    let key = unique_object_key(&format!("Screenshot-{card}.png")).map_err(core_error)?;
    let client = client_from_environment(ConfigOverrides::default(), None).map_err(core_error)?;
    client
        .with_cancellation(cancellation.inner.clone())
        .share(
            ShareInput {
                bytes,
                content_type: "image/png",
                key: &key,
            },
            ShareOptions::default(),
        )
        .map(Shared::from)
        .map_err(core_error)
}

#[cfg(not(feature = "cloud"))]
fn share_card_impl(
    _bytes: &[u8],
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
impl From<scrozz_cloud::ShareResult> for Shared {
    fn from(value: scrozz_cloud::ShareResult) -> Self {
        Self {
            url: value.url,
            key: value.key,
            provider: value.provider,
            expires_seconds: value.expires_seconds,
            expires_at: value.expires_at,
            lifecycle_rule: value.lifecycle_rule,
            encrypted: value.encrypted,
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
