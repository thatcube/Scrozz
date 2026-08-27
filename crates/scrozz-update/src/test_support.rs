use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs::{self, File},
    io::{Seek as _, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use ed25519_dalek::{Signer as _, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::{Error, FetchRequest, Fetcher, PinnedKey, PinnedKeyRing, Result};

static SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) const MANIFEST_URL: &str = "https://updates.example.test/manifest.json";
pub(crate) const SIGNATURE_URL: &str = "https://updates.example.test/manifest.sig";
pub(crate) const ARTIFACT_URL: &str = "https://updates.example.test/scrozz.bin";
pub(crate) const CANDIDATE_BYTES: &[u8] = b"signed candidate executable bytes";

pub(crate) struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub(crate) fn new(label: &str) -> Self {
        let executable = std::env::current_exe().expect("test binary has a path");
        let profile = executable
            .parent()
            .and_then(Path::parent)
            .expect("test binary lives below the profile directory");
        let path = profile.join("scrozz-update-scratch").join(format!(
            "{label}-{}-{}",
            std::process::id(),
            SCRATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("scratch directory is creatable");
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

pub(crate) fn ring(keys: &[(&str, &SigningKey)]) -> PinnedKeyRing {
    PinnedKeyRing::new(
        keys.iter()
            .map(|(id, key)| PinnedKey::new(*id, key.verifying_key().to_bytes()).unwrap()),
    )
    .unwrap()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn manifest_value(
    version: &str,
    generated: u64,
    platform: &str,
    artifact_bytes: &[u8],
) -> Value {
    json!({
        "schema": 1,
        "generated": generated,
        "version": version,
        "artifacts": {
            (platform): {
                "url": ARTIFACT_URL,
                "sha256": sha256_hex(artifact_bytes),
                "size": artifact_bytes.len(),
            }
        }
    })
}

pub(crate) fn signed_envelope(manifest_bytes: &[u8], key_id: &str, key: &SigningKey) -> Vec<u8> {
    let signature = key.sign(manifest_bytes);
    serde_json::to_vec(&json!({
        "schema": 1,
        "key_id": key_id,
        "signature": BASE64_STANDARD.encode(signature.to_bytes()),
    }))
    .unwrap()
}

pub(crate) enum FakeResponse {
    Bytes(Vec<u8>),
    PartialFailure(Vec<u8>),
}

#[derive(Default)]
pub(crate) struct FakeFetcher {
    responses: RefCell<BTreeMap<String, FakeResponse>>,
}

impl FakeFetcher {
    pub(crate) fn from_responses(
        responses: impl IntoIterator<Item = (&'static str, FakeResponse)>,
    ) -> Self {
        Self {
            responses: RefCell::new(
                responses
                    .into_iter()
                    .map(|(url, response)| (url.to_owned(), response))
                    .collect(),
            ),
        }
    }
}

impl Fetcher for FakeFetcher {
    fn fetch(&self, request: &FetchRequest, destination: &mut File) -> Result<()> {
        let response = self
            .responses
            .borrow_mut()
            .remove(request.url().as_str())
            .ok_or_else(|| Error::Recovery(format!("no fake response for {}", request.url())))?;
        destination.set_len(0).map_err(Error::FetchOutput)?;
        destination.rewind().map_err(Error::FetchOutput)?;
        match response {
            FakeResponse::Bytes(bytes) => {
                if bytes.len() as u64 > request.max_bytes() {
                    return Err(Error::FetchResponseTooLarge {
                        max_bytes: request.max_bytes(),
                    });
                }
                destination.write_all(&bytes).map_err(Error::FetchOutput)
            }
            FakeResponse::PartialFailure(bytes) => {
                destination.write_all(&bytes).map_err(Error::FetchOutput)?;
                Err(Error::FetchFailed {
                    status: Some(56),
                    stderr: "fake transfer stopped".into(),
                })
            }
        }
    }
}
