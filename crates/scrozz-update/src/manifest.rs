use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    fmt,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{Error as _, MapAccess, Visitor},
};

use crate::{Error, Result};

const MANIFEST_SCHEMA: u32 = 1;
const SIGNATURE_SCHEMA: u32 = 1;
const MAX_URL_LEN: usize = 2_048;
const MAX_PLATFORM_LEN: usize = 64;
const MAX_ARTIFACT_ID_LEN: usize = 96;
const MAX_KEY_ID_LEN: usize = 64;

/// The installation semantics attached to one signed artifact.
///
/// This is part of the exact-byte signed manifest. A download cannot be
/// reinterpreted as another package kind after verification.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    /// Legacy schema-1 payload containing one replacement executable.
    #[default]
    RawExecutable,
    /// ZIP containing a signed and notarized `Scrozz.app`.
    MacosAppZip,
    /// Windows package installed through package deployment APIs.
    WindowsMsix,
    /// ZIP containing the signed portable Windows executable and runtime files.
    WindowsPortableZip,
    /// Gzip-compressed tar archive containing `Scrozz.AppDir`.
    LinuxAppdirTarGz,
}

impl ArtifactKind {
    /// The stable signed-manifest and status token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RawExecutable => "raw-executable",
            Self::MacosAppZip => "macos-app-zip",
            Self::WindowsMsix => "windows-msix",
            Self::WindowsPortableZip => "windows-portable-zip",
            Self::LinuxAppdirTarGz => "linux-appdir-tar-gz",
        }
    }

    fn supports_platform(self, platform: &str) -> bool {
        match self {
            Self::RawExecutable => true,
            Self::MacosAppZip => platform.starts_with("macos-"),
            Self::WindowsMsix | Self::WindowsPortableZip => platform.starts_with("windows-"),
            Self::LinuxAppdirTarGz => platform.starts_with("linux-"),
        }
    }
}

/// A URL that has passed the updater's HTTPS-only validation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HttpsUrl(String);

impl HttpsUrl {
    /// Parses a tightly constrained, absolute HTTPS URL.
    ///
    /// Credentials, fragments, whitespace, backslashes, non-ASCII bytes, empty
    /// host labels, and non-numeric ports are rejected. Query strings are
    /// allowed because release providers commonly use signed query parameters.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidUrl`] if the URL could be interpreted as
    /// anything other than an ordinary HTTPS origin URL.
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_https_url(&value)?;
        Ok(Self(value))
    }

    /// Returns the validated URL text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HttpsUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl AsRef<str> for HttpsUrl {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Serialize for HttpsUrl {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for HttpsUrl {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

/// A syntactically valid lowercase SHA-256 digest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha256Digest {
    bytes: [u8; 32],
    hex: String,
}

impl Sha256Digest {
    /// Parses exactly 64 lowercase hexadecimal characters.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidManifest`] for uppercase, short, long, or
    /// non-hexadecimal input.
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let hex = value.into();
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::InvalidManifest(
                "artifact sha256 must be exactly 64 lowercase hexadecimal characters".into(),
            ));
        }

        let mut bytes = [0_u8; 32];
        let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
        debug_assert!(remainder.is_empty());
        for (index, pair) in pairs.iter().enumerate() {
            bytes[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
        }
        Ok(Self { bytes, hex })
    }

    /// Returns the 32 digest bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// Returns the canonical lowercase hexadecimal spelling.
    #[must_use]
    pub fn as_hex(&self) -> &str {
        &self.hex
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_hex())
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_hex())
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

/// Signed metadata for one platform artifact.
///
/// Values of this type are exposed for inspection. They cannot themselves
/// authorize a download; [`VerifiedArtifact`] is the capability required by
/// verification and download APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactMetadata {
    // Older updater state predates explicit distribution kinds; signed manifests use WireArtifact.
    #[serde(default, deserialize_with = "deserialize_platform")]
    platform: String,
    url: HttpsUrl,
    sha256: Sha256Digest,
    #[serde(deserialize_with = "deserialize_positive_size")]
    size: u64,
    #[serde(default)]
    kind: ArtifactKind,
}

