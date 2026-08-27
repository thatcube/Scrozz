# Windows packages

`tools/package.sh` emits both Windows distributions from the same release
binary:

- a deterministic portable ZIP, which carries its own Tesseract executable,
  dependent DLLs and English trained data for OCR;
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

## Portable OCR payload

`SCROZZ_TESSERACT_DIR` must name an absolute directory containing
`tesseract.exe`, its adjacent dependent DLLs, and
`tessdata/eng.traineddata`. Packaging copies that payload into
`tesseract/` beside `scrozz.exe`; the portable runtime never searches `PATH`.
The MSIX does not contain this payload because package identity selects
`Windows.Media.Ocr`.

The release workflow obtains that directory from the human-approved
`release-signing` environment. `WINDOWS_TESSERACT_ARCHIVE_URL` and
`WINDOWS_TESSERACT_ARCHIVE_SHA256` must identify a ZIP and its pinned digest.
The workflow rejects non-HTTPS downloads, hash mismatches, path traversal,
reparse points, oversized expansion, and ambiguous Tesseract roots before
packaging.

Set `SCROZZ_WINDOWS_VERIFY_DETERMINISM=1` to package the normalized staging
trees twice and require byte-identical portable ZIP and unsigned MSIX output
before signing. The legacy `SCROZZ_MSIX_VERIFY_DETERMINISM` name remains
accepted.

Each artifact has an adjacent `.artifact.json` file. Its `package_kind` and
`ocr_backend` fields make the distribution contract explicit: portable means
`tesseract`, while MSIX means `windows-media-ocr`.

Structural packaging cannot infer a native executable's complete DLL import
closure, so `tools/windows-smoke.ps1` also starts the staged executable with
ambient `PATH` removed and performs a real recognition through Scrozz. That
native smoke is the executable proof that the supplied payload is complete.

On a Windows SDK host, `powershell -NoProfile -File tools/test-windows-packaging.ps1`
runs MakeAppx against normalized inputs and checks both archive layouts,
checksums, metadata contracts, unsigned signing gate, manifest substitutions,
and byte-for-byte reproducibility.
