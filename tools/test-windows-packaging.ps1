[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$Root = Join-Path ([IO.Path]::GetTempPath()) (
    "scrozz-windows-package-test-{0}" -f [Guid]::NewGuid().ToString("N")
)
$Output = Join-Path $Root "artifacts"
$PrereleaseOutput = Join-Path $Root "prerelease-artifacts"
$BuildMetadataOutput = Join-Path $Root "build-metadata-artifacts"
$StoreOutput = Join-Path $Root "store-artifacts"
$Binary = Join-Path $Root "scrozz.exe"
$Tesseract = Join-Path $Root "tesseract-payload"
$PayloadManifest = Join-Path $Root "tesseract-payload.test.json"
$EnvironmentNames = @(
    "SCROZZ_WINDOWS_VERIFY_DETERMINISM",
    "SCROZZ_MSIX_VERIFY_DETERMINISM",
    "SCROZZ_MSIX_IDENTITY_NAME",
    "SCROZZ_MSIX_IDENTITY_MODE",
    "SCROZZ_MSIX_PUBLISHER",
    "SCROZZ_MSIX_PUBLISHER_DISPLAY_NAME",
    "SCROZZ_MSIX_VERSION",
    "SCROZZ_TESSERACT_DIR",
    "SCROZZ_MSIX_SIGN_PFX",
    "SCROZZ_MSIX_SIGN_PFX_PASSWORD",
    "SCROZZ_MSIX_SIGN_CERT_SHA1",
    "SCROZZ_MSIX_TIMESTAMP_URL",
    "SCROZZ_ALLOW_UNTIMESTAMPED_SIGNING",
    "SCROZZ_TEST_ALLOW_UNTRUSTED_SIGNATURE"
)
$SavedEnvironment = @{}
$TestCertificateThumbprint = $null

function Assert-Equal {
    param($Actual, $Expected, [string] $Description)
    if ($Actual -ne $Expected) {
        throw "$Description expected [$Expected], found [$Actual]"
    }
}

function Assert-ArchiveEntry {
    param([string[]] $Entries, [string] $Expected, [string] $Description)
    if ($Entries -notcontains $Expected) {
        throw "$Description is missing archive entry $Expected"
    }
}

function Get-ArchiveEntryNames {
    param([string] $Path)
    $Archive = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        return @($Archive.Entries | ForEach-Object { $_.FullName.Replace("\", "/") })
    } finally {
        $Archive.Dispose()
    }
}

function Read-ArchiveText {
    param([string] $Path, [string] $EntryName)
    $Archive = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $Entry = $Archive.GetEntry($EntryName)
        if ($null -eq $Entry) {
            throw "$Path is missing archive entry $EntryName"
        }
        $Stream = $Entry.Open()
        $Reader = [IO.StreamReader]::new($Stream, [Text.Encoding]::UTF8, $true)
        try {
            return $Reader.ReadToEnd()
        } finally {
            $Reader.Dispose()
            $Stream.Dispose()
        }
    } finally {
        $Archive.Dispose()
    }
}

function Assert-ArchiveTimestamps {
    param([string] $Path, [string] $Description)
    $Archive = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        foreach ($Entry in $Archive.Entries) {
            $Timestamp = $Entry.LastWriteTime
            if ($Timestamp.Year -ne 1980 -or
                $Timestamp.Month -ne 1 -or
                $Timestamp.Day -ne 1 -or
                $Timestamp.Hour -ne 0 -or
                $Timestamp.Minute -ne 0 -or
                $Timestamp.Second -ne 0) {
                throw (
                    "$Description entry $($Entry.FullName) has non-canonical " +
                    "timestamp $Timestamp"
                )
            }
        }
    } finally {
        $Archive.Dispose()
    }
}

function New-UnsignedPeFixture {
    param([string] $Destination)

    $Source = if (Test-Path -LiteralPath (Join-Path $PSHOME "pwsh.exe")) {
        Join-Path $PSHOME "pwsh.exe"
    } else {
        Join-Path $PSHOME "powershell.exe"
    }
    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
        throw "The running PowerShell executable was not found: $Source"
    }
    Copy-Item -LiteralPath $Source -Destination $Destination -Force

    [byte[]] $Bytes = [IO.File]::ReadAllBytes($Destination)
    if ($Bytes.Length -lt 512 -or $Bytes[0] -ne 0x4d -or $Bytes[1] -ne 0x5a) {
        throw "The unsigned fixture source is not a real PE image"
    }
    $PeOffset = [BitConverter]::ToInt32($Bytes, 0x3c)
    if ($PeOffset -lt 0 -or $PeOffset + 256 -gt $Bytes.Length -or
        $Bytes[$PeOffset] -ne 0x50 -or
        $Bytes[$PeOffset + 1] -ne 0x45 -or
        $Bytes[$PeOffset + 2] -ne 0 -or
        $Bytes[$PeOffset + 3] -ne 0) {
        throw "The unsigned fixture source has an invalid PE header"
    }

    # A catalog signature follows the content hash to a copy. Change the COFF
    # TimeDateStamp, which the Windows loader ignores but Authenticode hashes,
    # so the temporary PE becomes independent without changing executable code,
    # section layout, architecture, or entry point.
    $TimestampOffset = $PeOffset + 8
    $Timestamp = [BitConverter]::ToUInt32($Bytes, $TimestampOffset)
    $MutatedTimestamp = [UInt32] ($Timestamp -bxor 1)
    [BitConverter]::GetBytes($MutatedTimestamp).CopyTo($Bytes, $TimestampOffset)

    $OptionalHeader = $PeOffset + 24
    $Magic = [BitConverter]::ToUInt16($Bytes, $OptionalHeader)
    $DataDirectories = if ($Magic -eq 0x20b) {
        $OptionalHeader + 112
    } elseif ($Magic -eq 0x10b) {
        $OptionalHeader + 96
    } else {
        throw "The unsigned fixture source has unsupported PE magic 0x$($Magic.ToString('x'))"
    }
    # IMAGE_DIRECTORY_ENTRY_SECURITY is data-directory entry 4. Its address is
    # a file offset, not an RVA, and Authenticode stores the certificate at EOF.
    $SecurityDirectory = $DataDirectories + (4 * 8)
    $CertificateOffset = [BitConverter]::ToUInt32($Bytes, $SecurityDirectory)
    $CertificateSize = [BitConverter]::ToUInt32($Bytes, $SecurityDirectory + 4)
    if ($CertificateOffset -eq 0 -and $CertificateSize -eq 0) {
        # Windows PowerShell is catalog-signed on some Windows images. Persist
        # the safe COFF mutation above to invalidate that catalog hash.
        [IO.File]::WriteAllBytes($Destination, $Bytes)
        $CopiedSignature = Get-AuthenticodeSignature -FilePath $Destination
        if ($CopiedSignature.Status -eq "NotSigned") {
            return
        }
        throw (
            "The catalog-signed fixture copy retained unexpected Authenticode " +
            "status $($CopiedSignature.Status)"
        )
    }
    $CertificateEnd = [UInt64] $CertificateOffset + [UInt64] $CertificateSize
    if ($CertificateOffset -eq 0 -or $CertificateSize -eq 0 -or
        $CertificateEnd -gt $Bytes.Length) {
        throw "The fixture source has no removable Authenticode certificate"
    }
    for ($Index = 0; $Index -lt 8; $Index++) {
        $Bytes[$SecurityDirectory + $Index] = 0
    }

    if ($CertificateEnd -eq $Bytes.Length) {
        [byte[]] $Unsigned = [byte[]]::new([int] $CertificateOffset)
        [Array]::Copy($Bytes, $Unsigned, [int] $CertificateOffset)
        $Bytes = $Unsigned
    }
    [IO.File]::WriteAllBytes($Destination, $Bytes)

    $Signature = Get-AuthenticodeSignature -FilePath $Destination
    if ($Signature.Status -ne "NotSigned") {
        throw "The fresh PE fixture still has Authenticode status $($Signature.Status)"
    }
}

