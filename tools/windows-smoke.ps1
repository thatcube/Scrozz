# Native Windows smoke test for Scrozz's first vertical slice.
#
# Run from a Windows desktop session:
#
#   pwsh -File tools/windows-smoke.ps1
#
# Or point it at an already-built binary:
#
#   pwsh -File tools/windows-smoke.ps1 -Binary artifacts/scrozz.exe
#
# This is intentionally a CLI/headless probe. It does not automate egui or
# claim that the overlay behaves correctly; a person still has to verify focus,
# hit-testing and placement. What it does exercise is the same native display,
# capture, encoder, file and clipboard code the tray app's capture worker uses.

[CmdletBinding()]
param(
    [Parameter()]
    [string] $Binary = $env:SCROZZ_SMOKE_BINARY,

    [Parameter()]
    [ValidateSet("debug", "release")]
    [string] $Profile = "debug"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Assert-Smoke {
    param(
        [Parameter(Mandatory)]
        [bool] $Condition,

        [Parameter(Mandatory)]
        [string] $Message
    )

    if (-not $Condition) {
        throw "Windows smoke test failed: $Message"
    }
}

function Read-BigEndianUInt32 {
    param(
        [Parameter(Mandatory)]
        [byte[]] $Bytes,

        [Parameter(Mandatory)]
        [int] $Offset
    )

    return [uint32] (
        ([uint64] $Bytes[$Offset] * 16777216) +
        ([uint64] $Bytes[$Offset + 1] * 65536) +
        ([uint64] $Bytes[$Offset + 2] * 256) +
        [uint64] $Bytes[$Offset + 3]
    )
}

function Read-PngDimensions {
    param(
        [Parameter(Mandatory)]
        [string] $Path
    )

    $bytes = [System.IO.File]::ReadAllBytes($Path)
    Assert-Smoke ($bytes.Length -ge 24) "the capture is too short to contain a PNG IHDR"

    [byte[]] $signature = 137, 80, 78, 71, 13, 10, 26, 10
    for ($i = 0; $i -lt $signature.Length; $i++) {
        Assert-Smoke ($bytes[$i] -eq $signature[$i]) "the capture has an invalid PNG signature"
    }

    $chunkName = [System.Text.Encoding]::ASCII.GetString($bytes, 12, 4)
    Assert-Smoke ($chunkName -eq "IHDR") "the first PNG chunk is '$chunkName', not IHDR"
    Assert-Smoke ((Read-BigEndianUInt32 $bytes 8) -eq 13) "the PNG IHDR has the wrong length"

    return [pscustomobject] @{
        Width = Read-BigEndianUInt32 $bytes 16
        Height = Read-BigEndianUInt32 $bytes 20
    }
}

function Invoke-ScrozzJson {
    param(
        [Parameter(Mandatory)]
        [string] $Executable,

        [Parameter(Mandatory)]
        [string[]] $Arguments,

        [Parameter(Mandatory)]
        [string] $Name,

        [Parameter(Mandatory)]
        [string] $Scratch
    )

    $stderrPath = Join-Path $Scratch "$Name.stderr.log"
    $allArguments = @("--json", "--no-ipc") + $Arguments
    $stdoutLines = & $Executable @allArguments 2> $stderrPath
    $exitCode = $LASTEXITCODE
    $stdout = [string]::Join([Environment]::NewLine, @($stdoutLines))
    $stderr = if (Test-Path -LiteralPath $stderrPath) {
        [System.IO.File]::ReadAllText($stderrPath)
    } else {
        ""
    }

    if (-not [string]::IsNullOrWhiteSpace($stderr)) {
        Write-Host $stderr.TrimEnd()
    }

    Assert-Smoke (-not [string]::IsNullOrWhiteSpace($stdout)) `
        "'scrozz $($Arguments -join ' ')' wrote no JSON"

    try {
        $document = $stdout | ConvertFrom-Json
    } catch {
        throw "Windows smoke test failed: '$Name' wrote invalid JSON: $stdout"
    }

    if ($exitCode -ne 0 -or -not $document.ok) {
        $detail = if ($null -ne $document.error) {
            $document.error | ConvertTo-Json -Compress -Depth 8
        } else {
            $stdout
        }
        throw "Windows smoke test failed: '$Name' exited $exitCode`: $detail"
    }

    # This was the merge-review finding that prompted the apartment work. WGC
    # may legitimately be unsupported and fall back to GDI, but it must never
    # do so because Scrozz forgot to initialise the calling thread.
    Assert-Smoke (
        $stderr -notmatch "CO_E_NOTINITIALIZED|no COM apartment|thread with no COM apartment|has not entered a COM apartment"
    ) "'$Name' reached WinRT without a COM apartment"

    return [pscustomobject] @{
        Json = $document
        Stderr = $stderr
    }
}

