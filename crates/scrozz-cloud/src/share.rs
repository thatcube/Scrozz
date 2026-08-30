//! High-level PUT, share URL and `S3Uploader` implementation.

use std::{
    fmt,
    net::IpAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use scrozz_export::{S3Object, S3Uploader};

#[cfg(feature = "network")]
use crate::credentials::{CredentialResolver, ProcessEnvironment};
#[cfg(feature = "network")]
use crate::transport::UreqTransport;
use crate::{
    bundle::{encrypt, render_viewer},
    config::{Branding, ConfigOverrides, ShareConfig},
    credentials::Credentials,
    digest::sha256,
    encoding::{canonical_query, hex_lower, normalise_prefix, public_url},
    error::{Error, Result},
    lifecycle::{
        EXPIRY_TAG, Expiry, ObjectTag, expiry_prefix, lifecycle_prefix_rule_xml,
        lifecycle_rule_xml, lifecycle_versioned_prefix_rule_xml, tag_header,
    },
    provider::ObjectTarget,
    redact::Secret,
    sigv4::{AmzDate, presign_get, sign_headers_with_query},
    transport::{CancellationToken, HttpRequest, RetryPolicy, Transport, execute_with_retry},
};

/// Maximum source object accepted by Scrozz's bounded in-memory sharing API.
pub const MAX_SHARE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// Browser viewers embed ciphertext, so password shares use a tighter bound.
pub const MAX_PASSWORD_SHARE_BYTES: u64 = 256 * 1024 * 1024;
const MULTIPART_THRESHOLD: usize = 16 * 1024 * 1024;
const MULTIPART_PART_BYTES: usize = 8 * 1024 * 1024;
const MAX_MULTIPART_PARTS: usize = 10_000;

/// Time source used for request signatures and exact link expiry.
pub trait Clock: Send + Sync {
    /// Current wall-clock time.
    fn now(&self) -> SystemTime;
}

/// Operating-system wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// How this invocation chooses its link lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExpiryPolicy {
    /// Use the configuration default, one day when no setting is present.
    #[default]
    Configured,
    /// Return an unsigned public/custom-domain URL. The bucket must be public.
    Never,
    /// Return a private-object presigned URL with this lifetime.
    After(Expiry),
}

/// Bytes and object name to share.
#[derive(Clone, Copy)]
pub struct ShareInput<'a> {
    /// Encoded capture.
    pub bytes: &'a [u8],
    /// Browser media type.
    pub content_type: &'a str,
    /// Relative object key before the configured prefix.
    pub key: &'a str,
}

impl fmt::Debug for ShareInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShareInput")
            .field("bytes", &format_args!("[{} BYTES]", self.bytes.len()))
            .field("content_type", &self.content_type)
            .field("key", &self.key)
            .finish()
    }
}

/// Per-share behavior.
#[derive(Debug, Clone, Default)]
pub struct ShareOptions {
    /// Link expiry behavior.
    pub expiry: ExpiryPolicy,
    /// A password means encrypt locally and upload a viewer, never a plaintext
    /// capture plus a pretend server gate.
    pub password: Option<Secret>,
    /// Additional object tags.
    pub tags: Vec<ObjectTag>,
    /// Per-share viewer branding override.
    pub branding: Option<Branding>,
}

/// Result safe to present to the user. Its custom Debug avoids accidentally
/// logging bearer query parameters.
#[derive(Clone, PartialEq, Eq)]
pub struct ShareResult {
    /// URL to copy or print.
    pub url: String,
    /// Final object key.
    pub key: String,
    /// Provider slug.
    pub provider: &'static str,
    /// Signed lifetime, when provider-enforced.
    pub expires_seconds: Option<u32>,
    /// Exact wall-clock instant encoded by the signature expiry.
    pub expires_at: Option<SystemTime>,
    /// One-time lifecycle configuration matching the PUT tag or reserved prefix.
    pub lifecycle_rule: Option<String>,
    /// Whether only ciphertext left the machine.
    pub encrypted: bool,
    /// Configured and per-share user tags, excluding Scrozz's reserved expiry tag.
    pub tags: Vec<ObjectTag>,
}

impl fmt::Debug for ShareResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShareResult")
            .field("url", &"[BEARER URL REDACTED]")
            .field("key", &self.key)
            .field("provider", &self.provider)
            .field("expires_seconds", &self.expires_seconds)
            .field("expires_at", &self.expires_at)
            .field("lifecycle_rule", &self.lifecycle_rule)
            .field("encrypted", &self.encrypted)
            .field("tags", &self.tags)
            .finish()
    }
}

/// An S3-compatible sharing client over an injected transport.
pub struct ShareClient<T> {
    config: ShareConfig,
    credentials: Credentials,
    transport: T,
    retry: RetryPolicy,
    cancellation: CancellationToken,
    clock: Arc<dyn Clock>,
}

impl<T: fmt::Debug> fmt::Debug for ShareClient<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShareClient")
            .field("config", &self.config)
            .field("credentials", &self.credentials)
            .field("transport", &self.transport)
            .field("retry", &self.retry)
            .field("cancellation", &self.cancellation)
            .field("clock", &"[CLOCK]")
            .finish()
    }
}

