//! Native credential-vault storage.
//!
//! Only opaque binary secrets enter the platform store. Provider configuration,
//! profile names, and status are intentionally separate and non-secret.

use std::fmt;

use crate::{Credentials, Error, Result, Secret};

const SERVICE: &str = "com.thatcube.Scrozz.cloud-credentials";
const MAGIC: &[u8] = b"SCROZZ-CREDENTIALS\0";
const FORMAT: u8 = 1;
const NONE: u32 = u32::MAX;

/// Native store selected for this target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultBackend {
    /// macOS Keychain.
    MacOsKeychain,
    /// Windows Credential Manager.
    WindowsCredentialManager,
    /// Freedesktop Secret Service.
    LinuxSecretService,
    /// This target or build has no native adapter.
    Unavailable,
}

impl VaultBackend {
    /// Human-readable backend name for Settings and diagnostics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::MacOsKeychain => "macOS Keychain",
            Self::WindowsCredentialManager => "Windows Credential Manager",
            Self::LinuxSecretService => "Linux Secret Service",
            Self::Unavailable => "native credential vault",
        }
    }

    /// Adapter compiled for this target.
    #[must_use]
    pub const fn current() -> Self {
        if !cfg!(feature = "native-vault") {
            Self::Unavailable
        } else if cfg!(target_os = "macos") {
            Self::MacOsKeychain
        } else if cfg!(target_os = "windows") {
            Self::WindowsCredentialManager
        } else if cfg!(target_os = "linux") {
            Self::LinuxSecretService
        } else {
            Self::Unavailable
        }
    }
}

/// Runtime status of one provider profile's native entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultStatus {
    /// A complete credential bundle is stored.
    Stored {
        /// Native store in use.
        backend: VaultBackend,
    },
    /// The native store answered, but this profile has no entry.
    Missing {
        /// Native store in use.
        backend: VaultBackend,
    },
    /// The adapter is absent or the native store cannot be reached.
    Unavailable {
        /// Adapter selected for this target.
        backend: VaultBackend,
        /// Safe user-facing explanation.
        reason: String,
    },
}

impl VaultStatus {
    /// Whether a complete entry exists.
    #[must_use]
    pub const fn is_stored(&self) -> bool {
        matches!(self, Self::Stored { .. })
    }

    /// Backend named by this status.
    #[must_use]
    pub const fn backend(&self) -> VaultBackend {
        match self {
            Self::Stored { backend }
            | Self::Missing { backend }
            | Self::Unavailable { backend, .. } => *backend,
        }
    }
}

/// Provider credentials plus an optional default share password.
///
/// There is deliberately no derived `Debug`; neither component may enter logs,
/// crash reports, settings, or history.
#[derive(Clone, PartialEq, Eq)]
pub struct VaultBundle {
    /// S3-compatible provider credentials.
    pub credentials: Credentials,
    /// Password used by GUI default protection, when configured.
    pub share_password: Option<Secret>,
}

