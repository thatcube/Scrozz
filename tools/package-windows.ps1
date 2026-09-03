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
    [string] $TesseractPayloadManifest = "",

    [Parameter()]
    [string] $IdentityMode = $env:SCROZZ_MSIX_IDENTITY_MODE
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$RepoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$Binary = [IO.Path]::GetFullPath($Binary)
$OutputRoot = [IO.Path]::GetPathRoot($OutputDirectory)
$ProductionPayloadManifest = Join-Path `
    $RepoRoot "packaging\windows\tesseract-payload.json"
if ([String]::IsNullOrWhiteSpace($TesseractPayloadManifest)) {
    $TesseractPayloadManifest = $ProductionPayloadManifest
}
$TesseractPayloadManifest = [IO.Path]::GetFullPath($TesseractPayloadManifest)
$ProductionPayloadManifest = [IO.Path]::GetFullPath($ProductionPayloadManifest)

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
        "(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?" +
        "(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$"
    )) {
    throw "Invalid application version: $Version"
}
$VersionWithoutBuild = ($Version -split "\+", 2)[0]
if ($VersionWithoutBuild.Contains("-")) {
    $Prerelease = ($VersionWithoutBuild -split "-", 2)[1]
    foreach ($Identifier in $Prerelease.Split(".")) {
        if ($Identifier -match "^0[0-9]+$") {
            throw (
                "Numeric prerelease identifiers cannot have leading zeroes: $Version"
            )
        }
    }
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

function Get-CanonicalPayloadContract {
    param([string] $ManifestPath)
    $Manifest = Get-Content -LiteralPath $ManifestPath -Raw | ConvertFrom-Json
    $Lines = @(
        foreach ($Entry in @($Manifest.payload_files) | Sort-Object {
            ([string] $_.path).ToLowerInvariant()
        }) {
            "{0}`t{1}" -f (
                ([string] $Entry.path).ToLowerInvariant()
            ), (
                ([string] $Entry.sha256).ToLowerInvariant()
            )
        }
    )
    return ($Lines -join "`n")
}

