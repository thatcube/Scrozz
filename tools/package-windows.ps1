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
    [string] $Architecture
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$Binary = [IO.Path]::GetFullPath($Binary)
$OutputRoot = [IO.Path]::GetPathRoot($OutputDirectory)

if ($OutputDirectory.TrimEnd("\", "/") -eq $OutputRoot.TrimEnd("\", "/")) {
    throw "Refusing to package into a filesystem root: $OutputDirectory"
}
if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
    throw "No built executable exists at $Binary"
}
if ([IO.Path]::GetExtension($Binary) -ne ".exe") {
    throw "The Windows package binary must end in .exe: $Binary"
}
if ($Stamp -notmatch "^[0-9A-Za-z._-]+$") {
    throw "Unsafe package stamp: $Stamp"
}
if ($Version -notmatch "^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$") {
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

function Convert-ToMsixVersion {
    param([string] $ApplicationVersion)
    $Override = [Environment]::GetEnvironmentVariable("SCROZZ_MSIX_VERSION")
    if (-not [String]::IsNullOrWhiteSpace($Override)) {
        $Candidate = $Override
    } else {
        $Candidate = (($ApplicationVersion -split "-", 2)[0] + ".0")
    }
    if ($Candidate -notmatch "^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$") {
        throw "SCROZZ_MSIX_VERSION must contain four numeric parts: $Candidate"
    }
    foreach ($Part in $Candidate.Split(".")) {
        $Number = [UInt32]::Parse($Part, [Globalization.CultureInfo]::InvariantCulture)
        if ($Number -gt 65535) {
            throw "Every MSIX version component must be at most 65535: $Candidate"
        }
    }
    return $Candidate
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
        [bool] $Signed,
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
        ocr_backend = $OcrBackend
        package_identity = $IdentityName
        signed = $Signed
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
$MsixVersion = Convert-ToMsixVersion $Version
$MsixArchitecture = if ($Architecture -eq "x86_64") { "x64" } else { "arm64" }

if ($PackageIdentityName -notmatch "^[0-9A-Za-z.-]{3,50}$") {
    throw "SCROZZ_MSIX_IDENTITY_NAME is not a valid package identity name"
}
Assert-SingleLine $PackagePublisher "MSIX Publisher" 8192
Assert-SingleLine $PublisherDisplayName "MSIX PublisherDisplayName" 256

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
        -Signed $false `
        -IdentityName ""
    Write-ArtifactMetadata `
        -Artifact $Msix `
        -Platform "windows-$Architecture" `
        -PackageKind "msix" `
        -OcrBackend "windows-media-ocr" `
        -Signed $Signed `
        -IdentityName $PackageIdentityName
} finally {
    if (Test-Path -LiteralPath $Scratch) {
        Remove-Item -LiteralPath $Scratch -Recurse -Force
    }
}