impl<T: Transport> ShareClient<T> {
    /// Builds a client. Nothing is sent until [`Self::share`].
    #[must_use]
    pub fn new(config: ShareConfig, credentials: Credentials, transport: T) -> Self {
        Self {
            config,
            credentials,
            transport,
            retry: RetryPolicy::default(),
            cancellation: CancellationToken::default(),
            clock: Arc::new(SystemClock),
        }
    }

    /// Replaces retry bounds.
    #[must_use]
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Installs a cancellation token shared with the caller.
    #[must_use]
    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Replaces the wall clock for deterministic signing and tests.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Clock + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Public non-secret configuration.
    #[must_use]
    pub fn config(&self) -> &ShareConfig {
        &self.config
    }

    /// Performs a signed, read-only `HeadBucket` request.
    ///
    /// This validates endpoint reachability, credentials, region, and bucket
    /// access without creating or deleting an object.
    pub fn test_connection(&self) -> Result<()> {
        let target = self.config.provider.bucket_target(&self.config.bucket)?;
        self.execute_signed(
            "HEAD",
            &target,
            &[],
            vec![("host".to_owned(), target.host.clone())],
            Vec::new(),
            self.retry,
            &self.cancellation,
        )
        .map(|_| ())
    }

    /// Encrypts when requested, tags and signs the PUT, sends it with retries,
    /// then creates the honest public or presigned link.
    pub fn share(&self, input: ShareInput<'_>, options: ShareOptions) -> Result<ShareResult> {
        let key = format!(
            "{}{}",
            normalise_prefix(&self.config.prefix),
            input.key.trim_start_matches('/')
        );
        self.share_to_bucket(&self.config.bucket, &key, input, options)
    }

