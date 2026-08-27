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
$Binary = Join-Path $Root "scrozz.exe"
$Tesseract = Join-Path $Root "tesseract-payload"
$PayloadManifest = Join-Path $Root "tesseract-payload.test.json"
$EnvironmentNames = @(
    "SCROZZ_WINDOWS_VERIFY_DETERMINISM",
    "SCROZZ_MSIX_VERIFY_DETERMINISM",
    "SCROZZ_MSIX_IDENTITY_NAME",
    "SCROZZ_MSIX_PUBLISHER",
    "SCROZZ_MSIX_PUBLISHER_DISPLAY_NAME",
    "SCROZZ_MSIX_VERSION",
    "SCROZZ_TESSERACT_DIR",
    "SCROZZ_MSIX_SIGN_PFX",
    "SCROZZ_MSIX_SIGN_PFX_PASSWORD",
    "SCROZZ_MSIX_SIGN_CERT_SHA1"
)
$SavedEnvironment = @{}

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

function Test-ArtifactMetadata {
    param(
        [string] $Artifact,
        [string] $PackageKind,
        [string] $OcrBackend,
        [string] $NativePackageVersion,
        [string] $PackageIdentity
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
    Assert-Equal $Metadata.signed $false "metadata signed state"
    Assert-Equal $Metadata.payload_signed $false "metadata payload signing state"
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

    Test-ArtifactMetadata $Portable "portable" "tesseract" "1.2.3" ""
    Test-ArtifactMetadata `
        $Msix "msix" "windows-media-ocr" "2.515.65535.0" "com.thatcube.Scrozz"

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
        "com.thatcube.Scrozz"
    $PrereleaseManifest = Read-ArchiveText $PrereleaseMsix "AppxManifest.xml"
    if (-not $PrereleaseManifest.Contains('Version="2.515.42.0"')) {
        throw "Explicit compliant prerelease MSIX version was not preserved"
    }

    Write-Host "Windows packaging artifact checks passed"
} finally {
    foreach ($Name in $EnvironmentNames) {
        [Environment]::SetEnvironmentVariable($Name, $SavedEnvironment[$Name])
    }
    if (Test-Path -LiteralPath $Root) {
        Remove-Item -LiteralPath $Root -Recurse -Force
    }
}
