# Private sharing

Scrozz has no cloud service. The `cloud` feature uploads to S3-compatible
storage you control and returns either a private presigned URL or an
already-public/custom-domain URL. Distributed binaries enable it; source builds
can still exclude every network and credential-vault dependency.

```bash
cargo run -p scrozz --features cloud -- share capture.png
```

The Cargo default remains fully functional and compiles no HTTP client. Release
packaging and `tools/make-app-bundle.sh` deliberately enable cloud:

```bash
cargo tree -p scrozz            # default build: no `ureq`, no `keyring`
cargo tree -p scrozz --features cloud
tools/make-app-bundle.sh        # distributed bundle: builds with `--features cloud`
```

## Public configuration

The platform-adaptive Settings window persists these non-secret values in the
versioned `Scrozz/settings.json` document. It preserves unknown aggregate
sections when updating a known value. Precedence is command-line flag, an
explicitly stored value, matching `SCROZZ_S3_*` variable, then provider default.
None of these values is a secret.

| Provider | Required configuration | Derived behavior |
|---|---|---|
| AWS S3 | `SCROZZ_S3_BUCKET`; region defaults to `us-east-1` | Regional endpoint; virtual-hosted buckets, with path style for dotted bucket names; object tags |
| Cloudflare R2 | bucket and `SCROZZ_S3_ACCOUNT_ID` | Account R2 endpoint; SigV4 region `auto`; lifecycle-prefix fallback because R2 does not support S3 object tags |
| Backblaze B2 | bucket and `SCROZZ_S3_REGION` such as `us-west-004` | Regional B2 S3 endpoint; lifecycle-prefix fallback because B2 rejects S3 object tags |
| MinIO | bucket and `SCROZZ_S3_ENDPOINT` | Path-style endpoint; region defaults to `us-east-1`; object tags |

Optional values are `SCROZZ_S3_PREFIX` (default `captures`),
`SCROZZ_S3_PUBLIC_BASE_URL`, `SCROZZ_SHARE_TITLE`,
`SCROZZ_SHARE_ACCENT`, and comma-separated `SCROZZ_S3_TAGS=key=value,...`.
An endpoint override is available for every preset.
S3 API endpoints must use HTTPS. Plain HTTP is accepted only for development
servers addressed by a literal loopback IP; path-style addressing preserves that
destination. Hostnames such as `localhost` are rejected so DNS cannot route a
cleartext upload elsewhere. The network transport ignores ambient proxy
variables rather than forwarding a signed upload to another host.

## Credentials

The Settings credential pane adds, updates, or removes one provider-profile
entry in macOS Keychain, Windows Credential Manager, or Linux Secret Service.
The UI names the real backend and reports missing adapters/session services
instead of claiming storage succeeded. The entry can also hold the optional
default encryption password selected by `cloud.protection-mode=vault`.

Script resolution order is fixed:

1. `SCROZZ_S3_ACCESS_KEY_ID` + `SCROZZ_S3_SECRET_ACCESS_KEY`, falling back to
   `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY`.
2. A credential command whose stdout is exactly one secret-key line. Configure
   the access-key id in the environment. The command is executed directly, not
   through a shell; CLI `--credential-arg` values are visible process arguments
   and must contain only non-secret credential-store references.
3. `--secret-key-stdin`, paired with an access-key id from the environment.
4. The selected provider's native-vault entry.

Stdin and the native vault are read only if the higher-priority sources are not
usable. Native entries are opaque binary credential bundles; they never enter
settings, history, logs, crash output, or process arguments.

Temporary `SCROZZ_S3_SESSION_TOKEN`/`AWS_SESSION_TOKEN` values are supported.
Secrets have no value-taking CLI flag, are redacted from `Debug` and request
diagnostics, and have no settings key. Non-secret settings may contain a
credential-command path; its stdout is never persisted. Credential commands
have a 30-second timeout. The HTTP client's wire-log targets remain disabled
even when `RUST_LOG=trace`, preventing signed request headers from reaching
diagnostic logs.

For example, an external credential store can still supply a secret without
exposing it in process arguments:

```bash
export SCROZZ_S3_ACCESS_KEY_ID=...
cargo run -p scrozz --features cloud -- share capture.png \
  --credential-command security \
  --credential-arg find-generic-password \
  --credential-arg -w \
  --credential-arg -s \
  --credential-arg scrozz-s3
```

## Expiry and deletion

A share is private and expires after one day by default. `--expires 30m`,
`--expires 24h`, and `--expires 7d` change the provider-enforced SigV4 GET
lifetime; seven days is the protocol maximum.

On AWS and MinIO, the PUT also carries `scrozz-expiry-days=N`. On R2 and B2,
which do not support S3 object tags, Scrozz prepends a reserved
`scrozz-expiry-Nd/` key segment instead. Command output includes bucket
lifecycle XML rule fragment(s) matching the tag or prefix. Merge each needed
duration's fragments into the bucket's existing lifecycle configuration with
the provider's normal tooling; lifecycle configuration APIs replace the whole
rule set, so do not overwrite unrelated rules. Scrozz does not request
`PutBucketLifecycleConfiguration` permission merely to perform uploads. On
versioned AWS and MinIO buckets, the generated rule also permanently removes a
noncurrent version one day after it becomes noncurrent. B2 output contains its
required paired current-expiration/delete-marker rules plus a noncurrent-version
rule for the same prefix; B2's S3 lifecycle API requires bucket versioning.
Lifecycle deletion is day-granular and is reported that way; it is separate
from exact presigned-link expiry.