    fn share_to_bucket(
        &self,
        bucket: &str,
        requested_key: &str,
        input: ShareInput<'_>,
        options: ShareOptions,
    ) -> Result<ShareResult> {
        let password_protected = options.password.is_some();
        validate_source_length(input.bytes.len(), password_protected)?;
        validate_content_type(input.content_type)?;
        let expiry = match options.expiry {
            ExpiryPolicy::Configured => self.config.default_expiry,
            ExpiryPolicy::Never => None,
            ExpiryPolicy::After(expiry) => Some(expiry),
        };
        let supports_tags = self.config.provider.kind.supports_object_tags();
        let mut tags = self.config.tags.clone();
        tags.extend(options.tags);
        if !supports_tags && !tags.is_empty() {
            return Err(Error::Config(format!(
                "{} does not support S3 object tags; remove configured and per-share tags",
                self.config.provider.kind.slug()
            )));
        }
        if tags.iter().any(|tag| tag.key() == EXPIRY_TAG) {
            return Err(Error::Config(format!(
                "{EXPIRY_TAG:?} is reserved for Scrozz lifecycle expiry"
            )));
        }
        let result_tags = tags.clone();
        if let Some(expiry) = expiry
            && supports_tags
        {
            tags.push(expiry.lifecycle_tag());
        }
        let tagging = if tags.is_empty() {
            None
        } else {
            Some(tag_header(&tags)?)
        };
        let prefixed_key;
        let requested_key = if let Some(expiry) = expiry
            && !supports_tags
        {
            prefixed_key = format!("{}{requested_key}", expiry_prefix(expiry));
            prefixed_key.as_str()
        } else {
            requested_key
        };
        let branding = options
            .branding
            .unwrap_or_else(|| self.config.branding.clone());
        let recipient_base = if expiry.is_some() {
            self.config.provider.endpoint()
        } else {
            self.config
                .public_base_url
                .clone()
                .unwrap_or_else(|| self.config.provider.endpoint())
        };
        if options.password.is_some() && !is_trustworthy_webcrypto_origin(&recipient_base) {
            return Err(Error::Config(
                "password shares need an HTTPS recipient URL (HTTP is allowed only for a \
                 loopback development endpoint) because WebCrypto requires a secure context"
                    .to_owned(),
            ));
        }

        let (key, bytes, content_type, encrypted) = match options.password {
            Some(password) => {
                let encrypted = encrypt(input.bytes, input.content_type, &password)?;
                (
                    viewer_key(requested_key),
                    render_viewer(&encrypted, &branding)?,
                    "text/html; charset=utf-8".to_owned(),
                    true,
                )
            }
            None => (
                requested_key.to_owned(),
                input.bytes.to_vec(),
                input.content_type.to_owned(),
                false,
            ),
        };
        if bytes.len() as u64 > MAX_SHARE_BYTES {
            return Err(Error::Config(format!(
                "the prepared share is {} bytes; the maximum is {MAX_SHARE_BYTES}",
                bytes.len()
            )));
        }

        let target = self.config.provider.object_target(bucket, &key)?;
        let response = self.upload_object(&target, bytes, content_type, tagging)?;

        let (url, expires_at) = match expiry {
            Some(expiry) => {
                let signing_time = response
                    .header("date")
                    .and_then(|value| httpdate::parse_http_date(value).ok())
                    .unwrap_or_else(|| self.clock.now());
                let link_timestamp = AmzDate::from_system_time(signing_time)?;
                let signed_second = signing_time
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| Error::Config("SigV4 cannot sign a time before 1970".to_owned()))?
                    .as_secs();
                let expiry_second = signed_second
                    .checked_add(u64::from(expiry.seconds()))
                    .ok_or_else(|| Error::Config("share expiry timestamp overflow".to_owned()))?;
                let expires_at = UNIX_EPOCH
                    .checked_add(Duration::from_secs(expiry_second))
                    .ok_or_else(|| Error::Config("share expiry timestamp overflow".to_owned()))?;
                (
                    presign_get(
                        &self.credentials,
                        &target.url,
                        &target.canonical_uri,
                        &target.host,
                        &self.config.provider.region,
                        &link_timestamp,
                        expiry.seconds(),
                    )?,
                    Some(expires_at),
                )
            }
            None => (
                self.config
                    .public_base_url
                    .as_deref()
                    .map(|base| public_url(base, &key))
                    .unwrap_or(target.url),
                None,
            ),
        };
        Ok(ShareResult {
            url,
            key,
            provider: self.config.provider.kind.slug(),
            expires_seconds: expiry.map(Expiry::seconds),
            expires_at,
            lifecycle_rule: expiry.map(|expiry| {
                if supports_tags {
                    lifecycle_rule_xml(expiry)
                } else if self.config.provider.kind == crate::provider::ProviderKind::B2 {
                    lifecycle_versioned_prefix_rule_xml(expiry)
                } else {
                    lifecycle_prefix_rule_xml(expiry)
                }
            }),
            encrypted,
            tags: result_tags,
        })
    }

    fn upload_object(
        &self,
        target: &ObjectTarget,
        bytes: Vec<u8>,
        content_type: String,
        tagging: Option<String>,
    ) -> Result<crate::transport::HttpResponse> {
        if bytes.len() >= MULTIPART_THRESHOLD {
            self.multipart_upload(target, bytes, content_type, tagging)
        } else {
            let digest = hex_lower(&sha256(&bytes));
            let mut headers = vec![
                ("host".to_owned(), target.host.clone()),
                ("content-type".to_owned(), content_type),
                ("x-amz-meta-scrozz-sha256".to_owned(), digest),
            ];
            if let Some(tagging) = tagging {
                headers.push(("x-amz-tagging".to_owned(), tagging));
            }
            self.execute_signed(
                "PUT",
                target,
                &[],
                headers,
                bytes,
                self.retry,
                &self.cancellation,
            )
        }
    }

    fn multipart_upload(
        &self,
        target: &ObjectTarget,
        bytes: Vec<u8>,
        content_type: String,
        tagging: Option<String>,
    ) -> Result<crate::transport::HttpResponse> {
        let parts = bytes.len().div_ceil(MULTIPART_PART_BYTES);
        if parts == 0 || parts > MAX_MULTIPART_PARTS {
            return Err(Error::Config(format!(
                "the upload needs {parts} multipart chunks; S3 accepts at most {MAX_MULTIPART_PARTS}"
            )));
        }
        let digest = hex_lower(&sha256(&bytes));
        let mut headers = vec![
            ("host".to_owned(), target.host.clone()),
            ("content-type".to_owned(), content_type),
            ("x-amz-meta-scrozz-sha256".to_owned(), digest),
        ];
        if let Some(tagging) = tagging {
            headers.push(("x-amz-tagging".to_owned(), tagging));
        }
        // CreateMultipartUpload is not idempotent: retrying after a lost
        // response can orphan a second upload id. Parts and cleanup are retried;
        // creation itself is deliberately attempted once.
        let initiated = self.execute_signed(
            "POST",
            target,
            &[("uploads".to_owned(), String::new())],
            headers,
            Vec::new(),
            RetryPolicy {
                max_attempts: 1,
                ..self.retry
            },
            &self.cancellation,
        )?;
        let upload_id = xml_text(&initiated.body, "UploadId")?;

        let completed = (|| -> Result<crate::transport::HttpResponse> {
            let mut etags = Vec::with_capacity(parts);
            for (index, part) in bytes.chunks(MULTIPART_PART_BYTES).enumerate() {
                if self.cancellation.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                let number = index + 1;
                let response = self.execute_signed(
                    "PUT",
                    target,
                    &[
                        ("partNumber".to_owned(), number.to_string()),
                        ("uploadId".to_owned(), upload_id.clone()),
                    ],
                    vec![("host".to_owned(), target.host.clone())],
                    part.to_vec(),
                    self.retry,
                    &self.cancellation,
                )?;
                let etag = response.header("etag").ok_or_else(|| {
                    Error::Transport(format!(
                        "the provider omitted ETag for multipart chunk {number}"
                    ))
                })?;
                if etag.is_empty()
                    || etag.len() > 256
                    || etag.bytes().any(|byte| byte.is_ascii_control())
                {
                    return Err(Error::Transport(format!(
                        "the provider returned an invalid ETag for multipart chunk {number}"
                    )));
                }
                etags.push(etag.to_owned());
            }
            let mut complete = String::from("<CompleteMultipartUpload>");
            for (index, etag) in etags.iter().enumerate() {
                complete.push_str("<Part><PartNumber>");
                complete.push_str(&(index + 1).to_string());
                complete.push_str("</PartNumber><ETag>");
                complete.push_str(&xml_escape(etag));
                complete.push_str("</ETag></Part>");
            }
            complete.push_str("</CompleteMultipartUpload>");
            let response = self.execute_signed(
                "POST",
                target,
                &[("uploadId".to_owned(), upload_id.clone())],
                vec![
                    ("host".to_owned(), target.host.clone()),
                    ("content-type".to_owned(), "application/xml".to_owned()),
                ],
                complete.into_bytes(),
                RetryPolicy {
                    max_attempts: 1,
                    ..self.retry
                },
                &self.cancellation,
            )?;
            validate_multipart_completion(&response)?;
            Ok(response)
        })();

        match completed {
            Ok(response) => Ok(response),
            Err(error) => match self.abort_multipart(target, &upload_id) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(Error::Transport(format!(
                    "{error}; aborting the incomplete multipart upload also failed: {cleanup}"
                ))),
            },
        }
    }

    fn abort_multipart(&self, target: &ObjectTarget, upload_id: &str) -> Result<()> {
        let cleanup_cancellation = CancellationToken::default();
        self.execute_signed(
            "DELETE",
            target,
            &[("uploadId".to_owned(), upload_id.to_owned())],
            vec![("host".to_owned(), target.host.clone())],
            Vec::new(),
            RetryPolicy {
                max_attempts: 2,
                ..self.retry
            },
            &cleanup_cancellation,
        )
        .map(|_| ())
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_signed(
        &self,
        method: &str,
        target: &ObjectTarget,
        query: &[(String, String)],
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        retry: RetryPolicy,
        cancellation: &CancellationToken,
    ) -> Result<crate::transport::HttpResponse> {
        let canonical_query = canonical_query(query);
        let timestamp = AmzDate::from_system_time(self.clock.now())?;
        let payload_hash = hex_lower(&sha256(&body));
        let signed = sign_headers_with_query(
            &self.credentials,
            method,
            &target.canonical_uri,
            &canonical_query,
            &self.config.provider.region,
            &timestamp,
            &payload_hash,
            headers,
        )?;
        let url = if canonical_query.is_empty() {
            target.url.clone()
        } else {
            format!("{}?{canonical_query}", target.url)
        };
        execute_with_retry(
            &self.transport,
            &HttpRequest {
                method: method.to_owned(),
                url,
                headers: signed.headers,
                body,
            },
            retry,
            cancellation,
        )
    }
}