impl ArtifactMetadata {
    fn new(entry_key: String, wire: WireArtifact) -> Result<Self> {
        validate_artifact_id(&entry_key)?;
        let platform = wire.platform.unwrap_or(entry_key);
        validate_platform_key(&platform)?;
        if wire.size == 0 {
            return Err(Error::InvalidManifest(
                "artifact size must be greater than zero".into(),
            ));
        }
        let kind = wire.kind.unwrap_or_default();
        if !kind.supports_platform(&platform) {
            return Err(Error::InvalidManifest(format!(
                "artifact kind `{}` is incompatible with platform `{platform}`",
                kind.as_str()
            )));
        }
        Ok(Self {
            platform,
            url: HttpsUrl::parse(wire.url)?,
            sha256: Sha256Digest::parse(wire.sha256)?,
            size: wire.size,
            kind,
        })
    }

    /// Returns the `os-arch` key used by the manifest.
    #[must_use]
    pub fn platform(&self) -> &str {
        &self.platform
    }

    /// Returns the validated HTTPS download URL.
    #[must_use]
    pub fn url(&self) -> &HttpsUrl {
        &self.url
    }

    /// Returns the signed SHA-256 digest.
    #[must_use]
    pub fn sha256(&self) -> &Sha256Digest {
        &self.sha256
    }

    /// Returns the signed byte length.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Returns the signed installation semantics.
    #[must_use]
    pub const fn kind(&self) -> ArtifactKind {
        self.kind
    }
}

/// An artifact whose metadata came from an exact-byte verified manifest.
///
/// This marker cannot be constructed directly. Keeping it separate from
/// [`ArtifactMetadata`] prevents parsed-but-unverified JSON from authorizing a
/// download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedArtifact {
    metadata: ArtifactMetadata,
}

impl VerifiedArtifact {
    pub(crate) fn from_persisted(metadata: ArtifactMetadata) -> Self {
        Self { metadata }
    }

    /// Returns the signed artifact metadata.
    #[must_use]
    pub fn metadata(&self) -> &ArtifactMetadata {
        &self.metadata
    }
}

/// A manifest whose detached signature and fields have both been validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedManifest {
    generated: u64,
    version: Version,
    artifacts: BTreeMap<String, ArtifactMetadata>,
}

impl VerifiedManifest {
    /// Returns the strictly monotonic manifest generation.
    #[must_use]
    pub const fn generated(&self) -> u64 {
        self.generated
    }

    /// Returns the candidate semantic version.
    #[must_use]
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Returns the verified artifact for a platform key, if one was published.
    #[must_use]
    pub fn artifact_for(&self, platform: &str) -> Option<VerifiedArtifact> {
        self.artifacts
            .get(platform)
            .cloned()
            .map(VerifiedArtifact::from_persisted)
    }

    /// Returns the signed artifact matching both platform and distribution kind.
    #[must_use]
    pub fn artifact_for_kind(
        &self,
        platform: &str,
        kind: ArtifactKind,
    ) -> Option<VerifiedArtifact> {
        self.artifacts
            .values()
            .find(|artifact| artifact.platform() == platform && artifact.kind() == kind)
            .cloned()
            .map(VerifiedArtifact::from_persisted)
    }

    /// Iterates over all signed artifact metadata in key order.
    pub fn artifacts(&self) -> impl Iterator<Item = (&str, &ArtifactMetadata)> {
        self.artifacts
            .iter()
            .map(|(platform, artifact)| (platform.as_str(), artifact))
    }
}

/// Result of signature, schema, field, version, and generation verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestVerification {
    /// The signed manifest describes the exact installed version.
    Current {
        /// The semantic version shared by the manifest and installation.
        version: Version,
        /// The signed manifest generation, which is not accepted as an update.
        generated: u64,
    },
    /// A strictly newer version and generation are available.
    Update(VerifiedManifest),
}

