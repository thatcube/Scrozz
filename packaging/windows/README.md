# Windows packages

`tools/package.sh` emits both Windows distributions from the same release
binary:

- a deterministic portable ZIP, which uses a locally installed Tesseract for
  OCR and keeps per-user registry registration;
- an MSIX package, which has the package identity required by
  `Windows.Media.Ocr` and owns its protocol and startup task through
  `AppxManifest.xml`.

The startup task is declared with `Enabled="false"`. Installing the package
never opts the user into launch at login; `scrozz autostart enable` requests it
through `Windows.ApplicationModel.StartupTask`.

## Store identity and signing

The checked-in defaults are development identity values. Before a Store build,
set the exact values assigned by Partner Center:

```text
SCROZZ_MSIX_IDENTITY_NAME
SCROZZ_MSIX_PUBLISHER
SCROZZ_MSIX_PUBLISHER_DISPLAY_NAME
SCROZZ_MSIX_VERSION
```

`SCROZZ_MSIX_VERSION` is optional and must have four numeric components. Without
it, application version `1.2.3` becomes package version `1.2.3.0`.

Signing is deliberately inert unless a human-approved environment supplies one
of:

```text
SCROZZ_MSIX_SIGN_PFX
SCROZZ_MSIX_SIGN_PFX_PASSWORD
```

or:

```text
SCROZZ_MSIX_SIGN_CERT_SHA1
```

The certificate subject must match `SCROZZ_MSIX_PUBLISHER`. `SCROZZ_SIGNTOOL`,
`SCROZZ_MAKEAPPX`, and `SCROZZ_MSIX_TIMESTAMP_URL` can override SDK tool and
timestamp locations. No certificate, password, Store identity, or private key
belongs in the repository.

Set `SCROZZ_WINDOWS_VERIFY_DETERMINISM=1` to package the normalized staging
trees twice and require byte-identical portable ZIP and unsigned MSIX output
before signing. The legacy `SCROZZ_MSIX_VERIFY_DETERMINISM` name remains
accepted.

Each artifact has an adjacent `.artifact.json` file. Its `package_kind` and
`ocr_backend` fields make the distribution contract explicit: portable means
`tesseract`, while MSIX means `windows-media-ocr`.

On a Windows SDK host, `powershell -NoProfile -File tools/test-windows-packaging.ps1`
runs MakeAppx against normalized inputs and checks both archive layouts,
checksums, metadata contracts, unsigned signing gate, manifest substitutions,
and byte-for-byte reproducibility.
