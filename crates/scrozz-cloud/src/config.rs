//! Public cloud configuration. Credentials deliberately do not fit in this type.

use std::{ffi::OsString, fmt, path::PathBuf, str::FromStr};

use crate::{
    credentials::{CredentialCommand, Environment, ProcessEnvironment},
    error::{Error, Result},
    lifecycle::{Expiry, ObjectTag},
    provider::{ProviderConfig, ProviderKind, normalise_http_base},
};

/// Viewer title and accent. Both are non-secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branding {
    /// Browser title and viewer heading.
    pub title: String,
    /// Six-digit CSS hex color.
    pub accent: String,
}

impl Default for Branding {
    fn default() -> Self {
        Self {
            title: "Scrozz share".to_owned(),
            accent: "#7c3aed".to_owned(),
        }
    }
}

impl Branding {
    /// Validates values before they are inserted into the viewer.
    pub fn validate(&self) -> Result<()> {
        if self.title.trim().is_empty() || self.title.chars().count() > 120 {
            return Err(Error::Config(
                "viewer title must contain 1 to 120 characters".to_owned(),
            ));
        }
        if self.accent.len() != 7
            || !self.accent.starts_with('#')
            || !self.accent[1..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(Error::Config(
                "viewer accent must be a six-digit CSS color such as #7c3aed".to_owned(),
            ));
        }
        Ok(())
    }
}

/// CLI/API values that override non-secret environment configuration.
#[derive(Debug, Clone, Default)]
pub struct ConfigOverrides {
    /// Provider preset.
    pub provider: Option<ProviderKind>,
    /// Provider-specific endpoint.
    pub endpoint: Option<String>,
    /// SigV4 region.
    pub region: Option<String>,
    /// R2 account id.
    pub account_id: Option<String>,
    /// Bucket name.
    pub bucket: Option<String>,
    /// Object prefix.
    pub prefix: Option<String>,
    /// Custom public origin/path.
    pub public_base_url: Option<String>,
    /// `Some(None)` explicitly disables presigning; `None` keeps environment or
    /// default behavior.
    pub default_expiry: Option<Option<Expiry>>,
    /// Credential command. Its stdout is the secret.
    pub credential_command: Option<CredentialCommand>,
    /// Viewer branding.
    pub branding: Option<Branding>,
    /// Tags added to every object.
    pub tags: Vec<ObjectTag>,
}

/// Resolved public configuration.
#[derive(Clone)]
pub struct ShareConfig {
    /// Provider endpoint and SigV4 behavior.
    pub provider: ProviderConfig,
    /// Bucket name.
    pub bucket: String,
    /// Key prefix.
    pub prefix: String,
    /// Public custom origin/path for non-expiring public objects.
    pub public_base_url: Option<String>,
    /// Default private-object presigned lifetime.
    pub default_expiry: Option<Expiry>,
    /// Optional command that prints a secret access key.
    pub credential_command: Option<CredentialCommand>,
    /// Default viewer branding.
    pub branding: Branding,
    /// Tags applied to every object.
    pub tags: Vec<ObjectTag>,
}

impl fmt::Debug for ShareConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShareConfig")
            .field("provider", &self.provider)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("public_base_url", &self.public_base_url)
            .field("default_expiry", &self.default_expiry)
            .field("credential_command", &self.credential_command)
            .field("branding", &self.branding)
            .field("tags", &self.tags)
            .finish()
    }
}

impl ShareConfig {
    /// Resolves from the process environment.
    pub fn from_env(overrides: ConfigOverrides) -> Result<Self> {
        Self::from_environment(&ProcessEnvironment, overrides)
    }