fn validate_source_length(length: usize, password_protected: bool) -> Result<()> {
    if length == 0 {
        return Err(Error::Config("cannot share an empty capture".to_owned()));
    }
    let maximum = if password_protected {
        MAX_PASSWORD_SHARE_BYTES
    } else {
        MAX_SHARE_BYTES
    };
    if length as u64 > maximum {
        let mode = if password_protected {
            "password-protected share"
        } else {
            "share"
        };
        return Err(Error::Config(format!(
            "the {mode} source is {length} bytes; the maximum is {maximum}"
        )));
    }
    Ok(())
}

fn validate_content_type(content_type: &str) -> Result<()> {
    const ARTIFACT_TYPES: &[&str] = &[
        "image/jpeg",
        "image/gif",
        "image/png",
        "image/webp",
        "video/mp4",
        "video/quicktime",
        "video/webm",
        "video/x-matroska",
    ];
    if !ARTIFACT_TYPES.contains(&content_type) {
        return Err(Error::Config(format!(
            "unsupported artifact content type {content_type:?}; use PNG, JPEG, WebP, GIF, MP4, MOV, WebM or Matroska"
        )));
    }
    Ok(())
}

fn xml_text(body: &[u8], element: &str) -> Result<String> {
    let text = std::str::from_utf8(body).map_err(|_| {
        Error::Transport(format!(
            "the provider returned non-UTF-8 XML for multipart {element}"
        ))
    })?;
    let opening = format!("<{element}>");
    let closing = format!("</{element}>");
    let start = text.find(&opening).map(|offset| offset + opening.len());
    let value = start
        .and_then(|start| {
            text[start..]
                .find(&closing)
                .map(|end| &text[start..start + end])
        })
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Error::Transport(format!("the provider response omitted multipart {element}"))
        })?;
    Ok(xml_unescape(value))
}

fn validate_multipart_completion(response: &crate::transport::HttpResponse) -> Result<()> {
    let text = std::str::from_utf8(&response.body).map_err(|_| {
        Error::Transport("the multipart completion response was not UTF-8 XML".to_owned())
    })?;
    if text.contains("<Error>") || text.contains("<ErrorResult") {
        return Err(Error::Transport(
            "the provider returned an error document while completing the multipart upload"
                .to_owned(),
        ));
    }
    if !text.contains("<CompleteMultipartUploadResult") {
        return Err(Error::Transport(
            "the provider did not confirm multipart completion".to_owned(),
        ));
    }
    Ok(())
}

fn xml_unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