impl fmt::Debug for VaultBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VaultBundle")
            .field("credentials", &"[REDACTED]")
            .field(
                "share_password",
                &self.share_password.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Cross-platform native credential adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeCredentialVault;

impl NativeCredentialVault {
    /// Adapter compiled for this target.
    #[must_use]
    pub const fn backend(&self) -> VaultBackend {
        VaultBackend::current()
    }

    /// Checks the real native store without writing anything.
    #[must_use]
    pub fn status(&self, profile: &str) -> VaultStatus {
        let backend = self.backend();
        if backend == VaultBackend::Unavailable {
            return VaultStatus::Unavailable {
                backend,
                reason: "this binary was built without a native credential-vault adapter"
                    .to_owned(),
            };
        }
        match self.read_secret(profile) {
            Ok(Some(mut bytes)) => {
                let parsed = decode(&bytes);
                bytes.fill(0);
                match parsed {
                    Ok(_) => VaultStatus::Stored { backend },
                    Err(_) => VaultStatus::Unavailable {
                        backend,
                        reason: "the stored Scrozz credential entry is unreadable; replace it"
                            .to_owned(),
                    },
                }
            }
            Ok(None) => VaultStatus::Missing { backend },
            Err(error) => VaultStatus::Unavailable {
                backend,
                reason: error.to_string(),
            },
        }
    }

    /// Loads a complete provider entry.
    pub fn load(&self, profile: &str) -> Result<Option<VaultBundle>> {
        let Some(mut bytes) = self.read_secret(profile)? else {
            return Ok(None);
        };
        let result = decode(&bytes);
        bytes.fill(0);
        result.map(Some)
    }

    /// Adds or atomically replaces a provider entry.
    pub fn store(&self, profile: &str, bundle: &VaultBundle) -> Result<()> {
        validate_profile(profile)?;
        let mut bytes = encode(bundle)?;
        let result = self.write_secret(profile, &bytes);
        bytes.fill(0);
        result
    }

    /// Removes a provider entry. Missing entries are already removed.
    pub fn remove(&self, profile: &str) -> Result<bool> {
        validate_profile(profile)?;
        self.delete_secret(profile)
    }

    #[cfg(feature = "native-vault")]
    fn entry(profile: &str) -> Result<keyring::Entry> {
        validate_profile(profile)?;
        keyring::Entry::new(SERVICE, profile)
            .map_err(|error| vault_error("could not open the native credential entry", &error))
    }

    #[cfg(feature = "native-vault")]
    fn read_secret(&self, profile: &str) -> Result<Option<Vec<u8>>> {
        let entry = Self::entry(profile)?;
        match entry.get_secret() {
            Ok(bytes) => Ok(Some(bytes)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(vault_error(
                "could not read the native credential entry",
                &error,
            )),
        }
    }

    #[cfg(not(feature = "native-vault"))]
    fn read_secret(&self, profile: &str) -> Result<Option<Vec<u8>>> {
        validate_profile(profile)?;
        Err(Error::Credentials(
            "this build has no native credential-vault adapter".to_owned(),
        ))
    }

    #[cfg(feature = "native-vault")]
    fn write_secret(&self, profile: &str, secret: &[u8]) -> Result<()> {
        Self::entry(profile)?
            .set_secret(secret)
            .map_err(|error| vault_error("could not store the native credential entry", &error))
    }

    #[cfg(not(feature = "native-vault"))]
    fn write_secret(&self, profile: &str, _secret: &[u8]) -> Result<()> {
        validate_profile(profile)?;
        Err(Error::Credentials(
            "this build has no native credential-vault adapter".to_owned(),
        ))
    }

    #[cfg(feature = "native-vault")]
    fn delete_secret(&self, profile: &str) -> Result<bool> {
        let entry = Self::entry(profile)?;
        match entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(error) => Err(vault_error(
                "could not remove the native credential entry",
                &error,
            )),
        }
    }

    #[cfg(not(feature = "native-vault"))]
    fn delete_secret(&self, profile: &str) -> Result<bool> {
        validate_profile(profile)?;
        Err(Error::Credentials(
            "this build has no native credential-vault adapter".to_owned(),
        ))
    }
}

fn validate_profile(profile: &str) -> Result<()> {
    if profile.is_empty()
        || profile.len() > 120
        || profile
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(Error::Credentials(
            "a credential profile must contain 1 to 120 non-whitespace characters".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(feature = "native-vault")]
fn vault_error(context: &str, error: &keyring::Error) -> Error {
    Error::Credentials(format!(
        "{context} in {}: {error}",
        VaultBackend::current().label()
    ))
}

fn encode(bundle: &VaultBundle) -> Result<Vec<u8>> {
    let token = bundle.credentials.session_token().map(str::as_bytes);
    let password = bundle
        .share_password
        .as_ref()
        .map(Secret::expose)
        .filter(|bytes| !bytes.is_empty());
    let fields = [
        Some(bundle.credentials.access_key_id().as_bytes()),
        Some(bundle.credentials.secret_access_key()),
        token,
        password,
    ];
    let capacity = MAGIC.len()
        + 1
        + fields
            .iter()
            .map(|field| 4 + field.map_or(0, <[u8]>::len))
            .sum::<usize>();
    let mut output = Vec::with_capacity(capacity);
    output.extend_from_slice(MAGIC);
    output.push(FORMAT);
    for field in fields {
        match field {
            Some(bytes) => {
                let len = u32::try_from(bytes.len()).map_err(|_| {
                    Error::Credentials("a credential field is too large for the vault".to_owned())
                })?;
                output.extend_from_slice(&len.to_be_bytes());
                output.extend_from_slice(bytes);
            }
            None => output.extend_from_slice(&NONE.to_be_bytes()),
        }
    }
    Ok(output)
}

fn decode(bytes: &[u8]) -> Result<VaultBundle> {
    let Some(rest) = bytes.strip_prefix(MAGIC) else {
        return Err(Error::Credentials(
            "the native credential entry has an unknown format".to_owned(),
        ));
    };
    let Some((&format, mut rest)) = rest.split_first() else {
        return Err(Error::Credentials(
            "the native credential entry is truncated".to_owned(),
        ));
    };
    if format != FORMAT {
        return Err(Error::Credentials(format!(
            "the native credential entry uses unsupported format {format}"
        )));
    }
    let access = take_field(&mut rest)?.ok_or_else(|| {
        Error::Credentials("the native credential entry has no access-key id".to_owned())
    })?;
    let secret = take_field(&mut rest)?.ok_or_else(|| {
        Error::Credentials("the native credential entry has no secret access key".to_owned())
    })?;
    let token = take_field(&mut rest)?;
    let password = take_field(&mut rest)?;
    if !rest.is_empty() {
        return Err(Error::Credentials(
            "the native credential entry has trailing data".to_owned(),
        ));
    }
    let access = std::str::from_utf8(access)
        .map_err(|_| Error::Credentials("the stored access-key id is not UTF-8".to_owned()))?;
    let token = token
        .map(|token| {
            std::str::from_utf8(token)
                .map(str::to_owned)
                .map_err(|_| Error::Credentials("the stored session token is not UTF-8".to_owned()))
        })
        .transpose()?;
    let credentials = Credentials::new(
        access,
        Secret::new(secret.to_vec()),
        token.map(Secret::from_text),
    )?;
    Ok(VaultBundle {
        credentials,
        share_password: password.map(|password| Secret::new(password.to_vec())),
    })
}

fn take_field<'a>(rest: &mut &'a [u8]) -> Result<Option<&'a [u8]>> {
    let length = rest
        .get(..4)
        .ok_or_else(|| Error::Credentials("the native credential entry is truncated".to_owned()))?;
    *rest = &rest[4..];
    let length = u32::from_be_bytes(length.try_into().expect("four bytes"));
    if length == NONE {
        return Ok(None);
    }
    let length = usize::try_from(length)
        .map_err(|_| Error::Credentials("the native credential length is invalid".to_owned()))?;
    let field = rest
        .get(..length)
        .ok_or_else(|| Error::Credentials("the native credential entry is truncated".to_owned()))?;
    *rest = &rest[length..];
    Ok(Some(field))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle() -> VaultBundle {
        VaultBundle {
            credentials: Credentials::new(
                "vault-access",
                Secret::from_text("vault-secret"),
                Some(Secret::from_text("vault-token")),
            )
            .unwrap(),
            share_password: Some(Secret::from_text("viewer-password")),
        }
    }

    #[test]
    fn binary_format_round_trips_without_a_plaintext_settings_shape() {
        let expected = bundle();
        let mut encoded = encode(&expected).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded, expected);
        encoded.fill(0);
    }

    #[test]
    fn debug_output_is_fully_redacted() {
        let rendered = format!("{:?}", bundle());
        for forbidden in [
            "vault-access",
            "vault-secret",
            "vault-token",
            "viewer-password",
        ] {
            assert!(!rendered.contains(forbidden), "{rendered}");
        }
        assert!(rendered.contains("[REDACTED]"), "{rendered}");
    }

    #[test]
    fn malformed_entries_are_rejected_without_echoing_bytes() {
        let error = decode(b"credential-secret-that-must-not-be-echoed").unwrap_err();
        assert!(!error.to_string().contains("credential-secret"));
    }

    #[cfg(feature = "native-vault")]
    #[test]
    fn native_vault_smoke_when_explicitly_enabled() {
        if std::env::var("SCROZZ_TEST_NATIVE_VAULT").as_deref() != Ok("1") {
            return;
        }
        let profile = format!(
            "native-smoke-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let vault = NativeCredentialVault;
        vault.store(&profile, &bundle()).unwrap();
        assert!(vault.status(&profile).is_stored());
        assert_eq!(vault.load(&profile).unwrap(), Some(bundle()));
        assert!(vault.remove(&profile).unwrap());
        assert!(matches!(
            vault.status(&profile),
            VaultStatus::Missing { .. }
        ));
    }
}