/// One named, pinned Ed25519 verification key.
#[derive(Clone)]
pub struct PinnedKey {
    key_id: String,
    key: VerifyingKey,
}

impl fmt::Debug for PinnedKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PinnedKey")
            .field("key_id", &self.key_id)
            .field("public_key", &self.key.to_bytes())
            .finish()
    }
}

impl PinnedKey {
    /// Validates and constructs a named pinned public key.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe identifier or invalid Ed25519 point.
    pub fn new(key_id: impl Into<String>, public_key: [u8; 32]) -> Result<Self> {
        let key_id = key_id.into();
        validate_key_id(&key_id)?;
        let key = VerifyingKey::from_bytes(&public_key)
            .map_err(|_| Error::InvalidPublicKey(key_id.clone()))?;
        Ok(Self { key_id, key })
    }

    /// Returns the identifier used in detached signature envelopes.
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// Returns the pinned public-key bytes.
    #[must_use]
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.key.to_bytes()
    }
}

/// A rotation-capable set of pinned Ed25519 public keys.
///
/// The production ring is intentionally empty until a human-controlled signing
/// key is created and reviewed. An empty ring is a supported state: every
/// signature is rejected as an unknown key rather than bypassing verification.
#[derive(Debug, Clone, Default)]
pub struct PinnedKeyRing {
    keys: BTreeMap<String, VerifyingKey>,
}

impl PinnedKeyRing {
    /// Builds a ring and rejects duplicate identifiers.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DuplicateKeyId`] if two entries share an identifier.
    pub fn new(keys: impl IntoIterator<Item = PinnedKey>) -> Result<Self> {
        let mut ring = Self::default();
        for pinned in keys {
            match ring.keys.entry(pinned.key_id) {
                Entry::Vacant(entry) => {
                    entry.insert(pinned.key);
                }
                Entry::Occupied(entry) => {
                    return Err(Error::DuplicateKeyId(entry.key().clone()));
                }
            }
        }
        Ok(ring)
    }

    /// Returns the deliberately empty production key ring.
    ///
    /// A real public key must only be added after the corresponding human-held
    /// signing process exists. This method does not fabricate one.
    #[must_use]
    pub fn production() -> Self {
        Self::default()
    }

    /// Returns whether no keys are pinned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Returns the number of distinct pinned identifiers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    fn get(&self, key_id: &str) -> Result<&VerifyingKey> {
        self.keys
            .get(key_id)
            .ok_or_else(|| Error::UnknownKeyId(key_id.to_owned()))
    }
}

