//! Signed, crash-safe update preparation for Scrozz.
//!
//! The trust boundary is deliberately narrow:
//!
//! 1. [`Updater::check`] fetches a manifest as raw bytes.
//! 2. [`PinnedKeyRing`] verifies a detached Ed25519 signature over those exact
//!    bytes before any manifest field is deserialised.
//! 3. Only a [`VerifiedUpdate`] can be passed to [`Updater::download`], and the
//!    downloaded file must match the signed size and SHA-256 digest.
//! 4. Staging and installation are separate, explicit calls.
//!
//! **Checking and downloading never install anything.** [`Updater::install`]
//! is the only operation that starts an installation swap. Even then, the old
//! installed file is retained after success so [`Updater::rollback`] remains an
//! explicit, lossless choice.
//!
//! # Version 1 wire documents
//!
//! The signed manifest is UTF-8 JSON with this shape:
//!
//! ```text
//! {
//!   "schema": 1,
//!   "generated": 42,
//!   "version": "1.2.3",
//!   "artifacts": {
//!     "linux-x86_64": {
//!       "url": "https://updates.example.invalid/scrozz",
//!       "sha256": "<64 lowercase hexadecimal characters>",
//!       "size": 123456
//!     }
//!   }
//! }
//! ```
//!
//! Its detached signature envelope is
//! `{"schema":1,"key_id":"release-2026","signature":"<canonical base64>"}`.
//! Unknown fields and duplicate object keys are rejected. Formatting and object
//! key order remain significant because the signature covers the received
//! manifest bytes, not a reconstructed JSON value.
//!
//! This crate intentionally contains no HTTP stack, TLS implementation, native
//! library, platform API, or platform conditional. The default [`CurlFetcher`]
//! invokes `curl` directly with [`std::process::Command`] and a locked-down
//! HTTPS-only argument plan. Callers can provide another [`Fetcher`] without
//! changing the verification or state-machine code.
//!
//! Artifact unpacking, code signing, and executable permission changes remain
//! packaging responsibilities. This crate preserves permissions when copying a
//! downloaded file, but does not invent platform-specific metadata.

#![forbid(unsafe_code)]

mod artifact;
mod channel;
mod error;
mod fetch;
mod fsutil;
mod manifest;
mod state;
mod updater;

#[cfg(test)]
mod test_support;

pub use artifact::{StagedArtifact, VerifiedDownload, verify_artifact_bytes, verify_artifact_file};
pub use channel::{
    ChannelEndpointStatus, EndpointCatalog, ResolvedChannel, UpdateChannel, UpdateEndpoints,
};
pub use error::{Error, Result};
pub use fetch::{CurlFetcher, FetchRequest, Fetcher};
pub use manifest::{
    ArtifactMetadata, HttpsUrl, ManifestVerification, PinnedKey, PinnedKeyRing, Sha256Digest,
    VerifiedArtifact, VerifiedManifest, verify_manifest,
};
pub use state::{CandidateMetadata, InstallPlan, Phase, UpdateState};
pub use updater::{CheckOutcome, UpdateChecker, Updater, VerifiedUpdate};
