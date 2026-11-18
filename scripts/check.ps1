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
    Write-Host 'All checks passed.' -ForegroundColor Green
}
finally {
    Pop-Location
}
