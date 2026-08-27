use std::{io, path::PathBuf};

use semver::Version;
use thiserror::Error;

use crate::Phase;

/// Errors produced while checking, downloading, staging, or swapping an update.
#[derive(Debug, Error)]
pub enum Error {
    /// A filesystem or subprocess I/O operation failed.
    #[error("{operation} `{path}`: {source}")]
    Io {
        /// The operation that failed.
        operation: &'static str,
        /// The relevant path or executable.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// A JSON document could not be decoded.
    #[error("invalid {document} JSON: {source}")]
    Json {
        /// The kind of document being decoded.
        document: &'static str,
        /// The JSON parser error.
        #[source]
        source: serde_json::Error,
    },

    /// An update URL was not an acceptable HTTPS URL.
    #[error("invalid HTTPS URL: {0}")]
    InvalidUrl(String),

    /// A key identifier was empty or contained unsafe characters.
    #[error("invalid signing key id `{0}`")]
    InvalidKeyId(String),

    /// Two pinned keys used the same identifier.
    #[error("duplicate pinned signing key id `{0}`")]
    DuplicateKeyId(String),

    /// A pinned Ed25519 public key was not a valid compressed point.
    #[error("invalid Ed25519 public key for `{0}`")]
    InvalidPublicKey(String),

    /// No trusted verification key has been deliberately configured.
    #[error("no update signing keys are pinned")]
    NoPinnedKeys,

    /// The signature envelope selected a key that is not pinned.
    #[error("signature references unknown key id `{0}`")]
    UnknownKeyId(String),

    /// The signature was not canonical base64 containing exactly 64 bytes.
    #[error("invalid detached signature encoding")]
    InvalidSignatureEncoding,

    /// The detached signature did not verify over the exact manifest bytes.
    #[error("detached manifest signature is invalid")]
    BadSignature,

    /// The signature envelope uses a schema this crate does not implement.
    #[error("unsupported signature envelope schema {0}")]
    UnsupportedSignatureSchema(u32),

    /// The update manifest uses a schema this crate does not implement.
    #[error("unsupported update manifest schema {0}")]
    UnsupportedManifestSchema(u32),

    /// A signed manifest field violated the manifest contract.
    #[error("invalid update manifest: {0}")]
    InvalidManifest(String),

    /// A signed candidate version is older than the installed version.
    #[error("candidate version {candidate} is older than installed version {installed}")]
    VersionRollback {
        /// The signed candidate version.
        candidate: Version,
        /// The installed version supplied by the caller.
        installed: Version,
    },

    /// A signed generation was already accepted.
    #[error(
        "manifest generation {candidate} is not newer than accepted generation {highest_accepted}"
    )]
    GenerationReplay {
        /// The signed candidate generation.
        candidate: u64,
        /// The highest generation already accepted by this installation.
        highest_accepted: u64,
    },

    /// The bytes did not have the signed artifact size.
    #[error("artifact size mismatch: expected {expected} bytes, found {actual}")]
    ArtifactSizeMismatch {
        /// The size declared by the signed manifest.
        expected: u64,
        /// The size observed while hashing.
        actual: u64,
    },

    /// The bytes did not have the signed artifact digest.
    #[error("artifact SHA-256 mismatch: expected {expected}, found {actual}")]
    ArtifactDigestMismatch {
        /// The lowercase digest declared by the signed manifest.
        expected: String,
        /// The lowercase digest computed from the artifact.
        actual: String,
    },

    /// The fetch subprocess exited unsuccessfully.
    #[error("fetch command failed with status {status:?}: {stderr}")]
    FetchFailed {
        /// The numeric exit status, or `None` if the process was terminated.
        status: Option<i32>,
        /// Lossily decoded diagnostic output from the fetch process.
        stderr: String,
    },

    /// A user-agent string could have changed the subprocess argument meaning.
    #[error("invalid fetch user-agent")]
    InvalidUserAgent,

    /// A response byte ceiling was zero.
    #[error("fetch response limit must be greater than zero")]
    InvalidFetchLimit,

    /// A response exceeded its request-specific byte ceiling.
    #[error("fetch response exceeded the {max_bytes}-byte limit")]
    FetchResponseTooLarge {
        /// The maximum response size accepted by the request.
        max_bytes: u64,
    },

    /// Bytes from a successful transport could not be written to the held file.
    #[error("could not write fetched response: {0}")]
    FetchOutput(#[source] io::Error),

    /// The requested phase transition is not part of the update state machine.
    #[error("invalid update transition from {from:?} to {to:?}")]
    InvalidTransition {
        /// The current phase.
        from: Phase,
        /// The requested phase.
        to: Phase,
    },

    /// A path that must be a regular file was a directory.
    #[error("directory updates are not supported: `{0}`")]
    DirectoryUnsupported(PathBuf),

    /// A path was a symlink, device, or another non-regular file.
    #[error("update path is not a regular file: `{0}`")]
    NotRegularFile(PathBuf),

    /// An output path already existed and was therefore not replaced.
    #[error("refusing to replace existing update path `{0}`")]
    DestinationExists(PathBuf),

    /// Atomic rename paths were not all in one directory.
    #[error("update paths must be distinct siblings in one directory")]
    PathsNotSiblings,

    /// A token from another update lifecycle was supplied.
    #[error("verified update token does not match the persisted candidate")]
    VerifiedUpdateMismatch,

    /// The persisted state schema is newer or otherwise unsupported.
    #[error("unsupported update state schema {0}")]
    UnsupportedStateSchema(u32),

    /// The persisted state is internally inconsistent.
    #[error("invalid persisted update state: {0}")]
    InvalidState(String),

    /// Another process or updater instance holds the durable-state lock.
    #[error("update state is already in use at `{0}`")]
    StateLocked(PathBuf),

    /// Rollback was requested without a retained previous installation.
    #[error("no retained previous installation is available to restore")]
    NoPreviousInstallation,

    /// A recovery layout could not be reconciled without risking the only copy.
    #[error("update recovery stopped safely: {0}")]
    Recovery(String),
}

impl Error {
    pub(crate) fn io(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }

    pub(crate) fn json(document: &'static str, source: serde_json::Error) -> Self {
        Self::Json { document, source }
    }
}

/// Result type used throughout the update crate.
pub type Result<T> = std::result::Result<T, Error>;
