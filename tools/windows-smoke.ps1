# Native Windows smoke test for Scrozz's first vertical slice.
#
# Run from a Windows desktop session:
#
#   pwsh -File tools/windows-smoke.ps1 `
#     -TesseractDirectory C:\path\to\tesseract
#
# Or point it at an already-built binary:
#
#   pwsh -File tools/windows-smoke.ps1 `
#     -Binary artifacts/scrozz.exe `
#     -ArtifactType portable
#
# A packaged/sparse-package artifact must be declared explicitly:
#
#   pwsh -File tools/windows-smoke.ps1 `
#     -Binary C:\Program Files\WindowsApps\...\scrozz.exe `
#     -ArtifactType packaged
#
# This is intentionally a CLI/headless probe. It does not automate egui or
# claim that the overlay behaves correctly; a person still has to verify focus,
# hit-testing and placement. What it does exercise is the same native display,
# capture, encoder, file, clipboard and OCR code the tray app's worker uses.

[CmdletBinding()]
param(
    [Parameter()]
    [string] $Binary = $env:SCROZZ_SMOKE_BINARY,

    [Parameter()]
    [ValidateSet("debug", "release")]
    [string] $Profile = "debug",

    [Parameter()]
    [ValidateSet("portable", "packaged")]
    [string] $ArtifactType = "portable",

    [Parameter()]
    [string] $TesseractDirectory,

    [Parameter()]
    [switch] $RequireWgc,

    [Parameter()]
    [switch] $RequireNegativeCoordinates
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
        [string] $Scratch,

        [Parameter()]
        [switch] $AllowUnsupported
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

    $unsupported = (
        $AllowUnsupported -and
        $exitCode -ne 0 -and
        -not $document.ok -and
        $null -ne $document.error -and
        $document.error.kind -eq "unsupported"
    )
    if (($exitCode -ne 0 -or -not $document.ok) -and -not $unsupported) {
        $detail = if ($null -ne $document.error) {
            $document.error | ConvertTo-Json -Compress -Depth 8
        } else {
            $stdout
        }
        throw "Windows smoke test failed: '$Name' exited $exitCode`: $detail"
    }

    # This was the merge-review finding that prompted the apartment work. WGC
    # may legitimately be unsupported and fall back to GDI, but it must never
    # do so because Scrozz forgot to initialise the calling thread. Check both
    # streams: JSON errors keep the same diagnostic without relying on logging.
    Assert-Smoke (
        ($stderr + $stdout) -notmatch "CO_E_NOTINITIALIZED|no COM apartment|thread with no COM apartment|has not entered a COM apartment"
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

function Assert-TesseractStarts {
    param(
        [Parameter(Mandatory)]
        [string] $PayloadDirectory
    )

    $executable = Join-Path $PayloadDirectory "tesseract.exe"
    $start = [System.Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $executable
    $start.Arguments = "--version"
    $start.WorkingDirectory = $PayloadDirectory
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    # The Windows loader still searches the executable directory and System32,
    # but cannot borrow a missing OCR DLL from a developer tool on ambient PATH.
    $start.EnvironmentVariables["PATH"] = (
        (Join-Path $env:SystemRoot "System32") + ";" + $env:SystemRoot
    )

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $start
    try {
        Assert-Smoke $process.Start() "could not start artifact-local tesseract.exe"
        if (-not $process.WaitForExit(10000)) {
            $process.Kill()
            $process.WaitForExit()
            throw "Windows smoke test failed: artifact-local tesseract.exe timed out"
        }
        $stdout = $process.StandardOutput.ReadToEnd()
        $stderr = $process.StandardError.ReadToEnd()
        $diagnostic = ($stdout + [Environment]::NewLine + $stderr).Trim()
        Assert-Smoke ($process.ExitCode -eq 0) `
            "artifact-local tesseract.exe exited $($process.ExitCode): $diagnostic"
        Assert-Smoke ($diagnostic -match "(?i)tesseract") `
            "artifact-local OCR probe did not identify itself as Tesseract: $diagnostic"
    } finally {
        $process.Dispose()
    }
}

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw "tools/windows-smoke.ps1 must run natively on Windows"
}
$buildingFromSource = [string]::IsNullOrWhiteSpace($Binary)
if ($ArtifactType -eq "packaged" -and $buildingFromSource) {
    throw "-ArtifactType packaged requires -Binary to name the installed MSIX/sparse-package executable"
}
if ($ArtifactType -eq "packaged" -and
    -not [string]::IsNullOrWhiteSpace($TesseractDirectory)) {
    throw "-TesseractDirectory applies only to -ArtifactType portable"
}
if ($buildingFromSource -and
    $ArtifactType -eq "portable" -and
    [string]::IsNullOrWhiteSpace($TesseractDirectory)) {
    throw (
        "A source-built portable smoke run requires -TesseractDirectory. " +
        "An extracted portable artifact instead discovers its sibling tesseract directory."
    )
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
$oldTesseractDirectory = [Environment]::GetEnvironmentVariable(
    "SCROZZ_TESSERACT_DIR",
    [EnvironmentVariableTarget]::Process
)
$locationPushed = $false

try {
    [System.IO.Directory]::CreateDirectory($scratch) | Out-Null
    Push-Location $repoRoot
    $locationPushed = $true

    if ($buildingFromSource) {
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
    if (-not [string]::IsNullOrWhiteSpace($TesseractDirectory)) {
        Assert-Smoke (
            [System.IO.Path]::IsPathRooted($TesseractDirectory) -and
            $TesseractDirectory -notmatch "^[A-Za-z]:[^\\/]"
        ) "-TesseractDirectory must be an absolute path"
        $resolvedTesseract = [System.IO.Path]::GetFullPath($TesseractDirectory)
        Assert-Smoke (Test-Path -LiteralPath $resolvedTesseract -PathType Container) `
            "Tesseract directory not found at '$resolvedTesseract'"
        $env:SCROZZ_TESSERACT_DIR = $resolvedTesseract
    } else {
        # A default portable smoke run must prove the artifact's sibling payload,
        # not accidentally inherit a developer override from the parent shell.
        $env:SCROZZ_TESSERACT_DIR = $null
    }
    if ($ArtifactType -eq "portable") {
        $payloadDirectory = if (-not [string]::IsNullOrWhiteSpace($TesseractDirectory)) {
            $resolvedTesseract
        } else {
            Join-Path (Split-Path -Parent $Binary) "tesseract"
        }
        Assert-Smoke (Test-Path -LiteralPath $payloadDirectory -PathType Container) `
            "portable Tesseract payload not found at '$payloadDirectory'"
        foreach ($requiredPayloadFile in @(
            (Join-Path $payloadDirectory "tesseract.exe"),
            (Join-Path $payloadDirectory "tessdata\eng.traineddata")
        )) {
            Assert-Smoke (Test-Path -LiteralPath $requiredPayloadFile -PathType Leaf) `
                "portable OCR payload is missing '$requiredPayloadFile'"
        }
        Assert-TesseractStarts -PayloadDirectory $payloadDirectory
    }

    Write-Host "[1/5] Enumerating native displays"
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
        Write-Host (
            "      {0}: {1}x{2} at ({3}, {4}), scale {5}x{6}" -f
            $display.id,
            $display.width,
            $display.height,
            $display.x,
            $display.y,
            $display.scale,
            $(if ($display.primary) { " [primary]" } else { "" })
        )
    }
    $primary = @($displays | Where-Object { $_.primary })
    Assert-Smoke ($primary.Count -eq 1) `
        "expected exactly one primary display, found $($primary.Count)"
    Assert-Smoke ($primary[0].x -eq 0 -and $primary[0].y -eq 0) `
        "the Windows primary display must own virtual-desktop origin (0, 0)"
    if ($RequireNegativeCoordinates) {
        $negativeDisplays = @(
            $displays | Where-Object { $_.x -lt 0 -or $_.y -lt 0 }
        )
        Assert-Smoke ($negativeDisplays.Count -gt 0) `
            "-RequireNegativeCoordinates was set but no display has a negative origin"
    }

    Write-Host "[2/5] Capturing the primary display to file and clipboard"
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
    $backend = $captured.Json.data.backend
    Assert-Smoke (
        $backend -eq "Windows.Graphics.Capture" -or
        $backend -eq "GDI BitBlt"
    ) "capture reported unknown backend '$backend'"
    if ($RequireWgc) {
        Assert-Smoke ($backend -eq "Windows.Graphics.Capture") `
            "-RequireWgc was set but capture selected '$backend'"
    }
    if ($backend -eq "GDI BitBlt") {
        Assert-Smoke (
            $captured.Stderr -match "GDI fallback"
        ) "capture used GDI without explaining the WGC downgrade"
    }
    $identity = $captured.Json.data.runtime.package_identity
    Assert-Smoke ($null -ne $identity) "capture did not expose runtime package identity"
    $expectedIdentity = if ($ArtifactType -eq "packaged") {
        "packaged"
    } else {
        "unpackaged"
    }
    Assert-Smoke ($identity.state -eq $expectedIdentity) `
        "artifact was declared '$ArtifactType' but runtime package identity is '$($identity.state)'"
    if ($ArtifactType -eq "packaged") {
        Assert-Smoke (-not [string]::IsNullOrWhiteSpace($identity.full_name)) `
            "packaged runtime reported no package full name"
    } else {
        Assert-Smoke ($null -eq $identity.full_name) `
            "portable runtime unexpectedly reported package '$($identity.full_name)'"
    }
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

    Write-Host "[3/5] Parsing the PNG independently"
    $png = Read-PngDimensions $pngPath
    Assert-Smoke ($png.Width -gt 0 -and $png.Height -gt 0) `
        "capture.png has zero dimensions"
    Assert-Smoke (
        $png.Width -eq [uint32] $captured.Json.data.width -and
        $png.Height -eq [uint32] $captured.Json.data.height
    ) "PNG dimensions $($png.Width)x$($png.Height) disagree with capture JSON"

    Write-Host "[4/5] Reading the image back from the native clipboard"
    $clipboard = Read-ClipboardImageDimensions -Scratch $scratch
    Assert-Smoke (
        $clipboard.Width -eq $png.Width -and
        $clipboard.Height -eq $png.Height
    ) "clipboard image $($clipboard.Width)x$($clipboard.Height) disagrees with the saved PNG"

    Write-Host "[5/5] Exercising the artifact-selected OCR backend"
    $recognised = Invoke-ScrozzJson `
        -Executable $Binary `
        -Arguments @("ocr", "--file", $pngPath) `
        -Name "ocr" `
        -Scratch $scratch `
        -AllowUnsupported:($ArtifactType -eq "packaged")
    Assert-Smoke ($recognised.Json.command -eq "ocr") `
        "OCR returned command '$($recognised.Json.command)'"
    if ($recognised.Json.ok) {
        $expectedOcrBackend = if ($ArtifactType -eq "packaged") {
            "windows-media-ocr"
        } else {
            "tesseract"
        }
        Assert-Smoke ($recognised.Json.data.engine -eq $expectedOcrBackend) `
            "'$ArtifactType' artifact used OCR engine '$($recognised.Json.data.engine)', expected '$expectedOcrBackend'"
        $ocrBackend = $recognised.Json.data.engine
    } else {
        Assert-Smoke ($recognised.Json.error.kind -eq "unsupported") `
            "OCR failed with unexpected kind '$($recognised.Json.error.kind)'"
        Assert-Smoke ($ArtifactType -eq "packaged") `
            "portable OCR must use the artifact-local Tesseract payload, not report unsupported"
        Assert-Smoke (
            $recognised.Json.error.details.why -match "language pack"
        ) "packaged OCR was unsupported for a reason other than a missing Windows language pack"
        $ocrBackend = "windows-media-ocr (language pack unavailable)"
    }

    Write-Host ((
        "PASS: {0} display(s); captured {1}x{2}, saved one PNG, and " +
        "round-tripped the same image through the Windows clipboard via {3}; " +
        "{4} artifact identity selected {5}."
    ) -f $displays.Count, $png.Width, $png.Height, $backend, $ArtifactType, $ocrBackend)
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
    [Environment]::SetEnvironmentVariable(
        "SCROZZ_TESSERACT_DIR",
        $oldTesseractDirectory,
        [EnvironmentVariableTarget]::Process
    )
    if (Test-Path -LiteralPath $scratch) {
        Remove-Item -LiteralPath $scratch -Recurse -Force
    }
}