`--no-expiry` returns `SCROZZ_S3_PUBLIC_BASE_URL` plus the object key, or the
provider object URL. It does **not** make a bucket public. Use it only when the
bucket or CDN already permits public GET requests.

## Password sharing

`--password-stdin` derives an AES-256-GCM key with PBKDF2-HMAC-SHA256 and a
random salt, encrypts the capture locally, and uploads one self-contained HTML
viewer. The viewer uses browser WebCrypto and contains the ciphertext; it loads
no script, image or API from Scrozz.

WebCrypto requires a secure browser context, so password shares require an HTTPS
recipient URL. Plain HTTP is accepted only for loopback development endpoints.

This is encryption, not a claimed server password check. The storage provider
never receives plaintext. Scrozz cannot revoke a password, count views, recover
a forgotten password, or enforce organization policy because there is no
server.

The password and explicit S3 secret both use stdin, so one invocation cannot
select both flags. Use environment or a credential command for S3 credentials
when creating a password-encrypted share.

## Tags, branding and the card action

Repeat `--tag KEY=VALUE` for organization on AWS or MinIO. S3 allows ten tags;
the expiry tag uses one. R2 and B2 reject custom tags with an actionable error
instead of sending an unsupported header. Tags are validated against S3's
portable Unicode letter/number/space and punctuation set before upload.
`--title` and `--accent '#RRGGBB'` brand encrypted viewers. A custom domain is a
public-base URL override and therefore applies only to non-expiring public
links; expiring private links use the signed provider host.

Generated keys include a cryptographically random suffix, so a new share cannot
silently replace the object behind an older unexpired link. Supplying `--key`
chooses an exact key and therefore opts into the provider's normal overwrite
semantics. Password viewers append `.html` unless that key already ends in
`.html`, preserving the full requested key rather than replacing its extension.

The capture-card Upload action copies cached PNG bytes to a dedicated upload
queue, so networking never runs on the capture or UI thread. It reports that the
upload was queued, reads configuration and credentials from the
environment/credential command, uploads with bounded retry and shutdown
cancellation, and copies the returned bearer link to the clipboard without
writing it to logs. A clipboard retry reuses the upload while its link remains
valid; an expired cached link causes a fresh upload and signature.

The card is enabled only when this binary contains the cloud backend and current
provider settings resolve successfully. Disabled controls expose the reason to
assistive technology; upload and connection-test failures remain visible on the
card or Settings pane. Copy and Save use separate jobs and remain available
after any upload failure.

Editor and recorder output reaches the upload worker through one
`FinalizedArtifact` seam, and the revision travelling with it is the *same*
identity the rest of the app already uses: the vault's `CaptureBytes`
generation and revision for a card capture, and the editor's `RevisionedFrame`
revision for an open document. Nothing counts revisions separately, so a cached
link can never be attributed to pixels the user has since replaced — a second
Upload after a destructive redaction re-uploads rather than handing out the old
link. Editing does not overwrite the card's own capture (decision D14): an
edited card uploads through `Job::UploadImage`, exactly as it copies and saves
through `Job::CopyImage` and `Job::SaveImage`. A recording card uploads its
durable file with the recorder's own content type through
`Job::UploadRecording`, read on the capture worker rather than the UI thread.
Successful URL, expiry, provider, object id, status, deletion state, tags, and
media kind are attached to Capture History (schema 8); credentials never are.

Objects below 16 MiB use an idempotent signed PUT. Larger objects use 8 MiB S3
multipart parts (up to Scrozz's bounded 5 GiB in-memory limit); part retries keep the same part
number and upload id, and cancellation or failure sends a signed
`AbortMultipartUpload`. Multipart creation and completion are not blindly
retried because a lost response is not safely idempotent. Configure the
provider's abort-incomplete-multipart lifecycle rule as a final backstop.

All provider responses retained for diagnostics are capped at 64 KiB. S3
endpoints require TLS except literal loopback development addresses, redirects
and ambient proxies are disabled, and resolved link-local/metadata, multicast,
broadcast, and unspecified destinations are rejected. A successful provider
`Date` response anchors the presigned expiry when available, avoiding local
clock-skew drift.

## Native credential validation

Normal CI compiles and tests the macOS Keychain, Windows Credential Manager, and
Linux Secret Service adapters on their native runners. The contract tests cover
opaque encoding, redacted diagnostics, missing entries, and unsupported builds.
A maintainer may opt into a real, self-cleaning native smoke:

```bash
SCROZZ_TEST_NATIVE_VAULT=1 cargo test -p scrozz-cloud \
  --all-features native_vault_smoke_when_explicitly_enabled
```

The smoke uses a unique temporary profile and deletes it before returning. Run
it only in a logged-in desktop session with the native vault unlocked.

## Validation boundary

Automated coverage includes published SigV4 and cryptographic vectors, an
independent WebCrypto-compatible ciphertext vector, multipart cleanup,
secret-redaction checks, and real HTTP exchanges with a loopback fake-S3 server.
Release packaging also executes a credential-free `--json share` missing-file
smoke before signing. It does not deploy to or mutate a real AWS, R2, B2, or
MinIO account.

There is no project server, Scrozz account, telemetry, runtime AI, analytics or
team-management surface in this feature.
