[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $OutputDirectory,

    [Parameter(Mandatory = $true)]
    [string] $Binary,

    [Parameter(Mandatory = $true)]
    [string] $Version,

    [Parameter(Mandatory = $true)]
    [string] $Stamp,

    [Parameter(Mandatory = $true)]
    [ValidateSet("x86_64", "aarch64")]
    [string] $Architecture,

    [Parameter()]
    [string] $TesseractDirectory = $env:SCROZZ_TESSERACT_DIR,

    [Parameter()]
    [string] $TesseractPayloadManifest = ""
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$Binary = [IO.Path]::GetFullPath($Binary)
$OutputRoot = [IO.Path]::GetPathRoot($OutputDirectory)
if ([String]::IsNullOrWhiteSpace($TesseractPayloadManifest)) {
    $TesseractPayloadManifest = Join-Path `
        $RepoRoot "packaging\windows\tesseract-payload.json"
}
$TesseractPayloadManifest = [IO.Path]::GetFullPath($TesseractPayloadManifest)

if ($OutputDirectory.TrimEnd("\", "/") -eq $OutputRoot.TrimEnd("\", "/")) {
    throw "Refusing to package into a filesystem root: $OutputDirectory"
}
if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
    throw "No built executable exists at $Binary"
}
if ([IO.Path]::GetExtension($Binary) -ne ".exe") {
    throw "The Windows package binary must end in .exe: $Binary"
}
if ([String]::IsNullOrWhiteSpace($TesseractDirectory)) {
    throw "SCROZZ_TESSERACT_DIR must name the artifact-local Tesseract payload"
}
if (-not [IO.Path]::IsPathRooted($TesseractDirectory) -or
    $TesseractDirectory -match "^[A-Za-z]:[^\\/]") {
    throw "SCROZZ_TESSERACT_DIR must be an absolute path: $TesseractDirectory"
}
$TesseractDirectory = [IO.Path]::GetFullPath($TesseractDirectory)
if (-not (Test-Path -LiteralPath $TesseractDirectory -PathType Container)) {
    throw "SCROZZ_TESSERACT_DIR does not name a directory: $TesseractDirectory"
}
$TesseractExecutable = Join-Path $TesseractDirectory "tesseract.exe"
$EnglishTrainedData = Join-Path $TesseractDirectory "tessdata\eng.traineddata"
if (-not (Test-Path -LiteralPath $TesseractExecutable -PathType Leaf) -or
    -not (Test-Path -LiteralPath $EnglishTrainedData -PathType Leaf)) {
    throw (
        "The portable OCR payload is incomplete. Expected " +
        "$TesseractExecutable and $EnglishTrainedData"
    )
}
$ReparsePoint = @(
    Get-Item -LiteralPath $TesseractDirectory
    Get-ChildItem -LiteralPath $TesseractDirectory -Recurse -Force
) | Where-Object {
    ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0
} | Select-Object -First 1
if ($null -ne $ReparsePoint) {
    throw "The Tesseract payload cannot contain reparse points: $($ReparsePoint.FullName)"
}
if ($Stamp -notmatch "^[0-9A-Za-z._-]+$") {
    throw "Unsafe package stamp: $Stamp"
}
if ($Version -notmatch (
        "^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)" +
        "(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$"
    )) {
    throw "Invalid application version: $Version"
}
if ($OutputDirectory.Contains('"') -or
    $OutputDirectory.Contains("`r") -or
    $OutputDirectory.Contains("`n") -or
    $Binary.Contains('"') -or
    $Binary.Contains("`r") -or
    $Binary.Contains("`n")) {
    throw "MakeAppx mapping paths cannot contain a quote or line break"
}

function Get-EnvironmentOrDefault {
    param([string] $Name, [string] $Default)
    $Value = [Environment]::GetEnvironmentVariable($Name)
    if ([String]::IsNullOrWhiteSpace($Value)) {
        return $Default
    }
    return $Value
}

function Assert-SingleLine {
    param([string] $Value, [string] $Label, [int] $MaximumLength)
    if ([String]::IsNullOrWhiteSpace($Value) -or
        $Value.Length -gt $MaximumLength -or
        $Value.Contains("`0") -or
        $Value.Contains("`r") -or
        $Value.Contains("`n")) {
        throw "Invalid $Label"
    }
}

function Test-PathWithin {
    param([string] $Candidate, [string] $Parent)
    $Comparison = [StringComparison]::OrdinalIgnoreCase
    $ParentPrefix = $Parent.TrimEnd("\", "/") + [IO.Path]::DirectorySeparatorChar
    return (
        $Candidate.Equals($Parent, $Comparison) -or
        $Candidate.StartsWith($ParentPrefix, $Comparison)
    )
}

function Confirm-TesseractPayload {
    param([string] $Directory, [string] $ManifestPath)
    if (-not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) {
        throw "Tesseract payload manifest does not exist: $ManifestPath"
    }
    try {
        $Manifest = Get-Content -LiteralPath $ManifestPath -Raw | ConvertFrom-Json
    } catch {
        throw "Tesseract payload manifest is not valid JSON: $ManifestPath"
    }
    if ($Manifest.schema -ne 1) {
        throw "Unsupported Tesseract payload manifest schema in $ManifestPath"
    }

    $Entries = @($Manifest.payload_files)
    if ($Entries.Count -eq 0) {
        throw "Tesseract payload manifest contains no files: $ManifestPath"
    }
    $Seen = [Collections.Generic.HashSet[string]]::new(
        [StringComparer]::OrdinalIgnoreCase
    )
    foreach ($Entry in $Entries) {
        $RelativePath = [string] $Entry.path
        $ExpectedHash = ([string] $Entry.sha256).ToLowerInvariant()
        $IsRuntimeDll = $RelativePath -match "^[0-9A-Za-z.+_-]+\.dll$"
        if ($RelativePath -ne "tesseract.exe" -and
            $RelativePath -ne "doc/LICENSE" -and
            $RelativePath -ne "tessdata/eng.traineddata" -and
            -not $IsRuntimeDll) {
            throw "Unsafe or unexpected Tesseract payload path: $RelativePath"
        }
        if (-not $Seen.Add($RelativePath)) {
            throw "Duplicate Tesseract payload path: $RelativePath"
        }
        if ($ExpectedHash -notmatch "^[0-9a-f]{64}$") {
            throw "Invalid checksum for Tesseract payload path: $RelativePath"
        }

        $NativeRelativePath = $RelativePath.Replace(
            "/", [IO.Path]::DirectorySeparatorChar
        )
        $Source = Join-Path $Directory $NativeRelativePath
        if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
            throw "The portable OCR payload is missing $Source"
        }
        $ActualHash = (
            Get-FileHash -LiteralPath $Source -Algorithm SHA256
        ).Hash.ToLowerInvariant()
        if ($ActualHash -ne $ExpectedHash) {
            throw (
                "Checksum mismatch for Tesseract payload file $RelativePath. " +
                "Expected $ExpectedHash, found $ActualHash"
            )
        }
    }

    foreach ($Required in @(
        "tesseract.exe",
        "doc/LICENSE",
        "libtesseract-5.dll",
        "libleptonica-6.dll",
        "tessdata/eng.traineddata"
    )) {
        if (-not $Seen.Contains($Required)) {
            throw "Tesseract payload manifest is missing required file $Required"
        }
    }

    $ExpectedDlls = @(
        $Entries |
            ForEach-Object { [string] $_.path } |
            Where-Object { $_ -match "^[0-9A-Za-z.+_-]+\.dll$" } |
            Sort-Object
    )
    $ActualDlls = @(
        Get-ChildItem -LiteralPath $Directory -Filter "*.dll" -File |
            ForEach-Object { $_.Name } |
            Sort-Object
    )
    $Difference = @(
        Compare-Object -ReferenceObject $ExpectedDlls -DifferenceObject $ActualDlls
    )
    if ($Difference.Count -ne 0) {
        $Details = ($Difference | ForEach-Object {
            "$($_.InputObject) $($_.SideIndicator)"
        }) -join ", "
        throw "Tesseract runtime DLL closure differs from the manifest: $Details"
    }

    return $Entries
}

function Convert-ToMsixVersion {
    param([string] $ApplicationVersion)
    $Override = [Environment]::GetEnvironmentVariable("SCROZZ_MSIX_VERSION")
    if (-not [String]::IsNullOrWhiteSpace($Override)) {
        $Candidate = $Override
    } else {
        if ($ApplicationVersion.Contains("-")) {
            throw (
                "Prerelease artifacts require SCROZZ_MSIX_VERSION so distinct " +
                "prereleases cannot collapse to one Windows package version"
            )
        }
        $CoreParts = ($ApplicationVersion -split "-", 2)[0].Split(".")
        [UInt64] $SemanticMajor = 0
        [UInt64] $SemanticMinor = 0
        [UInt64] $SemanticPatch = 0
        if (-not [UInt64]::TryParse(
                $CoreParts[0],
                [Globalization.NumberStyles]::None,
                [Globalization.CultureInfo]::InvariantCulture,
                [ref] $SemanticMajor
            ) -or $SemanticMajor -gt 65534 -or
            -not [UInt64]::TryParse(
                $CoreParts[1],
                [Globalization.NumberStyles]::None,
                [Globalization.CultureInfo]::InvariantCulture,
                [ref] $SemanticMinor
            ) -or $SemanticMinor -gt 255 -or
            -not [UInt64]::TryParse(
                $CoreParts[2],
                [Globalization.NumberStyles]::None,
                [Globalization.CultureInfo]::InvariantCulture,
                [ref] $SemanticPatch
            ) -or $SemanticPatch -gt 255) {
            throw (
                "Stable MSIX mapping requires semantic major <= 65534 and " +
                "minor/patch <= 255: $ApplicationVersion"
            )
        }
        $NativeMajor = $SemanticMajor + 1
        $NativeMinor = ($SemanticMinor * 256) + $SemanticPatch
        $Candidate = "$NativeMajor.$NativeMinor.65535.0"
    }
    if ($Candidate -notmatch "^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$") {
        throw "SCROZZ_MSIX_VERSION must contain four numeric parts: $Candidate"
    }
    $Numbers = [Collections.Generic.List[UInt32]]::new()
    foreach ($Part in $Candidate.Split(".")) {
        [UInt64] $Number = 0
        if (-not [UInt64]::TryParse(
                $Part,
                [Globalization.NumberStyles]::None,
                [Globalization.CultureInfo]::InvariantCulture,
                [ref] $Number
            ) -or $Number -gt 65535) {
            throw "Every MSIX version component must be at most 65535: $Candidate"
        }
        $Numbers.Add([UInt32] $Number)
    }
    if ($Numbers[0] -eq 0) {
        throw "The first MSIX version component must be between 1 and 65535: $Candidate"
    }
    if ($Numbers[3] -ne 0) {
        throw "The fourth MSIX version component must be 0: $Candidate"
    }
    return ($Numbers -join ".")
}

function Resolve-WindowsSdkTool {
    param([string] $Name, [string] $OverrideVariable)
    $Override = [Environment]::GetEnvironmentVariable($OverrideVariable)
    if (-not [String]::IsNullOrWhiteSpace($Override)) {
        $Resolved = [IO.Path]::GetFullPath($Override)
        if (-not (Test-Path -LiteralPath $Resolved -PathType Leaf)) {
            throw "$OverrideVariable does not name a file: $Resolved"
        }
        return $Resolved
    }

    $Command = Get-Command "${Name}.exe" -ErrorAction SilentlyContinue
    if ($null -ne $Command) {
        return $Command.Source
    }

    $ProgramFilesX86 = [Environment]::GetEnvironmentVariable("ProgramFiles(x86)")
    if ([String]::IsNullOrWhiteSpace($ProgramFilesX86)) {
        throw "Windows did not report its Program Files (x86) directory"
    }
    $SdkRoot = Join-Path $ProgramFilesX86 "Windows Kits\10"
    $Candidates = @(
        @(
            foreach ($Directory in @(
                Get-ChildItem `
                    -LiteralPath (Join-Path $SdkRoot "bin") `
                    -Directory `
                    -ErrorAction SilentlyContinue
            )) {
                $Candidate = Join-Path $Directory.FullName "x64\${Name}.exe"
                if (Test-Path -LiteralPath $Candidate -PathType Leaf) {
                    $Candidate
                }
            }
        ) | Sort-Object -Descending
    )
    if ($Name -eq "makeappx") {
        $Candidates += Join-Path $SdkRoot "App Certification Kit\makeappx.exe"
    }
    $Found = $Candidates | Where-Object {
        Test-Path -LiteralPath $_ -PathType Leaf
    } | Select-Object -First 1
    if ($null -eq $Found) {
        throw "${Name}.exe was not found. Install the Windows SDK or set $OverrideVariable."
    }
    return $Found
}

function Write-Utf8NoBom {
    param([string] $Path, [string] $Text)
    [IO.File]::WriteAllText($Path, $Text, [Text.UTF8Encoding]::new($false))
}

function Copy-DistributionDocuments {
    param([string] $Destination)
    foreach ($Document in @("README.md", "LICENSE", "TRADEMARK.md")) {
        $Source = Join-Path $RepoRoot $Document
        if (Test-Path -LiteralPath $Source -PathType Leaf) {
            Copy-Item -LiteralPath $Source -Destination $Destination
        }
    }
}

function New-DeterministicZip {
    param([string] $Source, [string] $Destination)
    Add-Type -AssemblyName System.IO.Compression
    if (Test-Path -LiteralPath $Destination) {
        Remove-Item -LiteralPath $Destination -Force
    }
    $Stream = [IO.File]::Open(
        $Destination,
        [IO.FileMode]::CreateNew,
        [IO.FileAccess]::ReadWrite,
        [IO.FileShare]::None
    )
    try {
        $Archive = [IO.Compression.ZipArchive]::new(
            $Stream,
            [IO.Compression.ZipArchiveMode]::Create,
            $false
        )
        try {
            $Parent = ([IO.Directory]::GetParent($Source)).FullName
            $Files = [IO.Directory]::GetFiles(
                $Source,
                "*",
                [IO.SearchOption]::AllDirectories
            )
            [Array]::Sort($Files, [StringComparer]::Ordinal)
            foreach ($File in $Files) {
                $Relative = $File.Substring($Parent.Length + 1).Replace("\", "/")
                $Entry = $Archive.CreateEntry(
                    $Relative,
                    [IO.Compression.CompressionLevel]::Optimal
                )
                $Entry.LastWriteTime = [DateTimeOffset]::new(
                    1980, 1, 1, 0, 0, 0, [TimeSpan]::Zero
                )
                $Input = [IO.File]::OpenRead($File)
                $Output = $Entry.Open()
                try {
                    $Input.CopyTo($Output)
                } finally {
                    $Output.Dispose()
                    $Input.Dispose()
                }
            }
        } finally {
            $Archive.Dispose()
        }
    } finally {
        $Stream.Dispose()
    }
}

function New-MappingFile {
    param([string] $PackageRoot, [string] $Destination)
    $Lines = [Collections.Generic.List[string]]::new()
    $Lines.Add("[Files]")
    $Files = [IO.Directory]::GetFiles(
        $PackageRoot,
        "*",
        [IO.SearchOption]::AllDirectories
    )
    [Array]::Sort($Files, [StringComparer]::Ordinal)
    foreach ($File in $Files) {
        if ($File.Contains('"')) {
            throw "MakeAppx source paths cannot contain a quote: $File"
        }
        $Relative = $File.Substring($PackageRoot.Length + 1)
        $Lines.Add('"{0}" "{1}"' -f $File, $Relative)
    }
    Write-Utf8NoBom -Path $Destination -Text (($Lines -join "`r`n") + "`r`n")
}

function Invoke-MakeAppx {
    param(
        [string] $Tool,
        [string] $Mapping,
        [string] $Destination
    )
    if (Test-Path -LiteralPath $Destination) {
        Remove-Item -LiteralPath $Destination -Force
    }
    & $Tool pack /o /h SHA256 /f $Mapping /p $Destination
    if ($LASTEXITCODE -ne 0) {
        throw "MakeAppx failed with status $LASTEXITCODE"
    }
}

function Assert-IdenticalArtifact {
    param(
        [string] $First,
        [string] $Second,
        [string] $Description
    )
    $FirstHash = (Get-FileHash -LiteralPath $First -Algorithm SHA256).Hash
    $SecondHash = (Get-FileHash -LiteralPath $Second -Algorithm SHA256).Hash
    if ($FirstHash -ne $SecondHash) {
        throw "$Description is not byte-for-byte reproducible"
    }
}

function Write-ArtifactMetadata {
    param(
        [string] $Artifact,
        [string] $Platform,
        [string] $PackageKind,
        [string] $OcrBackend,
        [string] $NativePackageVersion,
        [bool] $Signed,
        [bool] $PayloadSigned,
        [string] $IdentityName
    )
    $Hash = (Get-FileHash -LiteralPath $Artifact -Algorithm SHA256).Hash.ToLowerInvariant()
    $Length = (Get-Item -LiteralPath $Artifact).Length
    $FileName = [IO.Path]::GetFileName($Artifact)
    [IO.File]::WriteAllText(
        "$Artifact.sha256",
        "$Hash  $FileName`n",
        [Text.Encoding]::ASCII
    )
    $Metadata = [ordered]@{
        schema = 1
        platform = $Platform
        version = $Version
        file = $FileName
        sha256 = $Hash
        size = $Length
        package_kind = $PackageKind
        native_package_version = $NativePackageVersion
        ocr_backend = $OcrBackend
        package_identity = $IdentityName
        signed = $Signed
        payload_signed = $PayloadSigned
        signed_manifest = $false
    }
    Write-Utf8NoBom -Path "$Artifact.artifact.json" -Text (
        ($Metadata | ConvertTo-Json -Depth 3) + "`n"
    )
    Write-Host "built: $Artifact"
    Write-Host "sha256: $Hash"
    Write-Host "bytes: $Length"
    Write-Host "metadata: $Artifact.artifact.json"
}

$PackageIdentityName = Get-EnvironmentOrDefault `
    "SCROZZ_MSIX_IDENTITY_NAME" "com.thatcube.Scrozz"
$PackagePublisher = Get-EnvironmentOrDefault `
    "SCROZZ_MSIX_PUBLISHER" "CN=Scrozz Development"
$PublisherDisplayName = Get-EnvironmentOrDefault `
    "SCROZZ_MSIX_PUBLISHER_DISPLAY_NAME" "Scrozz Development"
$TesseractPayloadFiles = @(
    Confirm-TesseractPayload `
        -Directory $TesseractDirectory `
        -ManifestPath $TesseractPayloadManifest
)
$MsixVersion = Convert-ToMsixVersion $Version
$MsixArchitecture = if ($Architecture -eq "x86_64") { "x64" } else { "arm64" }
$PayloadSigned = (Get-AuthenticodeSignature -FilePath $Binary).Status -eq "Valid"

if ($PackageIdentityName -notmatch "^[0-9A-Za-z.-]{3,50}$") {
    throw "SCROZZ_MSIX_IDENTITY_NAME is not a valid package identity name"
}
Assert-SingleLine $PackagePublisher "MSIX Publisher" 8192
Assert-SingleLine $PublisherDisplayName "MSIX PublisherDisplayName" 256
if ((Test-PathWithin $TesseractDirectory $OutputDirectory) -or
    (Test-PathWithin $OutputDirectory $TesseractDirectory)) {
    throw "SCROZZ_TESSERACT_DIR and the output directory must not overlap"
}

$Name = "scrozz-$Version-$Stamp-windows-$Architecture"
$Portable = Join-Path $OutputDirectory "$Name.zip"
$Msix = Join-Path $OutputDirectory "$Name.msix"
$Scratch = Join-Path $OutputDirectory (
    ".scrozz-windows-{0}-{1}" -f $PID, [Guid]::NewGuid().ToString("N")
)
$PortableRoot = Join-Path $Scratch $Name
$MsixRoot = Join-Path $Scratch "msix"
$AssetsRoot = Join-Path $MsixRoot "Assets"
$PortableStaged = Join-Path $Scratch "$Name.zip"
$MsixStaged = Join-Path $Scratch "$Name.msix"

New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
New-Item -ItemType Directory -Path $Scratch | Out-Null

try {
    New-Item -ItemType Directory -Path $PortableRoot | Out-Null
    New-Item -ItemType Directory -Path $AssetsRoot -Force | Out-Null
    Copy-Item -LiteralPath $Binary -Destination (Join-Path $PortableRoot "scrozz.exe")
    Copy-Item -LiteralPath $Binary -Destination (Join-Path $MsixRoot "scrozz.exe")
    Copy-DistributionDocuments $PortableRoot
    Copy-DistributionDocuments $MsixRoot
    $PortableTesseract = Join-Path $PortableRoot "tesseract"
    New-Item -ItemType Directory -Path $PortableTesseract | Out-Null
    foreach ($PayloadFile in $TesseractPayloadFiles) {
        $RelativePath = ([string] $PayloadFile.path).Replace(
            "/", [IO.Path]::DirectorySeparatorChar
        )
        $Source = Join-Path $TesseractDirectory $RelativePath
        $Destination = Join-Path $PortableTesseract $RelativePath
        $DestinationParent = [IO.Path]::GetDirectoryName($Destination)
        New-Item -ItemType Directory -Path $DestinationParent -Force | Out-Null
        Copy-Item -LiteralPath $Source -Destination $Destination
    }

    foreach ($Asset in @(
        "Square44x44Logo.png",
        "Square150x150Logo.png",
        "StoreLogo.png"
    )) {
        $Source = Join-Path $RepoRoot "packaging\windows\Assets\$Asset"
        if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
            throw "Required MSIX asset is missing: $Source"
        }
        Copy-Item -LiteralPath $Source -Destination (Join-Path $AssetsRoot $Asset)
    }

    $TemplatePath = Join-Path $RepoRoot "packaging\windows\AppxManifest.xml.in"
    $Manifest = [IO.File]::ReadAllText($TemplatePath)
    $Replacements = [ordered]@{
        PACKAGE_IDENTITY_NAME = $PackageIdentityName
        PACKAGE_PUBLISHER = $PackagePublisher
        PACKAGE_VERSION = $MsixVersion
        PACKAGE_ARCHITECTURE = $MsixArchitecture
        PUBLISHER_DISPLAY_NAME = $PublisherDisplayName
    }
    foreach ($Token in $Replacements.Keys) {
        $Escaped = [Security.SecurityElement]::Escape([string] $Replacements[$Token])
        $Manifest = $Manifest.Replace("@$Token@", $Escaped)
    }
    if ($Manifest -match "@[A-Z_]+@") {
        throw "The generated AppxManifest still contains an unsubstituted token"
    }
    Write-Utf8NoBom -Path (Join-Path $MsixRoot "AppxManifest.xml") -Text $Manifest

    $Epoch = [DateTime]::SpecifyKind(
        [DateTime]::new(1980, 1, 1, 0, 0, 0),
        [DateTimeKind]::Utc
    )
    Get-ChildItem -LiteralPath $MsixRoot -Recurse -Force | ForEach-Object {
        $_.LastWriteTimeUtc = $Epoch
    }
    Get-ChildItem -LiteralPath $PortableRoot -Recurse -Force | ForEach-Object {
        $_.LastWriteTimeUtc = $Epoch
    }

    New-DeterministicZip -Source $PortableRoot -Destination $PortableStaged

    $Mapping = Join-Path $Scratch "mapping.txt"
    New-MappingFile -PackageRoot $MsixRoot -Destination $Mapping
    $MakeAppx = Resolve-WindowsSdkTool "makeappx" "SCROZZ_MAKEAPPX"
    Invoke-MakeAppx -Tool $MakeAppx -Mapping $Mapping -Destination $MsixStaged

    $VerifyDeterminism = (
        [Environment]::GetEnvironmentVariable("SCROZZ_WINDOWS_VERIFY_DETERMINISM") -eq "1" -or
        [Environment]::GetEnvironmentVariable("SCROZZ_MSIX_VERIFY_DETERMINISM") -eq "1"
    )
    if ($VerifyDeterminism) {
        $SecondZip = Join-Path $Scratch "determinism-check.zip"
        $SecondMsix = Join-Path $Scratch "determinism-check.msix"
        New-DeterministicZip -Source $PortableRoot -Destination $SecondZip
        Invoke-MakeAppx -Tool $MakeAppx -Mapping $Mapping -Destination $SecondMsix
        Assert-IdenticalArtifact $PortableStaged $SecondZip "Portable ZIP output"
        Assert-IdenticalArtifact $MsixStaged $SecondMsix "MSIX output"
    }

    $SignPfx = [Environment]::GetEnvironmentVariable("SCROZZ_MSIX_SIGN_PFX")
    $SignThumbprint = [Environment]::GetEnvironmentVariable("SCROZZ_MSIX_SIGN_CERT_SHA1")
    if (-not [String]::IsNullOrWhiteSpace($SignPfx) -and
        -not [String]::IsNullOrWhiteSpace($SignThumbprint)) {
        throw "Set only one of SCROZZ_MSIX_SIGN_PFX and SCROZZ_MSIX_SIGN_CERT_SHA1"
    }

    $Signed = $false
    if (-not [String]::IsNullOrWhiteSpace($SignPfx) -or
        -not [String]::IsNullOrWhiteSpace($SignThumbprint)) {
        $SignTool = Resolve-WindowsSdkTool "signtool" "SCROZZ_SIGNTOOL"
        $SignArguments = [Collections.Generic.List[string]]::new()
        $SignArguments.Add("sign")
        $SignArguments.Add("/fd")
        $SignArguments.Add("SHA256")
        if (-not [String]::IsNullOrWhiteSpace($SignPfx)) {
            $ResolvedPfx = [IO.Path]::GetFullPath($SignPfx)
            if (-not (Test-Path -LiteralPath $ResolvedPfx -PathType Leaf)) {
                throw "SCROZZ_MSIX_SIGN_PFX does not name a file: $ResolvedPfx"
            }
            $SignArguments.Add("/f")
            $SignArguments.Add($ResolvedPfx)
            $Password = [Environment]::GetEnvironmentVariable(
                "SCROZZ_MSIX_SIGN_PFX_PASSWORD"
            )
            if (-not [String]::IsNullOrEmpty($Password)) {
                $SignArguments.Add("/p")
                $SignArguments.Add($Password)
            }
        } else {
            if ($SignThumbprint -notmatch "^[0-9A-Fa-f]{40}$") {
                throw "SCROZZ_MSIX_SIGN_CERT_SHA1 must be a 40-digit certificate thumbprint"
            }
            $SignArguments.Add("/sha1")
            $SignArguments.Add($SignThumbprint)
        }
        $Timestamp = Get-EnvironmentOrDefault `
            "SCROZZ_MSIX_TIMESTAMP_URL" "http://timestamp.digicert.com"
        $SignArguments.Add("/tr")
        $SignArguments.Add($Timestamp)
        $SignArguments.Add("/td")
        $SignArguments.Add("SHA256")
        $SignArguments.Add($MsixStaged)
        & $SignTool @SignArguments
        if ($LASTEXITCODE -ne 0) {
            throw "SignTool failed with status $LASTEXITCODE"
        }
        & $SignTool verify /pa $MsixStaged
        if ($LASTEXITCODE -ne 0) {
            throw "SignTool verification failed with status $LASTEXITCODE"
        }
        $Signed = $true
    } else {
        Write-Warning (
            "MSIX is unsigned. Set SCROZZ_MSIX_SIGN_PFX or " +
            "SCROZZ_MSIX_SIGN_CERT_SHA1 only in a human-approved signing environment."
        )
    }

    foreach ($Artifact in @($Portable, $Msix)) {
        foreach ($Existing in @($Artifact, "$Artifact.sha256", "$Artifact.artifact.json")) {
            if (Test-Path -LiteralPath $Existing) {
                Remove-Item -LiteralPath $Existing -Force
            }
        }
    }
    Move-Item -LiteralPath $PortableStaged -Destination $Portable
    Move-Item -LiteralPath $MsixStaged -Destination $Msix

    Write-ArtifactMetadata `
        -Artifact $Portable `
        -Platform "windows-$Architecture" `
        -PackageKind "portable" `
        -OcrBackend "tesseract" `
        -NativePackageVersion $Version `
        -Signed $false `
        -PayloadSigned $PayloadSigned `
        -IdentityName ""
    Write-ArtifactMetadata `
        -Artifact $Msix `
        -Platform "windows-$Architecture" `
        -PackageKind "msix" `
        -OcrBackend "windows-media-ocr" `
        -NativePackageVersion $MsixVersion `
        -Signed $Signed `
        -PayloadSigned $PayloadSigned `
        -IdentityName $PackageIdentityName
} finally {
    if (Test-Path -LiteralPath $Scratch) {
        Remove-Item -LiteralPath $Scratch -Recurse -Force
    }
}