    /// Resolves against an injected environment.
    pub fn from_environment(
        environment: &dyn Environment,
        overrides: ConfigOverrides,
    ) -> Result<Self> {
        let provider = match overrides.provider {
            Some(provider) => provider,
            None => environment
                .get("SCROZZ_S3_PROVIDER")
                .as_deref()
                .map(ProviderKind::from_str)
                .transpose()?
                .unwrap_or(ProviderKind::Aws),
        };
        let endpoint = choose(overrides.endpoint, environment, "SCROZZ_S3_ENDPOINT");
        let region = choose(overrides.region, environment, "SCROZZ_S3_REGION")
            .or_else(|| environment.get("AWS_REGION"))
            .or_else(|| environment.get("AWS_DEFAULT_REGION"));
        let account_id = choose(overrides.account_id, environment, "SCROZZ_S3_ACCOUNT_ID");
        let provider = ProviderConfig::from_options(
            provider,
            region.as_deref(),
            endpoint.as_deref(),
            account_id.as_deref(),
        )?;

        let bucket = choose(overrides.bucket, environment, "SCROZZ_S3_BUCKET")
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                Error::Config(
                    "no bucket is configured; set SCROZZ_S3_BUCKET or pass --bucket".to_owned(),
                )
            })?;
        if bucket.contains('/') {
            return Err(Error::Config(
                "the bucket name must not contain `/`; put folders in the prefix".to_owned(),
            ));
        }
        let prefix = choose(overrides.prefix, environment, "SCROZZ_S3_PREFIX")
            .unwrap_or_else(|| "captures".to_owned())
            .trim_matches('/')
            .to_owned();
        let public_base_url = choose(
            overrides.public_base_url,
            environment,
            "SCROZZ_S3_PUBLIC_BASE_URL",
        )
        .filter(|value| !value.trim().is_empty());
        let public_base_url = public_base_url
            .map(|base| normalise_http_base(&base, "public base URL"))
            .transpose()?;

        let default_expiry = match overrides.default_expiry {
            Some(expiry) => expiry,
            None => match environment.get("SCROZZ_S3_EXPIRES") {
                Some(value) if matches!(value.trim(), "" | "0" | "never" | "none") => None,
                Some(value) => Some(value.parse()?),
                None => Some(Expiry::one_day()),
            },
        };

        let credential_command = overrides.credential_command.or_else(|| {
            environment
                .get("SCROZZ_S3_CREDENTIAL_COMMAND")
                .filter(|value| !value.trim().is_empty())
                .map(|program| CredentialCommand {
                    program: PathBuf::from(program),
                    args: environment
                        .get("SCROZZ_S3_CREDENTIAL_ARGS")
                        .map(|args| args.lines().map(OsString::from).collect())
                        .unwrap_or_default(),
                    access_key_id: None,
                })
        });

        let branding = match overrides.branding {
            Some(branding) => branding,
            None => Branding {
                title: environment
                    .get("SCROZZ_SHARE_TITLE")
                    .unwrap_or_else(|| Branding::default().title),
                accent: environment
                    .get("SCROZZ_SHARE_ACCENT")
                    .unwrap_or_else(|| Branding::default().accent),
            },
        };
        branding.validate()?;

        let mut tags = environment
            .get("SCROZZ_S3_TAGS")
            .map(|raw| {
                raw.split(',')
                    .filter(|item| !item.trim().is_empty())
                    .map(|item| ObjectTag::parse(item.trim()))
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        tags.extend(overrides.tags);

        Ok(Self {
            provider,
            bucket,
            prefix,
            public_base_url,
            default_expiry,
            credential_command,
            branding,
            tags,
        })
    }

    /// A secret-free text representation suitable for a settings file.
    #[must_use]
    pub fn public_config_text(&self) -> String {
        format!(
            "provider = {:?}\nendpoint = {:?}\nregion = {:?}\nbucket = {:?}\n\
             prefix = {:?}\npublic-base-url = {:?}\nexpires = {:?}\n\
             viewer-title = {:?}\nviewer-accent = {:?}\n",
            self.provider.kind.slug(),
            self.provider.endpoint(),
            self.provider.region,
            self.bucket,
            self.prefix,
            self.public_base_url.as_deref().unwrap_or(""),
            self.default_expiry.map(Expiry::seconds),
            self.branding.title,
            self.branding.accent,
        )
    }
}

fn choose(
    explicit: Option<String>,
    environment: &dyn Environment,
    variable: &str,
) -> Option<String> {
    explicit.or_else(|| environment.get(variable))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::{Credentials, Secret};

    use super::*;

    #[derive(Default)]
    struct MapEnvironment(BTreeMap<String, String>);

    impl Environment for MapEnvironment {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    #[test]
    fn config_files_have_no_place_for_credentials() {
        let config = ShareConfig::from_environment(
            &MapEnvironment::default(),
            ConfigOverrides {
                bucket: Some("shots".into()),
                ..ConfigOverrides::default()
            },
        )
        .unwrap();
        let credentials = Credentials::new(
            "ACCESS_KEY_ID_THAT_MUST_NOT_BE_WRITTEN",
            Secret::from_text("never-write-this"),
            None,
        )
        .unwrap();
        let text = config.public_config_text();
        assert!(!text.contains("never-write-this"));
        assert!(!text.contains(credentials.access_key_id()));
        assert!(!text.to_ascii_lowercase().contains("secret"));
        assert!(!text.to_ascii_lowercase().contains("session-token"));
    }

    #[test]
    fn environment_resolves_all_provider_presets_without_credentials() {
        let cases = [
            (ProviderKind::Aws, Some("us-east-2"), None, None),
            (ProviderKind::R2, None, None, Some("account")),
            (ProviderKind::B2, Some("us-west-004"), None, None),
            (
                ProviderKind::Minio,
                None,
                Some("http://localhost:9000"),
                None,
            ),
        ];
        for (provider, region, endpoint, account_id) in cases {
            let config = ShareConfig::from_environment(
                &MapEnvironment::default(),
                ConfigOverrides {
                    provider: Some(provider),
                    region: region.map(str::to_owned),
                    endpoint: endpoint.map(str::to_owned),
                    account_id: account_id.map(str::to_owned),
                    bucket: Some("shots".into()),
                    ..ConfigOverrides::default()
                },
            )
            .unwrap();
            assert_eq!(config.provider.kind, provider);
        }
    }
}
