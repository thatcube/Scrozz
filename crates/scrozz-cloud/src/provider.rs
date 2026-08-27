//! Provider presets and endpoint/addressing behavior.

use std::{
    fmt,
    net::{IpAddr, Ipv6Addr},
    str::FromStr,
};

use crate::{
    encoding::aws_uri_encode,
    error::{Error, Result},
};

/// Supported S3-compatible provider presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// Amazon S3.
    Aws,
    /// Cloudflare R2.
    R2,
    /// Backblaze B2's S3-compatible API.
    B2,
    /// A user-operated MinIO endpoint.
    Minio,
}

impl ProviderKind {
    /// Stable configuration/JSON slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Aws => "aws",
            Self::R2 => "r2",
            Self::B2 => "b2",
            Self::Minio => "minio",
        }
    }

    /// Whether this provider accepts S3 object tags on `PutObject`.
    #[must_use]
    pub const fn supports_object_tags(self) -> bool {
        matches!(self, Self::Aws | Self::Minio)
    }
}

impl FromStr for ProviderKind {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "aws" | "s3" => Ok(Self::Aws),
            "r2" | "cloudflare-r2" => Ok(Self::R2),
            "b2" | "backblaze-b2" => Ok(Self::B2),
            "minio" => Ok(Self::Minio),
            other => Err(Error::Config(format!(
                "unknown cloud provider {other:?}; use aws, r2, b2 or minio"
            ))),
        }
    }
}

/// How the bucket appears in the request target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressingStyle {
    /// `bucket.s3.example/key`
    VirtualHosted,
    /// `s3.example/bucket/key`
    Path,
}

#[derive(Clone, PartialEq, Eq)]
struct Endpoint {
    scheme: String,
    authority: String,
    base_path: String,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_url())
    }
}

impl Endpoint {
    fn parse(raw: &str) -> Result<Self> {
        Self::parse_named(raw, "S3 endpoint")
    }

    fn parse_named(raw: &str, label: &str) -> Result<Self> {
        if raw != raw.trim()
            || raw
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
            || raw.contains('\\')
        {
            return Err(Error::Config(format!(
                "{label} must not contain whitespace, controls or backslashes"
            )));
        }
        let (scheme, rest) = raw
            .split_once("://")
            .ok_or_else(|| Error::Config(format!("{label} must begin with http:// or https://")))?;
        if !matches!(scheme, "http" | "https") {
            return Err(Error::Config(format!(
                "{label} scheme {scheme:?} is not http or https"
            )));
        }
        if rest.contains(['?', '#']) || rest.contains('@') {
            return Err(Error::Config(format!(
                "{label} cannot contain credentials, a query or a fragment"
            )));
        }
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let authority = normalise_authority(authority, scheme, label)?;
        if scheme == "http" && !authority_is_literal_loopback(&authority) {
            return Err(Error::Config(format!(
                "{label} must use HTTPS; plain HTTP is allowed only for a literal \
                 loopback IP development endpoint"
            )));
        }
        validate_base_path(path, label)?;
        let base_path = path.trim_matches('/');
        Ok(Self {
            scheme: scheme.to_owned(),
            authority,
            base_path: if base_path.is_empty() {
                String::new()
            } else {
                format!("/{base_path}")
            },
        })
    }

    fn as_url(&self) -> String {
        format!("{}://{}{}", self.scheme, self.authority, self.base_path)
    }
}

pub(crate) fn normalise_http_base(raw: &str, label: &str) -> Result<String> {
    Endpoint::parse_named(raw, label).map(|endpoint| endpoint.as_url())
}

/// Resolved provider configuration. It never contains credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    /// Provider preset.
    pub kind: ProviderKind,
    /// SigV4 region.
    pub region: String,
    /// Bucket addressing style.
    pub addressing: AddressingStyle,
    endpoint: Endpoint,
}

