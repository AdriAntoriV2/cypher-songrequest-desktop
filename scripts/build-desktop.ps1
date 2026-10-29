
$ErrorActionPreference = "Stop"

# CypherSR Desktop frontend build.
# Compiles the Rust/Slint GUI and creates a versioned ZIP release asset.
# The version is read automatically from Cargo.toml.
# The ZIP contains only CypherSR-Desktop.exe.
# Project root is detected relative to this script.

$FrontendRoot = Split-Path -Parent $PSScriptRoot
$CargoManifest = Join-Path $FrontendRoot "Cargo.toml"
$ReleaseDir = Join-Path $FrontendRoot "target\release"
$DistDir = Join-Path $FrontendRoot "release"

function Require-Path {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,

        [string]$Description = "Required path"
    )

    if (-not (Test-Path -LiteralPath $Path)) {
        throw "$Description not found: $Path"
    }
}

function Get-DesktopVersion {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ManifestPath
    )

    $manifest = Get-Content -LiteralPath $ManifestPath -Raw

    # Read package.version from the [package] section only.
    $packageMatch = [regex]::Match(
        $manifest,
        '(?ms)^\[package\]\s*(.*?)(?=^\[|\z)'
    )

    if (-not $packageMatch.Success) {
        throw "Could not find the [package] section in $ManifestPath."
    }

    $versionMatch = [regex]::Match(
        $packageMatch.Groups[1].Value,
        '(?m)^\s*version\s*=\s*"([^"]+)"'
    )

    if (-not $versionMatch.Success) {
        throw "Could not read package.version from $ManifestPath."
    }

    $version = $versionMatch.Groups[1].Value.Trim()

    if (
        $version -notmatch
        '^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$'
    ) {
        throw "Invalid Desktop version '$version' in Cargo.toml."
    }

    return $version
}

Require-Path -Path $CargoManifest -Description "Desktop Cargo.toml"

if (-not (Get-Command "cargo" -ErrorAction SilentlyContinue)) {
    throw "Cargo was not found. Install Rust and reopen the terminal."
}

if (-not (Get-Command "Compress-Archive" -ErrorAction SilentlyContinue)) {
    throw "Compress-Archive is unavailable. Use a supported Windows PowerShell version."
}

$DesktopVersion = Get-DesktopVersion -ManifestPath $CargoManifest

$BuiltExePath = Join-Path $ReleaseDir "CypherSR-Desktop.exe"
$VersionedZipName = "CypherSR-Desktop-GUI-v$DesktopVersion-win-x64.zip"
$VersionedZipPath = Join-Path $DistDir $VersionedZipName

Write-Host ""
Write-Host "============================================"
Write-Host "Building CypherSR Desktop"
Write-Host "============================================"
Write-Host "Project:         $FrontendRoot"
Write-Host "Desktop version: $DesktopVersion"
Write-Host "Release asset:   $VersionedZipPath"
Write-Host ""

Push-Location $FrontendRoot

try {
    cargo build --release --manifest-path $CargoManifest

    if ($LASTEXITCODE -ne 0) {
        throw "Desktop build failed with exit code $LASTEXITCODE."
    }
}
finally {
    Pop-Location
}

Require-Path -Path $ReleaseDir -Description "Cargo release output directory"
Require-Path -Path $BuiltExePath -Description "Built Desktop executable"

$builtFile = Get-Item -LiteralPath $BuiltExePath

if ($builtFile.Length -le 0) {
    throw "The built Desktop executable is empty: $BuiltExePath"
}

# Create the release directory without touching other release assets.
New-Item -ItemType Directory -Path $DistDir -Force | Out-Null

# Use a temporary staging directory so the ZIP contains only the intended EXE.
$StageDir = Join-Path $DistDir ".desktop-zip-staging"
$StagedExePath = Join-Path $StageDir "CypherSR-Desktop.exe"
$TempZipPath = Join-Path $DistDir ".tmp-$VersionedZipName"

try {
    if (Test-Path -LiteralPath $StageDir) {
        Remove-Item -LiteralPath $StageDir -Recurse -Force
    }

    if (Test-Path -LiteralPath $TempZipPath) {
        Remove-Item -LiteralPath $TempZipPath -Force
    }

    New-Item -ItemType Directory -Path $StageDir -Force | Out-Null

    Copy-Item `
        -LiteralPath $BuiltExePath `
        -Destination $StagedExePath `
        -Force

    $stagedFile = Get-Item -LiteralPath $StagedExePath

    if ($stagedFile.Length -ne $builtFile.Length) {
        throw "The staged executable size does not match the build output."
    }

    # Compress only the staged executable.
    Compress-Archive `
        -LiteralPath $StagedExePath `
        -DestinationPath $TempZipPath `
        -CompressionLevel Optimal `
        -Force

    Require-Path -Path $TempZipPath -Description "Temporary Desktop GUI ZIP"

    $zipFile = Get-Item -LiteralPath $TempZipPath

    if ($zipFile.Length -le 0) {
        throw "The generated Desktop GUI ZIP is empty."
    }

    # Replace the final ZIP only after compression succeeds.
    Move-Item `
        -LiteralPath $TempZipPath `
        -Destination $VersionedZipPath `
        -Force
}
finally {
    # Always remove temporary build staging files.
    if (Test-Path -LiteralPath $StageDir) {
        Remove-Item -LiteralPath $StageDir -Recurse -Force
    }

    if (Test-Path -LiteralPath $TempZipPath) {
        Remove-Item -LiteralPath $TempZipPath -Force
    }
}

Require-Path -Path $VersionedZipPath -Description "Versioned Desktop GUI ZIP"

$finalZip = Get-Item -LiteralPath $VersionedZipPath
$compressionRatio = if ($builtFile.Length -gt 0) {
    [math]::Round((1 - ($finalZip.Length / $builtFile.Length)) * 100, 1)
}
else {
    0
}

Write-Host ""
Write-Host "============================================"
Write-Host "Desktop build completed successfully"
Write-Host "============================================"
Write-Host "Desktop version: $DesktopVersion"
Write-Host "Build output:    $BuiltExePath"
Write-Host "Release asset:   $VersionedZipPath"
Write-Host "EXE size:        $([math]::Round($builtFile.Length / 1MB, 2)) MB"
Write-Host "ZIP size:        $([math]::Round($finalZip.Length / 1MB, 2)) MB"
Write-Host "Size reduction:  $compressionRatio%"
Write-Host ""
