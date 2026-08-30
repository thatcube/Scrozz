//! Forward-compatible sharing metadata stored beside each capture.

use std::{collections::BTreeMap, fmt};

use scrozz_core::{Error, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::model::Timestamp;

/// Sharing metadata attached to one capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureSharing {
    /// The share URL shown to the user.
    pub url: ShareUrl,
    /// When the provider-enforced URL expires, if it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    /// Which provider the object lives behind.
    pub provider: ShareProvider,
    /// Stable provider-side object identifier, typically an object key.
    pub remote_object_id: RemoteObjectId,
    /// Whether the provider-side object currently exists and is usable.
    #[serde(default)]
    pub remote_status: RemoteObjectStatus,
    /// Whether remote deletion has been requested or completed.
    #[serde(default)]
    pub deletion: RemoteDeletionState,
    /// Provider-side tags associated with the share.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<ShareTag>,
    /// What kind of media the remote object is serving.
    pub media_kind: SharedMediaKind,
    /// Forward-compatible fields newer builds may add.
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    extra: BTreeMap<String, Value>,
}

impl CaptureSharing {
    /// A completed share with the minimum required metadata.
    #[must_use]
    pub fn new(
        url: ShareUrl,
        provider: ShareProvider,
        remote_object_id: RemoteObjectId,
        media_kind: SharedMediaKind,
    ) -> Self {
        Self {
            url,
            expires_at: None,
            provider,
            remote_object_id,
            remote_status: RemoteObjectStatus::Available,
            deletion: RemoteDeletionState::NotRequested,
            tags: Vec::new(),
            media_kind,
            extra: BTreeMap::new(),
        }
    }

    /// Records an expiry instant.
    #[must_use]
    pub const fn expiring_at(mut self, at: Timestamp) -> Self {
        self.expires_at = Some(at);
        self
    }

    /// Replaces the tag set.
    #[must_use]
    pub fn tagged(mut self, tags: Vec<ShareTag>) -> Self {
        self.tags = tags;
        self
    }

    /// Replaces the remote-object status.
    #[must_use]
    pub fn with_remote_status(mut self, status: RemoteObjectStatus) -> Self {
        self.remote_status = status;
        self
    }

    /// Replaces the deletion state.
    #[must_use]
    pub fn with_deletion(mut self, deletion: RemoteDeletionState) -> Self {
        self.deletion = deletion;
        self
    }

    pub(crate) fn validate_for_storage(&self) -> Result<()> {
        self.url.validate()?;
        self.remote_object_id.validate()?;
        for tag in &self.tags {
            tag.validate()?;
        }
        Ok(())
    }
}

/// A share URL safe to persist.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ShareUrl(String);

impl ShareUrl {
    /// Validates and stores a URL.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_share_url(&value)?;
        Ok(Self(value))
    }

    /// The original URL string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> Result<()> {
        validate_share_url(&self.0)
    }
}

impl fmt::Display for ShareUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[BEARER URL REDACTED]")
    }
}

impl fmt::Debug for ShareUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ShareUrl([BEARER URL REDACTED])")
    }
}

/// A provider-side object identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RemoteObjectId(String);

impl RemoteObjectId {
    /// Validates and stores an object identifier.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_remote_object_id(&value)?;
        Ok(Self(value))
    }

    /// The original provider-side identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> Result<()> {
        validate_remote_object_id(&self.0)
    }
}

impl fmt::Display for RemoteObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A stored share tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareTag {
    key: String,
    value: String,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    extra: BTreeMap<String, Value>,
}

impl ShareTag {
    /// Validates and stores a tag.
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Result<Self> {
        let tag = Self {
            key: key.into(),
            value: value.into(),
            extra: BTreeMap::new(),
        };
        tag.validate()?;
        Ok(tag)
    }