impl ProviderConfig {
    /// Resolves provider defaults and validates required provider-specific input.
    pub fn from_options(
        kind: ProviderKind,
        region: Option<&str>,
        endpoint: Option<&str>,
        account_id: Option<&str>,
    ) -> Result<Self> {
        let (region, default_endpoint, addressing) = match kind {
            ProviderKind::Aws => {
                let region = nonempty(region).unwrap_or("us-east-1");
                let suffix = if region.starts_with("cn-") {
                    "amazonaws.com.cn"
                } else {
                    "amazonaws.com"
                };
                (
                    region.to_owned(),
                    format!("https://s3.{region}.{suffix}"),
                    AddressingStyle::VirtualHosted,
                )
            }
            ProviderKind::R2 => {
                let default_endpoint = match endpoint {
                    Some(_) => String::new(),
                    None => {
                        let account = nonempty(account_id).ok_or_else(|| {
                            Error::Config(
                                "Cloudflare R2 needs --account-id or SCROZZ_S3_ACCOUNT_ID \
                                 unless a complete endpoint override is supplied"
                                    .to_owned(),
                            )
                        })?;
                        validate_scope_component("Cloudflare R2 account id", account)?;
                        format!("https://{account}.r2.cloudflarestorage.com")
                    }
                };
                ("auto".to_owned(), default_endpoint, AddressingStyle::Path)
            }
            ProviderKind::B2 => {
                let region = nonempty(region).ok_or_else(|| {
                    Error::Config("Backblaze B2 needs its region, such as us-west-004".to_owned())
                })?;
                (
                    region.to_owned(),
                    format!("https://s3.{region}.backblazeb2.com"),
                    AddressingStyle::Path,
                )
            }
            ProviderKind::Minio => {
                let endpoint = nonempty(endpoint).ok_or_else(|| {
                    Error::Config("MinIO needs an explicit http:// or https:// endpoint".to_owned())
                })?;
                (
                    nonempty(region).unwrap_or("us-east-1").to_owned(),
                    endpoint.to_owned(),
                    AddressingStyle::Path,
                )
            }
        };
        validate_scope_component("SigV4 region", &region)?;
        let endpoint = endpoint.unwrap_or(&default_endpoint);
        Ok(Self {
            kind,
            region,
            addressing,
            endpoint: Endpoint::parse(endpoint)?,
        })
    }

    /// The configured endpoint without a bucket or key.
    #[must_use]
    pub fn endpoint(&self) -> String {
        self.endpoint.as_url()
    }

    /// Builds the URL, host and canonical URI for one object.
    pub fn object_target(&self, bucket: &str, key: &str) -> Result<ObjectTarget> {
        validate_bucket(bucket, self.addressing)?;
        if key.is_empty()
            || key.starts_with('/')
            || key.len() > 1024
            || key.split('/').any(|segment| matches!(segment, "." | ".."))
        {
            return Err(Error::Config(
                "an object key must contain 1 to 1024 bytes, must not begin with `/`, and \
                 must not contain `.` or `..` path segments"
                    .to_owned(),
            ));
        }

        let encoded_key = aws_uri_encode(key, false);
        let addressing = if self.endpoint.scheme == "http"
            || (self.addressing == AddressingStyle::VirtualHosted && bucket.contains('.'))
        {
            AddressingStyle::Path
        } else {
            self.addressing
        };
        let (host, canonical_uri) = match addressing {
            AddressingStyle::VirtualHosted => (
                format!("{bucket}.{}", self.endpoint.authority),
                format!("{}/{}", self.endpoint.base_path, encoded_key),
            ),
            AddressingStyle::Path => (
                self.endpoint.authority.clone(),
                format!(
                    "{}/{}/{}",
                    self.endpoint.base_path,
                    aws_uri_encode(bucket, true),
                    encoded_key
                ),
            ),
        };
        let canonical_uri = if canonical_uri.starts_with('/') {
            canonical_uri
        } else {
            format!("/{canonical_uri}")
        };
        Ok(ObjectTarget {
            url: format!("{}://{}{}", self.endpoint.scheme, host, canonical_uri),
            host,
            canonical_uri,
        })
    }
}

/// One resolved object request target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectTarget {
    /// Full HTTP URL.
    pub url: String,
    /// HTTP `Host` and canonical host value.
    pub host: String,
    /// Encoded canonical URI.
    pub canonical_uri: String,
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn validate_scope_component(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(Error::Config(format!(
            "{label} must contain only ASCII letters, digits and hyphens"
        )));
    }
    Ok(())
}

