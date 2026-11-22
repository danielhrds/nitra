#Requires -Version 5.1
<#
.SYNOPSIS
    Checks ziz logs for things that must never be in them: IP addresses,
    invites, e-mail addresses and secrets.

.DESCRIPTION
    A log can be sent to someone to diagnose a problem, so it may only say
    what happened, never who or where. Every .log file under -Path is read;
    each finding prints the file, the line number, what was found and the
    line (trimmed). Exit code 1 when anything was found, 0 when clean.

    Looked for:
    - IPv4 addresses other than loopback and 0.0.0.0 (version numbers with
      a part above 255 are not addresses; "runtime x.y.z.w" is a version);
    - IPv6 addresses other than ::1;
    - invites: room invites (zizroom:) and manual invites (ziz1:), which
      let anyone in;
    - e-mail addresses;
    - the words password, secret, token and private key.

    Keep this file ASCII-only: PowerShell 5.1 reads BOM-less files with the
    ANSI code page.

.EXAMPLE
    .\scripts\check-logs.ps1 -Path "$env:LOCALAPPDATA\ziz\profiles"
#>
[CmdletBinding()]
param(
    # A folder (searched recursively) or a log file.
    [Parameter(Mandatory)] [string[]] $Path
)

$ipv4 = '(?<![\d.])(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})(?![\d.])'
# Candidates for IPv6: hex digits and colons with at least two colons;
# .NET decides whether one is an address.
$ipv6 = '(?i)(?<![0-9a-f:])[0-9a-f]{0,4}(?::[0-9a-f]{0,4}){2,7}(?![0-9a-f:])'
$rules = @(
    @{ Name = 'invite'; Pattern = '(?i)zizroom:|ziz1:' },
    @{ Name = 'e-mail'; Pattern = '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}' },
    @{ Name = 'secret word'; Pattern = '(?i)password|secret|token|private[ _-]?key' }
)

function Test-Ipv4([System.Text.RegularExpressions.Match] $match, [string] $line) {
    $parts = 1..4 | ForEach-Object { [int] $match.Groups[$_].Value }
    if ($parts | Where-Object { $_ -gt 255 }) { return $false }
    if ($match.Value -eq '127.0.0.1' -or $match.Value -eq '0.0.0.0') { return $false }
    # A version, not an address: "AMF runtime 1.5.2.0".
    $before = $line.Substring(0, $match.Index)
    if ($before -match '(?i)(runtime|version)[ =:"]*$') { return $false }
    return $true
}

function Test-Ipv6([string] $value) {
    if ($value -eq '::1') { return $false }
    # A bare "::" is Rust's path separator (ziz_app::host), not an address.
    if ($value -notmatch '[0-9a-fA-F]') { return $false }
    # Times (12:34:56) are digits only.
    if ($value -match '^[\d:]+$' -and $value -notmatch '::') { return $false }
    $address = $null
    return [System.Net.IPAddress]::TryParse($value, [ref] $address) -and
        $address.AddressFamily -eq [System.Net.Sockets.AddressFamily]::InterNetworkV6
}

$files = foreach ($p in $Path) {
    if (Test-Path -LiteralPath $p -PathType Leaf) { Get-Item -LiteralPath $p }
    else { Get-ChildItem -LiteralPath $p -Recurse -File -Filter *.log -ErrorAction SilentlyContinue }
}
$found = 0; $lines = 0
foreach ($file in $files) {
    $number = 0
    # Shared read: the running app keeps its own log open for writing.
    $stream = [IO.File]::Open($file.FullName, 'Open', 'Read', 'ReadWrite')
    $reader = New-Object IO.StreamReader($stream)
    $content = $reader.ReadToEnd(); $reader.Dispose()
    foreach ($line in ($content -split "`r?`n")) {
        if ($line -eq '') { continue }
        $number++; $lines++
        $hits = @()
        foreach ($m in [regex]::Matches($line, $ipv4)) { if (Test-Ipv4 $m $line) { $hits += "IPv4 $($m.Value)" } }
        foreach ($m in [regex]::Matches($line, $ipv6)) { if (Test-Ipv6 $m.Value) { $hits += "IPv6 $($m.Value)" } }
        foreach ($rule in $rules) {
            foreach ($m in [regex]::Matches($line, $rule.Pattern)) { $hits += "$($rule.Name) '$($m.Value)'" }
        }
        foreach ($hit in $hits) {
            $found++
            $text = if ($line.Length -gt 160) { $line.Substring(0, 160) + '...' } else { $line }
            Write-Host ("{0}:{1}: {2}`n    {3}" -f $file.FullName, $number, $hit, $text) -ForegroundColor Red
        }
    }
}
$count = @($files).Count
if ($found -gt 0) {
    Write-Host "$found finding(s) in $count file(s), $lines line(s)." -ForegroundColor Red
    exit 1
}
Write-Host "Clean: $count file(s), $lines line(s)." -ForegroundColor Green
exit 0
