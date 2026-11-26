#Requires -Version 5.1
<#
.SYNOPSIS
    Builds the beta: nitra.exe for the beta channel, checked, with its
    SHA-256, in target\dist\nitra-<version>\.

.DESCRIPTION
    1. Refuses a working tree with changes: the executable must match a
       commit.
    2. Runs scripts\check.ps1 (skip with -SkipCheck).
    3. Builds with NITRA_CHANNEL=beta (data in %LOCALAPPDATA%\nitra, the
       production rooms server) in its own target folder, apart from the
       development builds.
    4. Checks the file version against the workspace version and that the
       executable needs no C runtime DLL (dumpbin from the Visual Studio
       Build Tools; skipped with a warning if it is not found).
    5. Copies nitra.exe and writes SHA256SUMS.txt next to it.
    6. With NITRA_RELEASE_KEY set to the release key's working copy
       (release.key), signs SHA256SUMS.txt (SHA256SUMS.txt.sig) and checks
       the signature against the key built into this build. Attach all
       three files to the GitHub release: the app installs an update only
       when they check out.
    7. With -Scan, has Windows Defender scan the executable (a read-only
       scan; nothing in Defender's settings changes).
    Prints the command for the local tag; it does not tag or push anything.
    Keep this file ASCII-only (PowerShell 5.1 reads BOM-less files with the
    ANSI code page).
#>
[CmdletBinding()]
param(
    [switch]$SkipCheck,
    [switch]$Scan
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    if (git status --porcelain) { throw 'The working tree has changes: commit them first.' }
    $commit = (git rev-parse --short HEAD).Trim()
    $version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
    "nitra $version at $commit"

    if (-not $SkipCheck) {
        & (Join-Path $PSScriptRoot 'check.ps1')
        if ($LASTEXITCODE -ne 0) { throw "check.ps1 failed ($LASTEXITCODE)" }
    }

    $env:NITRA_CHANNEL = 'beta'
    $env:CARGO_TARGET_DIR = Join-Path $root 'target\beta'
    try {
        $ErrorActionPreference = 'Continue'
        & cargo build --release -p nitra
        $built = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
    } finally {
        Remove-Item Env:NITRA_CHANNEL, Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
    }
    if ($built -ne 0) { throw "cargo build failed ($built)" }
    $exe = Join-Path $root 'target\beta\release\nitra.exe'

    $fileVersion = (Get-Item $exe).VersionInfo.ProductVersion
    if ($fileVersion -ne $version) { throw "file version $fileVersion is not $version" }
    "file version: $fileVersion"
    $dumpbin = Get-ChildItem 'D:\VS\BuildTools\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe', "${env:ProgramFiles(x86)}\Microsoft Visual Studio\*\*\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe" -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($dumpbin) {
        $imports = & $dumpbin.FullName /nologo /dependents $exe | Where-Object { $_ -match '^\s+\S+\.dll\s*$' } | ForEach-Object { $_.Trim() }
        $runtime = $imports | Where-Object { $_ -match '^(vcruntime|msvcp|ucrtbase|api-ms-win-crt)' }
        if ($runtime) { throw "needs C runtime DLLs: $($runtime -join ', ')" }
        "imports: $($imports -join ', ') (no C runtime DLL)"
    } else {
        Write-Warning 'dumpbin not found: C runtime imports not checked'
    }

    $dist = Join-Path $root "target\dist\nitra-$version"
    New-Item -ItemType Directory -Force $dist | Out-Null
    Copy-Item $exe (Join-Path $dist 'nitra.exe') -Force
    $hash = (Get-FileHash (Join-Path $dist 'nitra.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
    $size = (Get-Item (Join-Path $dist 'nitra.exe')).Length
    [IO.File]::WriteAllText((Join-Path $dist 'SHA256SUMS.txt'), "$hash  nitra.exe`n")
    "wrote $dist\nitra.exe ($([Math]::Round($size / 1MB, 1)) MB)"
    "SHA-256: $hash"

    # Sign the checksums with the release key, then check the signature with
    # the key built into this very build: what the app will do before it
    # installs the release.
    $cli = Join-Path $root 'target\beta\release\nitra-cli.exe'
    $sums = Join-Path $dist 'SHA256SUMS.txt'
    if ($env:NITRA_RELEASE_KEY) {
        & $cli --release-sign $sums --release-key $env:NITRA_RELEASE_KEY
        if ($LASTEXITCODE -ne 0) { throw "signing failed ($LASTEXITCODE)" }
        & $cli --release-verify $sums
        if ($LASTEXITCODE -ne 0) { throw 'the signature does not match the key built into this build' }
    } else {
        Write-Warning 'NITRA_RELEASE_KEY is not set: the release is not signed, and the app will not install it as an update'
    }

    if ($Scan) {
        $defender = Join-Path $env:ProgramFiles 'Windows Defender\MpCmdRun.exe'
        & $defender -Scan -ScanType 3 -File (Join-Path $dist 'nitra.exe') -DisableRemediation
        "Defender exit code: $LASTEXITCODE (0 = nothing found)"
    }
    "local tag: git tag -a v$version -m `"nitra $version`" $commit"
} finally {
    Pop-Location
}
