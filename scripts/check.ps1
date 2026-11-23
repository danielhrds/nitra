#Requires -Version 5.1
<#
.SYNOPSIS
    Runs the workspace checks: formatting, lints (warnings are errors) and tests.

.DESCRIPTION
    Stops at the first failing step and exits with that step's exit code.
    Compatible with Windows PowerShell 5.1. Keep this file ASCII-only:
    PowerShell 5.1 reads BOM-less files with the ANSI code page.
#>
[CmdletBinding()]
param()

# Native commands report failure through $LASTEXITCODE; cargo writes progress
# to stderr, which must not be treated as a PowerShell error.
$ErrorActionPreference = 'Continue'

$steps = @(
    @{ Name = 'fmt';    Args = @('fmt', '--all', '--check') },
    @{ Name = 'clippy'; Args = @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings') },
    @{ Name = 'test';   Args = @('test', '--workspace') }
)

$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot
try {
    foreach ($step in $steps) {
        Write-Host "==> cargo $($step.Args -join ' ')" -ForegroundColor Cyan
        $timer = [System.Diagnostics.Stopwatch]::StartNew()
        & cargo @($step.Args)
        $exitCode = $LASTEXITCODE
        $timer.Stop()

        if ($exitCode -ne 0) {
            Write-Host "FAILED: $($step.Name) (exit code $exitCode)" -ForegroundColor Red
            exit $exitCode
        }
        Write-Host ("ok: {0} ({1:N1} s)" -f $step.Name, $timer.Elapsed.TotalSeconds) -ForegroundColor Green
    }

    # The Cloudflare worker: type-check and tests (wrangler needs Node 22.12+).
    # NITRA_NODE_DIR may point at a folder with node.exe and npm.cmd; otherwise
    # the node on PATH is used. Skipped (not failed) without Node or packages.
    $worker = Join-Path $repoRoot 'worker'
    $nodeDir = $env:NITRA_NODE_DIR
    if ($nodeDir) { $env:Path = "$nodeDir;$env:Path" }
    $nodeVersion = $null
    if (Get-Command node -ErrorAction SilentlyContinue) {
        $nodeVersion = (& node --version 2>$null) -replace '^v', ''
    }
    $nodeOk = $false
    if ($nodeVersion -match '^\d+\.\d+\.\d+$') { $nodeOk = [version]$nodeVersion -ge [version]'22.12.0' }
    if (-not (Test-Path (Join-Path $worker 'node_modules'))) {
        Write-Host 'skipped: worker (run npm install in worker\ first)' -ForegroundColor Yellow
    }
    elseif (-not $nodeOk) {
        Write-Host "skipped: worker (needs Node 22.12+, found '$nodeVersion'; set NITRA_NODE_DIR)" -ForegroundColor Yellow
    }
    else {
        Write-Host '==> npm run check (worker)' -ForegroundColor Cyan
        $timer = [System.Diagnostics.Stopwatch]::StartNew()
        $env:WRANGLER_SEND_METRICS = 'false'
        Push-Location $worker
        try { & npm run check } finally { Pop-Location }
        $exitCode = $LASTEXITCODE
        if ($exitCode -ne 0) {
            Write-Host "FAILED: worker (exit code $exitCode)" -ForegroundColor Red
            exit $exitCode
        }
        Write-Host ("ok: worker ({0:N1} s)" -f $timer.Elapsed.TotalSeconds) -ForegroundColor Green
    }
    Write-Host 'All checks passed.' -ForegroundColor Green
}
finally {
    Pop-Location
}