fn normalise_authority(authority: &str, scheme: &str, label: &str) -> Result<String> {
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (address, suffix) = inner.split_once(']').ok_or_else(|| {
            Error::Config(format!("{label} contains an unterminated IPv6 address"))
        })?;
        let address = address
            .parse::<Ipv6Addr>()
            .map_err(|_| Error::Config(format!("{label} contains an invalid IPv6 address")))?;
        let port = if suffix.is_empty() {
            None
        } else {
            Some(suffix.strip_prefix(':').ok_or_else(|| {
                Error::Config(format!("{label} has invalid text after its IPv6 host"))
            })?)
        };
        (format!("[{address}]"), port)
    } else {
        if authority.bytes().filter(|byte| *byte == b':').count() > 1 {
            return Err(Error::Config(format!(
                "{label} must put an IPv6 address in square brackets"
            )));
        }
        let (host, port) = authority
            .rsplit_once(':')
            .map_or((authority, None), |(host, port)| (host, Some(port)));
        if host.is_empty()
            || host.starts_with('.')
            || host.ends_with('.')
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
            || host
                .split('.')
                .any(|label| label.is_empty() || label.starts_with('-') || label.ends_with('-'))
        {
            return Err(Error::Config(format!("{label} contains an invalid host")));
        }
        (host.to_ascii_lowercase(), port)
    };
    let port = port
        .map(|port| {
            port.parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(|| Error::Config(format!("{label} contains an invalid TCP port")))
        })
        .transpose()?;
    Ok(match port {
        Some(80) if scheme == "http" => host,
        Some(443) if scheme == "https" => host,
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn validate_base_path(path: &str, label: &str) -> Result<()> {
    let mut bytes = path.bytes().peekable();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let first = bytes.next();
            let second = bytes.next();
            if !first.is_some_and(|value| value.is_ascii_hexdigit())
                || !second.is_some_and(|value| value.is_ascii_hexdigit())
            {
                return Err(Error::Config(format!(
                    "{label} contains an invalid percent escape"
                )));
            }
        } else if !(byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/'))
        {
            return Err(Error::Config(format!(
                "{label} path must use URL-safe characters or percent escapes"
            )));
        }
    }
    if path.split('/').any(is_encoded_dot_segment) {
        return Err(Error::Config(format!(
            "{label} must not contain `.` or `..` path segments"
        )));
    }
    Ok(())
}

fn authority_is_literal_loopback(authority: &str) -> bool {
    let host = if let Some(inner) = authority.strip_prefix('[') {
        inner.split_once(']').map(|(host, _)| host)
    } else {
        authority
            .rsplit_once(':')
            .filter(|(_, port)| port.parse::<u16>().is_ok())
            .map_or(Some(authority), |(host, _)| Some(host))
    };
    host.is_some_and(|host| {
        host.parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
    })
}

fn is_encoded_dot_segment(segment: &str) -> bool {
    let mut dots = 0;
    let mut bytes = segment.bytes();
    while let Some(byte) = bytes.next() {
        let encoded_dot = byte == b'%'
            && bytes.next().is_some_and(|byte| byte == b'2')
            && bytes.next().is_some_and(|byte| matches!(byte, b'e' | b'E'));
        if byte == b'.' || encoded_dot {
            dots += 1;
        } else {
            return false;
        }
    }
    matches!(dots, 1 | 2)
}

fn validate_bucket(bucket: &str, addressing: AddressingStyle) -> Result<()> {
    if bucket.is_empty()
        || matches!(bucket, "." | "..")
        || bucket.len() > 255
        || !bucket
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
    {
        return Err(Error::Config(format!(
            "invalid bucket {bucket:?}; use only ASCII letters, digits, dots, hyphens or underscores"
        )));
    }
    if addressing == AddressingStyle::VirtualHosted
        && (bucket.len() < 3
            || bucket.len() > 63
            || !bucket.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
            })
            || !bucket.split('.').all(|label| {
                !label.is_empty()
                    && label
                        .bytes()
                        .next()
                        .is_some_and(|byte| byte.is_ascii_alphanumeric())
                    && label
                        .bytes()
                        .last()
                        .is_some_and(|byte| byte.is_ascii_alphanumeric())
            })
            || looks_like_ipv4(bucket))
    {
        return Err(Error::Config(format!(
            "AWS virtual-host bucket {bucket:?} must be a 3-63 character DNS name"
        )));
    }
    Ok(())
}

