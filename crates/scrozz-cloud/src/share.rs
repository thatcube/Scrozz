//! High-level PUT, share URL and `S3Uploader` implementation.

use std::{
    fmt,
    net::IpAddr,
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
    encoding::{hex_lower, normalise_prefix, public_url},
    error::{Error, Result},
    lifecycle::{
        EXPIRY_TAG, Expiry, ObjectTag, expiry_prefix, lifecycle_prefix_rule_xml,
        lifecycle_rule_xml, lifecycle_versioned_prefix_rule_xml, tag_header,
    },
    redact::Secret,
    sigv4::{AmzDate, presign_get, sign_headers},
    transport::{CancellationToken, HttpRequest, RetryPolicy, Transport, execute_with_retry},
};

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
}

impl<T: fmt::Debug> fmt::Debug for ShareClient<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShareClient")
            .field("config", &self.config)
            .field("credentials", &self.credentials)
            .field("transport", &self.transport)
            .field("retry", &self.retry)
            .field("cancellation", &self.cancellation)
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

    /// Public non-secret configuration.
    #[must_use]
    pub fn config(&self) -> &ShareConfig {
        &self.config
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
        if input.bytes.is_empty() {
            return Err(Error::Config("cannot share an empty capture".to_owned()));
        }
        if input.content_type.trim().is_empty() {
            return Err(Error::Config(
                "a shared object needs a content type".to_owned(),
            ));
        }
        if input
            .content_type
            .bytes()
            .any(|byte| byte.is_ascii_control())
        {
            return Err(Error::Config(
                "a shared object content type must not contain control characters".to_owned(),
            ));
        }
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

        let target = self.config.provider.object_target(bucket, &key)?;
        let put_timestamp = AmzDate::from_system_time(SystemTime::now())?;
        let payload_hash = hex_lower(&sha256(&bytes));
        let mut headers = vec![
            ("host".to_owned(), target.host.clone()),
            ("content-type".to_owned(), content_type),
        ];
        if let Some(value) = tagging {
            headers.push(("x-amz-tagging".to_owned(), value));
        }
        let signed = sign_headers(
            &self.credentials,
            "PUT",
            &target.canonical_uri,
            &self.config.provider.region,
            &put_timestamp,
            &payload_hash,
            headers,
        )?;
        execute_with_retry(
            &self.transport,
            &HttpRequest {
                method: "PUT".to_owned(),
                url: target.url.clone(),
                headers: signed.headers,
                body: bytes,
            },
            self.retry,
            &self.cancellation,
        )?;

        let (url, expires_at) = match expiry {
            Some(expiry) => {
                let signing_time = SystemTime::now();
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
        })
    }
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
    if file_name.is_empty() {
        return Err(Error::Config(
            "a generated object key needs a nonempty file name".to_owned(),
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
                body: Vec::new(),
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