/// Verifies exact manifest bytes, then validates and applies anti-rollback rules.
///
/// `manifest_bytes` are passed directly to Ed25519 verification. They are never
/// normalised, parsed, or reserialised first. Only after a valid signature does
/// JSON deserialisation occur.
///
/// # Errors
///
/// Returns an error for an unknown key, malformed or bad signature, invalid
/// manifest field, older version, or replayed generation.
pub fn verify_manifest(
    manifest_bytes: &[u8],
    signature_envelope_bytes: &[u8],
    keys: &PinnedKeyRing,
    installed_version: &Version,
    highest_accepted_generation: u64,
) -> Result<ManifestVerification> {
    let envelope: SignatureEnvelope = serde_json::from_slice(signature_envelope_bytes)
        .map_err(|error| Error::json("signature envelope", error))?;
    if envelope.schema != SIGNATURE_SCHEMA {
        return Err(Error::UnsupportedSignatureSchema(envelope.schema));
    }
    validate_key_id(&envelope.key_id)?;

    let signature_bytes = BASE64_STANDARD
        .decode(envelope.signature.as_bytes())
        .map_err(|_| Error::InvalidSignatureEncoding)?;
    if signature_bytes.len() != 64 || BASE64_STANDARD.encode(&signature_bytes) != envelope.signature
    {
        return Err(Error::InvalidSignatureEncoding);
    }
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|_| Error::InvalidSignatureEncoding)?;
    keys.get(&envelope.key_id)?
        .verify_strict(manifest_bytes, &signature)
        .map_err(|_| Error::BadSignature)?;

    let wire: WireManifest = serde_json::from_slice(manifest_bytes)
        .map_err(|error| Error::json("update manifest", error))?;
    if wire.schema != MANIFEST_SCHEMA {
        return Err(Error::UnsupportedManifestSchema(wire.schema));
    }
    if wire.generated == 0 {
        return Err(Error::InvalidManifest(
            "generated must be greater than zero".into(),
        ));
    }
    if wire.artifacts.0.is_empty() {
        return Err(Error::InvalidManifest(
            "artifacts must contain at least one platform".into(),
        ));
    }

    let mut artifacts = BTreeMap::new();
    let mut distributions = BTreeSet::new();
    for (entry_key, wire_artifact) in wire.artifacts.0 {
        let artifact = ArtifactMetadata::new(entry_key.clone(), wire_artifact)?;
        if !distributions.insert((artifact.platform.clone(), artifact.kind)) {
            return Err(Error::InvalidManifest(format!(
                "platform `{}` contains duplicate `{}` artifacts",
                artifact.platform(),
                artifact.kind().as_str()
            )));
        }
        artifacts.insert(entry_key, artifact);
    }

    match wire.version.cmp(installed_version) {
        std::cmp::Ordering::Less => Err(Error::VersionRollback {
            candidate: wire.version,
            installed: installed_version.clone(),
        }),
        std::cmp::Ordering::Equal => Ok(ManifestVerification::Current {
            version: wire.version,
            generated: wire.generated,
        }),
        std::cmp::Ordering::Greater if wire.generated <= highest_accepted_generation => {
            Err(Error::GenerationReplay {
                candidate: wire.generated,
                highest_accepted: highest_accepted_generation,
            })
        }
        std::cmp::Ordering::Greater => Ok(ManifestVerification::Update(VerifiedManifest {
            generated: wire.generated,
            version: wire.version,
            artifacts,
        })),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignatureEnvelope {
    schema: u32,
    key_id: String,
    signature: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireManifest {
    schema: u32,
    generated: u64,
    version: Version,
    artifacts: ArtifactEntries,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireArtifact {
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    kind: Option<ArtifactKind>,
    url: String,
    sha256: String,
    size: u64,
}

struct ArtifactEntries(BTreeMap<String, WireArtifact>);

impl<'de> Deserialize<'de> for ArtifactEntries {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ArtifactEntriesVisitor;

        impl<'de> Visitor<'de> for ArtifactEntriesVisitor {
            type Value = ArtifactEntries;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object of uniquely keyed platform artifacts")
            }

            fn visit_map<A>(self, mut access: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut artifacts = BTreeMap::new();
                while let Some((platform, artifact)) =
                    access.next_entry::<String, WireArtifact>()?
                {
                    if artifacts.insert(platform.clone(), artifact).is_some() {
                        return Err(A::Error::custom(format!(
                            "duplicate artifact platform key `{platform}`"
                        )));
                    }
                }
                Ok(ArtifactEntries(artifacts))
            }
        }

        deserializer.deserialize_map(ArtifactEntriesVisitor)
    }
}

fn validate_https_url(value: &str) -> Result<()> {
    let invalid = || Error::InvalidUrl(value.to_owned());
    if value.len() > MAX_URL_LEN
        || !value.is_ascii()
        || !value.starts_with("https://")
        || value.contains(['\\', '#'])
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(invalid());
    }

    let remainder = &value["https://".len()..];
    let authority_end = remainder.find(['/', '?']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return Err(invalid());
    }

    if let Some(bracketed) = authority.strip_prefix('[') {
        let Some(close) = bracketed.find(']') else {
            return Err(invalid());
        };
        let address = &bracketed[..close];
        let suffix = &bracketed[close + 1..];
        if address.is_empty()
            || !address
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b':' || byte == b'.')
            || (!suffix.is_empty() && (!suffix.starts_with(':') || !valid_port(&suffix[1..])))
        {
            return Err(invalid());
        }
    } else {
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        if host.is_empty()
            || host.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || !label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    || !label.as_bytes()[0].is_ascii_alphanumeric()
                    || !label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
            })
            || port.is_some_and(|port| !valid_port(port))
        {
            return Err(invalid());
        }
    }
    Ok(())
}

