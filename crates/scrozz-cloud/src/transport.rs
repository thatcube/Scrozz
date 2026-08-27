//! An injectable HTTP transport with bounded retries and cancellation.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::{
    error::{Error, Result},
    redact::sensitive_header,
};

/// A complete HTTP request. Debug output never prints bearer headers or bytes.
#[derive(Clone)]
pub struct HttpRequest {
    /// HTTP method.
    pub method: String,
    /// Configured provider URL.
    pub url: String,
    /// Request headers.
    pub headers: Vec<(String, String)>,
    /// Request body.
    pub body: Vec<u8>,
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers = self
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str(),
                    if sensitive_header(name) {
                        "[REDACTED]"
                    } else {
                        value.as_str()
                    },
                )
            })
            .collect::<Vec<_>>();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// The status and bounded response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status.
    pub status: u16,
    /// Response body, when retained.
    pub body: Vec<u8>,
}

/// An HTTP implementation. The default crate has only this seam, no client.
pub trait Transport: fmt::Debug + Send + Sync {
    /// Sends one attempt.
    fn send(&self, request: &HttpRequest, cancellation: &CancellationToken)
    -> Result<HttpResponse>;
}

/// Cooperative cancellation shared with a UI or queue.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// Requests cancellation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Retry bounds for transient transport and service failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts, including the first.
    pub max_attempts: u8,
    /// First backoff.
    pub base_delay: Duration,
    /// Backoff cap.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(125),
            max_delay: Duration::from_secs(2),
        }
    }
}

/// Sends with exponential backoff, checking cancellation before each attempt
/// and in small slices while waiting.
pub fn execute_with_retry(
    transport: &dyn Transport,
    request: &HttpRequest,
    policy: RetryPolicy,
    cancellation: &CancellationToken,
) -> Result<HttpResponse> {
    let attempts = policy.max_attempts.max(1);
    for attempt in 0..attempts {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        match transport.send(request, cancellation) {
            Ok(response) if (200..300).contains(&response.status) => return Ok(response),
            Ok(response) => {
                let error = Error::HttpStatus(response.status);
                if attempt + 1 == attempts || !error.is_retryable() {
                    return Err(error);
                }
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(error) => {
                if attempt + 1 == attempts || !error.is_retryable() {
                    return Err(error);
                }
            }
        }
        let multiplier = 1u32.checked_shl(u32::from(attempt)).unwrap_or(u32::MAX);
        let delay = policy
            .base_delay
            .saturating_mul(multiplier)
            .min(policy.max_delay);
        cancellable_sleep(delay, cancellation)?;
    }
    unreachable!("at least one attempt is always made")
}

fn cancellable_sleep(delay: Duration, cancellation: &CancellationToken) -> Result<()> {
    let slice = Duration::from_millis(20);
    let mut left = delay;
    while !left.is_zero() {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let current = left.min(slice);
        std::thread::sleep(current);
        left = left.saturating_sub(current);
    }
    Ok(())
}

/// Blocking `ureq` transport, present only with the non-default `network`
/// feature. Redirects and ambient proxies are disabled so a configured endpoint
/// cannot hand a signed PUT to another host.
#[cfg(feature = "network")]
#[derive(Debug, Clone)]
pub struct UreqTransport {
    agent: ureq::Agent,
}

#[cfg(feature = "network")]
impl UreqTransport {
    /// Builds a transport with a finite global timeout and no redirects.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(0)
            .proxy(None)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

#[cfg(feature = "network")]
impl Default for UreqTransport {
    fn default() -> Self {
        Self::new(Duration::from_secs(30))
    }
}

#[cfg(feature = "network")]
impl Transport for UreqTransport {
    fn send(
        &self,
        request: &HttpRequest,
        cancellation: &CancellationToken,
    ) -> Result<HttpResponse> {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut builder = match request.method.as_str() {
            "PUT" => self.agent.put(&request.url),
            other => {
                return Err(Error::Config(format!(
                    "the cloud transport does not send {other} requests"
                )));
            }
        };
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        match builder.send(request.body.as_slice()) {
            Ok(response) => Ok(HttpResponse {
                status: response.status().as_u16(),
                body: Vec::new(),
            }),
            Err(ureq::Error::StatusCode(status)) => Ok(HttpResponse {
                status,
                body: Vec::new(),
            }),
            Err(error) => {
                let mut diagnostic = error.to_string();
                for (name, value) in &request.headers {
                    if sensitive_header(name) && !value.is_empty() {
                        diagnostic = diagnostic.replace(value, "[REDACTED]");
                    }
                }
                Err(Error::Transport(diagnostic))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex, time::Instant};

    use super::*;

    #[derive(Debug)]
    struct Scripted {
        statuses: Mutex<VecDeque<u16>>,
        calls: Mutex<usize>,
    }

    impl Transport for Scripted {
        fn send(
            &self,
            _request: &HttpRequest,
            _cancellation: &CancellationToken,
        ) -> Result<HttpResponse> {
            *self.calls.lock().unwrap() += 1;
            Ok(HttpResponse {
                status: self.statuses.lock().unwrap().pop_front().unwrap_or(200),
                body: Vec::new(),
            })
        }
    }

    fn request() -> HttpRequest {
        HttpRequest {
            method: "PUT".into(),
            url: "https://storage.example/object".into(),
            headers: vec![("Authorization".into(), "credential-secret".into())],
            body: b"secret pixels".to_vec(),
        }
    }

    #[test]
    fn retries_transient_statuses_then_succeeds() {
        let transport = Scripted {
            statuses: Mutex::new(VecDeque::from([503, 429, 200])),
            calls: Mutex::new(0),
        };
        let response = execute_with_retry(
            &transport,
            &request(),
            RetryPolicy {
                max_attempts: 4,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            },
            &CancellationToken::default(),
        )
        .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(*transport.calls.lock().unwrap(), 3);
    }

    #[test]
    fn cancellation_interrupts_backoff() {
        let transport = Arc::new(Scripted {
            statuses: Mutex::new(VecDeque::from([503, 503])),
            calls: Mutex::new(0),
        });
        let cancellation = CancellationToken::default();
        let cancel_from_thread = cancellation.clone();
        let transport_from_thread = Arc::clone(&transport);
        let started = Instant::now();
        let handle = std::thread::spawn(move || {
            execute_with_retry(
                transport_from_thread.as_ref(),
                &request(),
                RetryPolicy {
                    max_attempts: 4,
                    base_delay: Duration::from_secs(2),
                    max_delay: Duration::from_secs(2),
                },
                &cancel_from_thread,
            )
        });
        std::thread::sleep(Duration::from_millis(40));
        cancellation.cancel();
        assert!(matches!(handle.join().unwrap(), Err(Error::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn request_debug_redacts_headers_and_body() {
        let rendered = format!("{:?}", request());
        assert!(!rendered.contains("credential-secret"));
        assert!(!rendered.contains("secret pixels"));
        assert!(rendered.contains("[REDACTED]"));
    }

    #[cfg(feature = "network")]
    #[test]
    fn network_transport_never_inherits_an_ambient_proxy() {
        let transport = UreqTransport::default();
        assert!(transport.agent.config().proxy().is_none());
    }
}