    /// The tag key.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The tag value.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    fn validate(&self) -> Result<()> {
        if self.key.is_empty() {
            return Err(Error::InvalidRequest(
                "share-tag keys must not be empty".into(),
            ));
        }
        if !self.key.chars().all(is_tag_character) || !self.value.chars().all(is_tag_character) {
            return Err(Error::InvalidRequest(
                "share tags may contain Unicode letters, numbers and spaces, plus `_ . : / = + - @`"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// Which provider preset produced the share.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum ShareProvider {
    Aws,
    R2,
    B2,
    Minio,
    Unknown(String),
}

impl ShareProvider {
    /// Stable stored token.
    #[must_use]
    pub fn as_token(&self) -> &str {
        match self {
            Self::Aws => "aws",
            Self::R2 => "r2",
            Self::B2 => "b2",
            Self::Minio => "minio",
            Self::Unknown(token) => token,
        }
    }

    /// Reads a stored token back, preserving unknown future values.
    #[must_use]
    pub fn from_token(token: impl Into<String>) -> Self {
        let token = token.into();
        match token.as_str() {
            "aws" => Self::Aws,
            "r2" => Self::R2,
            "b2" => Self::B2,
            "minio" => Self::Minio,
            _ => Self::Unknown(token),
        }
    }
}

impl Serialize for ShareProvider {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_token())
    }
}

impl<'de> Deserialize<'de> for ShareProvider {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from_token(String::deserialize(deserializer)?))
    }
}

/// Whether the remote object currently exists and can be used.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[allow(missing_docs)]
pub enum RemoteObjectStatus {
    #[default]
    Pending,
    Available,
    Missing,
    Failed,
    Unknown(String),
}

impl RemoteObjectStatus {
    /// Stable stored token.
    #[must_use]
    pub fn as_token(&self) -> &str {
        match self {
            Self::Pending => "pending",
            Self::Available => "available",
            Self::Missing => "missing",
            Self::Failed => "failed",
            Self::Unknown(token) => token,
        }
    }

    /// Reads a stored token back, preserving unknown future values.
    #[must_use]
    pub fn from_token(token: impl Into<String>) -> Self {
        let token = token.into();
        match token.as_str() {
            "pending" => Self::Pending,
            "available" => Self::Available,
            "missing" => Self::Missing,
            "failed" => Self::Failed,
            _ => Self::Unknown(token),
        }
    }
}

impl Serialize for RemoteObjectStatus {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_token())
    }
}

impl<'de> Deserialize<'de> for RemoteObjectStatus {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from_token(String::deserialize(deserializer)?))
    }
}

/// Where remote deletion stands.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[allow(missing_docs)]
pub enum RemoteDeletionState {
    #[default]
    NotRequested,
    Requested,
    Deleted,
    Failed,
    Unknown(String),
}

impl RemoteDeletionState {
    /// Stable stored token.
    #[must_use]
    pub fn as_token(&self) -> &str {
        match self {
            Self::NotRequested => "not_requested",
            Self::Requested => "requested",
            Self::Deleted => "deleted",
            Self::Failed => "failed",
            Self::Unknown(token) => token,
        }
    }

    /// Reads a stored token back, preserving unknown future values.
    #[must_use]
    pub fn from_token(token: impl Into<String>) -> Self {
        let token = token.into();
        match token.as_str() {
            "not_requested" => Self::NotRequested,
            "requested" => Self::Requested,
            "deleted" => Self::Deleted,
            "failed" => Self::Failed,
            _ => Self::Unknown(token),
        }
    }
}

impl Serialize for RemoteDeletionState {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_token())
    }
}

impl<'de> Deserialize<'de> for RemoteDeletionState {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from_token(String::deserialize(deserializer)?))
    }
}

/// What kind of remote media was shared.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum SharedMediaKind {
    Image,
    ViewerPage,
    EncryptedBundle,
    Unknown(String),
}

impl SharedMediaKind {
    /// Stable stored token.
    #[must_use]
    pub fn as_token(&self) -> &str {
        match self {
            Self::Image => "image",
            Self::ViewerPage => "viewer_page",
            Self::EncryptedBundle => "encrypted_bundle",
            Self::Unknown(token) => token,
        }
    }