fn valid_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|value| value != 0)
}

fn validate_platform_key(platform: &str) -> Result<()> {
    let valid_segment = |segment: &str| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            && segment.as_bytes()[0].is_ascii_alphanumeric()
            && segment.as_bytes()[segment.len() - 1].is_ascii_alphanumeric()
    };
    let Some((os, arch)) = platform.split_once('-') else {
        return Err(Error::InvalidManifest(format!(
            "artifact platform key `{platform}` must have os-arch form"
        )));
    };
    if platform.len() > MAX_PLATFORM_LEN
        || arch.contains('-')
        || !valid_segment(os)
        || !valid_segment(arch)
    {
        return Err(Error::InvalidManifest(format!(
            "artifact platform key `{platform}` must have lowercase os-arch form"
        )));
    }

    Ok(())
}

fn validate_artifact_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > MAX_ARTIFACT_ID_LEN
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
        || !id.as_bytes()[0].is_ascii_alphanumeric()
        || !id.as_bytes()[id.len() - 1].is_ascii_alphanumeric()
    {
        return Err(Error::InvalidManifest(format!(
            "artifact id `{id}` must contain only lowercase letters, digits, hyphens, and underscores"
        )));
    }
    Ok(())
}

fn validate_key_id(key_id: &str) -> Result<()> {
    if key_id.is_empty()
        || key_id.len() > MAX_KEY_ID_LEN
        || !key_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || !key_id.as_bytes()[0].is_ascii_alphanumeric()
        || !key_id.as_bytes()[key_id.len() - 1].is_ascii_alphanumeric()
    {
        return Err(Error::InvalidKeyId(key_id.to_owned()));
    }
    Ok(())
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("validated before conversion"),
    }
}

fn deserialize_platform<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    validate_platform_key(&value).map_err(D::Error::custom)?;
    Ok(value)
}