impl<T: Transport> S3Uploader for ShareClient<T> {
    fn upload(&self, object: &S3Object<'_>) -> scrozz_core::Result<String> {
        self.share_to_bucket(
            object.bucket,
            object.key,
            ShareInput {
                bytes: object.bytes,
                content_type: object.content_type,
                key: object.key,
            },
            ShareOptions::default(),
        )
        .map(|result| result.url)
        .map_err(scrozz_core::Error::from)
    }
}

/// Resolves public config and credentials and creates the optional network
/// client. The environment is checked before command and explicit credentials.
#[cfg(feature = "network")]
pub fn client_from_environment(
    overrides: ConfigOverrides,
    explicit: Option<Credentials>,
) -> Result<ShareClient<UreqTransport>> {
    client_from_environment_lazy(overrides, || Ok(explicit))
}

/// Builds the network client while deferring explicit credentials until the
/// environment and credential-command sources have both been ruled out.
#[cfg(feature = "network")]
pub fn client_from_environment_lazy<F>(
    overrides: ConfigOverrides,
    explicit: F,
) -> Result<ShareClient<UreqTransport>>
where
    F: FnOnce() -> Result<Option<Credentials>>,
{
    let environment = ProcessEnvironment;
    let config = ShareConfig::from_environment(&environment, overrides)?;
    let resolved = CredentialResolver::new(&environment)
        .resolve_lazy(config.credential_command.as_ref(), explicit)?;
    Ok(ShareClient::new(
        config,
        resolved.credentials,
        UreqTransport::default(),
    ))
}

/// The symbol remains available without networking so callers can compile one
/// path and report a precise build boundary.
#[cfg(not(feature = "network"))]
pub fn client_from_environment(
    _overrides: ConfigOverrides,
    _explicit: Option<Credentials>,
) -> Result<()> {
    Err(Error::Config(
        "this scrozz-cloud build has no network transport; enable the `network` feature".to_owned(),
    ))
}

/// Non-network counterpart to [`client_from_environment_lazy`].
#[cfg(not(feature = "network"))]
pub fn client_from_environment_lazy<F>(_overrides: ConfigOverrides, _explicit: F) -> Result<()>
where
    F: FnOnce() -> Result<Option<Credentials>>,
{
    Err(Error::Config(
        "this scrozz-cloud build has no network transport; enable the `network` feature".to_owned(),
    ))
}

fn viewer_key(key: &str) -> String {
    if key.to_ascii_lowercase().ends_with(".html") {
        key.to_owned()
    } else {
        format!("{key}.html")
    }
}

/// Adds a cryptographically random suffix while preserving a file extension.
pub fn unique_object_key(file_name: &str) -> Result<String> {
    if file_name.is_empty()
        || file_name.len() > 255
        || matches!(file_name, "." | "..")
        || file_name
            .bytes()
            .any(|byte| byte.is_ascii_control() || matches!(byte, b'/' | b'\\'))
    {
        return Err(Error::Config(
            "a generated object key needs a 1 to 255 byte file name without controls or path separators"
                .to_owned(),
        ));
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|_| Error::Crypto("secure object-key generation failed".to_owned()))?;
    let suffix = hex_lower(&random);
    Ok(match file_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            format!("{stem}-{suffix}.{extension}")
        }
        _ => format!("{file_name}-{suffix}"),
    })
}

