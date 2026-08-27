use std::{
    ffi::OsString,
    fs::File,
    io::{self, Read as _, Seek as _, SeekFrom},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
};

use crate::{Error, HttpsUrl, Result};

const CONNECT_TIMEOUT_SECONDS: u64 = 10;
const MIN_TRANSFER_TIMEOUT_SECONDS: u64 = 120;
const MAX_TRANSFER_TIMEOUT_SECONDS: u64 = 60 * 60;
const MIN_EXPECTED_BYTES_PER_SECOND: u64 = 64 * 1024;
const MAX_CAPTURED_STDERR_BYTES: usize = 64 * 1024;

/// One HTTPS fetch with a fixed user-agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    url: HttpsUrl,
    user_agent: String,
    max_bytes: u64,
    transfer_timeout_seconds: u64,
}

impl FetchRequest {
    /// Creates a request from a validated URL and inert user-agent value.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidUserAgent`] for an empty, oversized, non-ASCII,
    /// or control-character-bearing value.
    pub fn new(url: HttpsUrl, user_agent: impl Into<String>, max_bytes: u64) -> Result<Self> {
        let user_agent = user_agent.into();
        if user_agent.is_empty()
            || user_agent.len() > 512
            || !user_agent.is_ascii()
            || user_agent.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(Error::InvalidUserAgent);
        }
        if max_bytes == 0 {
            return Err(Error::InvalidFetchLimit);
        }
        Ok(Self {
            url,
            user_agent,
            max_bytes,
            transfer_timeout_seconds: transfer_timeout_seconds(max_bytes),
        })
    }

    /// Creates a request using the non-identifying Scrozz update user-agent.
    ///
    /// # Errors
    ///
    /// Returns an error only if the compile-time identity constants cease to
    /// satisfy [`Self::new`].
    pub fn for_scrozz(url: HttpsUrl, max_bytes: u64) -> Result<Self> {
        Self::new(url, scrozz_core::identity::user_agent(), max_bytes)
    }

    /// Returns the validated HTTPS URL.
    #[must_use]
    pub fn url(&self) -> &HttpsUrl {
        &self.url
    }

    /// Returns the complete user-agent header value.
    #[must_use]
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// Returns the maximum number of response bytes accepted.
    #[must_use]
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Returns the size-aware whole-transfer timeout passed to curl.
    #[must_use]
    pub const fn transfer_timeout_seconds(&self) -> u64 {
        self.transfer_timeout_seconds
    }
}

/// Destination-oriented transport used by the update engine.
///
/// Implementations only move bytes. They do not parse manifests, verify
/// signatures, verify artifacts, stage, or install.
pub trait Fetcher {
    /// Fetches one request into `destination`.
    ///
    /// The updater supplies an exclusively created same-directory temporary
    /// file. Implementations write through this held handle, never by reopening
    /// a pathname, and must enforce [`FetchRequest::max_bytes`].
    ///
    /// # Errors
    ///
    /// Returns a transport or filesystem error. The updater removes the
    /// temporary destination after any failure.
    fn fetch(&self, request: &FetchRequest, destination: &mut File) -> Result<()>;
}

/// HTTPS fetcher that invokes a `curl` subprocess without a shell.
#[derive(Debug, Clone)]
pub struct CurlFetcher {
    program: PathBuf,
}

impl Default for CurlFetcher {
    fn default() -> Self {
        Self {
            program: PathBuf::from("curl"),
        }
    }
}

impl CurlFetcher {
    /// Uses `curl` resolved through the process search path.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Uses a caller-selected curl executable.
    ///
    /// This is primarily useful to distributions that install curl outside the
    /// ordinary process search path.
    #[must_use]
    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// Returns the exact argument vector used for a request.
    ///
    /// Exposing the plan lets packaging and tests audit HTTPS redirect controls
    /// without making a network request.
    #[must_use]
    pub fn planned_arguments(&self, request: &FetchRequest) -> Vec<OsString> {
        [
            OsString::from("--disable"),
            OsString::from("--fail"),
            OsString::from("--silent"),
            OsString::from("--show-error"),
            OsString::from("--location"),
            OsString::from("--proto"),
            OsString::from("=https"),
            OsString::from("--proto-redir"),
            OsString::from("=https"),
            OsString::from("--max-redirs"),
            OsString::from("5"),
            OsString::from("--connect-timeout"),
            OsString::from(CONNECT_TIMEOUT_SECONDS.to_string()),
            OsString::from("--max-time"),
            OsString::from(request.transfer_timeout_seconds().to_string()),
            OsString::from("--max-filesize"),
            OsString::from(request.max_bytes().to_string()),
            OsString::from("--user-agent"),
            OsString::from(request.user_agent()),
            OsString::from("--url"),
            OsString::from(request.url().as_str()),
        ]
        .into()
    }
}

