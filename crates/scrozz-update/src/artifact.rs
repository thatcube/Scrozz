use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

use sha2::{Digest as _, Sha256};

use crate::{Error, Result, VerifiedArtifact, fsutil::ensure_regular_file};

/// A file downloaded and checked against signed artifact metadata.
///
/// This token is created only after both the declared size and SHA-256 digest
/// match. It can be passed to [`crate::Updater::stage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDownload {
    pub(crate) path: PathBuf,
    pub(crate) artifact: VerifiedArtifact,
}

impl VerifiedDownload {
    /// Returns the verified downloaded file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the signed metadata used for verification.
    #[must_use]
    pub fn artifact(&self) -> &VerifiedArtifact {
        &self.artifact
    }
}

/// A verified artifact copied to a caller-chosen sibling staging path.
///
/// This token is created only after the staged copy is synced and reverified.
/// It can be passed to [`crate::Updater::install`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedArtifact {
    pub(crate) path: PathBuf,
    pub(crate) artifact: VerifiedArtifact,
}

impl StagedArtifact {
    /// Returns the staged file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the signed metadata used for verification.
    #[must_use]
    pub fn artifact(&self) -> &VerifiedArtifact {
        &self.artifact
    }
}

/// Checks in-memory bytes against a verified manifest artifact.
///
/// # Errors
///
/// Returns a size or digest mismatch without modifying the bytes.
pub fn verify_artifact_bytes(bytes: &[u8], artifact: &VerifiedArtifact) -> Result<()> {
    let actual_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    verify_size(actual_size, artifact)?;
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    verify_digest(&digest, artifact)
}

/// Streams and checks a regular file against a verified manifest artifact.
///
/// Directories, symlinks, and device files are rejected. The file is not
/// modified.
///
/// # Errors
///
/// Returns an I/O error, non-regular-file error, size mismatch, or digest
/// mismatch.
pub fn verify_artifact_file(path: impl AsRef<Path>, artifact: &VerifiedArtifact) -> Result<()> {
    let path = path.as_ref();
    ensure_regular_file(path)?;
    let mut file = File::open(path)
        .map_err(|error| Error::io("open artifact for verification", path, error))?;
    let (actual_size, actual_digest) = hash_reader(&mut file)
        .map_err(|error| Error::io("read artifact for verification", path, error))?;
    verify_size(actual_size, artifact)?;
    verify_digest(&actual_digest, artifact)
}

fn hash_reader(reader: &mut impl Read) -> io::Result<(u64, [u8; 32])> {
    let mut hasher = Sha256::new();
    let mut count = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
            .ok_or_else(|| io::Error::other("artifact is larger than u64::MAX bytes"))?;
        hasher.update(&buffer[..read]);
    }
    Ok((count, hasher.finalize().into()))
}

fn verify_size(actual: u64, artifact: &VerifiedArtifact) -> Result<()> {
    let expected = artifact.metadata().size();
    if actual != expected {
        return Err(Error::ArtifactSizeMismatch { expected, actual });
    }
    Ok(())
}

fn verify_digest(actual: &[u8], artifact: &VerifiedArtifact) -> Result<()> {
    let expected = artifact.metadata().sha256();
    if actual != expected.as_bytes() {
        return Err(Error::ArtifactDigestMismatch {
            expected: expected.as_hex().to_owned(),
            actual: lowercase_hex(actual),
        });
    }
    Ok(())
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        result.push(char::from(HEX[usize::from(byte >> 4)]));
        result.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    result
}