fn is_trustworthy_webcrypto_origin(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if scheme == "https" {
        return true;
    }
    if scheme != "http" {
        return false;
    }
    let authority = rest.split('/').next().unwrap_or_default();
    let host = if let Some(inner) = authority.strip_prefix('[') {
        inner.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::{
        ConfigOverrides, HttpResponse, ProviderKind, Secret, credentials::Credentials,
        transport::HttpRequest,
    };

    use super::*;

    #[derive(Debug, Default)]
    struct RecordingTransport {
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl Transport for RecordingTransport {
        fn send(
            &self,
            request: &HttpRequest,
            _cancellation: &CancellationToken,
        ) -> Result<HttpResponse> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(HttpResponse {
                status: 200,
                headers: if request.url.contains("partNumber=") {
                    vec![("etag".to_owned(), "\"part-etag\"".to_owned())]
                } else {
                    Vec::new()
                },
                body: if request.method == "POST" && request.url.contains("uploads=") {
                    b"<InitiateMultipartUploadResult><UploadId>upload-1</UploadId></InitiateMultipartUploadResult>"
                        .to_vec()
                } else if request.method == "POST" && request.url.contains("uploadId=") {
                    b"<CompleteMultipartUploadResult><Key>recording.mp4</Key></CompleteMultipartUploadResult>"
                        .to_vec()
                } else {
                    Vec::new()
                },
            })
        }
    }

    fn config() -> ShareConfig {
        ShareConfig::from_env(ConfigOverrides {
            provider: Some(ProviderKind::Minio),
            endpoint: Some("http://127.0.0.1:9000".into()),
            bucket: Some("shots".into()),
            prefix: Some("team".into()),
            ..ConfigOverrides::default()
        })
        .unwrap()
    }

    fn credentials() -> Credentials {
        Credentials::new("access", Secret::from_text("secret"), None).unwrap()
    }

    #[test]
    fn put_is_signed_tagged_and_followed_by_an_expiring_get() {
        let client = ShareClient::new(config(), credentials(), RecordingTransport::default())
            .with_retry_policy(RetryPolicy {
                max_attempts: 1,
                ..RetryPolicy::default()
            });
        let result = client
            .share(
                ShareInput {
                    bytes: b"png bytes",
                    content_type: "image/png",
                    key: "capture one.png",
                },
                ShareOptions {
                    expiry: ExpiryPolicy::After("2h".parse().unwrap()),
                    tags: vec![ObjectTag::new("project", "demo").unwrap()],
                    ..ShareOptions::default()
                },
            )
            .unwrap();
        let request = &client.transport.requests.lock().unwrap()[0];
        assert_eq!(
            request.url,
            "http://127.0.0.1:9000/shots/team/capture%20one.png"
        );
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "authorization"
                    && value.starts_with("AWS4-HMAC-SHA256"))
        );
        let tagging = request
            .headers
            .iter()
            .find(|(name, _)| name == "x-amz-tagging")
            .unwrap();
        assert!(tagging.1.contains("project=demo"));
        assert!(tagging.1.contains("scrozz-expiry-days=1"));
        assert!(result.url.contains("X-Amz-Expires=7200"));
        assert!(result.url.contains("X-Amz-Signature="));
        let remaining = result
            .expires_at
            .unwrap()
            .duration_since(SystemTime::now())
            .unwrap()
            .as_secs();
        assert!((7_198..=7_200).contains(&remaining));
        assert!(result.lifecycle_rule.unwrap().contains("<Days>1</Days>"));
    }

    #[test]
    fn password_mode_uploads_only_a_self_contained_html_viewer() {
        let client = ShareClient::new(config(), credentials(), RecordingTransport::default());
        let result = client
            .share(
                ShareInput {
                    bytes: b"plaintext-capture-marker",
                    content_type: "image/png",
                    key: "secret.png",
                },
                ShareOptions {
                    password: Some(Secret::from_text("password")),
                    ..ShareOptions::default()
                },
            )
            .unwrap();
        let request = &client.transport.requests.lock().unwrap()[0];
        assert_eq!(result.key, "team/secret.png.html");
        assert!(result.encrypted);
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "content-type" && value == "text/html; charset=utf-8")
        );
        assert!(
            !request
                .body
                .windows(24)
                .any(|window| window == b"plaintext-capture-marker")
        );
        assert!(String::from_utf8_lossy(&request.body).contains("crypto.subtle"));
    }

    #[test]
    fn custom_domain_is_used_only_for_non_expiring_public_links() {
        let mut config = config();
        config.public_base_url = Some("https://shots.example/team".into());
        let client = ShareClient::new(config, credentials(), RecordingTransport::default());
        let result = client
            .share(
                ShareInput {
                    bytes: b"x",
                    content_type: "image/png",
                    key: "a b.png",
                },
                ShareOptions {
                    expiry: ExpiryPolicy::Never,
                    ..ShareOptions::default()
                },
            )
            .unwrap();
        assert_eq!(result.url, "https://shots.example/team/team/a%20b.png");
        assert!(result.expires_seconds.is_none());
        assert!(result.expires_at.is_none());
        assert!(result.lifecycle_rule.is_none());
    }

    #[test]
    fn providers_without_tags_use_an_expiry_prefix_and_reject_custom_tags() {
        let config = ShareConfig::from_env(ConfigOverrides {
            provider: Some(ProviderKind::B2),
            region: Some("us-west-004".into()),
            bucket: Some("shots".into()),
            prefix: Some("team".into()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        let client = ShareClient::new(config, credentials(), RecordingTransport::default());
        let result = client
            .share(
                ShareInput {
                    bytes: b"x",
                    content_type: "image/png",
                    key: "capture.png",
                },
                ShareOptions::default(),
            )
            .unwrap();
        assert_eq!(result.key, "scrozz-expiry-1d/team/capture.png".to_owned());
        let request = &client.transport.requests.lock().unwrap()[0];
        assert!(
            !request
                .headers
                .iter()
                .any(|(name, _)| name == "x-amz-tagging")
        );
        assert!(
            result
                .lifecycle_rule
                .unwrap()
                .contains("<Prefix>scrozz-expiry-1d/</Prefix>")
        );

        let tagged = client.share(
            ShareInput {
                bytes: b"x",
                content_type: "image/png",
                key: "capture.png",
            },
            ShareOptions {
                tags: vec![ObjectTag::new("project", "demo").unwrap()],
                ..ShareOptions::default()
            },
        );
        assert!(tagged.is_err());
    }

    #[test]
    fn password_viewers_require_a_secure_recipient_origin() {
        let mut config = ShareConfig::from_env(ConfigOverrides {
            provider: Some(ProviderKind::Minio),
            endpoint: Some("https://storage.example".into()),
            bucket: Some("shots".into()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        config.public_base_url = Some("http://storage.example".to_owned());
        let client = ShareClient::new(config, credentials(), RecordingTransport::default());
        let error = client
            .share(
                ShareInput {
                    bytes: b"x",
                    content_type: "image/png",
                    key: "capture.png",
                },
                ShareOptions {
                    expiry: ExpiryPolicy::Never,
                    password: Some(Secret::from_text("password")),
                    ..ShareOptions::default()
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("HTTPS"), "{error}");
    }

    #[test]
    fn generated_keys_are_random_and_keep_the_extension() {
        let first = unique_object_key("Screenshot.png").unwrap();
        let second = unique_object_key("Screenshot.png").unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with("Screenshot-"));
        assert!(first.ends_with(".png"));
        assert!(unique_object_key("../capture.png").is_err());
        assert!(unique_object_key("folder/capture.png").is_err());
    }

    #[test]
    fn connection_test_is_a_signed_read_only_head_request() {
        let client = ShareClient::new(config(), credentials(), RecordingTransport::default());
        client.test_connection().unwrap();
        let requests = client.transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "HEAD");
        assert!(requests[0].body.is_empty());
        assert!(
            requests[0]
                .headers
                .iter()
                .any(|(name, value)| name == "authorization"
                    && value.starts_with("AWS4-HMAC-SHA256"))
        );
    }

    #[test]
    fn large_objects_use_retryable_parts_and_complete_once() {
        let client = ShareClient::new(config(), credentials(), RecordingTransport::default())
            .with_retry_policy(RetryPolicy {
                max_attempts: 1,
                ..RetryPolicy::default()
            });
        let bytes = vec![7; MULTIPART_THRESHOLD];
        client
            .share(
                ShareInput {
                    bytes: &bytes,
                    content_type: "video/mp4",
                    key: "recording.mp4",
                },
                ShareOptions::default(),
            )
            .unwrap();
        let requests = client.transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].method, "POST");
        assert!(requests[0].url.ends_with("?uploads="));
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.contains("partNumber="))
                .count(),
            2
        );
        let complete = requests.last().unwrap();
        assert_eq!(complete.method, "POST");
        assert!(complete.url.contains("uploadId=upload-1"));
        let body = String::from_utf8_lossy(&complete.body);
        assert!(body.contains("<PartNumber>1</PartNumber>"), "{body}");
        assert!(body.contains("&quot;part-etag&quot;"), "{body}");
    }

    #[derive(Debug, Default)]
    struct CancellingMultipartTransport {
        requests: Mutex<Vec<String>>,
    }

    impl Transport for CancellingMultipartTransport {
        fn send(
            &self,
            request: &HttpRequest,
            cancellation: &CancellationToken,
        ) -> Result<HttpResponse> {
            self.requests
                .lock()
                .unwrap()
                .push(format!("{} {}", request.method, request.url));
            if request.method == "POST" && request.url.contains("uploads=") {
                return Ok(HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: b"<UploadId>cancel-me</UploadId>".to_vec(),
                });
            }
            if request.method == "PUT" && request.url.contains("partNumber=") {
                cancellation.cancel();
                return Ok(HttpResponse {
                    status: 503,
                    headers: Vec::new(),
                    body: Vec::new(),
                });
            }
            Ok(HttpResponse {
                status: 204,
                headers: Vec::new(),
                body: Vec::new(),
            })
        }
    }

    #[test]
    fn cancellation_aborts_an_incomplete_multipart_upload() {
        let transport = CancellingMultipartTransport::default();
        let token = CancellationToken::default();
        let client = ShareClient::new(config(), credentials(), transport)
            .with_cancellation(token)
            .with_retry_policy(RetryPolicy {
                max_attempts: 2,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            });
        let bytes = vec![9; MULTIPART_THRESHOLD];
        let result = client.share(
            ShareInput {
                bytes: &bytes,
                content_type: "video/mp4",
                key: "cancel.mp4",
            },
            ShareOptions::default(),
        );
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        let requests = client.transport.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .any(|request| request.starts_with("DELETE ")
                    && request.contains("uploadId=cancel-me")),
            "{requests:?}"
        );
    }

    #[derive(Debug, Default)]
    struct CompletionErrorTransport {
        requests: Mutex<Vec<String>>,
    }

    impl Transport for CompletionErrorTransport {
        fn send(
            &self,
            request: &HttpRequest,
            _cancellation: &CancellationToken,
        ) -> Result<HttpResponse> {
            self.requests
                .lock()
                .unwrap()
                .push(format!("{} {}", request.method, request.url));
            let (status, headers, body) =
                if request.method == "POST" && request.url.contains("uploads=") {
                    (
                        200,
                        Vec::new(),
                        b"<UploadId>error-at-complete</UploadId>".to_vec(),
                    )
                } else if request.method == "PUT" && request.url.contains("partNumber=") {
                    (
                        200,
                        vec![("etag".to_owned(), "\"part-etag\"".to_owned())],
                        Vec::new(),
                    )
                } else if request.method == "POST" && request.url.contains("uploadId=") {
                    (
                        200,
                        Vec::new(),
                        b"<Error><Code>InternalError</Code></Error>".to_vec(),
                    )
                } else {
                    (204, Vec::new(), Vec::new())
                };
            Ok(HttpResponse {
                status,
                headers,
                body,
            })
        }
    }

    #[test]
    fn http_200_completion_error_is_aborted_not_reported_as_a_share() {
        let transport = CompletionErrorTransport::default();
        let client =
            ShareClient::new(config(), credentials(), transport).with_retry_policy(RetryPolicy {
                max_attempts: 1,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            });
        let bytes = vec![3; MULTIPART_THRESHOLD];
        let error = client
            .share(
                ShareInput {
                    bytes: &bytes,
                    content_type: "video/mp4",
                    key: "complete-error.mp4",
                },
                ShareOptions::default(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("error document"), "{error}");
        let requests = client.transport.requests.lock().unwrap();
        assert!(
            requests.iter().any(|request| request.starts_with("DELETE ")
                && request.contains("uploadId=error-at-complete")),
            "{requests:?}"
        );
    }

    #[derive(Debug)]
    struct FixedClock(SystemTime);

    impl Clock for FixedClock {
        fn now(&self) -> SystemTime {
            self.0
        }
    }

    #[derive(Debug)]
    struct DatedTransport;

    impl Transport for DatedTransport {
        fn send(
            &self,
            _request: &HttpRequest,
            _cancellation: &CancellationToken,
        ) -> Result<HttpResponse> {
            Ok(HttpResponse {
                status: 200,
                headers: vec![(
                    "date".to_owned(),
                    "Mon, 01 Jan 2024 00:00:00 GMT".to_owned(),
                )],
                body: Vec::new(),
            })
        }
    }

    #[test]
    fn provider_date_anchors_the_exact_presigned_expiry() {
        let local = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        let provider = httpdate::parse_http_date("Mon, 01 Jan 2024 00:00:00 GMT").unwrap();
        let client =
            ShareClient::new(config(), credentials(), DatedTransport).with_clock(FixedClock(local));
        let result = client
            .share(
                ShareInput {
                    bytes: b"x",
                    content_type: "image/png",
                    key: "clock.png",
                },
                ShareOptions::default(),
            )
            .unwrap();
        assert_eq!(
            result.expires_at,
            Some(provider + Duration::from_secs(86_400))
        );
        assert!(result.url.contains("X-Amz-Date=20240101T000000Z"));
    }

    #[test]
    fn source_bounds_and_media_types_fail_before_transport() {
        assert!(validate_source_length(0, false).is_err());
        assert!(validate_source_length((MAX_SHARE_BYTES + 1) as usize, false).is_err());
        assert!(validate_source_length((MAX_PASSWORD_SHARE_BYTES + 1) as usize, true).is_err());
        assert!(validate_content_type("application/octet-stream").is_err());
    }

    #[test]
    fn multipart_completion_rejects_s3_error_documents_with_http_200() {
        let response = HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: b"<Error><Code>InternalError</Code></Error>".to_vec(),
        };
        let error = validate_multipart_completion(&response).unwrap_err();
        assert!(error.to_string().contains("error document"), "{error}");

        let missing = HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: Vec::new(),
        };
        assert!(validate_multipart_completion(&missing).is_err());
    }

    #[test]
    fn viewer_keys_preserve_the_full_requested_path_and_extension() {
        assert_eq!(viewer_key("folder.v1/capture"), "folder.v1/capture.html");
        assert_eq!(viewer_key("capture.png"), "capture.png.html");
        assert_eq!(viewer_key("capture.HTML"), "capture.HTML");
    }

    #[test]
    fn share_input_debug_never_prints_capture_bytes() {
        let input = ShareInput {
            bytes: b"private-pixel-marker",
            content_type: "image/png",
            key: "capture.png",
        };
        let rendered = format!("{input:?}");
        assert!(!rendered.contains("private-pixel-marker"));
        assert!(rendered.contains("20 BYTES"));
    }
}

