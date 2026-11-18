#Requires -Version 5.1
<#
.SYNOPSIS
    Runs the tests that need real hardware (GPU, display, drivers).

.DESCRIPTION
    These tests are marked #[ignore] so that check.ps1 stays hardware-free.
    They are read-only probes and short device creations; nothing changes
    system, display or GPU settings. Run on the development machine with an
    interactive desktop session. Keep this file ASCII-only (PowerShell 5.1).
#>
[CmdletBinding()]
param()

# Native commands report failure through $LASTEXITCODE; cargo writes progress
# to stderr, which must not be treated as a PowerShell error.
$ErrorActionPreference = 'Continue'

$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot
try {
    Write-Host '==> cargo test --workspace -- --ignored' -ForegroundColor Cyan
    $timer = [System.Diagnostics.Stopwatch]::StartNew()
    & cargo test --workspace -- --ignored
    $exitCode = $LASTEXITCODE
    $timer.Stop()

    if ($exitCode -ne 0) {
        Write-Host "FAILED: GPU tests (exit code $exitCode)" -ForegroundColor Red
        exit $exitCode
    }
    Write-Host ("GPU tests passed ({0:N1} s)." -f $timer.Elapsed.TotalSeconds) -ForegroundColor Green
}
finally {
    Pop-Location
}
