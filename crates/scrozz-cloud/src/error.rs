//! Errors that can cross the cloud-sharing boundary.

use std::{fmt, io};

/// A cloud configuration, credential, cryptographic or transport failure.
#[derive(Debug)]
pub enum Error {
    /// Public configuration was missing or incoherent.
    Config(String),
    /// No usable credential source was available.
    Credentials(String),
    /// Encryption or key derivation failed.
    Crypto(String),
    /// The HTTP exchange failed before a status was received.
    Transport(String),
    /// The object store rejected the request.
    HttpStatus(u16),
    /// Reading a source or credential stream failed.
    Io(io::Error),
    /// Cancellation was requested.
    Cancelled,
}

impl Error {
    /// Whether retrying the same request can reasonably succeed.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Transport(_) | Self::HttpStatus(408 | 429 | 500 | 502 | 503 | 504)
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => write!(f, "cloud configuration error: {message}"),
            Self::Credentials(message) => write!(f, "cloud credentials unavailable: {message}"),
            Self::Crypto(message) => write!(f, "share encryption failed: {message}"),
            Self::Transport(message) => write!(f, "object-store request failed: {message}"),
            Self::HttpStatus(status) => {
                write!(f, "object store rejected the request with HTTP {status}")
            }
            Self::Io(error) => write!(f, "cloud I/O failed: {error}"),
            Self::Cancelled => write!(f, "cloud operation cancelled"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<Error> for scrozz_core::Error {
    fn from(value: Error) -> Self {
        match value {
            Error::Config(message) => Self::InvalidRequest(message),
            Error::Credentials(why) => Self::Unsupported {
                what: "sharing to S3-compatible storage".to_owned(),
                why,
            },
            Error::Crypto(message) => Self::Storage(message),
            Error::Transport(message) => Self::Storage(message),
            Error::HttpStatus(status) => {
                Self::Storage(format!("object store returned HTTP {status}"))
            }
            Error::Io(error) => Self::Io(error),
            Error::Cancelled => Self::Cancelled,
        }
    }
}

/// The result type used by this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