function Test-ArtifactMetadata {
    param(
        [string] $Artifact,
        [string] $PackageKind,
        [string] $OcrBackend,
        [string] $NativePackageVersion,
        [string] $PackageIdentity,
        [string] $PackageIdentityMode,
        [bool] $Signed = $false,
        [bool] $PayloadSigned = $false,
        [bool] $TestSignature = $false
    )
    $MetadataPath = "$Artifact.artifact.json"
    $HashPath = "$Artifact.sha256"
    if (-not (Test-Path -LiteralPath $MetadataPath -PathType Leaf)) {
        throw "Missing artifact metadata: $MetadataPath"
    }
    if (-not (Test-Path -LiteralPath $HashPath -PathType Leaf)) {
        throw "Missing artifact checksum: $HashPath"
    }

    $Metadata = Get-Content -LiteralPath $MetadataPath -Raw | ConvertFrom-Json
    $Hash = (Get-FileHash -LiteralPath $Artifact -Algorithm SHA256).Hash.ToLowerInvariant()
    $Length = (Get-Item -LiteralPath $Artifact).Length
    Assert-Equal $Metadata.schema 1 "metadata schema"
    Assert-Equal $Metadata.file ([IO.Path]::GetFileName($Artifact)) "metadata filename"
    Assert-Equal $Metadata.sha256 $Hash "metadata hash"
    Assert-Equal $Metadata.size $Length "metadata length"
    Assert-Equal $Metadata.package_kind $PackageKind "metadata package kind"
    Assert-Equal (
        $Metadata.native_package_version
    ) $NativePackageVersion "metadata native package version"
    Assert-Equal $Metadata.ocr_backend $OcrBackend "metadata OCR backend"
    Assert-Equal $Metadata.package_identity $PackageIdentity "metadata package identity"
    Assert-Equal (
        $Metadata.package_identity_mode
    ) $PackageIdentityMode "metadata package identity mode"
    Assert-Equal $Metadata.test_signature $TestSignature "metadata test signature state"
    Assert-Equal $Metadata.signed $Signed "metadata signed state"
    Assert-Equal $Metadata.payload_signed $PayloadSigned "metadata payload signing state"
    Assert-Equal $Metadata.signed_manifest $false "metadata update-manifest state"
    Assert-Equal (
        [IO.File]::ReadAllText($HashPath).Trim()
    ) "$Hash  $([IO.Path]::GetFileName($Artifact))" "checksum sidecar"
}

