# WIP — private/self-hosted sharing (CLD-01..07)

**This file is a handoff note, not documentation. Delete it before the feature
lands.** It records research that was completed and verified but not yet written
as code, so the next implementer does not repeat it.

Status at checkpoint: **no code written.** The working tree was clean; this note
is the only artifact.

---

## The honest boundary, decided

Object storage cannot enforce a password, and it cannot run our code. Three of
the seven CLD rows therefore need a mechanism chosen deliberately rather than
pretended at:

- **CLD-02 expiry.** Two real mechanisms, no server:
  1. **Presigned GET URLs** — genuinely enforced by the provider, which rejects
     the request after the deadline. SigV4 caps this at 7 days.
  2. **Object deletion** via a bucket lifecycle rule keyed on an object tag we
     set at PUT time (`x-amz-tagging`). Scrozz should *generate* the lifecycle
     rule and tell the user to apply it once; applying it ourselves needs
     `PutBucketLifecycleConfiguration` permission we should not ask for.
  Anything beyond those two is a server, and we do not ship one.
- **CLD-03 password.** Do **not** claim a password check. Encrypt the capture
  client-side (AES-256-GCM, key from PBKDF2-HMAC-SHA256 over the password) and
  upload ciphertext plus a self-contained viewer page that decrypts in the
  browser via WebCrypto. This is strictly stronger than a server password gate —
  the storage provider never holds the plaintext — and it is honest, because the
  bucket really is public; only the bytes are unreadable.
- **CLD-05 custom domain/branding.** Free: a `public-base-url` override plus
  viewer title/accent. No infrastructure.

## Shape

New crate `scrozz-cloud`, implementing `scrozz_export::S3Uploader` (the seam
already exists in `crates/scrozz-export/src/destination.rs`, currently
`UnimplementedS3Uploader` with a `todo!()` that describes exactly this work).

Modules: `digest` (SHA-256/HMAC/PBKDF2), `encoding` (hex, base64, S3 URI
encoding), `sigv4`, `provider`, `config`, `credentials`, `transport`, `share`,
`bundle`, `lifecycle`, `redact`.

## Dependency decisions, already verified to compile

Checked in a scratch crate against the current toolchain (rustc 1.98):

- `aes-gcm = "0.11"` — **works**. `Aes256Gcm::new(key.into())` plus
  `encrypt/decrypt(nonce.into(), Payload { msg, aad })`.
- `hmac = "0.13"` — **API changed**: `<Hmac<Sha256> as Mac>::new_from_slice` no
  longer exists on `Mac`. Rather than chase it, hand-roll SHA-256, HMAC-SHA256
  and PBKDF2 in `scrozz-cloud::digest`. They are small, exactly specified, and
  provable against RFC 6234 / RFC 6070 vectors and the AWS SigV4 test suite —
  which leaves the whole signing path dependency-free. Only the AEAD stays a
  dependency, because hand-rolling GHASH is not defensible.
- `ureq = "3.4"` — **works**, default features `["rustls", "gzip"]`. Verified
  shape: `Agent::config_builder().timeout_global(..).build()`, `agent.put(url)`,
  `.header(k, v)`, `.send(bytes)`, `resp.status().as_u16()`,
  `resp.body_mut().read_to_string()`, and `Err(ureq::Error::StatusCode(code))`
  for non-2xx.

**Put the HTTP client behind a non-default `network` feature** on
`scrozz-cloud`. `AI_DISCLOSURE.md` currently promises the reader they can search
`Cargo.lock` for `ureq` and find nothing; that promise must be rewritten
truthfully rather than quietly broken. The rewrite should say: the default build
contains no HTTP client at all, the app's optional `cloud` feature adds one, the
only request it can make is to the endpoint the user configured, and there is no
default endpoint. Both halves stay verifiable.

## Credentials — no `keyring` dependency

`keyring` drags dbus/secret-service onto Linux. Instead resolve in order:
explicit → environment (`SCROZZ_S3_*`, falling back to `AWS_*`) → a
user-specified **credential command** whose stdout is the secret. That is the
integration with platform stores (`security find-generic-password -w` on macOS,
`secret-tool lookup` on Linux, any password manager anywhere) at zero
dependency cost. Never accept a secret on argv; offer `--secret-key-stdin`.
Non-secret config (endpoint, region, bucket, prefix, public base URL) may live
in a config file; a test must assert secrets never reach it, and `Debug` for the
credential type must be redacted.

Note that `scrozz settings set` does not persist yet, so the settings schema
keys are a contract for later; environment and flags are what work today.

## Wiring points, located

- `crates/scrozz-ui/src/card.rs:208` — `CardAction::Upload` already exists.
- `crates/scrozz-ui/src/overlay_app.rs:993` — already raises
  `OverlayEvent::UploadRequested`.
- `apps/scrozz/src/gui/overlay.rs:191` — currently **drops** that event; needs a
  `CardEvent::Upload` arm.
- `apps/scrozz/src/gui/card.rs:292` — add `CardEvent::Upload`.
- `apps/scrozz/src/gui/pipeline.rs:48` — add `Job::Upload`; the worker already
  caches encoded PNG bytes per card, which is exactly what an upload needs.
- `apps/scrozz/src/gui/app.rs:414` — route it.
- `apps/scrozz/src/cli.rs` / `commands.rs` — add a `share` subcommand, remembering
  that the CLI is a stable API (D11) and `--json` output is a contract.

## Tests still owed

Fake S3 over loopback TCP only (hand-written HTTP/1.1 responder in the test — no
external network, no new dependency), AWS SigV4 known-answer vectors, S3 URI
encoding edge cases, retry/backoff and cancellation, and redaction tests proving
no secret reaches a log line or a config file.