function Convert-ToMsixVersion {
    param([string] $ApplicationVersion)
    $Override = [Environment]::GetEnvironmentVariable("SCROZZ_MSIX_VERSION")
    if (-not [String]::IsNullOrWhiteSpace($Override)) {
        $Candidate = $Override
    } else {
        $VersionWithoutBuild = ($ApplicationVersion -split "\+", 2)[0]
        if ($VersionWithoutBuild.Contains("-")) {
            throw (
                "Prerelease artifacts require SCROZZ_MSIX_VERSION so distinct " +
                "prereleases cannot collapse to one Windows package version"
            )
        }
        $CoreParts = $VersionWithoutBuild.Split(".")
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

    $ProgramFilesX86 = [Environment]::GetEnvironmentVariable("ProgramFiles(x86)")
    if ([String]::IsNullOrWhiteSpace($ProgramFilesX86)) {
        throw "Windows did not report its Program Files (x86) directory"
    }
    $SdkRoot = Join-Path $ProgramFilesX86 "Windows Kits\10"
    $NativeArchitecture = if (
        $env:PROCESSOR_ARCHITECTURE -eq "ARM64" -or
        $env:PROCESSOR_ARCHITEW6432 -eq "ARM64"
    ) {
        "arm64"
    } else {
        "x64"
    }
    $ToolArchitectures = @($NativeArchitecture)
    if ($NativeArchitecture -ne "x64") {
        $ToolArchitectures += "x64"
    }
    $SdkDirectories = @(
        Get-ChildItem `
            -LiteralPath (Join-Path $SdkRoot "bin") `
            -Directory `
            -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending
    )
    foreach ($ToolArchitecture in $ToolArchitectures) {
        foreach ($Directory in $SdkDirectories) {
            $Candidate = Join-Path `
                $Directory.FullName "$ToolArchitecture\${Name}.exe"
            if (Test-Path -LiteralPath $Candidate -PathType Leaf) {
                return $Candidate
            }
        }
    }
    $Command = Get-Command "${Name}.exe" -ErrorAction SilentlyContinue
    if ($null -ne $Command) {
        return $Command.Source
    }
    if ($Name -eq "makeappx") {
        $CertificationKit = Join-Path $SdkRoot "App Certification Kit\makeappx.exe"
        if (Test-Path -LiteralPath $CertificationKit -PathType Leaf) {
            return $CertificationKit
        }
    }
    throw "${Name}.exe was not found. Install the Windows SDK or set $OverrideVariable."
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
        $Lines.Add(('"{0}" "{1}"' -f $File, $Relative))
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

function Find-ZipEndOfCentralDirectory {
    param(
        [IO.FileStream] $Stream,
        [IO.BinaryReader] $Reader,
        [string] $Path
    )
    if ($Stream.Length -lt 22) {
        throw "MSIX is too short to contain a ZIP end record: $Path"
    }

    $SearchLength = [int] [Math]::Min([Int64] 65557, [Int64] $Stream.Length)
    $TailStart = $Stream.Length - $SearchLength
    $Stream.Position = $TailStart
    [byte[]] $Tail = $Reader.ReadBytes($SearchLength)
    if ($Tail.Length -ne $SearchLength) {
        throw "Could not read the ZIP end record from $Path"
    }

    for ($Index = $Tail.Length - 22; $Index -ge 0; $Index--) {
        if ([BitConverter]::ToUInt32($Tail, $Index) -ne [UInt32] 0x06054b50) {
            continue
        }
        $CommentLength = [BitConverter]::ToUInt16($Tail, $Index + 20)
        if ($Index + 22 + $CommentLength -eq $Tail.Length) {
            return [Int64] ($TailStart + $Index)
        }
    }
    throw "MSIX has no valid ZIP end record: $Path"
}

function Get-Zip64LocalHeaderOffset {
    param(
        [byte[]] $Extra,
        [UInt32] $CompressedSize,
        [UInt32] $UncompressedSize
    )
    $Cursor = 0
    while ($Cursor -lt $Extra.Length) {
        if ($Cursor + 4 -gt $Extra.Length) {
            throw "MSIX contains a truncated ZIP extra-field header"
        }
        $HeaderId = [BitConverter]::ToUInt16($Extra, $Cursor)
        $DataLength = [BitConverter]::ToUInt16($Extra, $Cursor + 2)
        $DataStart = $Cursor + 4
        $DataEnd = $DataStart + $DataLength
        if ($DataEnd -gt $Extra.Length) {
            throw "MSIX contains a truncated ZIP extra field"
        }
        if ($HeaderId -eq 0x0001) {
            $ValueOffset = $DataStart
            if ($UncompressedSize -eq [UInt32]::MaxValue) {
                $ValueOffset += 8
            }
            if ($CompressedSize -eq [UInt32]::MaxValue) {
                $ValueOffset += 8
            }
            if ($ValueOffset + 8 -gt $DataEnd) {
                throw "MSIX ZIP64 extra field has no local-header offset"
            }
            return [BitConverter]::ToUInt64($Extra, $ValueOffset)
        }
        $Cursor = $DataEnd
    }
    throw "MSIX central entry has no required ZIP64 extra field"
}

function Normalize-MsixZipTimestamps {
    param([string] $Path)
    if (-not [BitConverter]::IsLittleEndian) {
        throw "MSIX timestamp normalization requires a little-endian host"
    }

    $Stream = $null
    $Reader = $null
    try {
        $Stream = [IO.File]::Open(
            $Path,
            [IO.FileMode]::Open,
            [IO.FileAccess]::ReadWrite,
            [IO.FileShare]::None
        )
        $Reader = [IO.BinaryReader]::new(
            $Stream,
            [Text.Encoding]::UTF8,
            $true
        )
        $EndOffset = Find-ZipEndOfCentralDirectory $Stream $Reader $Path
        $Stream.Position = $EndOffset + 4
        $DiskNumber = $Reader.ReadUInt16()
        $CentralDisk = $Reader.ReadUInt16()
        $EntriesOnDisk16 = $Reader.ReadUInt16()
        $EntryCount16 = $Reader.ReadUInt16()
        $CentralSize32 = $Reader.ReadUInt32()
        $CentralOffset32 = $Reader.ReadUInt32()
        $CommentLength = $Reader.ReadUInt16()
        if ($EndOffset + 22 + $CommentLength -ne $Stream.Length) {
            throw "MSIX ZIP end record has an inconsistent comment length"
        }

        [UInt64] $EntryCount = $EntryCount16
        [UInt64] $EntriesOnDisk = $EntriesOnDisk16
        [UInt64] $CentralSize = $CentralSize32
        [UInt64] $CentralOffset = $CentralOffset32
        # MakeAppx uses ZIP64 sentinels in the classic end record even for a
        # single-disk package; the ZIP64 record below carries the real values.
        $UsesZip64 = (
            $DiskNumber -eq [UInt16]::MaxValue -or
            $CentralDisk -eq [UInt16]::MaxValue -or
            $EntriesOnDisk16 -eq [UInt16]::MaxValue -or
            $EntryCount16 -eq [UInt16]::MaxValue -or
            $CentralSize32 -eq [UInt32]::MaxValue -or
            $CentralOffset32 -eq [UInt32]::MaxValue
        )
        if ($UsesZip64) {
            if (($DiskNumber -ne 0 -and
                    $DiskNumber -ne [UInt16]::MaxValue) -or
                ($CentralDisk -ne 0 -and
                    $CentralDisk -ne [UInt16]::MaxValue)) {
                throw "Multi-disk ZIP archives cannot be normalized as MSIX"
            }
            if ($EndOffset -lt 20) {
                throw "MSIX ZIP64 archive has no end-record locator"
            }
            $Stream.Position = $EndOffset - 20
            if ($Reader.ReadUInt32() -ne [UInt32] 0x07064b50) {
                throw "MSIX ZIP64 archive has no end-record locator"
            }
            $Zip64Disk = $Reader.ReadUInt32()
            $Zip64EndOffset = $Reader.ReadUInt64()
            $Zip64DiskCount = $Reader.ReadUInt32()
            if ($Zip64Disk -ne 0 -or $Zip64DiskCount -ne 1) {
                throw "Multi-disk ZIP64 archives cannot be normalized as MSIX"
            }
            if ($Zip64EndOffset -gt [UInt64] [Int64]::MaxValue) {
                throw "MSIX ZIP64 end record is outside the supported file range"
            }

            $Stream.Position = [Int64] $Zip64EndOffset
            if ($Reader.ReadUInt32() -ne [UInt32] 0x06064b50) {
                throw "MSIX ZIP64 end-record signature is invalid"
            }
            $Zip64RecordSize = $Reader.ReadUInt64()
            if ($Zip64RecordSize -lt 44) {
                throw "MSIX ZIP64 end record is truncated"
            }
            $null = $Reader.ReadUInt16()
            $null = $Reader.ReadUInt16()
            $Zip64DiskNumber = $Reader.ReadUInt32()
            $Zip64CentralDisk = $Reader.ReadUInt32()
            $EntriesOnDisk = $Reader.ReadUInt64()
            $EntryCount = $Reader.ReadUInt64()
            $CentralSize = $Reader.ReadUInt64()
            $CentralOffset = $Reader.ReadUInt64()
            if ($Zip64DiskNumber -ne 0 -or
                $Zip64CentralDisk -ne 0 -or
                $EntriesOnDisk -ne $EntryCount) {
                throw "Multi-disk ZIP64 archives cannot be normalized as MSIX"
            }
        } else {
            if ($DiskNumber -ne 0 -or
                $CentralDisk -ne 0 -or
                $EntriesOnDisk -ne $EntryCount) {
                throw "Multi-disk ZIP archives cannot be normalized as MSIX"
            }
        }

        $ArchiveLength = [UInt64] $Stream.Length
        $MaximumStreamOffset = [UInt64] [Int64]::MaxValue
        if ($CentralOffset -gt $MaximumStreamOffset -or
            $CentralSize -gt $MaximumStreamOffset -or
            $CentralOffset -gt ([UInt64]::MaxValue - $CentralSize)) {
            throw "MSIX central-directory range is outside the supported file range"
        }
        [UInt64] $CentralEnd = $CentralOffset + $CentralSize
        if ($CentralEnd -gt $ArchiveLength -or
            $CentralEnd -gt [UInt64] $EndOffset) {
            throw "MSIX central directory extends beyond the archive"
        }

        $CentralTimestampOffsets = [Collections.Generic.List[Int64]]::new()
        $LocalHeaderOffsets = [Collections.Generic.List[Int64]]::new()
        $Stream.Position = [Int64] $CentralOffset
        for ([UInt64] $Index = 0; $Index -lt $EntryCount; $Index++) {
            $CentralHeaderOffset = $Stream.Position
            if ([UInt64] ($CentralHeaderOffset + 46) -gt $CentralEnd -or
                $Reader.ReadUInt32() -ne [UInt32] 0x02014b50) {
                throw "MSIX central-directory entry $Index is invalid"
            }
            $null = $Reader.ReadUInt16()
            $null = $Reader.ReadUInt16()
            $null = $Reader.ReadUInt16()
            $null = $Reader.ReadUInt16()
            $null = $Reader.ReadUInt16()
            $null = $Reader.ReadUInt16()
            $null = $Reader.ReadUInt32()
            $CompressedSize = $Reader.ReadUInt32()
            $UncompressedSize = $Reader.ReadUInt32()
            $NameLength = $Reader.ReadUInt16()
            $ExtraLength = $Reader.ReadUInt16()
            $EntryCommentLength = $Reader.ReadUInt16()
            $EntryDisk = $Reader.ReadUInt16()
            $null = $Reader.ReadUInt16()
            $null = $Reader.ReadUInt32()
            $LocalOffset32 = $Reader.ReadUInt32()
            if ($EntryDisk -ne 0) {
                throw "Multi-disk ZIP entries cannot be normalized as MSIX"
            }

            $RecordEnd = (
                $CentralHeaderOffset + 46 +
                $NameLength + $ExtraLength + $EntryCommentLength
            )
            if ([UInt64] $RecordEnd -gt $CentralEnd) {
                throw "MSIX central-directory entry $Index is truncated"
            }
            [byte[]] $NameBytes = $Reader.ReadBytes($NameLength)
            [byte[]] $Extra = $Reader.ReadBytes($ExtraLength)
            [byte[]] $EntryComment = $Reader.ReadBytes($EntryCommentLength)
            if ($NameBytes.Length -ne $NameLength -or
                $Extra.Length -ne $ExtraLength -or
                $EntryComment.Length -ne $EntryCommentLength) {
                throw "MSIX central-directory entry $Index is truncated"
            }

            [UInt64] $LocalOffset = $LocalOffset32
            if ($LocalOffset32 -eq [UInt32]::MaxValue) {
                $LocalOffset = Get-Zip64LocalHeaderOffset `
                    $Extra $CompressedSize $UncompressedSize
            }
            if ($LocalOffset -gt $MaximumStreamOffset) {
                throw "MSIX local-header offset is outside the supported file range"
            }
            $CentralTimestampOffsets.Add($CentralHeaderOffset + 12)
            $LocalHeaderOffsets.Add([Int64] $LocalOffset)
        }
        if ([UInt64] $Stream.Position -ne $CentralEnd) {
            throw "MSIX central-directory size does not match its entries"
        }

        $LocalTimestampOffsets = [Collections.Generic.List[Int64]]::new()
        foreach ($LocalHeaderOffset in $LocalHeaderOffsets) {
            if ($LocalHeaderOffset -lt 0 -or
                $LocalHeaderOffset + 30 -gt $Stream.Length) {
                throw "MSIX local-file header is outside the archive"
            }
            $Stream.Position = $LocalHeaderOffset
            if ($Reader.ReadUInt32() -ne [UInt32] 0x04034b50) {
                throw "MSIX local-file header signature is invalid"
            }
            $LocalTimestampOffsets.Add($LocalHeaderOffset + 10)
        }

        # 1980-01-01 00:00:00 is the earliest valid MS-DOS ZIP timestamp.
        foreach ($TimestampOffset in @(
            $CentralTimestampOffsets
            $LocalTimestampOffsets
        )) {
            $Stream.Position = $TimestampOffset
            $Stream.WriteByte([byte] 0x00)
            $Stream.WriteByte([byte] 0x00)
            $Stream.WriteByte([byte] 0x21)
            $Stream.WriteByte([byte] 0x00)
        }
        $Stream.Flush($true)
    } finally {
        if ($null -ne $Reader) {
            $Reader.Dispose()
        }
        if ($null -ne $Stream) {
            $Stream.Dispose()
        }
    }
}

function Confirm-MakeAppxPackage {
    param(
        [string] $Tool,
        [string] $Package,
        [string] $Destination
    )
    if (Test-Path -LiteralPath $Destination) {
        Remove-Item -LiteralPath $Destination -Recurse -Force
    }
    & $Tool unpack /o /p $Package /d $Destination
    if ($LASTEXITCODE -ne 0) {
        throw "MakeAppx could not unpack the normalized MSIX (status $LASTEXITCODE)"
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
        [string] $IdentityName,
        [string] $IdentityMode,
        [bool] $TestSignature
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
        package_identity_mode = $IdentityMode
        test_signature = $TestSignature
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

$PackageIdentityName = [Environment]::GetEnvironmentVariable(
    "SCROZZ_MSIX_IDENTITY_NAME"
)
$PackagePublisher = [Environment]::GetEnvironmentVariable("SCROZZ_MSIX_PUBLISHER")
$PublisherDisplayName = [Environment]::GetEnvironmentVariable(
    "SCROZZ_MSIX_PUBLISHER_DISPLAY_NAME"
)
$SignPfx = [Environment]::GetEnvironmentVariable("SCROZZ_MSIX_SIGN_PFX")
$SignThumbprint = [Environment]::GetEnvironmentVariable("SCROZZ_MSIX_SIGN_CERT_SHA1")
$AllowUntrustedTestSignature = (
    [Environment]::GetEnvironmentVariable(
        "SCROZZ_TEST_ALLOW_UNTRUSTED_SIGNATURE"
    ) -eq "1"
)
if ($AllowUntrustedTestSignature) {
    if ($IdentityMode -ne "store") {
        throw (
            "SCROZZ_TEST_ALLOW_UNTRUSTED_SIGNATURE is valid only in store " +
            "identity mode"
        )
    }
    if ($Stamp -notmatch "(^|[._-])test([._-]|$)") {
        throw (
            "SCROZZ_TEST_ALLOW_UNTRUSTED_SIGNATURE requires a test-labelled " +
            "stamp segment"
        )
    }
    $CandidatePayloadContract = Get-CanonicalPayloadContract $TesseractPayloadManifest
    $ProductionPayloadContract = Get-CanonicalPayloadContract $ProductionPayloadManifest
    if ($CandidatePayloadContract -eq $ProductionPayloadContract) {
        throw (
            "SCROZZ_TEST_ALLOW_UNTRUSTED_SIGNATURE requires a non-production " +
            "payload manifest"
        )
    }
}
if ($IdentityMode -notin @("development", "store")) {
    throw (
        "SCROZZ_MSIX_IDENTITY_MODE must explicitly be development or store; " +
        "the packager will not silently select a weaker identity"
    )
}
if ($IdentityMode -eq "development") {
    if (-not [String]::IsNullOrWhiteSpace($PackageIdentityName) -or
        -not [String]::IsNullOrWhiteSpace($PackagePublisher) -or
        -not [String]::IsNullOrWhiteSpace($PublisherDisplayName) -or
        -not [String]::IsNullOrWhiteSpace($SignPfx) -or
        -not [String]::IsNullOrWhiteSpace($SignThumbprint)) {
        throw (
            "development identity mode rejects Store identity and signing " +
            "overrides; select store mode explicitly for release identity"
        )
    }
    $PackageIdentityName = "com.thatcube.Scrozz"
    $PackagePublisher = "CN=Scrozz Development"
    $PublisherDisplayName = "Scrozz Development"
} else {
    foreach ($RequiredIdentity in ([ordered]@{
        SCROZZ_MSIX_IDENTITY_NAME = $PackageIdentityName
        SCROZZ_MSIX_PUBLISHER = $PackagePublisher
        SCROZZ_MSIX_PUBLISHER_DISPLAY_NAME = $PublisherDisplayName
    }).GetEnumerator()) {
        if ([String]::IsNullOrWhiteSpace([string] $RequiredIdentity.Value)) {
            throw (
                "$($RequiredIdentity.Key) is required in store identity mode; " +
                "development identity values are never a release fallback"
            )
        }
    }
    if ([String]::IsNullOrWhiteSpace($SignPfx) -and
        [String]::IsNullOrWhiteSpace($SignThumbprint)) {
        throw (
            "store identity mode requires SCROZZ_MSIX_SIGN_PFX or " +
            "SCROZZ_MSIX_SIGN_CERT_SHA1"
        )
    }
}
if (-not [String]::IsNullOrWhiteSpace($SignPfx) -and
    -not [String]::IsNullOrWhiteSpace($SignThumbprint)) {
    throw "Set only one of SCROZZ_MSIX_SIGN_PFX and SCROZZ_MSIX_SIGN_CERT_SHA1"
}
$ResolvedSignPfx = $null
$SignPassword = [Environment]::GetEnvironmentVariable(
    "SCROZZ_MSIX_SIGN_PFX_PASSWORD"
)
$SigningCertificate = $null
if (-not [String]::IsNullOrWhiteSpace($SignPfx)) {
    $ResolvedSignPfx = [IO.Path]::GetFullPath($SignPfx)
    if (-not (Test-Path -LiteralPath $ResolvedSignPfx -PathType Leaf)) {
        throw "SCROZZ_MSIX_SIGN_PFX does not name a file: $ResolvedSignPfx"
    }
    $Certificates = [Security.Cryptography.X509Certificates.X509Certificate2Collection]::new()
    $Certificates.Import(
        $ResolvedSignPfx,
        $SignPassword,
        [Security.Cryptography.X509Certificates.X509KeyStorageFlags]::EphemeralKeySet
    )
    $SigningCertificate = @(
        $Certificates | Where-Object { $_.HasPrivateKey }
    ) | Select-Object -First 1
    if ($null -eq $SigningCertificate) {
        throw "SCROZZ_MSIX_SIGN_PFX contains no certificate with a private key"
    }
} elseif (-not [String]::IsNullOrWhiteSpace($SignThumbprint) -and
    $SignThumbprint -notmatch "^[0-9A-Fa-f]{40}$") {
    throw "SCROZZ_MSIX_SIGN_CERT_SHA1 must be a 40-digit certificate thumbprint"
}
$TesseractPayloadFiles = @(
    Confirm-TesseractPayload `
        -Directory $TesseractDirectory `
        -ManifestPath $TesseractPayloadManifest
)
$MsixVersion = Convert-ToMsixVersion $Version
$MsixArchitecture = if ($Architecture -eq "x86_64") { "x64" } else { "arm64" }
$PayloadSignature = Get-AuthenticodeSignature -FilePath $Binary
$PayloadHasEmbeddedSignature = (
    [string] $PayloadSignature.SignatureType -eq "Authenticode"
)
$PayloadSigned = (
    $PayloadHasEmbeddedSignature -and (
        $PayloadSignature.Status -eq "Valid" -or
        (
            $AllowUntrustedTestSignature -and
            $null -ne $PayloadSignature.SignerCertificate -and
            $PayloadSignature.Status -in @("NotTrusted", "UnknownError")
        )
    )
)
if ($IdentityMode -eq "store" -and
    (-not $PayloadSigned -or $null -eq $PayloadSignature.SignerCertificate)) {
    throw "store identity mode requires a valid Authenticode signature on scrozz.exe"
}
$ExpectedThumbprint = $null
if ($IdentityMode -eq "store") {
    $ExpectedThumbprint = if ($null -ne $SigningCertificate) {
        $SigningCertificate.Thumbprint
    } else {
        $SignThumbprint
    }
    $PayloadSigner = $PayloadSignature.SignerCertificate
    if (-not $PayloadSigner.Thumbprint.Equals(
            $ExpectedThumbprint,
            [StringComparison]::OrdinalIgnoreCase
        )) {
        throw (
            "scrozz.exe is signed by $($PayloadSigner.Thumbprint), but the MSIX " +
            "signing identity is $ExpectedThumbprint"
        )
    }
    if (-not $PayloadSigner.Subject.Equals(
            $PackagePublisher,
            [StringComparison]::OrdinalIgnoreCase
        )) {
        throw (
            "the scrozz.exe signer subject '$($PayloadSigner.Subject)' does not " +
            "match SCROZZ_MSIX_PUBLISHER '$PackagePublisher'"
        )
    }
    if ($null -ne $SigningCertificate -and
        -not $SigningCertificate.Subject.Equals(
            $PackagePublisher,
            [StringComparison]::OrdinalIgnoreCase
        )) {
        throw (
            "the MSIX signing certificate subject '$($SigningCertificate.Subject)' " +
            "does not match SCROZZ_MSIX_PUBLISHER '$PackagePublisher'"
        )
    }
}

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
$ScratchToken = [Guid]::NewGuid().ToString("N").Substring(0, 8)
$Scratch = Join-Path $OutputDirectory (
    ".scz-{0}-{1}" -f $PID, $ScratchToken
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
    Normalize-MsixZipTimestamps $MsixStaged
    Confirm-MakeAppxPackage `
        -Tool $MakeAppx `
        -Package $MsixStaged `
        -Destination (Join-Path $Scratch "msix-validation")

    $VerifyDeterminism = (
        [Environment]::GetEnvironmentVariable("SCROZZ_WINDOWS_VERIFY_DETERMINISM") -eq "1" -or
        [Environment]::GetEnvironmentVariable("SCROZZ_MSIX_VERIFY_DETERMINISM") -eq "1"
    )
    if ($VerifyDeterminism) {
        $SecondZip = Join-Path $Scratch "determinism-check.zip"
        $SecondMsix = Join-Path $Scratch "determinism-check.msix"
        New-DeterministicZip -Source $PortableRoot -Destination $SecondZip
        Invoke-MakeAppx -Tool $MakeAppx -Mapping $Mapping -Destination $SecondMsix
        Normalize-MsixZipTimestamps $SecondMsix
        Assert-IdenticalArtifact $PortableStaged $SecondZip "Portable ZIP output"
        Assert-IdenticalArtifact $MsixStaged $SecondMsix "MSIX output"
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
            $SignArguments.Add("/f")
            $SignArguments.Add($ResolvedSignPfx)
            if (-not [String]::IsNullOrEmpty($SignPassword)) {
                $SignArguments.Add("/p")
                $SignArguments.Add($SignPassword)
            }
        } else {
            $SignArguments.Add("/sha1")
            $SignArguments.Add($SignThumbprint)
        }
        $Timestamp = Get-EnvironmentOrDefault `
            "SCROZZ_MSIX_TIMESTAMP_URL" "http://timestamp.digicert.com"
        if ($Timestamp -eq "none") {
            if ([Environment]::GetEnvironmentVariable(
                    "SCROZZ_ALLOW_UNTIMESTAMPED_SIGNING"
                ) -ne "1") {
                throw (
                    "SCROZZ_MSIX_TIMESTAMP_URL=none requires " +
                    "SCROZZ_ALLOW_UNTIMESTAMPED_SIGNING=1"
                )
            }
        } else {
            $SignArguments.Add("/tr")
            $SignArguments.Add($Timestamp)
            $SignArguments.Add("/td")
            $SignArguments.Add("SHA256")
        }
        $SignArguments.Add($MsixStaged)
        & $SignTool @SignArguments
        if ($LASTEXITCODE -ne 0) {
            throw "SignTool failed with status $LASTEXITCODE"
        }
        & $SignTool verify /pa $MsixStaged
        if ($LASTEXITCODE -ne 0) {
            if (-not $AllowUntrustedTestSignature) {
                throw "SignTool verification failed with status $LASTEXITCODE"
            }
            $PackageSignature = Get-AuthenticodeSignature -FilePath $MsixStaged
            if ($null -eq $PackageSignature.SignerCertificate -or
                [string] $PackageSignature.SignatureType -ne "Authenticode" -or
                -not $PackageSignature.SignerCertificate.Thumbprint.Equals(
                    $ExpectedThumbprint,
                    [StringComparison]::OrdinalIgnoreCase
                ) -or
                $PackageSignature.Status -notin @("Valid", "NotTrusted", "UnknownError")) {
                throw (
                    "the self-signed test MSIX did not retain the expected " +
                    "Authenticode signer after SignTool verification failed"
                )
            }
            # The expected self-signed trust failure must not become this
            # otherwise-successful script's process exit code.
            $global:LASTEXITCODE = 0
        }
        $Signed = $true
    } else {
        Write-Warning (
            "The explicitly selected development MSIX identity is unsigned. " +
            "Store identity mode requires a human-approved signing credential."
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
        -IdentityName "" `
        -IdentityMode "none" `
        -TestSignature $AllowUntrustedTestSignature
    Write-ArtifactMetadata `
        -Artifact $Msix `
        -Platform "windows-$Architecture" `
        -PackageKind "msix" `
        -OcrBackend "windows-media-ocr" `
        -NativePackageVersion $MsixVersion `
        -Signed $Signed `
        -PayloadSigned $PayloadSigned `
        -IdentityName $PackageIdentityName `
        -IdentityMode $IdentityMode `
        -TestSignature $AllowUntrustedTestSignature
} finally {
    if (Test-Path -LiteralPath $Scratch) {
        Remove-Item -LiteralPath $Scratch -Recurse -Force
    }
}