    /// Reads a stored token back, preserving unknown future values.
    #[must_use]
    pub fn from_token(token: impl Into<String>) -> Self {
        let token = token.into();
        match token.as_str() {
            "image" => Self::Image,
            "viewer_page" => Self::ViewerPage,
            "encrypted_bundle" => Self::EncryptedBundle,
            _ => Self::Unknown(token),
        }
    }
}

impl Serialize for SharedMediaKind {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_token())
    }
}

impl<'de> Deserialize<'de> for SharedMediaKind {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from_token(String::deserialize(deserializer)?))
    }
}

fn validate_share_url(value: &str) -> Result<()> {
    if value.is_empty()
        || value != value.trim()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || value.contains('\\')
    {
        return Err(Error::InvalidRequest(
            "share URLs must not be empty and must not contain whitespace, controls or backslashes"
                .into(),
        ));
    }
    let (scheme, rest) = value.split_once("://").ok_or_else(|| {
        Error::InvalidRequest("share URLs must begin with http:// or https://".into())
    })?;
    if !matches!(scheme, "http" | "https") {
        return Err(Error::InvalidRequest(format!(
            "share URL scheme {scheme:?} is not http or https"
        )));
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return Err(Error::InvalidRequest(
            "share URLs must not contain embedded credentials".into(),
        ));
    }
    Ok(())
}

fn validate_remote_object_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value != value.trim()
        || value.starts_with('/')
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b'\\')
        || value
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
    {
        return Err(Error::InvalidRequest(
            "remote object ids must be non-empty, slash-relative, and free of controls".into(),
        ));
    }
    Ok(())
}

fn is_tag_character(ch: char) -> bool {
    ch.is_alphanumeric() || ch == ' ' || matches!(ch, '_' | '.' | ':' | '/' | '=' | '+' | '-' | '@')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_urls_reject_embedded_credentials() {
        assert!(ShareUrl::new("https://alice:secret@example.com/shot.png").is_err());
        assert!(ShareUrl::new("ftp://example.com/shot.png").is_err());
        assert!(ShareUrl::new("https://example.com/shot.png?x=1").is_ok());
    }

    #[test]
    fn share_url_diagnostics_redact_bearer_query_values() {
        let url =
            ShareUrl::new("https://example.com/shot.png?X-Amz-Signature=never-print").unwrap();
        for rendered in [format!("{url}"), format!("{url:?}")] {
            assert!(!rendered.contains("never-print"), "{rendered}");
            assert!(rendered.contains("REDACTED"), "{rendered}");
        }
        assert!(url.as_str().contains("never-print"));
    }

    #[test]
    fn enum_tokens_round_trip_without_losing_unknown_values() {
        let sharing: CaptureSharing = serde_json::from_value(serde_json::json!({
            "url": "https://example.com/share",
            "provider": "custom-cloud",
            "remote_object_id": "shots/1.png",
            "remote_status": "archived",
            "deletion": "purging",
            "tags": [{ "key": "project", "value": "scrozz" }],
            "media_kind": "immersive_viewer",
            "future_flag": true
        }))
        .expect("decode");

        assert_eq!(
            sharing.provider,
            ShareProvider::Unknown("custom-cloud".into())
        );
        assert_eq!(
            sharing.remote_status,
            RemoteObjectStatus::Unknown("archived".into())
        );
        assert_eq!(
            sharing.deletion,
            RemoteDeletionState::Unknown("purging".into())
        );
        assert_eq!(
            sharing.media_kind,
            SharedMediaKind::Unknown("immersive_viewer".into())
        );

        let back = serde_json::to_value(&sharing).expect("encode");
        assert_eq!(back["future_flag"], Value::Bool(true));
        assert_eq!(back["provider"], Value::String("custom-cloud".into()));
    }
}