fn looks_like_ipv4(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    parts.len() == 4
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.parse::<u8>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_uses_virtual_hosted_addressing() {
        let provider =
            ProviderConfig::from_options(ProviderKind::Aws, Some("us-west-2"), None, None).unwrap();
        let target = provider.object_target("shots", "team/a b.png").unwrap();
        assert_eq!(target.host, "shots.s3.us-west-2.amazonaws.com");
        assert_eq!(target.canonical_uri, "/team/a%20b.png");

        let dotted = provider
            .object_target("shots.example", "capture.png")
            .unwrap();
        assert_eq!(dotted.host, "s3.us-west-2.amazonaws.com");
        assert_eq!(dotted.canonical_uri, "/shots.example/capture.png");

        let loopback = ProviderConfig::from_options(
            ProviderKind::Aws,
            Some("us-west-2"),
            Some("http://127.0.0.1:9000"),
            None,
        )
        .unwrap();
        let target = loopback.object_target("shots", "capture.png").unwrap();
        assert_eq!(target.host, "127.0.0.1:9000");
        assert_eq!(target.url, "http://127.0.0.1:9000/shots/capture.png");
    }

    #[test]
    fn r2_b2_and_minio_have_their_real_sigv4_regions_and_endpoints() {
        let r2 =
            ProviderConfig::from_options(ProviderKind::R2, None, None, Some("account")).unwrap();
        assert_eq!(r2.region, "auto");
        assert_eq!(r2.endpoint(), "https://account.r2.cloudflarestorage.com");

        let b2 = ProviderConfig::from_options(ProviderKind::B2, Some("us-west-004"), None, None)
            .unwrap();
        assert_eq!(b2.endpoint(), "https://s3.us-west-004.backblazeb2.com");

        let minio = ProviderConfig::from_options(
            ProviderKind::Minio,
            None,
            Some("http://127.0.0.1:9000"),
            None,
        )
        .unwrap();
        let target = minio.object_target("shots", "a.png").unwrap();
        assert_eq!(target.url, "http://127.0.0.1:9000/shots/a.png");

        let china = ProviderConfig::from_options(ProviderKind::Aws, Some("cn-north-1"), None, None)
            .unwrap();
        assert_eq!(china.endpoint(), "https://s3.cn-north-1.amazonaws.com.cn");
    }

    #[test]
    fn provider_specific_required_values_are_not_guessed() {
        assert!(ProviderConfig::from_options(ProviderKind::R2, None, None, None).is_err());
        assert!(ProviderConfig::from_options(ProviderKind::B2, None, None, None).is_err());
        assert!(ProviderConfig::from_options(ProviderKind::Minio, None, None, None).is_err());
    }

    #[test]
    fn host_forming_inputs_cannot_escape_into_another_authority() {
        assert!(
            ProviderConfig::from_options(ProviderKind::Aws, Some("us/evil"), None, None).is_err()
        );
        assert!(
            ProviderConfig::from_options(ProviderKind::R2, None, None, Some("account/evil"))
                .is_err()
        );
        let aws =
            ProviderConfig::from_options(ProviderKind::Aws, Some("us-east-1"), None, None).unwrap();
        for bucket in ["a@evil", "UPPERCASE", "127.0.0.1", "-leading"] {
            assert!(
                aws.object_target(bucket, "capture.png").is_err(),
                "{bucket}"
            );
        }
    }

    #[test]
    fn endpoint_authority_and_path_are_validated_before_signing() {
        for endpoint in [
            "https://user@example.com",
            "https://example.com:bad",
            "https://example.com/path with space",
            "https://example.com/%zz",
            "https://example.com\\evil",
            "https://[::::]:9000",
            "https://example.com/a/../b",
            "https://example.com/%2e%2e/b",
        ] {
            assert!(
                ProviderConfig::from_options(ProviderKind::Minio, None, Some(endpoint), None)
                    .is_err(),
                "{endpoint}"
            );
        }
        assert!(
            ProviderConfig::from_options(
                ProviderKind::Minio,
                None,
                Some("http://[::1]:9000/base"),
                None,
            )
            .is_ok()
        );
        let normalised = ProviderConfig::from_options(
            ProviderKind::Minio,
            None,
            Some("http://127.0.0.1:80/base"),
            None,
        )
        .unwrap();
        assert_eq!(normalised.endpoint(), "http://127.0.0.1/base");
        assert!(
            ProviderConfig::from_options(
                ProviderKind::Minio,
                None,
                Some("http://localhost:9000"),
                None,
            )
            .is_err()
        );
        assert!(
            ProviderConfig::from_options(
                ProviderKind::Minio,
                None,
                Some("http://storage.example"),
                None,
            )
            .unwrap_err()
            .to_string()
            .contains("HTTPS")
        );
        assert!(
            normalised
                .object_target("shots", "a/../capture.png")
                .is_err()
        );
    }
}
