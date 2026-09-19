<#
.SYNOPSIS
Build Windows x64 setup EXE and MSI installers for local testing.
.EXAMPLE
.\scripts\build-release.cmd
.EXAMPLE
.\scripts\build-release.cmd -Bundle nsis -SkipDependencyInstall
#>
[CmdletBinding()]
param(
    [ValidateSet('all', 'nsis', 'msi')]
    [string]$Bundle = 'all',
    [switch]$SkipDependencyInstall
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Invoke-Checked {
    param([string]$Command, [string[]]$Arguments)
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Command failed with exit code $LASTEXITCODE" }
}

if ($env:OS -ne 'Windows_NT') { throw 'Run this script on Windows.' }
foreach ($tool in @('node', 'npm.cmd', 'cargo', 'rustc')) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "Install $tool first. See the Windows build prerequisites in README.md."
    }
}

$repoRoot = Split-Path $PSScriptRoot -Parent
$previousTargetDir = $env:CARGO_TARGET_DIR
Push-Location $repoRoot
try {
    # A fixed location makes the output paths predictable even if the caller
    # normally uses a shared Cargo target directory.
    $env:CARGO_TARGET_DIR = Join-Path $repoRoot 'src-tauri\target'
    $rustInfo = & rustc -vV
    if ($LASTEXITCODE -ne 0) { throw 'Could not inspect the active Rust toolchain.' }
    if ($rustInfo -notcontains 'host: x86_64-pc-windows-msvc') {
        throw 'Use the Windows x64 MSVC toolchain: rustup default stable-x86_64-pc-windows-msvc'
    }

    if (-not $SkipDependencyInstall) {
        Invoke-Checked 'npm.cmd' @('ci')
    } elseif (-not (Test-Path -LiteralPath 'node_modules\@tauri-apps\cli\tauri.js')) {
        throw 'Frontend dependencies are missing. Run again without -SkipDependencyInstall.'
    }

    $bundles = if ($Bundle -eq 'all') { 'nsis,msi' } else { $Bundle }
    # Always rebuild frontend and Rust code, rather than packaging an old binary.
    # Tauri downloads NSIS/WiX when needed. Cargo uses the checked-in lockfile.
    $buildStarted = Get-Date
    Invoke-Checked 'npm.cmd' @('run', 'tauri', '--', 'build', '--ci', '--no-sign', '--bundles', $bundles, '--', '--locked')

    $bundleRoot = Join-Path $env:CARGO_TARGET_DIR 'release\bundle'
    Write-Host "`nLocal installers built successfully:"
    foreach ($format in $bundles.Split(',')) {
        $extension = if ($format -eq 'nsis') { '*.exe' } else { '*.msi' }
        $outputDir = Join-Path $bundleRoot $format
        $files = @(Get-ChildItem -LiteralPath $outputDir -Filter $extension -File |
            Where-Object { $_.LastWriteTime -ge $buildStarted })
        if ($files.Count -eq 0) { throw "No $format installer found in $outputDir" }
        $files | ForEach-Object { Write-Host "  $($_.FullName)" }
    }
    Write-Host "`nRun the setup EXE to test Only me (recommended), including when setup is elevated."
    Write-Host 'Installers are unsigned local builds. No installer is launched automatically.'
} finally {
    $env:CARGO_TARGET_DIR = $previousTargetDir
    Pop-Location
}
