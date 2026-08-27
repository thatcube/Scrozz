# Windows packages

`tools/package.sh` emits both Windows distributions from the same release
binary:

- a deterministic portable ZIP, which carries its own Tesseract executable,
  dependent DLLs and English trained data for OCR;
- an MSIX package, which has the package identity required by
  `Windows.Media.Ocr` and owns its startup task through `AppxManifest.xml`.

The startup task is declared with `Enabled="false"`. Installing the package
never opts the user into launch at login. To opt in, open **Windows Settings →
Apps → Startup** and turn on **Scrozz**.

## Store identity and signing

The checked-in defaults are development identity values. Before a Store build,
set the exact values assigned by Partner Center:

```text
SCROZZ_MSIX_IDENTITY_NAME
SCROZZ_MSIX_PUBLISHER
SCROZZ_MSIX_PUBLISHER_DISPLAY_NAME
SCROZZ_MSIX_VERSION
```

`SCROZZ_MSIX_VERSION` is optional for stable versions and required for
prereleases. It must satisfy Partner Center's rules: four components no greater
than 65535, a nonzero first component, and a fourth component of exactly zero.

Stable semantic versions are encoded deterministically:

```text
native major = semantic major + 1
native minor = semantic minor * 256 + semantic patch
native build = 65535
native revision = 0
```

Semantic major must be at most 65534; minor and patch must each be at most 255.
For example, application version `1.2.3` becomes `2.515.65535.0`. Release
prereleases use the same first two components, `GITHUB_RUN_NUMBER` in the range
1–65534 as native build, and zero as native revision. This keeps each
prerelease ordered and unique while reserving 65535 for the stable release.

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
`tesseract`, while MSIX means `windows-media-ocr`. `signed` records the package
container signature (and is always false for a ZIP); `payload_signed` separately
records whether the enclosed `scrozz.exe` has a valid Authenticode signature.

The portable build requires `SCROZZ_TESSERACT_DIR` to be an absolute directory
matching the checked-in `tesseract-payload.json` checksum manifest. That
manifest pins the exact Chocolatey nupkg and embedded installer, every runtime
file copied from it, and `eng.traineddata` from a full immutable
`tessdata_fast` commit URL.

```text
tesseract.exe
the complete manifest-listed runtime DLL closure
doc/
  LICENSE
tessdata/
  eng.traineddata
```

Only manifest-listed, checksum-verified files are copied to `tesseract/` beside
`scrozz.exe`. Packaging fails on a missing, changed, or unexpected runtime DLL,
if the source overlaps the output directory, or if the payload contains reparse
points. Scrozz never uses an ambient `tesseract.exe` from `PATH`.

At runtime, the portable executable uses that sibling `tesseract/` directory by
default. `SCROZZ_TESSERACT_DIR` remains an absolute override for source builds
and managed installations; an invalid override fails without falling back.

On a Windows SDK host, `powershell -NoProfile -File tools/test-windows-packaging.ps1`
runs MakeAppx against normalized inputs and checks both archive layouts,
checksums, metadata contracts, unsigned signing gate, manifest substitutions,
and byte-for-byte reproducibility.