#[cfg(all(test, feature = "network"))]
mod loopback_tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        time::Duration,
    };

    use super::*;
    use crate::{ProviderKind, UreqTransport};

    #[test]
    fn real_transport_puts_to_loopback_fake_s3() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sent, received) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..count]);
                let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let header_text = String::from_utf8_lossy(&bytes[..header_end]);
                let length = header_text
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= header_end + 4 + length {
                    break;
                }
            }
            sent.send(bytes).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        });

        let config = ShareConfig::from_env(ConfigOverrides {
            provider: Some(ProviderKind::Minio),
            endpoint: Some(format!("http://{address}")),
            bucket: Some("fake".into()),
            prefix: Some("captures".into()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        let client = ShareClient::new(
            config,
            Credentials::new("access", Secret::from_text("secret"), None).unwrap(),
            UreqTransport::new(Duration::from_secs(2)),
        )
        .with_retry_policy(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        });
        let result = client
            .share(
                ShareInput {
                    bytes: b"loopback-body",
                    content_type: "image/png",
                    key: "test.png",
                },
                ShareOptions::default(),
            )
            .unwrap();
        server.join().unwrap();
        let request = String::from_utf8_lossy(&received.recv().unwrap()).into_owned();
        assert!(request.starts_with("PUT /fake/captures/test.png HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: aws4-hmac-sha256")
        );
        assert!(request.ends_with("loopback-body"));
        assert!(result.url.contains("X-Amz-Signature="));
    }
}
