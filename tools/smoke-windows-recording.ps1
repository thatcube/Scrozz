# Manual WGC + Media Foundation + WASAPI smoke test for a native Windows desktop.
[CmdletBinding()]
param(
    [double]$Seconds = 4,
    [string]$DisplayId = "\\.\DISPLAY1",
    [string]$Output = "",
    [switch]$NoSystemAudio,
    [switch]$Microphone,
    [switch]$NoCursor,
    [ValidateSet("low", "balanced", "high")]
    [string]$Quality = "balanced",
    [string]$Resolution = "native"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if ($env:OS -ne "Windows_NT") {
    throw "This smoke test requires a native Windows desktop session."
}
if ($Seconds -lt 1) {
    throw "Seconds must be at least 1."
}

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
if ([string]::IsNullOrWhiteSpace($Output)) {
    $stamp = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
    $Output = Join-Path $root "scrozz-windows-smoke-$stamp.mp4"
}
$Output = [IO.Path]::GetFullPath($Output)
if (Test-Path -LiteralPath $Output) {
    throw "Refusing to overwrite existing output: $Output"
}

$arguments = @(
    "run", "-p", "scrozz-record",
    "--example", "windows_recording_smoke", "--",
    "--display", $DisplayId,
    "--seconds", $Seconds.ToString([Globalization.CultureInfo]::InvariantCulture),
    "--quality", $Quality,
    "--resolution", $Resolution,
    "--output", $Output
)
if (!$NoSystemAudio) { $arguments += "--system-audio" }
if ($Microphone) { $arguments += "--microphone" }
if (!$NoCursor) { $arguments += "--cursor" }

& cargo @arguments
if ($LASTEXITCODE -ne 0) {
    throw "Windows recording smoke test failed with exit code $LASTEXITCODE."
}
if (!(Test-Path -LiteralPath $Output)) {
    throw "Recording reported success but did not create $Output."
}
$file = Get-Item -LiteralPath $Output
if ($file.Length -lt 1024) {
    throw "Recording output is unexpectedly small ($($file.Length) bytes)."
}

Write-Host "Windows recording smoke test passed: $Output ($($file.Length) bytes)"
