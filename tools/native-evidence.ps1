# Preserve one native Windows runtime command without interpreting its result.
[CmdletBinding(PositionalBinding = $false)]
param(
    [Parameter(Mandatory = $true)]
    [string]$Output,

    [Parameter(Mandatory = $true)]
    [string]$Label,

    [string]$SourceSha,

    [string]$Artifact,

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Command
)

$ErrorActionPreference = 'Stop'

if (-not [IO.Path]::IsPathRooted($Output)) {
    throw '-Output must be an absolute path'
}
if ([string]::IsNullOrEmpty($Label)) {
    throw '-Label cannot be empty'
}
if ($Label.Contains("`n") -or $Label.Contains("`t")) {
    throw '-Label cannot contain tabs or newlines'
}
if ($Command.Count -gt 0 -and $Command[0] -eq '--') {
    $Command = @($Command | Select-Object -Skip 1)
}
if ($Command.Count -eq 0) {
    throw 'A command is required after --'
}
if (Test-Path -LiteralPath $Output) {
    throw "Output already exists: $Output"
}
if ($Artifact -and -not (Test-Path -LiteralPath $Artifact -PathType Leaf)) {
    throw "Artifact is not a file: $Artifact"
}

$repoRoot = (& git -C $PSScriptRoot rev-parse --show-toplevel 2>$null)
if ($LASTEXITCODE -ne 0 -or -not $repoRoot) {
    throw 'native-evidence.ps1 must run from a Git worktree'
}
$repoRoot = [IO.Path]::GetFullPath($repoRoot.Trim())
$Output = [IO.Path]::GetFullPath($Output)
$repoPrefix = $repoRoot.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
if ($Output.StartsWith($repoPrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw '-Output must be outside the repository'
}

if (-not $SourceSha) {
    $SourceSha = (& git -C $repoRoot rev-parse HEAD 2>$null)
    if ($LASTEXITCODE -ne 0 -or -not $SourceSha) {
        $SourceSha = 'unknown'
    }
}

$null = Get-Command $Command[0] -ErrorAction Stop
$parent = Split-Path -Parent $Output
$null = New-Item -ItemType Directory -Path $parent -Force
$null = New-Item -ItemType Directory -Path $Output

$stdout = Join-Path $Output 'stdout.log'
$stderr = Join-Path $Output 'stderr.log'
$commandArgs = if ($Command.Count -gt 1) {
    @($Command | Select-Object -Skip 1)
} else {
    @()
}

$environment = [ordered]@{}
@(
    'DBUS_SESSION_BUS_ADDRESS',
    'DESKTOP_SESSION',
    'DISPLAY',
    'PROCESSOR_ARCHITECTURE',
    'SESSIONNAME',
    'WAYLAND_DISPLAY',
    'XDG_CURRENT_DESKTOP',
    'XDG_RUNTIME_DIR',
    'XDG_SESSION_TYPE'
) | ForEach-Object {
    $value = [Environment]::GetEnvironmentVariable($_)
    if ($null -ne $value) {
        $environment[$_] = $value
    }
}

$sourceStatus = @(& git -C $repoRoot status --porcelain=v1 2>$null)
$artifactRecord = $null
if ($Artifact) {
    $artifactPath = [IO.Path]::GetFullPath($Artifact)
    $artifactRecord = [ordered]@{
        path = $artifactPath
        sha256 = (Get-FileHash -LiteralPath $artifactPath -Algorithm SHA256).Hash.ToLowerInvariant()
    }
}

$previousErrorAction = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
& $Command[0] @commandArgs 1> $stdout 2> $stderr
$status = if ($null -ne $LASTEXITCODE) {
    $LASTEXITCODE
} elseif ($?) {
    0
} else {
    1
}
$ErrorActionPreference = $previousErrorAction

$skipPattern = '(?i)(^|[^a-z])skip(ped)?([^a-z]|$)'
$skipMarker = $false
foreach ($log in @($stdout, $stderr)) {
    if ((Get-Item -LiteralPath $log).Length -gt 0 -and
        (Select-String -LiteralPath $log -Pattern $skipPattern -Quiet)) {
        $skipMarker = $true
    }
}

$manifest = [ordered]@{
    schema = 1
    classification = 'unreviewed'
    label = $Label
    captured_utc = [DateTime]::UtcNow.ToString('o')
    source_sha = $SourceSha.Trim()
    source_dirty = $sourceStatus.Count -gt 0
    host = [ordered]@{
        os = [Environment]::OSVersion.VersionString
        architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
        powershell = $PSVersionTable.PSVersion.ToString()
    }
    session_environment = $environment
    artifact = $artifactRecord
    command = $Command
    command_exit = $status
    skip_marker = if ($skipMarker) { 'present' } else { 'absent' }
}
$manifest | ConvertTo-Json -Depth 5 |
    Set-Content -LiteralPath (Join-Path $Output 'run.json') -Encoding utf8

Write-Host "native-evidence: retained $Output"
Write-Host "native-evidence: command exit $status; classification remains unreviewed"
if ($skipMarker) {
    Write-Warning 'Skip marker found; this run is not pass evidence'
}

exit $status
