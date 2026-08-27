[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$Root = Join-Path ([IO.Path]::GetTempPath()) (
    "scrozz-windows-package-test-{0}" -f [Guid]::NewGuid().ToString("N")
)
$Output = Join-Path $Root "artifacts"
$Binary = Join-Path $Root "scrozz.exe"
$Tesseract = Join-Path $Root "tesseract-payload"
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

function Test-ArtifactMetadata {
    param(
        [string] $Artifact,
        [string] $PackageKind,
        [string] $OcrBackend,
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
    Assert-Equal $Metadata.ocr_backend $OcrBackend "metadata OCR backend"
    Assert-Equal $Metadata.package_identity $PackageIdentity "metadata package identity"
    Assert-Equal $Metadata.signed $false "metadata signed state"
    Assert-Equal $Metadata.signed_manifest $false "metadata update-manifest state"
    Assert-Equal (
        [IO.File]::ReadAllText($HashPath).Trim()
    ) "$Hash  $([IO.Path]::GetFileName($Artifact))" "checksum sidecar"
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
    # execute the payload. A minimal MZ marker keeps this test independent of a
    # release build while exercising the exact production packaging path.
    [IO.File]::WriteAllBytes($Binary, [byte[]] @(0x4d, 0x5a))
    [Environment]::SetEnvironmentVariable(
        "SCROZZ_TESSERACT_DIR",
        (Join-Path $Root "missing-tesseract")
    )
    $RejectedMissingPayload = $false
    try {
        & (Join-Path $RepoRoot "tools\package-windows.ps1") `
            -OutputDirectory $Output `
            -Binary $Binary `
            -Version "1.2.3" `
            -Stamp "rejection-test" `
            -Architecture "x86_64"
    } catch {
        if ($_.Exception.Message -notmatch "SCROZZ_TESSERACT_DIR") {
            throw
        }
        $RejectedMissingPayload = $true
    }
    if (-not $RejectedMissingPayload) {
        throw "Windows packaging accepted a missing Tesseract payload"
    }

    New-Item -ItemType Directory -Path (Join-Path $Tesseract "tessdata") -Force |
        Out-Null
    [IO.File]::WriteAllBytes(
        (Join-Path $Tesseract "tesseract.exe"),
        [byte[]] @(0x4d, 0x5a)
    )
    [IO.File]::WriteAllBytes(
        (Join-Path $Tesseract "libtesseract-5.dll"),
        [byte[]] @(0x4d, 0x5a)
    )
    [IO.File]::WriteAllText(
        (Join-Path $Tesseract "tessdata\eng.traineddata"),
        "fixture",
        [Text.Encoding]::ASCII
    )
    [Environment]::SetEnvironmentVariable("SCROZZ_TESSERACT_DIR", $Tesseract)

    & (Join-Path $RepoRoot "tools\package-windows.ps1") `
        -OutputDirectory $Output `
        -Binary $Binary `
        -Version "1.2.3" `
        -Stamp "artifact-test" `
        -Architecture "x86_64"

    $Portable = Join-Path $Output "scrozz-1.2.3-artifact-test-windows-x86_64.zip"
    $Msix = Join-Path $Output "scrozz-1.2.3-artifact-test-windows-x86_64.msix"
    if (-not (Test-Path -LiteralPath $Portable -PathType Leaf)) {
        throw "Portable ZIP was not emitted"
    }
    if (-not (Test-Path -LiteralPath $Msix -PathType Leaf)) {
        throw "MSIX was not emitted"
    }

    Test-ArtifactMetadata $Portable "portable" "tesseract" ""
    Test-ArtifactMetadata $Msix "msix" "windows-media-ocr" "com.thatcube.Scrozz"

    $PortableEntries = Get-ArchiveEntryNames $Portable
    Assert-ArchiveEntry `
        $PortableEntries `
        "scrozz-1.2.3-artifact-test-windows-x86_64/scrozz.exe" `
        "portable ZIP"
    foreach ($Entry in @(
        "scrozz-1.2.3-artifact-test-windows-x86_64/tesseract/tesseract.exe",
        "scrozz-1.2.3-artifact-test-windows-x86_64/tesseract/libtesseract-5.dll",
        "scrozz-1.2.3-artifact-test-windows-x86_64/tesseract/tessdata/eng.traineddata"
    )) {
        Assert-ArchiveEntry $PortableEntries $Entry "portable ZIP OCR payload"
    }
    if ($PortableEntries -contains "AppxManifest.xml") {
        throw "Portable ZIP unexpectedly contains package identity"
    }

    $MsixEntries = Get-ArchiveEntryNames $Msix
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
        'Version="1.2.3.0"',
        'uap10:RuntimeBehavior="packagedClassicApp"',
        'Category="windows.protocol"',
        'uap10:Parameters="url handle"',
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

    Write-Host "Windows packaging artifact checks passed"
} finally {
    foreach ($Name in $EnvironmentNames) {
        [Environment]::SetEnvironmentVariable($Name, $SavedEnvironment[$Name])
    }
    if (Test-Path -LiteralPath $Root) {
        Remove-Item -LiteralPath $Root -Recurse -Force
    }
}
