$ErrorActionPreference = 'Stop'

$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$wrapper = Join-Path $root 'tools/native-evidence.ps1'
$scratch = Join-Path ([IO.Path]::GetTempPath()) (
    'scrozz-native-evidence-test-' + [Guid]::NewGuid().ToString('N')
)
$sourceRoot = Join-Path $scratch 'source'

try {
    $null = New-Item -ItemType Directory -Path $sourceRoot
    & git -C $sourceRoot init -q
    & git -C $sourceRoot config user.name 'Scrozz Test'
    & git -C $sourceRoot config user.email 'scrozz-test@example.invalid'
    Set-Content -LiteralPath (Join-Path $sourceRoot 'fixture.txt') -Value 'clean'
    & git -C $sourceRoot add fixture.txt
    & git -C $sourceRoot commit -q -m fixture
    $expectedSha = (& git -C $sourceRoot rev-parse HEAD).Trim()
    $shell = (Get-Process -Id $PID).Path

    $cleanEvidence = Join-Path $scratch 'clean-evidence'
    Push-Location $sourceRoot
    try {
        & $wrapper `
            -Output $cleanEvidence `
            -Label source-root-clean `
            -- $shell -NoProfile -Command '[Console]::Out.Write((Get-Location).Path)'
        if ($LASTEXITCODE -ne 0) {
            throw "Clean evidence command exited $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }

    $clean = Get-Content -LiteralPath (Join-Path $cleanEvidence 'run.json') -Raw |
        ConvertFrom-Json
    if ([IO.Path]::GetFullPath($clean.source_root) -ne
        [IO.Path]::GetFullPath($sourceRoot)) {
        throw "Recorded source root '$($clean.source_root)' instead of '$sourceRoot'"
    }
    if ($clean.source_sha -ne $expectedSha) {
        throw "Recorded source SHA '$($clean.source_sha)' instead of '$expectedSha'"
    }
    if ($clean.source_dirty) {
        throw 'Clean source was recorded as dirty'
    }

    Set-Content -LiteralPath (Join-Path $sourceRoot 'fixture.txt') -Value 'dirty'
    $dirtyEvidence = Join-Path $scratch 'dirty-evidence'
    Push-Location $sourceRoot
    try {
        & $wrapper `
            -Output $dirtyEvidence `
            -Label source-root-dirty `
            -- $shell -NoProfile -Command 'exit 0'
        if ($LASTEXITCODE -ne 0) {
            throw "Dirty evidence command exited $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }

    $dirty = Get-Content -LiteralPath (Join-Path $dirtyEvidence 'run.json') -Raw |
        ConvertFrom-Json
    if (-not $dirty.source_dirty) {
        throw 'Dirty source was recorded as clean'
    }
    if ($dirty.source_sha -ne $expectedSha) {
        throw "Dirty source recorded SHA '$($dirty.source_sha)' instead of '$expectedSha'"
    }

    Write-Host 'native evidence PowerShell source-root checks passed'
} finally {
    if (Test-Path -LiteralPath $scratch) {
        Remove-Item -LiteralPath $scratch -Recurse -Force
    }
}
