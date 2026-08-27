# Private sharing

Scrozz has no cloud service. The optional `cloud` feature uploads to
S3-compatible storage you control and returns either a private presigned URL or
an already-public/custom-domain URL.

```bash
cargo run -p scrozz --features cloud -- share capture.png
```

The default build remains fully functional and compiles no HTTP client. The
feature is a deployment choice:

```bash
cargo tree -p scrozz --no-default-features
cargo tree -p scrozz --features cloud
```

## Public configuration

Command-line flags override the matching `SCROZZ_S3_*` variables. None of these
values is a secret.

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
S3 API endpoints must use HTTPS. Plain HTTP is accepted only for loopback
development servers so credentials, signatures, and unencrypted captures cannot
cross a network in cleartext. The network transport ignores ambient proxy
variables rather than forwarding a signed upload to another host.

## Credentials

Resolution order is fixed:

1. `SCROZZ_S3_ACCESS_KEY_ID` + `SCROZZ_S3_SECRET_ACCESS_KEY`, falling back to
   `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY`.
2. A credential command whose stdout is exactly one secret-key line. Configure
   the access-key id in the environment. The command is executed directly, not
   through a shell; CLI `--credential-arg` values are visible process arguments
   and must contain only non-secret credential-store references.
3. `--secret-key-stdin`, paired with an access-key id from the environment. Stdin
   is read only if neither higher-priority source is usable.

Temporary `SCROZZ_S3_SESSION_TOKEN`/`AWS_SESSION_TOKEN` values are supported.
Secrets have no value-taking CLI flag, are redacted from `Debug` and request
diagnostics, and have no settings key. Non-secret settings may contain a
credential-command path; its stdout is never persisted. Credential commands
have a 30-second timeout.

For example, a platform credential store can supply the secret without exposing
it in process arguments:

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

## Validation boundary

Automated coverage includes published SigV4 and cryptographic vectors, an
independent WebCrypto-compatible ciphertext vector, secret-redaction checks, and
real HTTP exchanges with a loopback fake-S3 server. It does not deploy to or
mutate a real AWS, R2, B2, or MinIO account.

There is no project server, Scrozz account, telemetry, runtime AI, analytics or
team-management surface in this feature.