function Invoke-TestPackager {
    param(
        [string] $PackageOutput,
        [string] $ApplicationVersion,
        [string] $PackageStamp
    )
    & (Join-Path $RepoRoot "tools\package-windows.ps1") `
        -OutputDirectory $PackageOutput `
        -Binary $Binary `
        -Version $ApplicationVersion `
        -Stamp $PackageStamp `
        -Architecture "x86_64" `
        -TesseractPayloadManifest $PayloadManifest
}

function Assert-MsixVersionRejected {
    param([string] $Candidate, [string] $ExpectedMessage)
    [Environment]::SetEnvironmentVariable("SCROZZ_MSIX_VERSION", $Candidate)
    $Rejected = $false
    try {
        Invoke-TestPackager $Output "1.2.3" "version-rejection-test"
    } catch {
        if ($_.Exception.Message -notmatch $ExpectedMessage) {
            throw
        }
        $Rejected = $true
    } finally {
        [Environment]::SetEnvironmentVariable("SCROZZ_MSIX_VERSION", $null)
    }
    if (-not $Rejected) {
        throw "Windows packaging accepted invalid MSIX version $Candidate"
    }
}

New-Item -ItemType Directory -Path $Output -Force | Out-Null
Add-Type -AssemblyName System.IO.Compression.FileSystem