impl Fetcher for CurlFetcher {
    fn fetch(&self, request: &FetchRequest, destination: &mut File) -> Result<()> {
        destination
            .set_len(0)
            .and_then(|()| destination.seek(SeekFrom::Start(0)).map(|_| ()))
            .map_err(Error::FetchOutput)?;

        let mut child = Command::new(&self.program)
            .args(self.planned_arguments(request))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| Error::io("run fetch command", &self.program, error))?;
        let stdout = child.stdout.take().ok_or_else(|| {
            terminate_child(&mut child);
            Error::FetchFailed {
                status: None,
                stderr: "fetch command did not expose stdout".into(),
            }
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            terminate_child(&mut child);
            Error::FetchFailed {
                status: None,
                stderr: "fetch command did not expose stderr".into(),
            }
        })?;
        let stderr_worker = thread::spawn(move || capture_stderr(stderr));
        let mut limited = stdout.take(request.max_bytes().saturating_add(1));
        let copied = io::copy(&mut limited, destination);
        drop(limited);
        let copied = match copied {
            Ok(copied) => copied,
            Err(error) => {
                terminate_child(&mut child);
                let _ = join_stderr(stderr_worker);
                return Err(Error::FetchOutput(error));
            }
        };

        if copied > request.max_bytes() {
            terminate_child(&mut child);
            let _ = join_stderr(stderr_worker);
            return Err(Error::FetchResponseTooLarge {
                max_bytes: request.max_bytes(),
            });
        }
        let status = match child.wait() {
            Ok(status) => status,
            Err(error) => {
                terminate_child(&mut child);
                let _ = join_stderr(stderr_worker);
                return Err(Error::io("wait for fetch command", &self.program, error));
            }
        };
        let stderr = join_stderr(stderr_worker)?;
        if !status.success() {
            return Err(Error::FetchFailed {
                status: status.code(),
                stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
            });
        }
        Ok(())
    }
}

fn capture_stderr(mut stderr: impl io::Read) -> io::Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = stderr.read(&mut buffer)?;
        if read == 0 {
            return Ok(captured);
        }
        let remaining = MAX_CAPTURED_STDERR_BYTES.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..read.min(remaining)]);
    }
}

fn join_stderr(worker: thread::JoinHandle<io::Result<Vec<u8>>>) -> Result<Vec<u8>> {
    worker
        .join()
        .map_err(|_| Error::FetchOutput(io::Error::other("fetch stderr worker panicked")))?
        .map_err(Error::FetchOutput)
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

const fn transfer_timeout_seconds(max_bytes: u64) -> u64 {
    let scaled = max_bytes.saturating_add(MIN_EXPECTED_BYTES_PER_SECOND - 1)
        / MIN_EXPECTED_BYTES_PER_SECOND
        + 30;
    if scaled < MIN_TRANSFER_TIMEOUT_SECONDS {
        MIN_TRANSFER_TIMEOUT_SECONDS
    } else if scaled > MAX_TRANSFER_TIMEOUT_SECONDS {
        MAX_TRANSFER_TIMEOUT_SECONDS
    } else {
        scaled
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn curl_plan_is_https_only_and_uses_no_shell_tokens() {
        let request = FetchRequest::new(
            HttpsUrl::parse("https://updates.example.test/manifest.json").unwrap(),
            "Scrozz/1.2.3 (linux-x86_64)",
            1_048_576,
        )
        .unwrap();
        let arguments = CurlFetcher::new().planned_arguments(&request);
        let values: Vec<&OsStr> = arguments.iter().map(OsString::as_os_str).collect();

        assert_eq!(
            values,
            [
                "--disable",
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--max-redirs",
                "5",
                "--connect-timeout",
                "10",
                "--max-time",
                "120",
                "--max-filesize",
                "1048576",
                "--user-agent",
                "Scrozz/1.2.3 (linux-x86_64)",
                "--url",
                "https://updates.example.test/manifest.json",
            ]
            .map(OsStr::new)
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == OsStr::new("sh"))
        );
    }

    #[test]
    fn user_agent_rejects_argument_injection_characters() {
        let url = HttpsUrl::parse("https://updates.example.test/file").unwrap();
        assert!(FetchRequest::new(url.clone(), "Scrozz\n--output bad", 1).is_err());
        assert!(FetchRequest::new(url.clone(), "", 1).is_err());
        assert!(FetchRequest::new(url, "Scrozz/1", 0).is_err());
    }

    #[test]
    fn artifact_timeout_scales_with_the_signed_size_and_remains_bounded() {
        let url = HttpsUrl::parse("https://updates.example.test/file").unwrap();
        let medium = FetchRequest::new(url.clone(), "Scrozz/1", 64 * 1024 * 1024).unwrap();
        let huge = FetchRequest::new(url, "Scrozz/1", u64::MAX).unwrap();

        assert!(medium.transfer_timeout_seconds() > MIN_TRANSFER_TIMEOUT_SECONDS);
        assert_eq!(
            huge.transfer_timeout_seconds(),
            MAX_TRANSFER_TIMEOUT_SECONDS
        );
    }
}