fn deserialize_positive_size<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u64::deserialize(deserializer)?;
    if value == 0 {
        return Err(D::Error::custom("artifact size must be greater than zero"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::test_support::{
        ARTIFACT_URL, CANDIDATE_BYTES, manifest_value, ring, sha256_hex, signed_envelope,
        signing_key,
    };

    fn verify_value(value: &Value) -> Result<ManifestVerification> {
        let key = signing_key(7);
        let bytes = serde_json::to_vec(value).unwrap();
        let signature = signed_envelope(&bytes, "test-2026", &key);
        verify_manifest(
            &bytes,
            &signature,
            &ring(&[("test-2026", &key)]),
            &Version::new(1, 0, 0),
            0,
        )
    }

    #[test]
    fn signature_covers_the_exact_manifest_bytes() {
        let key = signing_key(11);
        let value = manifest_value("2.0.0", 9, "linux-x86_64", CANDIDATE_BYTES);
        let exact = serde_json::to_vec_pretty(&value).unwrap();
        let envelope = signed_envelope(&exact, "exact", &key);
        let keys = ring(&[("exact", &key)]);

        assert!(matches!(
            verify_manifest(&exact, &envelope, &keys, &Version::new(1, 0, 0), 0),
            Ok(ManifestVerification::Update(_))
        ));

        let equivalent = serde_json::to_vec(&value).unwrap();
        assert_ne!(exact, equivalent);
        assert!(matches!(
            verify_manifest(&equivalent, &envelope, &keys, &Version::new(1, 0, 0), 0),
            Err(Error::BadSignature)
        ));
    }

    #[test]
    fn distribution_kinds_select_dual_windows_artifacts_without_reinterpretation() {
        let bytes = CANDIDATE_BYTES;
        let value = json!({
            "schema": 1,
            "generated": 8,
            "version": "2.0.0",
            "artifacts": {
                "windows-x86_64-msix": {
                    "platform": "windows-x86_64",
                    "kind": "windows-msix",
                    "url": "https://updates.example.test/scrozz.msix",
                    "sha256": sha256_hex(bytes),
                    "size": bytes.len(),
                },
                "windows-x86_64-portable": {
                    "platform": "windows-x86_64",
                    "kind": "windows-portable-zip",
                    "url": "https://updates.example.test/scrozz.zip",
                    "sha256": sha256_hex(bytes),
                    "size": bytes.len(),
                }
            }
        });
        let ManifestVerification::Update(manifest) = verify_value(&value).unwrap() else {
            panic!("newer manifest must produce an update");
        };

        let msix = manifest
            .artifact_for_kind("windows-x86_64", ArtifactKind::WindowsMsix)
            .unwrap();
        let portable = manifest
            .artifact_for_kind("windows-x86_64", ArtifactKind::WindowsPortableZip)
            .unwrap();
        assert_eq!(msix.metadata().kind(), ArtifactKind::WindowsMsix);
        assert_eq!(portable.metadata().kind(), ArtifactKind::WindowsPortableZip);
        assert_eq!(
            msix.metadata().url().as_str(),
            "https://updates.example.test/scrozz.msix"
        );
        assert_eq!(
            portable.metadata().url().as_str(),
            "https://updates.example.test/scrozz.zip"
        );
    }

    #[test]
    fn legacy_persisted_artifact_metadata_remains_readable() {
        let metadata: ArtifactMetadata = serde_json::from_value(json!({
            "url": ARTIFACT_URL,
            "sha256": sha256_hex(CANDIDATE_BYTES),
            "size": CANDIDATE_BYTES.len(),
        }))
        .unwrap();

        assert_eq!(metadata.platform(), "");
        assert_eq!(metadata.kind(), ArtifactKind::RawExecutable);
    }

    #[test]
    fn duplicate_or_cross_platform_distribution_kinds_are_rejected() {
        let digest = sha256_hex(CANDIDATE_BYTES);
        let duplicate = json!({
            "schema": 1,
            "generated": 8,
            "version": "2.0.0",
            "artifacts": {
                "first": {
                    "platform": "windows-x86_64",
                    "kind": "windows-msix",
                    "url": ARTIFACT_URL,
                    "sha256": digest,
                    "size": CANDIDATE_BYTES.len(),
                },
                "second": {
                    "platform": "windows-x86_64",
                    "kind": "windows-msix",
                    "url": ARTIFACT_URL,
                    "sha256": sha256_hex(CANDIDATE_BYTES),
                    "size": CANDIDATE_BYTES.len(),
                }
            }
        });
        assert!(matches!(
            verify_value(&duplicate),
            Err(Error::InvalidManifest(message)) if message.contains("duplicate")
        ));

        let incompatible = json!({
            "schema": 1,
            "generated": 8,
            "version": "2.0.0",
            "artifacts": {
                "linux-app": {
                    "platform": "linux-x86_64",
                    "kind": "macos-app-zip",
                    "url": ARTIFACT_URL,
                    "sha256": sha256_hex(CANDIDATE_BYTES),
                    "size": CANDIDATE_BYTES.len(),
                }
            }
        });
        assert!(matches!(
            verify_value(&incompatible),
            Err(Error::InvalidManifest(message)) if message.contains("incompatible")
        ));
    }

    #[test]
    fn key_rotation_selects_by_id_and_rejects_unknown_or_duplicate_ids() {
        let old = signing_key(1);
        let current = signing_key(2);
        let bytes = serde_json::to_vec(&manifest_value(
            "2.0.0",
            10,
            "linux-x86_64",
            CANDIDATE_BYTES,
        ))
        .unwrap();
        let envelope = signed_envelope(&bytes, "current", &current);
        let rotating = ring(&[("old", &old), ("current", &current)]);
        assert!(matches!(
            verify_manifest(&bytes, &envelope, &rotating, &Version::new(1, 0, 0), 0),
            Ok(ManifestVerification::Update(_))
        ));

        let unknown_envelope = signed_envelope(&bytes, "future", &current);
        assert!(matches!(
            verify_manifest(
                &bytes,
                &unknown_envelope,
                &rotating,
                &Version::new(1, 0, 0),
                0
            ),
            Err(Error::UnknownKeyId(id)) if id == "future"
        ));

        let duplicate = PinnedKeyRing::new([
            PinnedKey::new("same", old.verifying_key().to_bytes()).unwrap(),
            PinnedKey::new("same", current.verifying_key().to_bytes()).unwrap(),
        ]);
        assert!(matches!(
            duplicate,
            Err(Error::DuplicateKeyId(id)) if id == "same"
        ));
    }

    #[test]
    fn a_signature_from_the_wrong_key_is_rejected() {
        let pinned = signing_key(3);
        let other = signing_key(4);
        let bytes = serde_json::to_vec(&manifest_value(
            "2.0.0",
            11,
            "linux-x86_64",
            CANDIDATE_BYTES,
        ))
        .unwrap();
        let envelope = signed_envelope(&bytes, "pinned", &other);
        assert!(matches!(
            verify_manifest(
                &bytes,
                &envelope,
                &ring(&[("pinned", &pinned)]),
                &Version::new(1, 0, 0),
                0
            ),
            Err(Error::BadSignature)
        ));
    }

    #[test]
    fn malformed_manifest_fields_are_rejected_after_signature_verification() {
        let valid = manifest_value("2.0.0", 12, "linux-x86_64", CANDIDATE_BYTES);
        let mut cases = Vec::new();

        let mut wrong_schema = valid.clone();
        wrong_schema["schema"] = json!(2);
        cases.push(wrong_schema);

        let mut zero_generation = valid.clone();
        zero_generation["generated"] = json!(0);
        cases.push(zero_generation);

        let mut invalid_version = valid.clone();
        invalid_version["version"] = json!("two");
        cases.push(invalid_version);

        let mut empty_artifacts = valid.clone();
        empty_artifacts["artifacts"] = json!({});
        cases.push(empty_artifacts);

        cases.push(manifest_value("2.0.0", 12, "Linux x86_64", CANDIDATE_BYTES));

        let mut insecure_url = valid.clone();
        insecure_url["artifacts"]["linux-x86_64"]["url"] =
            json!("http://updates.example.test/scrozz.bin");
        cases.push(insecure_url);

        let mut uppercase_digest = valid.clone();
        uppercase_digest["artifacts"]["linux-x86_64"]["sha256"] =
            json!(sha256_hex(CANDIDATE_BYTES).to_uppercase());
        cases.push(uppercase_digest);

        let mut zero_size = valid.clone();
        zero_size["artifacts"]["linux-x86_64"]["size"] = json!(0);
        cases.push(zero_size);

        let mut unknown_field = valid;
        unknown_field["unexpected"] = json!(true);
        cases.push(unknown_field);

        for value in cases {
            assert!(verify_value(&value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn duplicate_json_keys_are_rejected_in_structs_and_artifact_maps() {
        let key = signing_key(13);
        let digest = sha256_hex(CANDIDATE_BYTES);
        let size = CANDIDATE_BYTES.len();
        let artifact = serde_json::to_string(&json!({
            "url": ARTIFACT_URL,
            "sha256": digest,
            "size": size,
        }))
        .unwrap();
        let duplicate_artifact = [
            r#"{"schema":1,"generated":13,"version":"2.0.0","artifacts":{"linux-x86_64":"#,
            &artifact,
            r#","linux-x86_64":"#,
            &artifact,
            "}}",
        ]
        .concat();
        let envelope = signed_envelope(duplicate_artifact.as_bytes(), "duplicates", &key);
        assert!(matches!(
            verify_manifest(
                duplicate_artifact.as_bytes(),
                &envelope,
                &ring(&[("duplicates", &key)]),
                &Version::new(1, 0, 0),
                0
            ),
            Err(Error::Json {
                document: "update manifest",
                ..
            })
        ));

        let duplicate_top_level = [
            r#"{"schema":1,"schema":1,"generated":13,"version":"2.0.0","artifacts":{"linux-x86_64":"#,
            &artifact,
            "}}",
        ]
        .concat();
        let envelope = signed_envelope(duplicate_top_level.as_bytes(), "duplicates", &key);
        assert!(matches!(
            verify_manifest(
                duplicate_top_level.as_bytes(),
                &envelope,
                &ring(&[("duplicates", &key)]),
                &Version::new(1, 0, 0),
                0
            ),
            Err(Error::Json {
                document: "update manifest",
                ..
            })
        ));
    }

    #[test]
    fn version_rollback_and_generation_replay_are_distinct() {
        let old = manifest_value("0.9.0", 20, "linux-x86_64", CANDIDATE_BYTES);
        assert!(matches!(
            verify_value(&old),
            Err(Error::VersionRollback { .. })
        ));

        let key = signing_key(21);
        let newer = serde_json::to_vec(&manifest_value(
            "2.0.0",
            20,
            "linux-x86_64",
            CANDIDATE_BYTES,
        ))
        .unwrap();
        let envelope = signed_envelope(&newer, "replay", &key);
        assert!(matches!(
            verify_manifest(
                &newer,
                &envelope,
                &ring(&[("replay", &key)]),
                &Version::new(1, 0, 0),
                20
            ),
            Err(Error::GenerationReplay {
                candidate: 20,
                highest_accepted: 20
            })
        ));

        let current =
            serde_json::to_vec(&manifest_value("1.0.0", 1, "linux-x86_64", CANDIDATE_BYTES))
                .unwrap();
        let envelope = signed_envelope(&current, "replay", &key);
        assert!(matches!(
            verify_manifest(
                &current,
                &envelope,
                &ring(&[("replay", &key)]),
                &Version::new(1, 0, 0),
                20
            ),
            Ok(ManifestVerification::Current { .. })
        ));
    }

    #[test]
    fn the_empty_production_ring_fails_closed() {
        let key = signing_key(22);
        let bytes = serde_json::to_vec(&manifest_value(
            "2.0.0",
            22,
            "linux-x86_64",
            CANDIDATE_BYTES,
        ))
        .unwrap();
        let envelope = signed_envelope(&bytes, "not-pinned", &key);
        let production = PinnedKeyRing::production();
        assert!(production.is_empty());
        assert!(matches!(
            verify_manifest(
                &bytes,
                &envelope,
                &production,
                &Version::new(1, 0, 0),
                0
            ),
            Err(Error::UnknownKeyId(id)) if id == "not-pinned"
        ));
    }

    #[test]
    fn url_validation_rejects_downgrades_credentials_and_fragments() {
        for value in [
            "http://updates.example.test/file",
            "HTTPS://updates.example.test/file",
            "https://user@updates.example.test/file",
            "https://updates.example.test/file#fragment",
            "https://updates.example.test\\file",
            "https://updates..example.test/file",
            "https://updates.example.test:0/file",
        ] {
            assert!(HttpsUrl::parse(value).is_err(), "accepted {value}");
        }
        assert!(HttpsUrl::parse(ARTIFACT_URL).is_ok());
    }
}