try {
    foreach ($Name in $EnvironmentNames) {
        $SavedEnvironment[$Name] = [Environment]::GetEnvironmentVariable($Name)
        [Environment]::SetEnvironmentVariable($Name, $null)
    }
    [Environment]::SetEnvironmentVariable("SCROZZ_WINDOWS_VERIFY_DETERMINISM", "1")

    # MakeAppx validates package structure and manifest references, but does not
    # execute the payload. Minimal MZ markers keep this test independent of a
    # release build while exercising the exact production packaging path.
    [IO.File]::WriteAllBytes($Binary, [byte[]] @(0x4d, 0x5a))
    New-Item -ItemType Directory -Path (Join-Path $Tesseract "tessdata") -Force |
        Out-Null
    $FixtureFiles = [ordered]@{
        "tesseract.exe" = [byte[]] @(0x4d, 0x5a)
        "doc/LICENSE" = [Text.Encoding]::ASCII.GetBytes("license")
        "libtesseract-5.dll" = [byte[]] @(0x4d, 0x5a, 0x01)
        "libleptonica-6.dll" = [byte[]] @(0x4d, 0x5a, 0x02)
        "tessdata/eng.traineddata" = [Text.Encoding]::ASCII.GetBytes("fixture")
    }
    $FixtureEntries = @(
        foreach ($Entry in $FixtureFiles.GetEnumerator()) {
            $NativePath = $Entry.Key.Replace(
                "/", [IO.Path]::DirectorySeparatorChar
            )
            $Path = Join-Path $Tesseract $NativePath
            New-Item `
                -ItemType Directory `
                -Path ([IO.Path]::GetDirectoryName($Path)) `
                -Force | Out-Null
            [IO.File]::WriteAllBytes($Path, $Entry.Value)
            [ordered]@{
                path = $Entry.Key
                sha256 = (
                    Get-FileHash -LiteralPath $Path -Algorithm SHA256
                ).Hash.ToLowerInvariant()
            }
        }
    )
    [IO.File]::WriteAllText(
        $PayloadManifest,
        (([ordered]@{
            schema = 1
            payload_files = $FixtureEntries
        } | ConvertTo-Json -Depth 4) + "`n"),
        [Text.UTF8Encoding]::new($false)
    )
    [Environment]::SetEnvironmentVariable("SCROZZ_TESSERACT_DIR", $Tesseract)

    [Environment]::SetEnvironmentVariable(
        "SCROZZ_TEST_ALLOW_UNTRUSTED_SIGNATURE",
        "1"
    )
    [Environment]::SetEnvironmentVariable("SCROZZ_MSIX_IDENTITY_MODE", "store")
    $RejectedNonTestStamp = $false
    try {
        Invoke-TestPackager $Output "1.2.3" "latest"
    } catch {
        if ($_.Exception.Message -notmatch "test-labelled") {
            throw
        }
        $RejectedNonTestStamp = $true
    }
    if (-not $RejectedNonTestStamp) {
        throw "The untrusted-signature gate accepted a non-test stamp"
    }

    $ProductionManifestAlias = Join-Path $Root "production-manifest-alias.json"
    $ProductionManifest = Get-Content `
        -LiteralPath (Join-Path $RepoRoot "packaging\windows\tesseract-payload.json") `
        -Raw | ConvertFrom-Json
    [IO.File]::WriteAllText(
        $ProductionManifestAlias,
        (($ProductionManifest | ConvertTo-Json -Depth 8 -Compress) + "`n"),
        [Text.UTF8Encoding]::new($false)
    )
    $RejectedProductionManifest = $false
    try {
        & (Join-Path $RepoRoot "tools\package-windows.ps1") `
            -OutputDirectory $Output `
            -Binary $Binary `
            -Version "1.2.3" `
            -Stamp "store-test" `
            -Architecture "x86_64" `
            -TesseractPayloadManifest $ProductionManifestAlias
    } catch {
        if ($_.Exception.Message -notmatch "non-production payload manifest") {
            throw
        }
        $RejectedProductionManifest = $true
    }
    if (-not $RejectedProductionManifest) {
        throw "The untrusted-signature gate accepted the production payload manifest"
    }

    [Environment]::SetEnvironmentVariable("SCROZZ_MSIX_IDENTITY_MODE", "development")
    $RejectedDevelopmentMode = $false
    try {
        Invoke-TestPackager $Output "1.2.3" "development-test"
    } catch {
        if ($_.Exception.Message -notmatch "only in store identity mode") {
            throw
        }
        $RejectedDevelopmentMode = $true
    }
    if (-not $RejectedDevelopmentMode) {
        throw "Development identity mode enabled the untrusted-signature gate"
    }
    [Environment]::SetEnvironmentVariable(
        "SCROZZ_TEST_ALLOW_UNTRUSTED_SIGNATURE",
        $null
    )
    [Environment]::SetEnvironmentVariable("SCROZZ_MSIX_IDENTITY_MODE", $null)

    $RejectedImplicitIdentity = $false
    try {
        Invoke-TestPackager $Output "1.2.3" "identity-rejection-test"
    } catch {
        if ($_.Exception.Message -notmatch "SCROZZ_MSIX_IDENTITY_MODE") {
            throw
        }
        $RejectedImplicitIdentity = $true
    }
    if (-not $RejectedImplicitIdentity) {
        throw "Windows packaging silently selected a development package identity"
    }
    [Environment]::SetEnvironmentVariable(
        "SCROZZ_MSIX_IDENTITY_MODE",
        "development"
    )
    [Environment]::SetEnvironmentVariable(
        "SCROZZ_MSIX_IDENTITY_NAME",
        "Store.Identity.Must.Not.Leak"
    )
    $RejectedDevelopmentOverride = $false
    try {
        Invoke-TestPackager $Output "1.2.3" "development-override-test"
    } catch {
        if ($_.Exception.Message -notmatch "rejects Store identity") {
            throw
        }
        $RejectedDevelopmentOverride = $true
    } finally {
        [Environment]::SetEnvironmentVariable("SCROZZ_MSIX_IDENTITY_NAME", $null)
    }
    if (-not $RejectedDevelopmentOverride) {
        throw "Development packaging retained a Store identity override"
    }

    $RejectedFourPartApplicationVersion = $false
    try {
        Invoke-TestPackager $Output "1.2.3.4" "rejection-test"
    } catch {
        if ($_.Exception.Message -notmatch "Invalid application version") {
            throw
        }
        $RejectedFourPartApplicationVersion = $true
    }
    if (-not $RejectedFourPartApplicationVersion) {
        throw "Windows packaging accepted a four-part application version"
    }

    $RejectedLeadingZeroPrerelease = $false
    try {
        Invoke-TestPackager $Output "1.2.3-01" "leading-zero-rejection-test"
    } catch {
        if ($_.Exception.Message -notmatch "leading zeroes") {
            throw
        }
        $RejectedLeadingZeroPrerelease = $true
    }
    if (-not $RejectedLeadingZeroPrerelease) {
        throw "Windows packaging accepted invalid numeric prerelease identifier 01"
    }

    $RejectedPrereleaseVersion = $false
    try {
        Invoke-TestPackager $Output "1.2.3-beta.1" "rejection-test"
    } catch {
        if ($_.Exception.Message -notmatch "SCROZZ_MSIX_VERSION") {
            throw
        }
        $RejectedPrereleaseVersion = $true
    }
    if (-not $RejectedPrereleaseVersion) {
        throw "Windows packaging collapsed a prerelease onto its stable MSIX version"
    }

    $MutatedDll = Join-Path $Tesseract "libtesseract-5.dll"
    [IO.File]::WriteAllBytes($MutatedDll, [byte[]] @(0x4d, 0x5a, 0xff))
    $RejectedChecksum = $false
    try {
        Invoke-TestPackager $Output "1.2.3" "checksum-rejection-test"
    } catch {
        if ($_.Exception.Message -notmatch "Checksum mismatch") {
            throw
        }
        $RejectedChecksum = $true
    } finally {
        [IO.File]::WriteAllBytes(
            $MutatedDll,
            $FixtureFiles["libtesseract-5.dll"]
        )
    }
    if (-not $RejectedChecksum) {
        throw "Windows packaging accepted a changed Tesseract runtime DLL"
    }

    $UnexpectedDll = Join-Path $Tesseract "unexpected.dll"
    [IO.File]::WriteAllBytes($UnexpectedDll, [byte[]] @(0x4d, 0x5a))
    $RejectedDllClosure = $false
    try {
        Invoke-TestPackager $Output "1.2.3" "closure-rejection-test"
    } catch {
        if ($_.Exception.Message -notmatch "runtime DLL closure") {
            throw
        }
        $RejectedDllClosure = $true
    } finally {
        Remove-Item -LiteralPath $UnexpectedDll -Force
    }
    if (-not $RejectedDllClosure) {
        throw "Windows packaging accepted an unexpected runtime DLL"
    }

    Assert-MsixVersionRejected "0.515.42.0" "first MSIX version component"
    Assert-MsixVersionRejected "2.515.42.1" "fourth MSIX version component"

    [Environment]::SetEnvironmentVariable(
        "SCROZZ_TESSERACT_DIR",
        (Join-Path $Root "missing-tesseract")
    )
    $RejectedMissingPayload = $false
    try {
        Invoke-TestPackager $Output "1.2.3" "rejection-test"
    } catch {
        if ($_.Exception.Message -notmatch "SCROZZ_TESSERACT_DIR") {
            throw
        }
        $RejectedMissingPayload = $true
    }
    if (-not $RejectedMissingPayload) {
        throw "Windows packaging accepted a missing Tesseract payload"
    }

    [Environment]::SetEnvironmentVariable("SCROZZ_TESSERACT_DIR", $Tesseract)

    Invoke-TestPackager $Output "1.2.3" "artifact-test"

    $Portable = Join-Path $Output "scrozz-1.2.3-artifact-test-windows-x86_64.zip"
    $Msix = Join-Path $Output "scrozz-1.2.3-artifact-test-windows-x86_64.msix"
    if (-not (Test-Path -LiteralPath $Portable -PathType Leaf)) {
        throw "Portable ZIP was not emitted"
    }
    if (-not (Test-Path -LiteralPath $Msix -PathType Leaf)) {
        throw "MSIX was not emitted"
    }

    Test-ArtifactMetadata $Portable "portable" "tesseract" "1.2.3" "" "none"
    Test-ArtifactMetadata `
        $Msix `
        "msix" `
        "windows-media-ocr" `
        "2.515.65535.0" `
        "com.thatcube.Scrozz" `
        "development"

    $PortableEntries = Get-ArchiveEntryNames $Portable
    Assert-ArchiveTimestamps $Portable "portable ZIP"
    Assert-ArchiveEntry `
        $PortableEntries `
        "scrozz-1.2.3-artifact-test-windows-x86_64/scrozz.exe" `
        "portable ZIP"
    foreach ($Entry in @(
        "scrozz-1.2.3-artifact-test-windows-x86_64/tesseract/tesseract.exe",
        "scrozz-1.2.3-artifact-test-windows-x86_64/tesseract/doc/LICENSE",
        "scrozz-1.2.3-artifact-test-windows-x86_64/tesseract/libtesseract-5.dll",
        "scrozz-1.2.3-artifact-test-windows-x86_64/tesseract/libleptonica-6.dll",
        "scrozz-1.2.3-artifact-test-windows-x86_64/tesseract/tessdata/eng.traineddata"
    )) {
        Assert-ArchiveEntry $PortableEntries $Entry "portable ZIP OCR payload"
    }
    if ($PortableEntries -contains "AppxManifest.xml") {
        throw "Portable ZIP unexpectedly contains package identity"
    }

    $MsixEntries = Get-ArchiveEntryNames $Msix
    Assert-ArchiveTimestamps $Msix "MSIX"
    foreach ($Entry in @(
        "AppxManifest.xml",
        "AppxBlockMap.xml",
        "Assets/Square44x44Logo.png",
        "Assets/Square150x150Logo.png",
        "Assets/StoreLogo.png",
        "scrozz.exe"
    )) {
        Assert-ArchiveEntry $MsixEntries $Entry "MSIX"
    }
    if ($MsixEntries -contains "AppxSignature.p7x") {
        throw "Unsigned package test unexpectedly produced a package signature"
    }
    if ($MsixEntries -contains "tesseract/tesseract.exe") {
        throw "MSIX unexpectedly contains the portable Tesseract payload"
    }

    $Manifest = Read-ArchiveText $Msix "AppxManifest.xml"
    foreach ($Required in @(
        'Name="com.thatcube.Scrozz"',
        'Version="2.515.65535.0"',
        'uap10:RuntimeBehavior="packagedClassicApp"',
        'Category="windows.startupTask"',
        'TaskId="ScrozzStartup"',
        'Enabled="false"',
        'Category="windows.appExecutionAlias"',
        '<rescap:Capability Name="runFullTrust"'
    )) {
        if (-not $Manifest.Contains($Required)) {
            throw "Generated AppxManifest is missing: $Required"
        }
    }
    if ($Manifest -match "@[A-Z_]+@") {
        throw "Generated AppxManifest contains an unsubstituted token"
    }

    [Environment]::SetEnvironmentVariable("SCROZZ_WINDOWS_VERIFY_DETERMINISM", "0")
    [Environment]::SetEnvironmentVariable("SCROZZ_MSIX_VERSION", "2.515.42.0")
    try {
        Invoke-TestPackager `
            $PrereleaseOutput "1.2.3-beta.1" "prerelease-artifact-test"
    } finally {
        [Environment]::SetEnvironmentVariable("SCROZZ_MSIX_VERSION", $null)
    }
    $PrereleaseMsix = Join-Path $PrereleaseOutput (
        "scrozz-1.2.3-beta.1-prerelease-artifact-test-windows-x86_64.msix"
    )
    Test-ArtifactMetadata `
        $PrereleaseMsix `
        "msix" `
        "windows-media-ocr" `
        "2.515.42.0" `
        "com.thatcube.Scrozz" `
        "development"
    $PrereleaseManifest = Read-ArchiveText $PrereleaseMsix "AppxManifest.xml"
    if (-not $PrereleaseManifest.Contains('Version="2.515.42.0"')) {
        throw "Explicit compliant prerelease MSIX version was not preserved"
    }

    Invoke-TestPackager `
        $BuildMetadataOutput "1.2.3+build.7" "build-metadata-test"
    $BuildMetadataMsix = Join-Path $BuildMetadataOutput (
        "scrozz-1.2.3+build.7-build-metadata-test-windows-x86_64.msix"
    )
    $BuildMetadata = Get-Content `
        -LiteralPath "$BuildMetadataMsix.artifact.json" `
        -Raw | ConvertFrom-Json
    Assert-Equal $BuildMetadata.version "1.2.3+build.7" "build metadata version"
    Assert-Equal (
        $BuildMetadata.native_package_version
    ) "2.515.65535.0" "build metadata native version"

    [Environment]::SetEnvironmentVariable("SCROZZ_MSIX_IDENTITY_MODE", "store")
    [Environment]::SetEnvironmentVariable(
        "SCROZZ_MSIX_IDENTITY_NAME",
        "store.assigned.Scrozz"
    )
    [Environment]::SetEnvironmentVariable("SCROZZ_MSIX_PUBLISHER", "CN=Store Publisher")
    [Environment]::SetEnvironmentVariable(
        "SCROZZ_MSIX_PUBLISHER_DISPLAY_NAME",
        $null
    )
    $RejectedMissingDisplayIdentity = $false
    try {
        Invoke-TestPackager $Output "1.2.3" "store-identity-rejection-test"
    } catch {
        if ($_.Exception.Message -notmatch "SCROZZ_MSIX_PUBLISHER_DISPLAY_NAME") {
            throw
        }
        $RejectedMissingDisplayIdentity = $true
    }
    if (-not $RejectedMissingDisplayIdentity) {
        throw "Store packaging fell back to the development publisher display name"
    }

    [Environment]::SetEnvironmentVariable(
        "SCROZZ_MSIX_PUBLISHER_DISPLAY_NAME",
        "Store Publisher"
    )
    $RejectedMissingCredential = $false
    try {
        Invoke-TestPackager $Output "1.2.3" "store-credential-rejection-test"
    } catch {
        if ($_.Exception.Message -notmatch "requires SCROZZ_MSIX_SIGN") {
            throw
        }
        $RejectedMissingCredential = $true
    }
    if (-not $RejectedMissingCredential) {
        throw "Store packaging accepted no signing credential"
    }

    [Environment]::SetEnvironmentVariable(
        "SCROZZ_MSIX_SIGN_CERT_SHA1",
        ("0" * 40)
    )
    $RejectedUnsignedPayload = $false
    try {
        Invoke-TestPackager $Output "1.2.3" "unsigned-payload-rejection-test"
    } catch {
        if ($_.Exception.Message -notmatch "valid Authenticode signature") {
            throw
        }
        $RejectedUnsignedPayload = $true
    }
    if (-not $RejectedUnsignedPayload) {
        throw "Store packaging accepted an unsigned executable payload"
    }

    Remove-Item -LiteralPath $Binary -Force
    New-UnsignedPeFixture $Binary

    $Certificate = New-SelfSignedCertificate `
        -Type CodeSigningCert `
        -Subject "CN=Scrozz Packaging Test" `
        -KeyExportPolicy Exportable `
        -CertStoreLocation "Cert:\CurrentUser\My"
    $TestCertificateThumbprint = $Certificate.Thumbprint
    $CertificatePasswordText = "scrozz-packaging-test"
    $CertificatePassword = ConvertTo-SecureString `
        -String $CertificatePasswordText `
        -AsPlainText `
        -Force
    $Pfx = Join-Path $Root "scrozz-test-signing.pfx"
    Export-PfxCertificate `
        -Cert $Certificate `
        -FilePath $Pfx `
        -Password $CertificatePassword | Out-Null

    $WindowsSdk = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
    $NativeSdkArchitecture = if (
        $env:PROCESSOR_ARCHITECTURE -eq "ARM64" -or
        $env:PROCESSOR_ARCHITEW6432 -eq "ARM64"
    ) {
        "arm64"
    } else {
        "x64"
    }
    $SdkArchitectures = @($NativeSdkArchitecture)
    if ($NativeSdkArchitecture -ne "x64") {
        $SdkArchitectures += "x64"
    }
    $SignTool = $null
    foreach ($SdkArchitecture in $SdkArchitectures) {
        $SignTool = Get-ChildItem `
            -LiteralPath $WindowsSdk `
            -Filter "signtool.exe" `
            -File `
            -Recurse |
            Where-Object {
                $_.FullName -match (
                    "\\{0}\\signtool\.exe$" -f [Regex]::Escape($SdkArchitecture)
                )
            } |
            Sort-Object FullName |
            Select-Object -Last 1
        if ($null -ne $SignTool) {
            break
        }
    }
    if ($null -eq $SignTool) {
        throw "SignTool is required for the Store identity test"
    }
    & $SignTool.FullName sign `
        /fd SHA256 `
        /f $Pfx `
        /p $CertificatePasswordText `
        $Binary
    if ($LASTEXITCODE -ne 0) {
        throw "SignTool could not sign the Store payload fixture"
    }
    $FixtureSignature = Get-AuthenticodeSignature -FilePath $Binary
    if ($null -eq $FixtureSignature.SignerCertificate -or
        -not $FixtureSignature.SignerCertificate.Thumbprint.Equals(
            $Certificate.Thumbprint,
            [StringComparison]::OrdinalIgnoreCase
        ) -or
        $FixtureSignature.Status -notin @("Valid", "NotTrusted", "UnknownError")) {
        throw "the self-signed Store payload fixture did not retain its signer"
    }

    [Environment]::SetEnvironmentVariable(
        "SCROZZ_TEST_ALLOW_UNTRUSTED_SIGNATURE",
        "1"
    )
    $RejectedMismatchedSigner = $false
    try {
        Invoke-TestPackager $Output "1.2.3" "mismatched-signer-test"
    } catch {
        if ($_.Exception.Message -notmatch "MSIX signing identity") {
            throw
        }
        $RejectedMismatchedSigner = $true
    }
    if (-not $RejectedMismatchedSigner) {
        throw "Store packaging accepted different payload and package signers"
    }

    [Environment]::SetEnvironmentVariable("SCROZZ_MSIX_SIGN_CERT_SHA1", $null)
    [Environment]::SetEnvironmentVariable("SCROZZ_MSIX_SIGN_PFX", $Pfx)
    [Environment]::SetEnvironmentVariable(
        "SCROZZ_MSIX_SIGN_PFX_PASSWORD",
        $CertificatePasswordText
    )
    [Environment]::SetEnvironmentVariable(
        "SCROZZ_MSIX_PUBLISHER",
        $Certificate.Subject
    )
    [Environment]::SetEnvironmentVariable(
        "SCROZZ_MSIX_PUBLISHER_DISPLAY_NAME",
        "Scrozz Packaging Test"
    )
    [Environment]::SetEnvironmentVariable(
        "SCROZZ_MSIX_IDENTITY_NAME",
        "Scrozz.Packaging.Test"
    )
    [Environment]::SetEnvironmentVariable("SCROZZ_MSIX_TIMESTAMP_URL", "none")
    [Environment]::SetEnvironmentVariable(
        "SCROZZ_ALLOW_UNTIMESTAMPED_SIGNING",
        "1"
    )
    Invoke-TestPackager $StoreOutput "1.2.3" "store-artifact-test"

    $StorePortable = Join-Path $StoreOutput (
        "scrozz-1.2.3-store-artifact-test-windows-x86_64.zip"
    )
    $StoreMsix = Join-Path $StoreOutput (
        "scrozz-1.2.3-store-artifact-test-windows-x86_64.msix"
    )
    Test-ArtifactMetadata `
        $StorePortable `
        "portable" `
        "tesseract" `
        "1.2.3" `
        "" `
        "none" `
        $false `
        $true `
        $true
    Test-ArtifactMetadata `
        $StoreMsix `
        "msix" `
        "windows-media-ocr" `
        "2.515.65535.0" `
        "Scrozz.Packaging.Test" `
        "store" `
        $true `
        $true `
        $true

    Write-Host "Windows packaging artifact checks passed"
} finally {
    foreach ($Name in $EnvironmentNames) {
        [Environment]::SetEnvironmentVariable($Name, $SavedEnvironment[$Name])
    }
    if (-not [String]::IsNullOrWhiteSpace($TestCertificateThumbprint)) {
        $PrivateCertificatePath = (
            "Cert:\CurrentUser\My\{0}" -f $TestCertificateThumbprint
        )
        if (Test-Path -LiteralPath $PrivateCertificatePath) {
            Remove-Item `
                -LiteralPath $PrivateCertificatePath `
                -DeleteKey `
                -Force `
                -Confirm:$false
        }
    }
    if (Test-Path -LiteralPath $Root) {
        Remove-Item -LiteralPath $Root -Recurse -Force
    }
}