function Read-ClipboardImageDimensions {
    param(
        [Parameter(Mandatory)]
        [string] $Scratch
    )

    # System.Windows.Forms.Clipboard requires an STA. A GitHub Actions `pwsh`
    # process and many VM shells are MTA, so probe from a short-lived Windows
    # PowerShell STA rather than accepting a false negative.
    $probePath = Join-Path $Scratch "clipboard-probe.ps1"
    $probe = @'
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Windows.Forms

$lastError = $null
for ($attempt = 0; $attempt -lt 20; $attempt++) {
    try {
        if ([System.Windows.Forms.Clipboard]::ContainsImage()) {
            $image = [System.Windows.Forms.Clipboard]::GetImage()
            if ($null -ne $image) {
                try {
                    Write-Output ("{0}x{1}" -f $image.Width, $image.Height)
                    exit 0
                } finally {
                    $image.Dispose()
                }
            }
        }
    } catch {
        $lastError = $_
    }
    Start-Sleep -Milliseconds 100
}

if ($null -ne $lastError) {
    Write-Error "clipboard image read failed after retries: $lastError"
} else {
    Write-Error "the clipboard does not contain an image"
}
exit 1
'@
    [System.IO.File]::WriteAllText(
        $probePath,
        $probe,
        [System.Text.UTF8Encoding]::new($false)
    )

    $windowsPowerShell = Join-Path $env:SystemRoot `
        "System32\WindowsPowerShell\v1.0\powershell.exe"
    Assert-Smoke (Test-Path -LiteralPath $windowsPowerShell) `
        "Windows PowerShell is required for the STA clipboard probe"

    $stderrPath = Join-Path $Scratch "clipboard-probe.stderr.log"
    $output = & $windowsPowerShell `
        -NoLogo `
        -NoProfile `
        -NonInteractive `
        -STA `
        -ExecutionPolicy Bypass `
        -File $probePath 2> $stderrPath
    $exitCode = $LASTEXITCODE
    $text = ([string]::Join("", @($output))).Trim()

    if ($exitCode -ne 0) {
        $detail = if (Test-Path -LiteralPath $stderrPath) {
            [System.IO.File]::ReadAllText($stderrPath).Trim()
        } else {
            "no diagnostic"
        }
        throw "Windows smoke test failed: clipboard readback failed: $detail"
    }

    Assert-Smoke ($text -match "^([0-9]+)x([0-9]+)$") `
        "clipboard probe returned '$text', expected WIDTHxHEIGHT"

    return [pscustomobject] @{
        Width = [uint32] $Matches[1]
        Height = [uint32] $Matches[2]
    }
}

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw "tools/windows-smoke.ps1 must run natively on Windows"
}

$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$scratch = Join-Path ([System.IO.Path]::GetTempPath()) `
    ("scrozz-windows-smoke-" + [Guid]::NewGuid().ToString("N"))
$oldUnstable = [Environment]::GetEnvironmentVariable(
    "SCROZZ_UNSTABLE_BACKENDS",
    [EnvironmentVariableTarget]::Process
)
$oldRustLog = [Environment]::GetEnvironmentVariable(
    "RUST_LOG",
    [EnvironmentVariableTarget]::Process
)
$locationPushed = $false

try {
    [System.IO.Directory]::CreateDirectory($scratch) | Out-Null
    Push-Location $repoRoot
    $locationPushed = $true

    if ([string]::IsNullOrWhiteSpace($Binary)) {
        $cargo = Get-Command cargo -ErrorAction Stop
        Write-Host "[build] Building the native Scrozz CLI ($Profile)"
        $buildArguments = @("build", "-p", "scrozz", "--bin", "scrozz")
        if ($Profile -eq "release") {
            $buildArguments += "--release"
        }
        & $cargo.Source @buildArguments
        Assert-Smoke ($LASTEXITCODE -eq 0) "cargo build failed"

        $metadataText = & $cargo.Source metadata --format-version 1 --no-deps
        Assert-Smoke ($LASTEXITCODE -eq 0) "cargo metadata failed"
        $metadata = $metadataText | ConvertFrom-Json
        $Binary = Join-Path (Join-Path $metadata.target_directory $Profile) "scrozz.exe"
    }

    $Binary = [System.IO.Path]::GetFullPath($Binary)
    Assert-Smoke (Test-Path -LiteralPath $Binary -PathType Leaf) `
        "Scrozz binary not found at '$Binary'"

    $env:SCROZZ_UNSTABLE_BACKENDS = "1"
    $env:RUST_LOG = "scrozz=info,warn"

    Write-Host "[1/4] Enumerating native displays"
    $listed = Invoke-ScrozzJson `
        -Executable $Binary `
        -Arguments @("list", "displays") `
        -Name "list-displays" `
        -Scratch $scratch
    Assert-Smoke ($listed.Json.command -eq "list.displays") `
        "display enumeration returned command '$($listed.Json.command)'"

    $displays = @($listed.Json.data)
    Assert-Smoke ($displays.Count -gt 0) "Windows reported no displays"
    foreach ($display in $displays) {
        Assert-Smoke ($display.width -gt 0) "display '$($display.id)' has zero width"
        Assert-Smoke ($display.height -gt 0) "display '$($display.id)' has zero height"
        Assert-Smoke ($display.scale -gt 0) "display '$($display.id)' has invalid scale"
    }
    $primary = @($displays | Where-Object { $_.primary })
    Assert-Smoke ($primary.Count -eq 1) `
        "expected exactly one primary display, found $($primary.Count)"

    Write-Host "[2/4] Capturing the primary display to file and clipboard"
    $pngPath = [System.IO.Path]::GetFullPath((Join-Path $scratch "capture.png"))
    $captured = Invoke-ScrozzJson `
        -Executable $Binary `
        -Arguments @(
            "capture",
            "--display", "primary",
            "--output", $pngPath,
            "--clipboard",
            "--format", "png"
        ) `
        -Name "capture" `
        -Scratch $scratch

    Assert-Smoke ($captured.Json.command -eq "capture") `
        "capture returned command '$($captured.Json.command)'"
    Assert-Smoke (Test-Path -LiteralPath $pngPath -PathType Leaf) `
        "capture reported success but did not create '$pngPath'"

    $file = Get-Item -LiteralPath $pngPath
    Assert-Smoke ($file.Length -gt 24) "capture.png is empty"
    Assert-Smoke ($captured.Json.data.bytes -eq $file.Length) `
        "JSON reports $($captured.Json.data.bytes) bytes but the file has $($file.Length)"

    $sinkKinds = @($captured.Json.data.plan.sinks | ForEach-Object { $_.kind })
    Assert-Smoke ($sinkKinds.Count -eq 2) `
        "capture planned $($sinkKinds.Count) sinks instead of exactly file + clipboard"
    Assert-Smoke ($sinkKinds -contains "file") "capture plan omitted the file sink"
    Assert-Smoke ($sinkKinds -contains "clipboard") "capture plan omitted the clipboard sink"

    $written = @($captured.Json.data.written)
    Assert-Smoke ($written -contains $pngPath) "capture did not report the requested save path"
    Assert-Smoke ($written -contains "clipboard") "capture did not report clipboard delivery"

    $savedPngs = @(Get-ChildItem -LiteralPath $scratch -Filter "*.png" -File)
    Assert-Smoke ($savedPngs.Count -eq 1) `
        "one capture request wrote $($savedPngs.Count) PNG files; Save must happen once"

    Write-Host "[3/4] Parsing the PNG independently"
    $png = Read-PngDimensions $pngPath
    Assert-Smoke ($png.Width -gt 0 -and $png.Height -gt 0) `
        "capture.png has zero dimensions"
    Assert-Smoke (
        $png.Width -eq [uint32] $captured.Json.data.width -and
        $png.Height -eq [uint32] $captured.Json.data.height
    ) "PNG dimensions $($png.Width)x$($png.Height) disagree with capture JSON"

    Write-Host "[4/4] Reading the image back from the native clipboard"
    $clipboard = Read-ClipboardImageDimensions -Scratch $scratch
    Assert-Smoke (
        $clipboard.Width -eq $png.Width -and
        $clipboard.Height -eq $png.Height
    ) "clipboard image $($clipboard.Width)x$($clipboard.Height) disagrees with the saved PNG"

    Write-Host (
        "PASS: {0} display(s); captured {1}x{2}, saved one PNG, and " +
        "round-tripped the same image through the Windows clipboard."
    ) -f $displays.Count, $png.Width, $png.Height
} finally {
    if ($locationPushed) {
        Pop-Location
    }
    [Environment]::SetEnvironmentVariable(
        "SCROZZ_UNSTABLE_BACKENDS",
        $oldUnstable,
        [EnvironmentVariableTarget]::Process
    )
    [Environment]::SetEnvironmentVariable(
        "RUST_LOG",
        $oldRustLog,
        [EnvironmentVariableTarget]::Process
    )
    if (Test-Path -LiteralPath $scratch) {
        Remove-Item -LiteralPath $scratch -Recurse -Force
    }
}
